//! Resolution of the engine globals (GObjects, FNamePool, GWorld, ProcessEvent).
//!
//! Per section: `[raw_offsets]` override -> configured strategy -> fallback -> error.
//! Every function works on [`Region`]s + [`Memory`] so it can be tested on synthetic bytes.

use crate::mem::{is_plausible_ptr, Memory};
use crate::pattern::{resolve_first, Pattern, Region};
use crate::pe::ModuleInfo;
use crate::reflection::{detect_item_size, Ue, UeGlobals};
use crate::sigs::{FNamePoolLayout, GObjectsLayout, Signatures};

/// A resolved address plus a human-readable description of how it was found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub addr: usize,
    pub how: String,
}

fn found(addr: usize, how: impl Into<String>) -> Found {
    Found { addr, how: how.into() }
}

/// Inputs shared by all resolvers.
pub struct Scan<'a> {
    pub mem: &'a dyn Memory,
    pub module: &'a ModuleInfo,
    /// Code sections (`[module].scan_sections`).
    pub code: Vec<Region<'a>>,
    /// Copy of `.data` for the heuristics (absent if the section is missing / unreadable).
    pub data: Option<Region<'a>>,
    /// Addresses of `InitializeSRWLock` / `RtlInitializeSRWLock` (IAT targets).
    pub srwlock_fns: Vec<usize>,
    pub sigs: &'a Signatures,
}

// ---- GObjects ---------------------------------------------------------------------------

/// Validates an `FChunkedFixedUObjectArray` at `addr` (Dumper-7's checks). Returns the
/// implied elements-per-chunk.
pub fn validate_chunked_array(mem: &dyn Memory, addr: usize, l: &GObjectsLayout) -> Option<usize> {
    let objects = mem.read_ptr(addr + l.objects_offset)?;
    let max = mem.read_i32(addr + l.max_elements_offset)?;
    let num = mem.read_i32(addr + l.num_elements_offset)?;
    let max_chunks = mem.read_i32(addr + l.max_chunks_offset)?;
    let num_chunks = mem.read_i32(addr + l.num_chunks_offset)?;
    if !is_plausible_ptr(objects) || num <= 0 || max <= 0 || num > max {
        return None;
    }
    if !(1..=0x14).contains(&num_chunks) || max_chunks < num_chunks || max_chunks <= 0 {
        return None;
    }
    let per = (max / max_chunks) as usize;
    if !(0x8000..=0x80000).contains(&per) || !per.is_multiple_of(0x10) {
        return None;
    }
    if num as usize / per + 1 != num_chunks as usize {
        return None;
    }
    let chunk0 = mem.read_ptr(objects)?;
    is_plausible_ptr(chunk0).then_some(per)
}

/// Human-readable verdict on `addr` as ObjObjects (shared by the hook's errors and sig-test).
pub fn describe_gobjects(mem: &dyn Memory, addr: usize, l: &GObjectsLayout) -> (bool, String) {
    match validate_chunked_array(mem, addr, l) {
        Some(per) => {
            let num = mem.read_i32(addr + l.num_elements_offset).unwrap_or(-1);
            let ok = (1000..20_000_000).contains(&num);
            (ok, format!("FChunkedFixedUObjectArray ok: NumElements {num}, {per} per chunk"))
        }
        None => {
            let rd = |o: usize| mem.read_i32(addr + o).map_or("unreadable".to_string(), |v| v.to_string());
            (
                false,
                format!(
                    "not an FChunkedFixedUObjectArray (Objects {}, MaxElements {}, NumElements {}, MaxChunks {}, NumChunks {})",
                    mem.read_ptr(addr + l.objects_offset).map_or("unreadable".into(), |p| format!("{p:#x}")),
                    rd(l.max_elements_offset),
                    rd(l.num_elements_offset),
                    rd(l.max_chunks_offset),
                    rd(l.num_chunks_offset)
                ),
            )
        }
    }
}

/// Dumper-7 style scan of `.data` for the chunked object array (4-byte aligned).
pub fn heuristic_gobjects(mem: &dyn Memory, data: &Region<'_>, l: &GObjectsLayout) -> Option<(usize, usize)> {
    let span = l.num_chunks_offset + 4;
    let end = data.bytes.len().checked_sub(span)?;
    (0..=end).step_by(4).find_map(|off| {
        let b = &data.bytes[off..];
        // Cheap pre-filter on the local copy before touching live memory.
        let rd = |o: usize| i32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let nc = rd(l.num_chunks_offset);
        if !(1..=0x14).contains(&nc) || rd(l.num_elements_offset) <= 0 {
            return None;
        }
        let addr = data.base + off;
        validate_chunked_array(mem, addr, l).map(|per| (addr, per))
    })
}

