//! Minimal UE5 reflection over [`Memory`]: GObjects iteration, FNamePool name resolution,
//! class / function / property lookup. All reads are fault-tolerant.

use crate::mem::{is_plausible_ptr, Memory};
use crate::sigs::{FNamePoolLayout, GObjectsLayout, UObjectLayout};
use std::collections::HashMap;
use std::sync::Mutex;

/// `RF_ClassDefaultObject`.
pub const RF_CLASS_DEFAULT_OBJECT: u32 = 0x10;
/// Upper bound on chain walks (super classes, field lists) to survive corrupt data.
const MAX_WALK: usize = 4096;

/// Addresses of the engine globals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UeGlobals {
    /// Address of `FUObjectArray::ObjObjects` (an `FChunkedFixedUObjectArray`).
    pub gobjects: usize,
    /// Address of the `FNamePool` singleton.
    pub fnamepool: usize,
    /// Address of the `UWorld* GWorld` variable.
    pub gworld: Option<usize>,
    /// `UObject::ProcessEvent`.
    pub process_event: Option<usize>,
}

/// Parameter / member property of a UStruct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropInfo {
    pub name: String,
    pub offset: usize,
    pub size: usize,
    pub flags: u64,
}

/// A resolved UFunction with its parameter layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionInfo {
    pub addr: usize,
    pub name: String,
    pub parms_size: usize,
    pub params: Vec<PropInfo>,
}

impl FunctionInfo {
    pub fn param(&self, name: &str) -> Option<&PropInfo> {
        self.params.iter().find(|p| p.name == name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjHeader {
    pub obj: usize,
    pub class: usize,
    pub flags: u32,
}

#[derive(Clone, Debug, Default)]
pub struct ObjectIndex {
    pub classes: HashMap<String, usize>,
    /// UClass -> its class default object.
    pub cdos: HashMap<usize, usize>,
    pub object_count: usize,
}

impl ObjectIndex {
    pub fn class(&self, name: &str) -> Option<usize> {
        self.classes.get(name).copied()
    }

    pub fn cdo_of(&self, name: &str) -> Option<usize> {
        self.cdos.get(&self.class(name)?).copied()
    }
}

pub struct Ue<M: Memory> {
    pub mem: M,
    pub globals: UeGlobals,
    pub objects: GObjectsLayout,
    pub names: FNamePoolLayout,
    pub layout: UObjectLayout,
    name_cache: Mutex<HashMap<u32, String>>,
}

impl<M: Memory> Ue<M> {
    pub fn new(
        mem: M,
        globals: UeGlobals,
        objects: GObjectsLayout,
        names: FNamePoolLayout,
        layout: UObjectLayout,
    ) -> Self {
        Self { mem, globals, objects, names, layout, name_cache: Mutex::new(HashMap::new()) }
    }

    // ---- GObjects ------------------------------------------------------------------------

    pub fn num_objects(&self) -> Option<usize> {
        let n = self.mem.read_i32(self.globals.gobjects + self.objects.num_elements_offset)?;
        usize::try_from(n).ok()
    }

    fn chunk_table(&self) -> Option<usize> {
        self.mem.read_valid_ptr(self.globals.gobjects + self.objects.objects_offset)
    }

    /// Object at `index`, or `None` for empty slots / unreadable memory.
    pub fn object_at(&self, index: usize) -> Option<usize> {
        let per = self.objects.elements_per_chunk.max(1);
        let chunk = self.mem.read_valid_ptr(self.chunk_table()? + (index / per) * 8)?;
        let item = chunk + (index % per) * self.objects.fuobjectitem_size;
        self.mem.read_valid_ptr(item + self.objects.fuobjectitem_object_offset)
    }

    /// Every non-null object pointer, read chunk-by-chunk in bulk.
    pub fn all_objects(&self) -> Option<Vec<usize>> {
        let n = self.num_objects()?;
        let table = self.chunk_table()?;
        let per = self.objects.elements_per_chunk.max(1);
        let isz = self.objects.fuobjectitem_size.max(8);
        let mut out = Vec::with_capacity(n);
        let mut index = 0usize;
        while index < n {
            let count = per.min(n - index);
            if let Some(chunk) = self.mem.read_valid_ptr(table + (index / per) * 8) {
                if let Some(items) = self.mem.read_vec(chunk, count * isz) {
                    for i in 0..count {
                        let o = i * isz + self.objects.fuobjectitem_object_offset;
                        let p = u64::from_le_bytes(items[o..o + 8].try_into().ok()?) as usize;
                        if is_plausible_ptr(p) {
                            out.push(p);
                        }
                    }
                }
            }
            index += count;
        }
        Some(out)
    }

    /// `(object, class, flags)` for every object, one read per object.
    pub fn object_headers(&self) -> Option<Vec<ObjHeader>> {
        let l = &self.layout;
        let span = (l.class + 8).max(l.object_flags + 4);
        let mut buf = vec![0u8; span];
        let mut out = Vec::new();
        for obj in self.all_objects()? {
            if !self.mem.read(obj, &mut buf) {
                continue;
            }
            let class = u64::from_le_bytes(buf[l.class..l.class + 8].try_into().ok()?) as usize;
            let flags = u32::from_le_bytes(buf[l.object_flags..l.object_flags + 4].try_into().ok()?);
            out.push(ObjHeader { obj, class, flags });
        }
        Some(out)
    }

    /// Name -> UClass and UClass -> CDO maps, built in one pass over GObjects.
    pub fn build_index(&self) -> Option<ObjectIndex> {
        let headers = self.object_headers()?;
        let class_class =
            headers.iter().find(|h| h.class == h.obj && self.name(h.obj).as_deref() == Some("Class"))?.obj;
        let mut idx = ObjectIndex::default();
        for h in &headers {
            if h.class == class_class {
                if let Some(n) = self.name(h.obj) {
                    idx.classes.entry(n).or_insert(h.obj);
                }
            }
            if h.flags & RF_CLASS_DEFAULT_OBJECT != 0 && is_plausible_ptr(h.class) {
                idx.cdos.entry(h.class).or_insert(h.obj);
            }
        }
        idx.object_count = headers.len();
        Some(idx)
    }

    // ---- Names ---------------------------------------------------------------------------

    /// Resolves an FName comparison index through the FNamePool.
    pub fn name_entry(&self, index: u32) -> Option<String> {
        if let Some(s) = self.name_cache.lock().ok()?.get(&index) {
            return Some(s.clone());
        }
        let l = &self.names;
        let block = (index >> l.block_offset_bits) as usize;
        let offset = (index & ((1u32 << l.block_offset_bits) - 1)) as usize * l.entry_stride;
        let block_ptr = self.mem.read_valid_ptr(self.globals.fnamepool + l.blocks_offset + block * 8)?;
        let entry = block_ptr + offset;
        let header = self.mem.read_u16(entry)?;
        let wide = header & l.header_wide_mask != 0;
        let len = (header >> l.header_len_shift) as usize;
        if len == 0 || len > 1024 {
            return None;
        }
        let data = entry + l.header_size;
        let s = if wide {
            let raw = self.mem.read_vec(data, len * 2)?;
            let (pairs, _) = raw.as_chunks::<2>();
            let units: Vec<u16> = pairs.iter().map(|c| u16::from_le_bytes(*c)).collect();
            String::from_utf16_lossy(&units)
        } else {
            let raw = self.mem.read_vec(data, len)?;
            raw.iter().map(|&b| b as char).collect()
        };
        if let Ok(mut c) = self.name_cache.lock() {
            c.insert(index, s.clone());
        }
        Some(s)
    }

    /// Reads an `FName { ComparisonIndex, Number }` at `addr`.
    pub fn fname(&self, addr: usize) -> Option<String> {
        let idx = self.mem.read_u32(addr)?;
        let number = self.mem.read_u32(addr + 4)?;
        let base = self.name_entry(idx)?;
        Some(if number > 0 { format!("{base}_{}", number - 1) } else { base })
    }

    // ---- UObject -------------------------------------------------------------------------

    pub fn name(&self, obj: usize) -> Option<String> {
        self.fname(obj + self.layout.name)
    }

    pub fn class_of(&self, obj: usize) -> Option<usize> {
        self.mem.read_valid_ptr(obj + self.layout.class)
    }

    pub fn class_name(&self, obj: usize) -> Option<String> {
        self.name(self.class_of(obj)?)
    }

    pub fn outer(&self, obj: usize) -> Option<usize> {
        self.mem.read_valid_ptr(obj + self.layout.outer)
    }

    pub fn flags(&self, obj: usize) -> Option<u32> {
        self.mem.read_u32(obj + self.layout.object_flags)
    }

    pub fn is_cdo(&self, obj: usize) -> bool {
        self.flags(obj).is_none_or(|f| f & RF_CLASS_DEFAULT_OBJECT != 0)
    }

    /// `Outer.Outer.Name` path (without class).
    pub fn path_name(&self, obj: usize) -> Option<String> {
        let mut parts = vec![self.name(obj)?];
        let mut cur = self.outer(obj);
        let mut guard = 0;
        while let Some(o) = cur {
            parts.push(self.name(o)?);
            cur = self.outer(o);
            guard += 1;
            if guard > 64 {
                break;
            }
        }
        parts.reverse();
        Some(parts.join("."))
    }

    pub fn super_of(&self, ustruct: usize) -> Option<usize> {
        self.mem.read_valid_ptr(ustruct + self.layout.ustruct_super)
    }

    /// True if `class` is `target` or derives from it.
    pub fn class_is_a(&self, class: usize, target: usize) -> bool {
        let mut cur = Some(class);
        for _ in 0..MAX_WALK {
            match cur {
                Some(c) if c == target => return true,
                Some(c) => cur = self.super_of(c),
                None => return false,
            }
        }
        false
    }

    pub fn is_a(&self, obj: usize, class: usize) -> bool {
        self.class_of(obj).is_some_and(|c| self.class_is_a(c, class))
    }

    /// Names of `class` and its super classes, most derived first.
    pub fn class_hierarchy(&self, class: usize) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = Some(class);
        while let Some(c) = cur {
            match self.name(c) {
                Some(n) => out.push(n),
                None => break,
            }
            cur = self.super_of(c);
            if out.len() > 64 {
                break;
            }
        }
        out
    }

    /// First object whose class is named `class_name` and whose name is `name`.
    pub fn find_object(&self, class_name: &str, name: &str) -> Option<usize> {
        self.all_objects()?
            .into_iter()
            .find(|&o| self.name(o).as_deref() == Some(name) && self.class_name(o).as_deref() == Some(class_name))
    }

    pub fn find_class(&self, name: &str) -> Option<usize> {
        self.find_object("Class", name)
    }

    /// First live (non-CDO) instance whose class is named `class_name` or derives from a
    /// class with that name.
    pub fn find_live_instance_of(&self, class: usize) -> Option<usize> {
        self.all_objects()?.into_iter().find(|&o| self.is_a(o, class) && !self.is_cdo(o))
    }

    /// `Default__<Class>`.
    pub fn find_cdo(&self, class_name: &str) -> Option<usize> {
        self.find_object(class_name, &format!("Default__{class_name}"))
    }

    // ---- UStruct members -----------------------------------------------------------------

    /// UFunction named `name` declared on `class` or any super class.
    pub fn find_function_in(&self, class: usize, name: &str) -> Option<usize> {
        let mut cls = Some(class);
        let mut depth = 0;
        while let Some(c) = cls {
            let mut field = self.mem.read_valid_ptr(c + self.layout.ustruct_children);
            let mut n = 0;
            while let Some(f) = field {
                if self.name(f).as_deref() == Some(name) {
                    return Some(f);
                }
                field = self.mem.read_valid_ptr(f + self.layout.ufield_next);
                n += 1;
                if n > MAX_WALK {
                    break;
                }
            }
            cls = self.super_of(c);
            depth += 1;
            if depth > 64 {
                break;
            }
        }
        None
    }

    pub fn find_function(&self, class_name: &str, name: &str) -> Option<usize> {
        self.find_function_in(self.find_class(class_name)?, name)
    }

    fn props_of(&self, ustruct: usize, out: &mut Vec<PropInfo>) {
        let l = &self.layout;
        let mut field = self.mem.read_valid_ptr(ustruct + l.ustruct_child_properties);
        let mut n = 0;
        while let Some(f) = field {
            let (Some(name), Some(offset), Some(size), Some(flags)) = (
                self.fname(f + l.ffield_name),
                self.mem.read_i32(f + l.fproperty_offset),
                self.mem.read_i32(f + l.fproperty_element_size),
                self.mem.read_u64(f + l.fproperty_flags),
            ) else {
                break;
            };
            if let (Ok(offset), Ok(size)) = (usize::try_from(offset), usize::try_from(size)) {
                out.push(PropInfo { name, offset, size, flags });
            }
            field = self.mem.read_valid_ptr(f + l.ffield_next);
            n += 1;
            if n > MAX_WALK {
                break;
            }
        }
    }

    /// Property `name` on `ustruct` or a super struct.
    pub fn find_property(&self, ustruct: usize, name: &str) -> Option<PropInfo> {
        let mut cur = Some(ustruct);
        let mut depth = 0;
        while let Some(s) = cur {
            let mut props = Vec::new();
            self.props_of(s, &mut props);
            if let Some(p) = props.into_iter().find(|p| p.name == name) {
                return Some(p);
            }
            cur = self.super_of(s);
            depth += 1;
            if depth > 64 {
                break;
            }
        }
        None
    }

    pub fn function_info(&self, func: usize) -> Option<FunctionInfo> {
        let parms_size = self.mem.read_i32(func + self.layout.ustruct_properties_size)?;
        let mut params = Vec::new();
        self.props_of(func, &mut params);
        Some(FunctionInfo { addr: func, name: self.name(func)?, parms_size: usize::try_from(parms_size).ok()?, params })
    }

    /// Reads an object pointer member found by reflection.
    pub fn read_object_property(&self, obj: usize, prop: &str) -> Option<usize> {
        let class = self.class_of(obj)?;
        let p = self.find_property(class, prop)?;
        self.mem.read_valid_ptr(obj + p.offset)
    }

    /// Current `UWorld*`.
    pub fn world(&self) -> Option<usize> {
        self.mem.read_valid_ptr(self.globals.gworld?)
    }
}

/// Validates the FUObjectItem stride: `GObjects[i]->InternalIndex == i` for the first slots.
pub fn detect_item_size(
    mem: &dyn Memory,
    chunk0: usize,
    object_offset: usize,
    internal_index_offset: usize,
    candidates: &[usize],
) -> Option<usize> {
    candidates.iter().copied().find(|&size| {
        (1..=8).all(|i| {
            mem.read_valid_ptr(chunk0 + i * size + object_offset).and_then(|o| mem.read_i32(o + internal_index_offset))
                == Some(i as i32)
        })
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::mem::SliceMemory;

    /// Fake UE object graph builder over a [`SliceMemory`] heap.
    pub struct FakeUe {
        pub mem: SliceMemory,
        pub cursor: usize,
        pub objects: Vec<usize>,
        pub layout: UObjectLayout,
        pub gobjects: usize,
        pub pool: usize,
        pub name_block: usize,
        pub name_cursor: usize,
        pub chunk: usize,
    }

    pub const HEAP: usize = 0x10_0000_0000;

    impl FakeUe {
        pub fn new() -> Self {
            let mem = SliceMemory::new(HEAP, vec![0u8; 0x40_0000]);
            let mut f = Self {
                mem,
                cursor: HEAP + 0x100,
                objects: Vec::new(),
                layout: UObjectLayout::default(),
                gobjects: 0,
                pool: 0,
                name_block: 0,
                name_cursor: 0,
                chunk: 0,
            };
            f.gobjects = f.alloc(0x40);
            f.pool = f.alloc(0x100);
            f.name_block = f.alloc(0x2_0000);
            f.chunk = f.alloc(0x18 * 4096);
            let table = f.alloc(8);
            f.w64(table, f.chunk as u64);
            f.w64(f.gobjects, table as u64);
            f.w32(f.gobjects + 0x10, 65536);
            f.w32(f.gobjects + 0x18, 1);
            f.w32(f.gobjects + 0x1C, 1);
            f.w64(f.pool + 0x10, f.name_block as u64);
            f.add_name("None");
            f
        }

        pub fn alloc(&mut self, size: usize) -> usize {
            let a = (self.cursor + 15) & !15;
            self.cursor = a + size;
            a
        }
        pub fn w32(&self, a: usize, v: u32) {
            assert!(self.mem.write_u32(a, v));
        }
        pub fn w64(&self, a: usize, v: u64) {
            assert!(self.mem.write(a, &v.to_le_bytes()));
        }

        /// Appends an ANSI entry to block 0 and returns its FName index.
        pub fn add_name(&mut self, s: &str) -> u32 {
            let idx = (self.name_cursor / 2) as u32;
            let header = ((s.len() as u16) << 6).to_le_bytes();
            let at = self.name_block + self.name_cursor;
            assert!(self.mem.write(at, &header));
            assert!(self.mem.write(at + 2, s.as_bytes()));
            self.name_cursor += (2 + s.len() + 1) & !1;
            idx
        }

        pub fn add_wide_name(&mut self, s: &str) -> u32 {
            let idx = (self.name_cursor / 2) as u32;
            let units: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
            let header = (((s.encode_utf16().count() as u16) << 6) | 1).to_le_bytes();
            let at = self.name_block + self.name_cursor;
            assert!(self.mem.write(at, &header));
            assert!(self.mem.write(at + 2, &units));
            self.name_cursor += 2 + units.len();
            idx
        }

        /// Creates a UObject (0x100 bytes) registered in GObjects.
        pub fn object(&mut self, name: &str, class: usize, outer: usize, flags: u32) -> usize {
            let o = self.alloc(0x100);
            let idx = self.add_name(name);
            let index = self.objects.len();
            self.w64(o + self.layout.vtable, 0x1111_0000);
            self.w32(o + self.layout.object_flags, flags);
            self.w32(o + self.layout.internal_index, index as u32);
            self.w64(o + self.layout.class, class as u64);
            self.w32(o + self.layout.name, idx);
            self.w64(o + self.layout.outer, outer as u64);
            self.w64(self.chunk + index * 0x18, o as u64);
            self.objects.push(o);
            self.w32(self.gobjects + 0x14, self.objects.len() as u32);
            o
        }

        pub fn set_class(&self, obj: usize, class: usize) {
            self.w64(obj + self.layout.class, class as u64);
        }
        pub fn set_super(&self, s: usize, sup: usize) {
            self.w64(s + self.layout.ustruct_super, sup as u64);
        }

        /// Adds a UFunction to `class`'s Children list (prepends).
        pub fn function(&mut self, class: usize, func_class: usize, name: &str, parms_size: u32) -> usize {
            let f = self.object(name, func_class, class, 0);
            let head = self.mem.read_u64(class + self.layout.ustruct_children).unwrap();
            self.w64(f + self.layout.ufield_next, head);
            self.w64(class + self.layout.ustruct_children, f as u64);
            self.w32(f + self.layout.ustruct_properties_size, parms_size);
            f
        }

        /// Appends an FProperty to `ustruct`'s ChildProperties chain.
        pub fn property(&mut self, ustruct: usize, name: &str, offset: u32, size: u32) -> usize {
            let p = self.alloc(0x80);
            let idx = self.add_name(name);
            self.w32(p + self.layout.ffield_name, idx);
            self.w32(p + self.layout.fproperty_offset, offset);
            self.w32(p + self.layout.fproperty_element_size, size);
            let mut slot = ustruct + self.layout.ustruct_child_properties;
            while let Some(next) = self.mem.read_valid_ptr(slot) {
                slot = next + self.layout.ffield_next;
            }
            self.w64(slot, p as u64);
            p
        }

        pub fn ue(&self) -> Ue<&SliceMemory> {
            Ue::new(
                &self.mem,
                UeGlobals { gobjects: self.gobjects, fnamepool: self.pool, gworld: None, process_event: None },
                GObjectsLayout::default(),
                FNamePoolLayout::default(),
                self.layout,
            )
        }
    }

    /// Class graph: Class, Object, Actor : Object, Pawn : Actor, MyCar : Pawn, Function.
    pub struct Graph {
        pub class: usize,
        pub object: usize,
        pub actor: usize,
        pub pawn: usize,
        pub car: usize,
        pub function: usize,
    }

    pub fn graph(f: &mut FakeUe) -> Graph {
        let class = f.object("Class", 0, 0, 0);
        f.set_class(class, class);
        let object = f.object("Object", class, 0, 0);
        let actor = f.object("Actor", class, 0, 0);
        f.set_super(actor, object);
        let pawn = f.object("Pawn", class, 0, 0);
        f.set_super(pawn, actor);
        let car = f.object("MyCar", class, 0, 0);
        f.set_super(car, pawn);
        let function = f.object("Function", class, 0, 0);
        Graph { class, object, actor, pawn, car, function }
    }

    #[test]
    fn names_and_objects() {
        let mut f = FakeUe::new();
        let g = graph(&mut f);
        let wide = f.add_wide_name("Wïde");
        let pkg = f.object("Package_A", g.object, 0, 0);
        let car_inst = f.object("MyCar", g.car, pkg, 0);
        // FName number suffix
        f.w32(car_inst + f.layout.name + 4, 3);
        let ue = f.ue();
        assert_eq!(ue.num_objects(), Some(f.objects.len()));
        assert_eq!(ue.object_at(0), Some(g.class));
        assert_eq!(ue.object_at(9999), None);
        assert_eq!(ue.all_objects().unwrap(), f.objects);
        assert_eq!(ue.name_entry(0).as_deref(), Some("None"));
        assert_eq!(ue.name_entry(wide).as_deref(), Some("Wïde"));
        assert_eq!(ue.name(car_inst).as_deref(), Some("MyCar_2"));
        assert_eq!(ue.class_name(car_inst).as_deref(), Some("MyCar"));
        assert_eq!(ue.path_name(car_inst).as_deref(), Some("Package_A.MyCar_2"));
        assert_eq!(ue.find_class("Pawn"), Some(g.pawn));
        assert_eq!(ue.find_class("Nope"), None);
        assert!(ue.is_a(car_inst, g.actor));
        assert!(!ue.is_a(car_inst, g.function));
        assert_eq!(ue.class_hierarchy(g.car), vec!["MyCar", "Pawn", "Actor", "Object"]);
        assert_eq!(ue.find_live_instance_of(g.pawn), Some(car_inst));
    }

    #[test]
    fn cdo_lookup_and_flags() {
        let mut f = FakeUe::new();
        let g = graph(&mut f);
        let gs = f.object("GameplayStatics", g.class, 0, 0);
        let cdo = f.object("Default__GameplayStatics", gs, 0, RF_CLASS_DEFAULT_OBJECT);
        let ue = f.ue();
        let idx = ue.build_index().unwrap();
        assert_eq!(idx.class("GameplayStatics"), Some(gs));
        assert_eq!(idx.class("Pawn"), Some(g.pawn));
        assert_eq!(idx.cdo_of("GameplayStatics"), Some(cdo));
        assert_eq!(idx.cdo_of("Pawn"), None);
        assert_eq!(idx.object_count, f.objects.len());
        assert_eq!(ue.find_cdo("GameplayStatics"), Some(cdo));
        assert!(ue.is_cdo(cdo));
        assert!(!ue.is_cdo(gs));
        assert_eq!(ue.find_live_instance_of(gs), None);
    }

    #[test]
    fn functions_and_properties() {
        let mut f = FakeUe::new();
        let g = graph(&mut f);
        let func = f.function(g.actor, g.function, "K2_GetActorLocation", 24);
        f.property(func, "ReturnValue", 0, 24);
        let set = f.function(g.actor, g.function, "K2_SetActorLocationAndRotation", 0x120);
        f.property(set, "NewLocation", 0, 24);
        f.property(set, "NewRotation", 24, 24);
        f.property(set, "bSweep", 48, 1);
        f.property(g.actor, "CustomTimeDilation", 0x64, 4);
        f.property(g.car, "Mesh", 0x300, 8);
        let ue = f.ue();
        assert_eq!(ue.find_function("MyCar", "K2_GetActorLocation"), Some(func));
        assert_eq!(ue.find_function("Actor", "Missing"), None);
        let info = ue.function_info(set).unwrap();
        assert_eq!(info.name, "K2_SetActorLocationAndRotation");
        assert_eq!(info.parms_size, 0x120);
        assert_eq!(info.params.len(), 3);
        assert_eq!(info.param("NewRotation").unwrap().offset, 24);
        assert_eq!(info.param("bSweep").unwrap().size, 1);
        assert_eq!(ue.find_property(g.car, "CustomTimeDilation").unwrap().offset, 0x64);
        assert_eq!(ue.find_property(g.car, "Mesh").unwrap().offset, 0x300);
        assert!(ue.find_property(g.actor, "Mesh").is_none());
    }

    #[test]
    fn item_size_detection() {
        let mut f = FakeUe::new();
        let g = graph(&mut f);
        let _ = g;
        for i in 0..4 {
            f.object(&format!("Extra{i}"), 0, 0, 0);
        }
        let d = detect_item_size(&f.mem, f.chunk, 0, f.layout.internal_index, &[0x10, 0x18, 0x20]);
        assert_eq!(d, Some(0x18));
        assert_eq!(detect_item_size(&f.mem, f.chunk, 0, f.layout.internal_index, &[0x10, 0x20]), None);
    }
}