/// Resolves GObjects and returns it with the effective layout (chunk size / item size may be
/// auto-detected).
pub fn resolve_gobjects(s: &Scan<'_>) -> Result<(Found, GObjectsLayout), String> {
    let spec = &s.sigs.gobjects;
    let mut layout = spec.layout;
    let u = &s.sigs.raw_offsets.uobject;
    let finish = |f: Found, mut layout: GObjectsLayout| -> Result<(Found, GObjectsLayout), String> {
        let per = validate_chunked_array(s.mem, f.addr, &layout)
            .ok_or_else(|| format!("{:#x} ({}) is not a valid FChunkedFixedUObjectArray (yet?)", f.addr, f.how))?;
        layout.elements_per_chunk = per;
        let chunk0 = s.mem.read_ptr(s.mem.read_ptr(f.addr + layout.objects_offset).unwrap_or(0)).unwrap_or(0);
        let mut sizes = vec![layout.fuobjectitem_size];
        sizes.extend([0x18, 0x10, 0x20, 0x28].iter().filter(|&&x| x != layout.fuobjectitem_size));
        match detect_item_size(s.mem, chunk0, layout.fuobjectitem_object_offset, u.internal_index, &sizes) {
            Some(sz) => layout.fuobjectitem_size = sz,
            None => {
                return Err(format!(
                    "GObjects at {:#x}: FUObjectItem size not detectable (InternalIndex check failed)",
                    f.addr
                ))
            }
        }
        Ok((f, layout))
    };
    let mut errors = Vec::new();
    if let Some(rva) = s.sigs.raw_offsets.gobjects_rva {
        // Dumper-7's Offsets::GObjects is ObjObjects itself, used exactly as given (acr-probe
        // sig-test runs this same function). If it does not validate (engine not up yet, or a
        // wrong RVA) the configured strategy is tried before reporting both.
        let at = s.module.rva(rva);
        match finish(found(at, format!("raw_offsets.gobjects_rva {rva:#x}")), layout) {
            Ok(r) => return Ok(r),
            Err(e) => errors.push(format!("{e}: {}", describe_gobjects(s.mem, at, &layout).1)),
        }
    }
    let try_heuristic = |errors: &mut Vec<String>| match s.data.as_ref() {
        Some(d) => match heuristic_gobjects(s.mem, d, &spec.layout) {
            Some((addr, per)) => Some((found(addr, "heuristic .data scan"), per)),
            None => {
                errors.push("heuristic: no FChunkedFixedUObjectArray in .data".into());
                None
            }
        },
        None => {
            errors.push("heuristic: .data unavailable".into());
            None
        }
    };
    match spec.strategy.as_str() {
        "offset" if errors.is_empty() => return Err("strategy = offset but raw_offsets.gobjects_rva is not set".into()),
        "offset" => {}
        "heuristic" => {
            if let Some((f, per)) = try_heuristic(&mut errors) {
                layout.elements_per_chunk = per;
                return finish(f, layout);
            }
        }
        "pattern" => {
            match resolve_first(&spec.candidates, &s.code, s.mem) {
                Ok(addr) => match finish(found(addr, "pattern"), layout) {
                    Ok(r) => return Ok(r),
                    Err(e) => errors.push(e),
                },
                Err(e) => errors.extend(e.iter().map(ToString::to_string)),
            }
            if spec.fallback_heuristic {
                if let Some((f, per)) = try_heuristic(&mut errors) {
                    layout.elements_per_chunk = per;
                    return finish(f, layout);
                }
            }
        }
        other => return Err(format!("unknown gobjects.strategy '{other}'")),
    }
    Err(errors.join("; "))
}

// ---- FNamePool --------------------------------------------------------------------------

/// `lea r64, [rip+disp32]` (REX.W 8D /r with mod=00 rm=101) at `at`; returns the target.
fn lea_rip_target(r: &Region<'_>, at: usize) -> Option<usize> {
    let b = r.slice_from(at, 3)?;
    if b.len() == 3 && (b[0] == 0x48 || b[0] == 0x4C) && b[1] == 0x8D && (b[2] & 0xC7) == 0x05 {
        crate::pattern::rip_target(r, at, 3, 7)
    } else {
        None
    }
}

fn is_string_at(mem: &dyn Memory, addr: usize, s: &str) -> bool {
    let narrow = s.as_bytes();
    if mem.read_vec(addr, narrow.len()).as_deref() == Some(narrow) {
        return true;
    }
    let wide = crate::pe::utf16_bytes(s);
    mem.read_vec(addr, wide.len()).as_deref() == Some(&wide[..])
}

/// Dumper-7: `lea rcx, X; call F` where F calls `InitializeSRWLock` within 0x50 bytes and
/// references `"ByteProperty"` within 0x2A0 bytes. Returns X.
pub fn dumper7_fnamepool(s: &Scan<'_>) -> Option<usize> {
    let pat = Pattern::parse("48 8D 0D ?? ?? ?? ?? E8").ok()?;
    for r in &s.code {
        for off in pat.find_iter(r.bytes) {
            let at = r.base + off;
            let Some(func) = crate::pattern::rip_target(r, at + 7, 1, 5) else {
                continue;
            };
            let Some(region) = s.code.iter().find(|c| c.contains(func)) else {
                continue;
            };
            let calls_srw = (0..0x50).any(|i| {
                let ip = func + i;
                region.slice_from(ip, 2) == Some(&[0xFF, 0x15][..])
                    && crate::pattern::rip_target(region, ip, 2, 6)
                        .and_then(|slot| s.mem.read_ptr(slot))
                        .is_some_and(|t| s.srwlock_fns.contains(&t))
            });
            if !calls_srw {
                continue;
            }
            let refs_byteprop = (0..0x2A0)
                .any(|i| lea_rip_target(region, func + i).is_some_and(|t| is_string_at(s.mem, t, "ByteProperty")));
            if refs_byteprop {
                return crate::pattern::rip_target(r, at, 3, 7);
            }
        }
    }
    None
}

/// Checks that FName index 0 resolves to "None" through the pool at `pool`.
pub fn validate_fnamepool(mem: &dyn Memory, pool: usize, s: &Signatures) -> bool {
    let l = s.fnamepool.layout;
    let Some(block0) = mem.read_valid_ptr(pool + l.blocks_offset) else {
        return false;
    };
    let Some(header) = mem.read_u16(block0) else {
        return false;
    };
    let len = (header >> l.header_len_shift) as usize;
    len == 4 && header & l.header_wide_mask == 0 && mem.read_vec(block0 + l.header_size, 4).as_deref() == Some(b"None")
}

/// Whether `block` starts with the FNameEntry "None" immediately followed by "ByteProperty"
/// (FName indices 0 and 1 of every UE5 name pool).
pub fn is_first_name_block(block: &[u8], l: &FNamePoolLayout) -> bool {
    let entry = |at: usize, s: &[u8]| -> Option<usize> {
        let h = u16::from_le_bytes(block.get(at..at + 2)?.try_into().ok()?);
        let len = (h >> l.header_len_shift) as usize;
        let text = block.get(at + l.header_size..at + l.header_size + s.len())?;
        (h & l.header_wide_mask == 0 && len == s.len() && text == s).then(|| {
            let size = l.header_size + len;
            size.div_ceil(l.entry_stride.max(1)) * l.entry_stride.max(1)
        })
    };
    entry(0, b"None").and_then(|n| entry(n, b"ByteProperty")).is_some()
}

/// The allocator fields around a candidate pool are sane: `CurrentBlock` within the FName block
/// index range, `CurrentByteCursor` within a block, and `Blocks[0..=CurrentBlock]` all pointers.
fn plausible_pool(mem: &dyn Memory, pool: usize, l: &FNamePoolLayout) -> bool {
    let (Some(cur), Some(cursor)) =
        (mem.read_u32(pool + l.current_block_offset), mem.read_u32(pool + l.current_byte_cursor_offset))
    else {
        return false;
    };
    let block_bytes = (l.entry_stride as u64) << l.block_offset_bits;
    cur < 8192
        && u64::from(cursor) <= block_bytes
        && (0..=cur as usize).all(|i| mem.read_valid_ptr(pool + l.blocks_offset + i * 8).is_some())
}

/// Fallback for when no pattern / RVA works: a `.data` qword pointing at a block that starts
/// with "None", "ByteProperty" is `Blocks[0]`; the pool is that slot minus `blocks_offset`.
pub fn data_scan_fnamepool(mem: &dyn Memory, data: &Region<'_>, l: &FNamePoolLayout) -> Option<usize> {
    // Block test per pointer value (many slots share pointers); the pool test is per slot.
    let mut is_block: std::collections::HashMap<usize, bool> = std::collections::HashMap::new();
    let mut head = [0u8; 32];
    for (i, w) in data.bytes.as_chunks::<8>().0.iter().enumerate() {
        let p = u64::from_le_bytes(*w) as usize;
        if !is_plausible_ptr(p) || data.contains(p) || !p.is_multiple_of(2) {
            continue;
        }
        let Some(pool) = (data.base + i * 8).checked_sub(l.blocks_offset) else { continue };
        let block = *is_block.entry(p).or_insert_with(|| mem.read(p, &mut head) && is_first_name_block(&head, l));
        if block && plausible_pool(mem, pool, l) {
            return Some(pool);
        }
    }
    None
}

/// Human-readable verdict on `pool` as the FNamePool (shared by the hook's errors and sig-test).
pub fn describe_fnamepool(mem: &dyn Memory, pool: usize, s: &Signatures) -> (bool, String) {
    let l = &s.fnamepool.layout;
    let Some(b0) = mem.read_valid_ptr(pool + l.blocks_offset) else {
        return (false, format!("Blocks[0] at +{:#x} is not a pointer", l.blocks_offset));
    };
    let mut head = [0u8; 32];
    if !mem.read(b0, &mut head) {
        return (false, format!("Blocks[0] {b0:#x} unreadable"));
    }
    if !validate_fnamepool(mem, pool, s) {
        return (false, format!("FName 0 is not \"None\" (Blocks[0] {b0:#x} starts {:02x?})", &head[..8]));
    }
    let both = is_first_name_block(&head, l);
    let sane = plausible_pool(mem, pool, l);
    (
        both && sane,
        format!(
            "block 0 starts with \"None\"; \"ByteProperty\" next: {both}; CurrentBlock {:?}, allocator fields sane: {sane}",
            mem.read_u32(pool + l.current_block_offset)
        ),
    )
}

pub fn resolve_fnamepool(s: &Scan<'_>) -> Result<Found, String> {
    let spec = &s.sigs.fnamepool;
    let check = |f: Found| -> Result<Found, String> {
        if validate_fnamepool(s.mem, f.addr, s.sigs) {
            Ok(f)
        } else {
            Err(format!("{:#x} ({}): {}", f.addr, f.how, describe_fnamepool(s.mem, f.addr, s.sigs).1))
        }
    };
    let mut errors = Vec::new();
    if let Some(rva) = s.sigs.raw_offsets.gnames_rva {
        // A wrong RVA (e.g. Dumper-7's GNames, which is not always the pool) falls through to
        // the scans instead of blocking init; the log names both.
        match check(found(s.module.rva(rva), format!("raw_offsets.gnames_rva {rva:#x}"))) {
            Ok(f) => return Ok(f),
            Err(e) => errors.push(e),
        }
    }
    let data_scan = |errors: &mut Vec<String>| -> Option<Found> {
        let Some(d) = s.data.as_ref() else {
            errors.push("data scan: .data unavailable".into());
            return None;
        };
        match data_scan_fnamepool(s.mem, d, &spec.layout) {
            Some(pool) => match check(found(pool, "data scan (Blocks[0] -> \"None\", \"ByteProperty\")")) {
                Ok(f) => Some(f),
                Err(e) => {
                    errors.push(e);
                    None
                }
            },
            None => {
                errors.push("data scan: no .data pointer to a \"None\", \"ByteProperty\" name block".into());
                None
            }
        }
    };
    let pattern = |errors: &mut Vec<String>| -> Option<Found> {
        match resolve_first(&spec.candidates, &s.code, s.mem) {
            Ok(a) => match check(found(a, "pattern")) {
                Ok(f) => Some(f),
                Err(e) => {
                    errors.push(e);
                    None
                }
            },
            Err(e) => {
                errors.extend(e.iter().map(ToString::to_string));
                None
            }
        }
    };
    match spec.strategy.as_str() {
        "offset" if errors.is_empty() => return Err("strategy = offset but raw_offsets.gnames_rva is not set".into()),
        "offset" => {}
        "data_scan" => {}
        "pattern" => {
            if let Some(f) = pattern(&mut errors) {
                return Ok(f);
            }
        }
        "dumper7" => {
            match dumper7_fnamepool(s) {
                Some(a) => match check(found(a, "dumper7 SRWLock/ByteProperty scan")) {
                    Ok(f) => return Ok(f),
                    Err(e) => errors.push(e),
                },
                None => errors.push("dumper7 scan: no matching FNamePool constructor".into()),
            }
            if spec.fallback_pattern {
                if let Some(f) = pattern(&mut errors) {
                    return Ok(f);
                }
            }
        }
        other => return Err(format!("unknown fnamepool.strategy '{other}'")),
    }
    if spec.fallback_data_scan || spec.strategy == "data_scan" {
        if let Some(f) = data_scan(&mut errors) {
            return Ok(f);
        }
    }
    Err(errors.join("; "))
}

// ---- GWorld -----------------------------------------------------------------------------

/// Finds `.data` slots (8-byte aligned) holding a pointer to a live, non-CDO `World`.
pub fn reflection_gworld<M: Memory>(ue: &Ue<M>, data: &Region<'_>) -> Option<usize> {
    let worlds: Vec<usize> = ue
        .all_objects()?
        .into_iter()
        .filter(|&o| ue.class_name(o).as_deref() == Some("World") && !ue.is_cdo(o))
        .collect();
    if worlds.is_empty() {
        return None;
    }
    let mut hits = Vec::new();
    for off in (0..data.bytes.len().saturating_sub(7)).step_by(8) {
        let v = u64::from_le_bytes(data.bytes[off..off + 8].try_into().ok()?) as usize;
        if worlds.contains(&v) {
            hits.push(data.base + off);
        }
    }
    hits.into_iter().next()
}

/// The GWorld slot address plus a description. A configured slot that is still null (engine
/// up, no level loaded yet) is kept: GWorld is read fresh on every use, so it starts
/// working once a level loads.
pub fn gworld_slot<M: Memory>(s: &Scan<'_>, ue: &Ue<M>) -> (Option<usize>, Result<String, String>) {
    match resolve_gworld(s, ue) {
        Ok(f) => (Some(f.addr), Ok(format!("{:#x} via {}", f.addr, f.how))),
        Err(e) if e.contains("is null") => match s.sigs.raw_offsets.gworld_rva {
            Some(rva) => {
                let slot = s.module.rva(rva);
                (
                    Some(slot),
                    Ok(format!("{slot:#x} via raw_offsets.gworld_rva {rva:#x} (null for now; read every tick)")),
                )
            }
            None => (None, Err(e)),
        },
        Err(e) => (None, Err(e)),
    }
}

pub fn resolve_gworld<M: Memory>(s: &Scan<'_>, ue: &Ue<M>) -> Result<Found, String> {
    let spec = &s.sigs.gworld;
    let check = |f: Found| -> Result<Found, String> {
        match s.mem.read_valid_ptr(f.addr) {
            Some(w) if ue.class_name(w).as_deref() == Some("World") => Ok(f),
            Some(_) => Err(format!("{:#x} ({}) does not point to a World", f.addr, f.how)),
            None => Err(format!("{:#x} ({}) is null (no world yet?)", f.addr, f.how)),
        }
    };
    if let Some(rva) = s.sigs.raw_offsets.gworld_rva {
        return check(found(s.module.rva(rva), format!("raw_offsets.gworld_rva {rva:#x}")));
    }
    let mut errors = Vec::new();
    let pattern = |errors: &mut Vec<String>| -> Option<Found> {
        match resolve_first(&spec.candidates, &s.code, s.mem) {
            Ok(a) => check(found(a, "pattern")).map_err(|e| errors.push(e)).ok(),
            Err(e) => {
                errors.extend(e.iter().map(ToString::to_string));
                None
            }
        }
    };
    match spec.strategy.as_str() {
        "offset" => return Err("strategy = offset but raw_offsets.gworld_rva is not set".into()),
        "pattern" => {
            if let Some(f) = pattern(&mut errors) {
                return Ok(f);
            }
        }
        "reflection" => {
            match s.data.as_ref().and_then(|d| reflection_gworld(ue, d)) {
                Some(a) => match check(found(a, "reflection (.data pointer to live World)")) {
                    Ok(f) => return Ok(f),
                    Err(e) => errors.push(e),
                },
                None => errors.push("reflection: no .data pointer to a live World".into()),
            }
            if spec.fallback_pattern {
                if let Some(f) = pattern(&mut errors) {
                    return Ok(f);
                }
            }
        }
        other => return Err(format!("unknown gworld.strategy '{other}'")),
    }
    Err(errors.join("; "))
}

// ---- ProcessEvent -----------------------------------------------------------------------

/// `test dword ptr [reg+flags_off], imm32` (`F7 /0 disp32 imm32`) with the given immediate.
fn test_flags_pattern(flags_off: usize, imm: u32) -> Pattern {
    let mut p = vec![Some(0xF7), None];
    p.extend((flags_off as u32).to_le_bytes().map(Some));
    p.extend(imm.to_le_bytes().map(Some));
    Pattern::from_bytes(&p)
}

/// True if `func` looks like ProcessEvent (Dumper-7's FUNC_Native / FUNC_HasOutParms test).
pub fn looks_like_process_event(mem: &dyn Memory, func: usize, flags_off: usize) -> bool {
    let native = test_flags_pattern(flags_off, 0x400);
    let out_parms = test_flags_pattern(flags_off, 0x40_0000);
    let mut buf = vec![0u8; 0xF00];
    // Read as much as is readable (functions near the end of .text).
    let mut len = buf.len();
    while len >= 0x100 && !mem.read(func, &mut buf[..len]) {
        len -= 0x100;
    }
    if len < 0x100 {
        return false;
    }
    let code = &buf[..len];
    native.find_iter(&code[..code.len().min(0x400)]).next().is_some() && out_parms.find_iter(code).next().is_some()
}

/// Walks the VFT of `GObjects[0]` looking for ProcessEvent. Returns `(address, index)`.
pub fn flags_scan_process_event<M: Memory>(
    ue: &Ue<M>,
    module: &ModuleInfo,
    max_slots: usize,
) -> Option<(usize, usize)> {
    let obj = ue.object_at(0)?;
    let vft = ue.mem.read_valid_ptr(obj + ue.layout.vtable)?;
    let flags_off = ue.layout.ufunction_flags;
    (0..max_slots).find_map(|i| {
        let f = ue.mem.read_valid_ptr(vft + i * 8)?;
        if !module.contains(f) {
            return None;
        }
        looks_like_process_event(&ue.mem, f, flags_off).then_some((f, i))
    })
}

pub fn resolve_process_event<M: Memory>(s: &Scan<'_>, ue: &Ue<M>) -> Result<Found, String> {
    let spec = &s.sigs.process_event;
    let raw = &s.sigs.raw_offsets;
    let from_index = |idx: usize, how: &str| -> Result<Found, String> {
        let obj = ue.object_at(0).ok_or("GObjects[0] unreadable")?;
        let vft = ue.mem.read_valid_ptr(obj).ok_or("GObjects[0] vtable unreadable")?;
        let f = ue.mem.read_valid_ptr(vft + idx * 8).ok_or("vtable slot unreadable")?;
        if !s.module.contains(f) {
            return Err(format!("vtable[{idx:#x}] = {f:#x} is outside the module"));
        }
        Ok(found(f, format!("{how} vtable[{idx:#x}]")))
    };
    if let Some(rva) = raw.process_event_rva {
        return Ok(found(s.module.rva(rva), format!("raw_offsets.process_event_rva {rva:#x}")));
    }
    if let Some(idx) = raw.process_event_index {
        return from_index(idx, "raw_offsets.process_event_index");
    }
    match spec.strategy.as_str() {
        "offset" => Err("strategy = offset but raw_offsets.process_event_rva/index not set".into()),
        "vtable_index" => {
            let idx = spec.vtable_index.ok_or("process_event.vtable_index not set")?;
            from_index(idx, "process_event.vtable_index")
        }
        "pattern" => resolve_first(&spec.candidates, &s.code, s.mem)
            .map(|a| found(a, "pattern"))
            .map_err(|e| e.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ")),
        "flags_scan" => flags_scan_process_event(ue, s.module, spec.max_vtable_scan)
            .map(|(f, i)| found(f, format!("flags_scan vtable[{i:#x}]")))
            .ok_or_else(|| "flags_scan: no VFT slot of GObjects[0] matches ProcessEvent".into()),
        other => Err(format!("unknown process_event.strategy '{other}'")),
    }
}

/// Everything the adapter needs, plus per-item diagnostics.
#[derive(Clone, Debug)]
pub struct Resolution {
    pub globals: UeGlobals,
    pub gobjects_layout: GObjectsLayout,
    pub gobjects_how: String,
    pub fnamepool_how: String,
    pub gworld: Result<String, String>,
    pub process_event: Result<String, String>,
}

/// Resolves all globals. GObjects and FNamePool are mandatory; GWorld and ProcessEvent are
/// reported individually.
pub fn resolve_all(s: &Scan<'_>) -> Result<Resolution, String> {
    let (gobj, gobjects_layout) = resolve_gobjects(s).map_err(|e| format!("GObjects: {e}"))?;
    let names = resolve_fnamepool(s).map_err(|e| format!("FNamePool: {e}"))?;
    let mut globals = UeGlobals { gobjects: gobj.addr, fnamepool: names.addr, gworld: None, process_event: None };
    let ue = Ue::new(s.mem, globals, gobjects_layout, s.sigs.fnamepool.layout, s.sigs.raw_offsets.uobject);
    let (slot, gworld) = gworld_slot(s, &ue);
    globals.gworld = slot;
    let process_event = resolve_process_event(s, &ue).map(|f| {
        globals.process_event = Some(f.addr);
        format!("{:#x} via {}", f.addr, f.how)
    });
    Ok(Resolution {
        globals,
        gobjects_layout,
        gobjects_how: format!("{:#x} via {}", gobj.addr, gobj.how),
        fnamepool_how: format!("{:#x} via {}", names.addr, names.how),
        gworld,
        process_event,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reflection::tests::{graph, FakeUe};

    const IMG: usize = 0x1_4000_0000;

    /// Shipped strategies without the Dumper-7 RVAs (so the scans themselves are exercised)
    /// and with the stock UObject layout the fake object graph is built with.
    fn sigs() -> Signatures {
        let mut s = Signatures::from_toml_str(include_str!("../../../config/signatures.toml")).unwrap();
        s.raw_offsets = crate::sigs::RawOffsets { sim_car: s.raw_offsets.sim_car.clone(), ..Default::default() };
        s
    }

    fn module(size: usize) -> ModuleInfo {
        ModuleInfo {
            base: IMG,
            size,
            sections: vec![
                crate::pe::Section { name: ".text".into(), rva: 0x1000, size: 0x1000 },
                crate::pe::Section { name: ".data".into(), rva: 0x3000, size: 0x1000 },
            ],
        }
    }

    /// Fake heap (FakeUe) + image region [IMG, IMG+0x4000).
    fn setup() -> (FakeUe, Vec<u8>) {
        let f = FakeUe::new();
        (f, vec![0u8; 0x4000])
    }

    /// Copies a GObjects header (with realistic Dumper-7 values) into the image `.data`.
    fn put_gobjects(f: &mut FakeUe, img: &mut [u8], data_off: usize) {
        let table = f.mem.read_u64(f.gobjects).unwrap();
        let num = f.objects.len() as i32;
        img[data_off..data_off + 8].copy_from_slice(&table.to_le_bytes());
        img[data_off + 0x10..data_off + 0x14].copy_from_slice(&(0x21_0000i32).to_le_bytes());
        img[data_off + 0x14..data_off + 0x18].copy_from_slice(&num.to_le_bytes());
        img[data_off + 0x18..data_off + 0x1C].copy_from_slice(&0x21i32.to_le_bytes());
        img[data_off + 0x1C..data_off + 0x20].copy_from_slice(&1i32.to_le_bytes());
    }

    #[test]
    fn chunked_array_validation() {
        let mut f = FakeUe::new();
        let _ = graph(&mut f);
        for i in 0..8 {
            f.object(&format!("O{i}"), 0, 0, 0);
        }
        let l = GObjectsLayout::default();
        // FakeUe's header has MaxElements 65536, MaxChunks 1 -> per = 65536.
        assert_eq!(validate_chunked_array(&f.mem, f.gobjects, &l), Some(65536));
        f.w32(f.gobjects + 0x1C, 3); // NumChunks inconsistent
        assert_eq!(validate_chunked_array(&f.mem, f.gobjects, &l), None);
    }

    #[test]
    fn gobjects_via_heuristic_and_pattern() {
        let (mut f, mut img) = setup();
        let _ = graph(&mut f);
        for i in 0..8 {
            f.object(&format!("O{i}"), 0, 0, 0);
        }
        put_gobjects(&mut f, &mut img, 0x3000 + 0x124);
        // mov rax,[rip+X] pointing at the header (rel from IMG+0x1100+7).
        let target = IMG + 0x3124;
        let rel = (target as i64 - (IMG + 0x1107) as i64) as i32;
        img[0x1100..0x1107].copy_from_slice(&[0x48, 0x8B, 0x05, 0, 0, 0, 0]);
        img[0x1103..0x1107].copy_from_slice(&rel.to_le_bytes());
        img[0x1107..0x110F].copy_from_slice(&[0x48, 0x8B, 0x0C, 0xC8, 0x48, 0x8D, 0x04, 0xD1]);
        f.mem.add_region(IMG, img.clone());
        let m = module(0x4000);
        let code = [Region { base: IMG + 0x1000, bytes: &img[0x1000..0x2000] }];
        let data = Region { base: IMG + 0x3000, bytes: &img[0x3000..0x4000] };
        let mut sg = sigs();
        let mk = |sg: &Signatures| -> Result<(Found, GObjectsLayout), String> {
            let scan =
                Scan { mem: &f.mem, module: &m, code: code.to_vec(), data: Some(data), srwlock_fns: vec![], sigs: sg };
            resolve_gobjects(&scan)
        };
        let (g, layout) = mk(&sg).unwrap();
        assert_eq!(g.addr, target);
        assert_eq!(g.how, "pattern");
        assert_eq!(layout.elements_per_chunk, 0x10000);
        assert_eq!(layout.fuobjectitem_size, 0x18);

        assert_eq!(heuristic_gobjects(&f.mem, &data, &sg.gobjects.layout), Some((target, 0x10000)));

        // No pattern match -> heuristic fallback.
        sg.gobjects.candidates[0].pattern = "DE AD BE EF".into();
        let (g, _) = mk(&sg).unwrap();
        assert_eq!(g.how, "heuristic .data scan");

        // Raw override wins: Dumper-7's Offsets::GObjects (ObjObjects itself)...
        sg.raw_offsets.gobjects_rva = Some(0x3124);
        let (g, _) = mk(&sg).unwrap();
        assert_eq!(g.addr, target);
        assert!(g.how.starts_with("raw_offsets"));
        // ...used exactly as given (never + 0x10). A non-validating RVA falls back to the
        // configured strategy, and the error names it when that fails too.
        sg.raw_offsets.gobjects_rva = Some(0x3114);
        let (g, _) = mk(&sg).unwrap();
        assert_eq!(g.addr, target);
        assert_eq!(g.how, "heuristic .data scan");
        sg.gobjects.fallback_heuristic = false;
        let e = mk(&sg).unwrap_err();
        assert!(e.contains("raw_offsets.gobjects_rva 0x3114") && !e.contains("+ 0x10"), "{e}");
        assert!(e.contains("not an FChunkedFixedUObjectArray (Objects"), "{e}");
    }

    #[test]
    fn gobjects_fail_safe_without_fallback() {
        let (f, img) = setup();
        f.mem.add_region(IMG, img.clone());
        let m = module(0x4000);
        let mut sg = sigs();
        sg.gobjects.fallback_heuristic = false;
        let scan = Scan {
            mem: &f.mem,
            module: &m,
            code: vec![Region { base: IMG + 0x1000, bytes: &img[0x1000..0x2000] }],
            data: None,
            srwlock_fns: vec![],
            sigs: &sg,
        };
        let err = resolve_gobjects(&scan).unwrap_err();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn fnamepool_dumper7_scan() {
        let (f, mut img) = setup();
        let pool = f.pool;
        // lea rcx,[pool]; call F at IMG+0x1200. Pool lives on the heap -> far rel32 isn't
        // representable, so place a "pool" copy in .data instead and point the heap at it.
        let pool_img = IMG + 0x3800;
        img[0x3810..0x3818].copy_from_slice(&(f.name_block as u64).to_le_bytes());
        let _ = pool;
        let lea = IMG + 0x1200;
        let rel = (pool_img as i64 - (lea + 7) as i64) as i32;
        img[0x1200..0x1203].copy_from_slice(&[0x48, 0x8D, 0x0D]);
        img[0x1203..0x1207].copy_from_slice(&rel.to_le_bytes());
        let func = IMG + 0x1400;
        img[0x1207] = 0xE8;
        img[0x1208..0x120C].copy_from_slice(&((func as i64 - (lea + 12) as i64) as i32).to_le_bytes());
        // F: call [rip+slot] -> slot holds the SRWLock fn address.
        let slot = IMG + 0x3900;
        let srw = 0x7FF8_1234_5678usize;
        img[0x3900..0x3908].copy_from_slice(&(srw as u64).to_le_bytes());
        img[0x1410..0x1412].copy_from_slice(&[0xFF, 0x15]);
        img[0x1412..0x1416].copy_from_slice(&((slot as i64 - (func + 0x16) as i64) as i32).to_le_bytes());
        // F+0x100: lea rdx,[rip+str] -> "ByteProperty" (wide)
        let s_addr = IMG + 0x3A00;
        let wide = crate::pe::utf16_bytes("ByteProperty");
        img[0x3A00..0x3A00 + wide.len()].copy_from_slice(&wide);
        img[0x1500..0x1503].copy_from_slice(&[0x48, 0x8D, 0x15]);
        img[0x1503..0x1507].copy_from_slice(&((s_addr as i64 - (func + 0x107) as i64) as i32).to_le_bytes());
        f.mem.add_region(IMG, img.clone());
        let m = module(0x4000);
        let sg = sigs();
        let code = vec![Region { base: IMG + 0x1000, bytes: &img[0x1000..0x2000] }];
        let mut scan = Scan { mem: &f.mem, module: &m, code, data: None, srwlock_fns: vec![srw], sigs: &sg };
        assert_eq!(dumper7_fnamepool(&scan), Some(pool_img));
        let r = resolve_fnamepool(&scan).unwrap();
        assert_eq!(r.addr, pool_img);
        // Without the SRWLock target the scan must not match.
        scan.srwlock_fns = vec![1];
        assert_eq!(dumper7_fnamepool(&scan), None);
        assert!(resolve_fnamepool(&scan).is_err());
    }

    #[test]
    fn fnamepool_bad_rva_falls_back_to_data_scan() {
        let (mut f, mut img) = setup();
        f.add_name("ByteProperty");
        let block = f.name_block as u64;
        // Decoy: a cached copy of the Blocks[0] pointer with junk allocator fields before it.
        img[0x3200..0x3208].copy_from_slice(&block.to_le_bytes());
        img[0x31F8..0x31FC].copy_from_slice(&u32::MAX.to_le_bytes());
        // The real pool at .data+0x800: FRWLock, CurrentBlock 0, cursor, Blocks[0].
        img[0x380C..0x3810].copy_from_slice(&0x40u32.to_le_bytes());
        img[0x3810..0x3818].copy_from_slice(&block.to_le_bytes());
        f.mem.add_region(IMG, img.clone());
        let m = module(0x4000);
        let mut sg = sigs();
        sg.raw_offsets.gnames_rva = Some(0x3400); // e.g. Dumper-7's GNames: not the pool
        let data = Region { base: IMG + 0x3000, bytes: &img[0x3000..0x4000] };
        assert!(is_first_name_block(&f.mem.bytes(f.name_block, 32), &sg.fnamepool.layout));
        assert_eq!(data_scan_fnamepool(&f.mem, &data, &sg.fnamepool.layout), Some(IMG + 0x3800));
        let scan = Scan { mem: &f.mem, module: &m, code: vec![], data: Some(data), srwlock_fns: vec![], sigs: &sg };
        let r = resolve_fnamepool(&scan).unwrap();
        assert_eq!(r.addr, IMG + 0x3800);
        assert!(r.how.starts_with("data scan"), "{}", r.how);
        let (ok, d) = describe_fnamepool(&f.mem, r.addr, &sg);
        assert!(ok, "{d}");
        // Without the fallback the error names the rejected RVA first.
        sg.fnamepool.fallback_data_scan = false;
        let scan = Scan { mem: &f.mem, module: &m, code: vec![], data: Some(data), srwlock_fns: vec![], sigs: &sg };
        let e = resolve_fnamepool(&scan).unwrap_err();
        assert!(e.starts_with(&format!("{:#x} (raw_offsets.gnames_rva 0x3400)", IMG + 0x3400)), "{e}");
        // A valid RVA is used directly.
        sg.raw_offsets.gnames_rva = Some(0x3800);
        let scan = Scan { mem: &f.mem, module: &m, code: vec![], data: Some(data), srwlock_fns: vec![], sigs: &sg };
        assert!(resolve_fnamepool(&scan).unwrap().how.starts_with("raw_offsets"));
    }

    #[test]
    fn first_name_block_detection() {
        let l = FNamePoolLayout::default();
        let mut b = Vec::new();
        b.extend((4u16 << 6).to_le_bytes());
        b.extend(b"None");
        b.extend((12u16 << 6).to_le_bytes());
        b.extend(b"ByteProperty");
        assert_eq!(&b[..2], b"\x00\x01", "header 0x100 (len 4)");
        assert!(is_first_name_block(&b, &l));
        // Live bytes: "\x1e\x01None\x10\x03ByteProperty" (hash bits set, len 4 / 12).
        let live = b"\x1e\x01None\x10\x03ByteProperty";
        assert!(is_first_name_block(live, &l));
        assert!(!is_first_name_block(b"\x1e\x01None\x10\x03IntProperty_", &l));
        assert!(!is_first_name_block(b"\x1f\x01None", &l), "wide flag");
    }

    #[test]
    fn gworld_by_reflection() {
        let (mut f, mut img) = setup();
        let g = graph(&mut f);
        let world_cls = f.object("World", g.class, 0, 0);
        let _cdo = f.object("Default__World", world_cls, 0, crate::reflection::RF_CLASS_DEFAULT_OBJECT);
        let world = f.object("Stage01", world_cls, 0, 0);
        img[0x3208..0x3210].copy_from_slice(&(world as u64).to_le_bytes());
        f.mem.add_region(IMG, img.clone());
        let ue = f.ue();
        let data = Region { base: IMG + 0x3000, bytes: &img[0x3000..0x4000] };
        assert_eq!(reflection_gworld(&ue, &data), Some(IMG + 0x3208));
        let m = module(0x4000);
        let sg = sigs();
        let scan = Scan { mem: &f.mem, module: &m, code: vec![], data: Some(data), srwlock_fns: vec![], sigs: &sg };
        let r = resolve_gworld(&scan, &ue).unwrap();
        assert_eq!(r.addr, IMG + 0x3208);
        let mut ue2 = f.ue();
        ue2.globals.gworld = Some(r.addr);
        assert_eq!(ue2.world(), Some(world));
    }

    #[test]
    fn null_gworld_rva_slot_is_kept() {
        let (mut f, img) = setup();
        let _ = graph(&mut f);
        f.mem.add_region(IMG, img);
        let ue = f.ue();
        let m = module(0x4000);
        let mut sg = sigs();
        sg.raw_offsets.gworld_rva = Some(0x3300);
        let scan = Scan { mem: &f.mem, module: &m, code: vec![], data: None, srwlock_fns: vec![], sigs: &sg };
        let (slot, how) = gworld_slot(&scan, &ue);
        assert_eq!(slot, Some(IMG + 0x3300));
        assert!(how.unwrap().contains("null for now"));
        // Without a configured slot a null world stays unresolved (retried later).
        sg.raw_offsets.gworld_rva = None;
        sg.gworld.strategy = "reflection".into();
        let scan = Scan { mem: &f.mem, module: &m, code: vec![], data: None, srwlock_fns: vec![], sigs: &sg };
        assert_eq!(gworld_slot(&scan, &ue).0, None);
    }

    #[test]
    fn process_event_flags_scan() {
        let (mut f, mut img) = setup();
        let g = graph(&mut f);
        let _ = g;
        // vtable in .data with 3 slots; slot 2 = PE-like function.
        let vft = IMG + 0x3300;
        let funcs = [IMG + 0x2400, IMG + 0x1C00, IMG + 0x1400];
        for (i, fp) in funcs.iter().enumerate() {
            img[0x3300 + i * 8..0x3308 + i * 8].copy_from_slice(&(*fp as u64).to_le_bytes());
        }
        let flags = f.layout.ufunction_flags as u32;
        let mut put = |at: usize, imm: u32| {
            img[at..at + 2].copy_from_slice(&[0xF7, 0x81]);
            img[at + 2..at + 6].copy_from_slice(&flags.to_le_bytes());
            img[at + 6..at + 10].copy_from_slice(&imm.to_le_bytes());
        };
        put(0x1420, 0x400);
        put(0x1800, 0x40_0000);
        put(0x1110, 0x400); // decoy: native test only
        f.w64(f.objects[0], vft as u64);
        f.mem.add_region(IMG, img.clone());
        let ue = f.ue();
        let m = module(0x4000);
        assert_eq!(flags_scan_process_event(&ue, &m, 8), Some((IMG + 0x1400, 2)));
        let mut sg = sigs();
        let pe = |sg: &Signatures| {
            let scan = Scan { mem: &f.mem, module: &m, code: vec![], data: None, srwlock_fns: vec![], sigs: sg };
            resolve_process_event(&scan, &ue).map(|f| f.addr)
        };
        assert_eq!(pe(&sg), Ok(IMG + 0x1400));
        sg.raw_offsets.process_event_index = Some(1);
        assert_eq!(pe(&sg), Ok(IMG + 0x1C00));
        sg.raw_offsets.process_event_index = Some(7);
        assert!(pe(&sg).is_err());
    }
}
