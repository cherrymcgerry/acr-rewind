//! Engine-global resolution against the remote process, reusing `acr_ue`'s resolvers and
//! reflection walker unchanged: `sig-test` and `ue-dump-classes`.

use crate::mem::{PageCache, ReadMem};
use crate::oracle::ShmSample;
use crate::process::ModuleEntry;
use crate::variants::vec3_variants;
use acr_ue::mem::Memory;
use acr_ue::pattern::{resolve_candidate, Pattern, Region};
use acr_ue::pe::ModuleInfo;
use acr_ue::reflection::{Ue, UeGlobals, RF_CLASS_DEFAULT_OBJECT};
use acr_ue::resolve::{
    describe_fnamepool, describe_gobjects, looks_like_process_event, resolve_fnamepool, resolve_gobjects,
    resolve_gworld, resolve_process_event, Scan,
};
use acr_ue::sigs::{Candidate, ChainRoot, GObjectsLayout, Signatures, SimCarLocator, ValueType};
use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::collections::HashMap;

/// The game module with local copies of its code sections and `.data`.
pub struct LoadedModule {
    pub entry: ModuleEntry,
    pub info: ModuleInfo,
    pub code: Vec<(usize, Vec<u8>)>,
    pub data: Option<(usize, Vec<u8>)>,
    pub rsrc_has_original_filename: Option<bool>,
}

impl LoadedModule {
    pub fn load<R: ReadMem>(mem: &PageCache<R>, modules: &[ModuleEntry], sigs: &Signatures) -> Result<Self> {
        let entry = modules
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case(&sigs.module.name))
            .cloned()
            .ok_or_else(|| anyhow!("module {} not loaded in target", sigs.module.name))?;
        let info = ModuleInfo::parse(mem, entry.base).context("parsing remote PE headers")?;
        let read = |name: &str| -> Option<(usize, Vec<u8>)> {
            let (a, l) = info.section_range(name)?;
            read_big(mem.inner(), a, l).map(|v| (a, v))
        };
        let code = sigs.module.scan_sections.iter().filter_map(|s| read(s)).collect::<Vec<_>>();
        if code.is_empty() {
            anyhow::bail!("none of the scan_sections {:?} are readable", sigs.module.scan_sections);
        }
        let data = read(".data");
        let rsrc_has_original_filename = (!sigs.module.original_filename.is_empty())
            .then(|| read(".rsrc").map(|(_, b)| acr_ue::pe::contains_utf16(&b, &sigs.module.original_filename)))
            .flatten();
        Ok(Self { entry, info, code, data, rsrc_has_original_filename })
    }

    pub fn regions(&self) -> Vec<Region<'_>> {
        self.code.iter().map(|(b, v)| Region { base: *b, bytes: v }).collect()
    }

    pub fn data_region(&self) -> Option<Region<'_>> {
        self.data.as_ref().map(|(b, v)| Region { base: *b, bytes: v })
    }

    pub fn scan<'a>(&'a self, mem: &'a dyn Memory, sigs: &'a Signatures) -> Scan<'a> {
        Scan {
            mem,
            module: &self.info,
            code: self.regions(),
            data: self.data_region(),
            srwlock_fns: srwlock_fns(),
            sigs,
        }
    }

    pub fn rva(&self, addr: usize) -> String {
        if self.info.contains(addr) {
            format!("{:#x}", addr - self.info.base)
        } else {
            "-".into()
        }
    }
}

/// Reads a large range in 1 MiB pieces (zero-filling unreadable pieces).
fn read_big<R: ReadMem>(mem: &R, addr: usize, len: usize) -> Option<Vec<u8>> {
    let mut out = vec![0u8; len];
    let mut ok_any = false;
    for (i, chunk) in out.chunks_mut(1 << 20).enumerate() {
        let segs = crate::mem::read_segments(mem, addr + i * (1 << 20), chunk);
        ok_any |= !segs.is_empty();
    }
    ok_any.then_some(out)
}

/// `InitializeSRWLock` / `RtlInitializeSRWLock` addresses. System DLLs share their base across
/// processes within a boot session, so the probe's own addresses are valid in the target.
#[cfg(windows)]
pub fn srwlock_fns() -> Vec<usize> {
    use windows::core::{s, w};
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    let mut out = Vec::new();
    for (module, func) in [
        (w!("kernel32.dll"), s!("InitializeSRWLock")),
        (w!("kernelbase.dll"), s!("InitializeSRWLock")),
        (w!("ntdll.dll"), s!("RtlInitializeSRWLock")),
    ] {
        // SAFETY: lookups of already-loaded system modules in the probe itself.
        unsafe {
            if let Ok(h) = GetModuleHandleW(module) {
                if let Some(p) = GetProcAddress(h, func) {
                    out.push(p as usize);
                }
            }
        }
    }
    out
}

#[cfg(not(windows))]
pub fn srwlock_fns() -> Vec<usize> {
    Vec::new()
}

// ---- sig-test ------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct SigItem {
    pub section: String,
    pub source: String,
    pub matches: Option<usize>,
    pub resolved: Option<String>,
    pub rva: Option<String>,
    pub plausible: Option<bool>,
    pub detail: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SigReport {
    pub module: String,
    pub module_base: String,
    pub module_size: String,
    pub original_filename_ok: Option<bool>,
    pub items: Vec<SigItem>,
    pub objects: Option<usize>,
    pub classes: Option<usize>,
    pub missing_core_classes: Vec<String>,
    pub functions_found: Vec<String>,
    pub functions_missing: Vec<String>,
    pub required_functions_missing: Vec<String>,
    pub vehicle_pawn_classes_present: Vec<String>,
    pub movement_classes_present: Vec<String>,
    pub blocked_online_classes_live: Vec<String>,
    pub summary: Vec<String>,
}

fn count_matches(pat: &Pattern, regions: &[Region<'_>], cap: usize) -> usize {
    regions.iter().map(|r| pat.find_iter(r.bytes).take(cap).count()).sum::<usize>().min(cap)
}

/// The hook's own verdicts (`acr_ue::resolve`), so sig-test and the hook cannot disagree.
fn gobjects_check(mem: &dyn Memory, addr: usize, l: &GObjectsLayout) -> (bool, String) {
    describe_gobjects(mem, addr, l)
}

fn fnamepool_check(mem: &dyn Memory, addr: usize, sigs: &Signatures) -> (bool, String) {
    describe_fnamepool(mem, addr, sigs)
}

fn candidate_item(
    section: &str,
    idx: usize,
    c: &Candidate,
    module: &LoadedModule,
    mem: &dyn Memory,
    check: &dyn Fn(usize) -> (Option<bool>, String),
) -> SigItem {
    let regions = module.regions();
    let matches = Pattern::parse(&c.pattern).ok().map(|p| count_matches(&p, &regions, 10_000));
    let (resolved, rva, plausible, detail) = match resolve_candidate(c, &regions, mem) {
        Ok(a) => {
            let (ok, d) = check(a);
            (Some(format!("{a:#x}")), Some(module.rva(a)), ok, d)
        }
        Err(e) => (None, None, Some(false), e.to_string()),
    };
    SigItem {
        section: section.into(),
        source: format!(
            "candidate[{idx}] '{}'{}",
            c.pattern,
            if c.note.is_empty() { String::new() } else { format!(" ({})", c.note) }
        ),
        matches,
        resolved,
        rva,
        plausible,
        detail,
    }
}

fn strategy_item(
    section: &str,
    r: Result<(usize, String), String>,
    module: &LoadedModule,
    check: Option<(bool, String)>,
) -> SigItem {
    match r {
        Ok((a, how)) => {
            let (ok, d) = check.map_or((None, String::new()), |(o, d)| (Some(o), d));
            SigItem {
                section: section.into(),
                source: format!("configured resolution: {how}"),
                matches: None,
                resolved: Some(format!("{a:#x}")),
                rva: Some(module.rva(a)),
                plausible: ok.or(Some(true)),
                detail: d,
            }
        }
        Err(e) => SigItem {
            section: section.into(),
            source: "configured resolution".into(),
            matches: None,
            resolved: None,
            rva: None,
            plausible: Some(false),
            detail: e,
        },
    }
}

/// First live instance of any `vehicle_pawn_class_candidates` class (or of `class` if given).
pub fn find_pawn(ue: &Ue<&dyn Memory>, sigs: &Signatures, class: Option<&str>) -> Option<(String, usize)> {
    let index = ue.build_index()?;
    let names: Vec<String> = match class {
        Some(c) => vec![c.to_string()],
        None => sigs.reflection.vehicle_pawn_class_candidates.clone(),
    };
    names.into_iter().find_map(|n| index.class(&n).and_then(|c| ue.find_live_instance_of(c)).map(|p| (n, p)))
}

/// Cheap check of the `vtable_scan` vtable: inside the module and its first slot points into
/// a code section. Locating the bodies is a full heap sweep, left to `find-car`.
fn vtable_item(mem: &dyn Memory, module: &LoadedModule, rva: u64) -> SigItem {
    let vt = module.info.base + rva as usize;
    let first = mem.read_ptr(vt);
    let in_code = first.is_some_and(|f| module.code.iter().any(|(a, b)| f >= *a && f < a + b.len()));
    SigItem {
        section: "sim_car".into(),
        source: format!("vtable_scan rva {rva:#x}"),
        matches: None,
        resolved: Some(format!("{vt:#x}")),
        rva: Some(format!("{rva:#x}")),
        plausible: Some(rva < module.info.size as u64 && in_code),
        detail: format!(
            "vtable[0] = {}{}; run `acr-probe find-car` to locate the car bodies",
            first.map_or("unreadable".into(), |f| format!("{f:#x}")),
            if in_code { " (code)" } else { " (NOT in a code section)" }
        ),
    }
}

/// Resolves `[raw_offsets.sim_car]` and compares its position with shm (any axis order/sign).
fn sim_car_item(
    mem: &dyn Memory,
    module: &LoadedModule,
    sigs: &Signatures,
    pawn: Option<usize>,
    shm: Option<&ShmSample>,
) -> Option<SigItem> {
    let sp = &sigs.raw_offsets.sim_car;
    if !sp.is_configured() {
        return None;
    }
    if sp.locator == SimCarLocator::VtableScan {
        return Some(vtable_item(mem, module, sp.vtable_rva?));
    }
    let (root, root_desc) = match sp.chain_root {
        ChainRoot::Module => (Some(module.info.base), "module".to_string()),
        ChainRoot::Pawn => (pawn, format!("pawn {}", pawn.map_or("not found".into(), |p| format!("{p:#x}")))),
    };
    let mut item = SigItem {
        section: "sim_car".into(),
        source: format!("chain_root {root_desc}, chain {:x?}", sp.chain),
        matches: None,
        resolved: None,
        rva: None,
        plausible: Some(false),
        detail: String::new(),
    };
    let Some(car) = root.and_then(|r| acr_ue::sim_car::resolve_chain(mem, r, &sp.chain)) else {
        item.detail = "chain does not resolve".into();
        return Some(item);
    };
    item.resolved = Some(format!("{car:#x}"));
    let Some(pos_off) = sp.position else {
        item.detail = "position offset unset".into();
        return Some(item);
    };
    let size = sp.value_type.size();
    let vals: Option<Vec<f64>> = (0..3)
        .map(|k| {
            let a = car + pos_off + k * size;
            match sp.value_type {
                ValueType::F32 => mem.read_f32(a).map(f64::from),
                ValueType::F64 => mem.read_f64(a),
            }
        })
        .collect();
    let Some(vals) = vals else {
        item.detail = "position unreadable".into();
        return Some(item);
    };
    let pos: Vec<f64> = vals.iter().map(|v| v / sp.length_scale).collect();
    match shm {
        Some(s) => {
            let (v, err) = vec3_variants(&[1.0], true)
                .into_iter()
                .map(|v| {
                    let e = v.apply(&s.pos).iter().zip(&pos).fold(0.0f64, |m, (a, b)| m.max((a - b).abs()));
                    (v, e)
                })
                .min_by(|a, b| a.1.total_cmp(&b.1))?;
            item.plausible = Some(err < 0.5);
            item.detail =
                format!("position {pos:.3?} (m) vs shm {:.3?}: best axis fit {} error {err:.3} m", s.pos, v.describe());
        }
        None => {
            item.plausible = None;
            item.detail = format!("position {pos:.3?} (m); shm unavailable for comparison");
        }
    }
    Some(item)
}

pub fn sig_test<R: ReadMem>(
    mem: &PageCache<R>,
    modules: &[ModuleEntry],
    sigs: &Signatures,
    shm: Option<&ShmSample>,
) -> Result<SigReport> {
    let module = LoadedModule::load(mem, modules, sigs)?;
    let dmem: &dyn Memory = mem;
    let mut rep = SigReport {
        module: module.entry.name.clone(),
        module_base: format!("{:#x}", module.info.base),
        module_size: format!("{:#x}", module.info.size),
        original_filename_ok: module.rsrc_has_original_filename,
        ..Default::default()
    };
    let gl = sigs.gobjects.layout;
    for (i, c) in sigs.gobjects.candidates.iter().enumerate() {
        rep.items.push(candidate_item("gobjects", i, c, &module, dmem, &|a| {
            let (ok, d) = gobjects_check(dmem, a, &gl);
            (Some(ok), d)
        }));
    }
    for (i, c) in sigs.fnamepool.candidates.iter().enumerate() {
        rep.items.push(candidate_item("fnamepool", i, c, &module, dmem, &|a| {
            let (ok, d) = fnamepool_check(dmem, a, sigs);
            (Some(ok), d)
        }));
    }
    for (i, c) in sigs.process_event.candidates.iter().enumerate() {
        let flags = sigs.raw_offsets.uobject.ufunction_flags;
        rep.items.push(candidate_item("process_event", i, c, &module, dmem, &|a| {
            let ok = looks_like_process_event(dmem, a, flags);
            (Some(ok), format!("FUNC_Native/HasOutParms tests present: {ok}"))
        }));
    }
    for (i, c) in sigs.world_tick.candidates.iter().enumerate() {
        rep.items.push(candidate_item("world_tick", i, c, &module, dmem, &|_| (None, "no automatic check".into())));
    }

    // The configured resolution path, exactly as the hook runs it.
    let scan = module.scan(dmem, sigs);
    let gobj = resolve_gobjects(&scan);
    let names = resolve_fnamepool(&scan);
    rep.items.push(strategy_item(
        "gobjects",
        gobj.as_ref().map(|(f, _)| (f.addr, f.how.clone())).map_err(Clone::clone),
        &module,
        gobj.as_ref().ok().map(|(f, l)| gobjects_check(dmem, f.addr, l)),
    ));
    rep.items.push(strategy_item(
        "fnamepool",
        names.as_ref().map(|f| (f.addr, f.how.clone())).map_err(Clone::clone),
        &module,
        names.as_ref().ok().map(|f| fnamepool_check(dmem, f.addr, sigs)),
    ));
    let (Ok((gobj, layout)), Ok(names)) = (gobj, names) else {
        rep.summary.push(
            "GObjects and/or FNamePool unresolved: reflection checks skipped. Run Dumper-7 + import-dumper7.".into(),
        );
        for (i, c) in sigs.gworld.candidates.iter().enumerate() {
            rep.items
                .push(candidate_item("gworld", i, c, &module, dmem, &|_| (None, "needs GObjects+FNamePool".into())));
        }
        rep.items.extend(sim_car_item(dmem, &module, sigs, None, shm));
        return Ok(rep);
    };
    let ue = Ue::new(
        dmem,
        UeGlobals { gobjects: gobj.addr, fnamepool: names.addr, gworld: None, process_event: None },
        layout,
        sigs.fnamepool.layout,
        sigs.raw_offsets.uobject,
    );
    for (i, c) in sigs.gworld.candidates.iter().enumerate() {
        rep.items.push(candidate_item("gworld", i, c, &module, dmem, &|a| {
            let cls = dmem.read_ptr(a).and_then(|w| ue.class_name(w));
            (Some(cls.as_deref() == Some("World")), format!("*slot class = {cls:?}"))
        }));
    }
    let gw = resolve_gworld(&scan, &ue);
    rep.items.push(strategy_item(
        "gworld",
        gw.as_ref().map(|f| (f.addr, f.how.clone())).map_err(Clone::clone),
        &module,
        gw.as_ref().ok().map(|f| {
            let w = dmem.read_ptr(f.addr).unwrap_or(0);
            (true, format!("UWorld {w:#x} '{}'", ue.name(w).unwrap_or_default()))
        }),
    ));
    let pe = resolve_process_event(&scan, &ue);
    rep.items.push(strategy_item(
        "process_event",
        pe.as_ref().map(|f| (f.addr, f.how.clone())).map_err(Clone::clone),
        &module,
        pe.as_ref().ok().map(|f| {
            let ok = looks_like_process_event(dmem, f.addr, sigs.raw_offsets.uobject.ufunction_flags);
            (ok, format!("FUNC_Native/HasOutParms tests present: {ok}"))
        }),
    ));

    rep.objects = ue.num_objects();
    match ue.build_index() {
        Some(index) => {
            rep.classes = Some(index.classes.len());
            for c in ["Object", "Class", "Actor", "Pawn", "World", "GameplayStatics", "PlayerController"] {
                if index.class(c).is_none() {
                    rep.missing_core_classes.push(c.into());
                }
            }
            for (key, f) in &sigs.reflection.functions {
                let found = index.class(&f.class).and_then(|c| ue.find_function_in(c, &f.name)).is_some();
                let label = format!("{key} ({}::{})", f.class, f.name);
                if found {
                    rep.functions_found.push(label);
                } else {
                    rep.functions_missing.push(label);
                    if sigs.reflection.required_functions.contains(key) {
                        rep.required_functions_missing.push(key.clone());
                    }
                }
            }
            rep.vehicle_pawn_classes_present = sigs
                .reflection
                .vehicle_pawn_class_candidates
                .iter()
                .filter(|c| index.class(c).is_some())
                .cloned()
                .collect();
            rep.movement_classes_present = sigs
                .reflection
                .movement_component_class_candidates
                .iter()
                .filter(|c| index.class(c).is_some())
                .cloned()
                .collect();
            let mut cache = HashMap::new();
            if let Some(Some(c)) =
                acr_ue::game::find_live_blocked_class(&ue, &sigs.online_guard.blocked_live_classes, &mut cache)
            {
                rep.blocked_online_classes_live.push(c);
            }
        }
        None => rep.summary.push("GObjects iteration found no 'Class' UClass: names or layout wrong".into()),
    }
    let pawn = find_pawn(&ue, sigs, None).map(|(_, p)| p);
    rep.items.extend(sim_car_item(dmem, &module, sigs, pawn, shm));
    let bad: Vec<&SigItem> = rep.items.iter().filter(|i| i.plausible == Some(false)).collect();
    rep.summary.push(format!(
        "{} items checked, {} failed; objects {:?}, classes {:?}",
        rep.items.len(),
        bad.len(),
        rep.objects,
        rep.classes
    ));
    if !rep.required_functions_missing.is_empty() {
        rep.summary.push(format!("required UFunctions missing: {:?}", rep.required_functions_missing));
    }
    if rep.vehicle_pawn_classes_present.is_empty() {
        rep.summary.push(
            "no vehicle_pawn_class_candidates exist: add the real pawn class (import-dumper7 --apply-classes)".into(),
        );
    }
    Ok(rep)
}

// ---- ue-dump-classes -----------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct PropOut {
    pub name: String,
    pub offset: String,
    pub size: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ClassOut {
    pub kind: String,
    pub name: String,
    pub path: String,
    pub addr: String,
    pub hierarchy: Vec<String>,
    pub properties_size: Option<usize>,
    pub properties: Vec<PropOut>,
    pub functions: Vec<String>,
    pub instances: Vec<String>,
    pub instance_count: usize,
}

pub struct DumpOptions {
    pub filters: Vec<String>,
    pub props: bool,
    pub funcs: bool,
    pub instances: bool,
    pub max_instances: usize,
}

/// Resolves GObjects + FNamePool with the configured strategies and returns a reflection view.
pub fn open_ue<'a>(mem: &'a dyn Memory, module: &LoadedModule, sigs: &Signatures) -> Result<Ue<&'a dyn Memory>> {
    let scan = module.scan(mem, sigs);
    let (gobj, layout) = resolve_gobjects(&scan).map_err(|e| anyhow!("GObjects: {e}"))?;
    let names = resolve_fnamepool(&scan).map_err(|e| anyhow!("FNamePool: {e}"))?;
    let mut ue = Ue::new(
        mem,
        UeGlobals { gobjects: gobj.addr, fnamepool: names.addr, gworld: None, process_event: None },
        layout,
        sigs.fnamepool.layout,
        sigs.raw_offsets.uobject,
    );
    if let Ok(gw) = resolve_gworld(&scan, &ue) {
        ue.globals.gworld = Some(gw.addr);
    }
    Ok(ue)
}

fn walk_functions(ue: &Ue<&dyn Memory>, ustruct: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut f = ue.mem.read_valid_ptr(ustruct + ue.layout.ustruct_children);
    while let Some(field) = f {
        if let Some(n) = ue.name(field) {
            if ue.class_name(field).as_deref() == Some("Function") {
                out.push(n);
            }
        }
        f = ue.mem.read_valid_ptr(field + ue.layout.ufield_next);
        if out.len() > 2000 {
            break;
        }
    }
    out
}

pub fn dump_classes(ue: &Ue<&dyn Memory>, o: &DumpOptions) -> Result<Vec<ClassOut>> {
    let headers = ue.object_headers().context("GObjects unreadable")?;
    let class_class = headers
        .iter()
        .find(|h| h.class == h.obj && ue.name(h.obj).as_deref() == Some("Class"))
        .map(|h| h.obj)
        .context("no 'Class' UClass in GObjects (names/layout wrong)")?;
    let struct_class = headers
        .iter()
        .find(|h| h.class == class_class && ue.name(h.obj).as_deref() == Some("ScriptStruct"))
        .map(|h| h.obj);
    let lower: Vec<String> = o.filters.iter().map(|f| f.to_ascii_lowercase()).collect();
    let mut instances: HashMap<usize, Vec<usize>> = HashMap::new();
    if o.instances {
        for h in &headers {
            if h.flags & RF_CLASS_DEFAULT_OBJECT == 0 {
                instances.entry(h.class).or_default().push(h.obj);
            }
        }
    }
    let mut out = Vec::new();
    for h in &headers {
        let kind = if h.class == class_class {
            "Class"
        } else if Some(h.class) == struct_class {
            "ScriptStruct"
        } else {
            continue;
        };
        let Some(name) = ue.name(h.obj) else { continue };
        let ln = name.to_ascii_lowercase();
        if !lower.is_empty() && !lower.iter().any(|f| ln.contains(f.as_str())) {
            continue;
        }
        let info = ue.function_info(h.obj);
        let properties = if o.props {
            info.as_ref()
                .map(|i| {
                    i.params
                        .iter()
                        .map(|p| PropOut {
                            name: p.name.clone(),
                            offset: format!("{:#x}", p.offset),
                            size: format!("{:#x}", p.size),
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let inst = instances.get(&h.obj).cloned().unwrap_or_default();
        out.push(ClassOut {
            kind: kind.into(),
            name,
            path: ue.path_name(h.obj).unwrap_or_default(),
            addr: format!("{:#x}", h.obj),
            hierarchy: ue.class_hierarchy(h.obj),
            properties_size: info.map(|i| i.parms_size),
            properties,
            functions: if o.funcs { walk_functions(ue, h.obj) } else { Vec::new() },
            instance_count: inst.len(),
            instances: inst.iter().take(o.max_instances).map(|a| format!("{a:#x}")).collect(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::{FakeMem, RegionKind};
    use acr_ue::sigs::UObjectLayout;

    const IMG: usize = 0x1_4000_0000;
    const HEAP: usize = 0x10_0000_0000;
    const GOBJ_RVA: usize = 0x2100;
    const POOL_RVA: usize = 0x2400;

    /// Fake game image (PE headers, .text, .data with GObjects + FNamePool) plus a heap.
    struct Fake {
        m: FakeMem,
        l: UObjectLayout,
        cur: usize,
        names: usize,
        ncur: usize,
        chunk: usize,
        count: usize,
    }

    impl Fake {
        fn new(l: UObjectLayout) -> Self {
            let mut m = FakeMem::new();
            m.add(IMG, vec![0u8; 0x4000], RegionKind::Image, true);
            m.add(HEAP, vec![0u8; 0x10_0000], RegionKind::Private, true);
            m.put(IMG, b"MZ");
            m.put(IMG + 0x3C, &0x80u32.to_le_bytes());
            let nt = IMG + 0x80;
            m.put(nt, &0x4550u32.to_le_bytes());
            m.put(nt + 6, &2u16.to_le_bytes());
            m.put(nt + 20, &0xF0u16.to_le_bytes());
            let opt = nt + 24;
            m.put(opt, &0x20Bu16.to_le_bytes());
            m.put(opt + 56, &0x4000u32.to_le_bytes());
            for (i, (name, rva)) in [(".text", 0x1000u32), (".data", 0x2000)].iter().enumerate() {
                let sh = opt + 0xF0 + i * 40;
                let mut n = [0u8; 8];
                n[..name.len()].copy_from_slice(name.as_bytes());
                m.put(sh, &n);
                m.put(sh + 8, &0x1000u32.to_le_bytes());
                m.put(sh + 12, &rva.to_le_bytes());
            }
            let mut f = Self { m, l, cur: HEAP + 0x100, names: 0, ncur: 0, chunk: 0, count: 0 };
            let table = f.alloc(8);
            f.chunk = f.alloc(0x18 * 256);
            f.names = f.alloc(0x8000);
            f.m.put_u64(table, f.chunk as u64);
            let g = IMG + GOBJ_RVA;
            f.m.put_u64(g, table as u64);
            f.m.put(g + 0x10, &65536u32.to_le_bytes());
            f.m.put(g + 0x18, &1u32.to_le_bytes());
            f.m.put(g + 0x1C, &1u32.to_le_bytes());
            f.m.put_u64(IMG + POOL_RVA + 0x10, f.names as u64);
            f.name("None");
            f.name("ByteProperty");
            f
        }

        fn alloc(&mut self, size: usize) -> usize {
            let a = (self.cur + 15) & !15;
            self.cur = a + size;
            a
        }

        fn name(&mut self, s: &str) -> u32 {
            let idx = (self.ncur / 2) as u32;
            let at = self.names + self.ncur;
            self.m.put(at, &((s.len() as u16) << 6).to_le_bytes());
            self.m.put(at + 2, s.as_bytes());
            self.ncur += (2 + s.len() + 1) & !1;
            idx
        }

        fn object(&mut self, name: &str, class: usize, flags: u32) -> usize {
            let o = self.alloc(0x100);
            let idx = self.name(name);
            let l = self.l;
            self.m.put_u64(o + l.vtable, 0x1111_0000);
            self.m.put(o + l.object_flags, &flags.to_le_bytes());
            self.m.put(o + l.internal_index, &(self.count as u32).to_le_bytes());
            self.m.put_u64(o + l.class, class as u64);
            self.m.put(o + l.name, &idx.to_le_bytes());
            self.m.put_u64(self.chunk + self.count * 0x18, o as u64);
            self.count += 1;
            self.m.put(IMG + GOBJ_RVA + 0x14, &(self.count as u32).to_le_bytes());
            o
        }

        fn class(&mut self, name: &str, class_class: usize, sup: usize) -> usize {
            let c = self.object(name, class_class, 0);
            self.m.put_u64(c + self.l.ustruct_super, sup as u64);
            c
        }

        fn function(&mut self, class: usize, func_class: usize, name: &str) {
            let f = self.object(name, func_class, 0);
            let head = self.m.read_u64(class + self.l.ustruct_children).unwrap();
            self.m.put_u64(f + self.l.ufield_next, head);
            self.m.put_u64(class + self.l.ustruct_children, f as u64);
        }

        fn property(&mut self, ustruct: usize, name: &str, offset: u32, size: u32) {
            let p = self.alloc(0x80);
            let idx = self.name(name);
            self.m.put(p + self.l.ffield_name, &idx.to_le_bytes());
            self.m.put(p + self.l.fproperty_offset, &offset.to_le_bytes());
            self.m.put(p + self.l.fproperty_element_size, &size.to_le_bytes());
            self.m.put_u64(ustruct + self.l.ustruct_child_properties, p as u64);
        }
    }

    fn setup() -> (Fake, Signatures, Vec<ModuleEntry>) {
        let text =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/signatures.toml")).unwrap();
        let mut sigs = Signatures::from_toml_str(&text).unwrap();
        sigs.module.name = "fake.exe".into();
        sigs.module.original_filename.clear();
        sigs.module.scan_sections = vec![".text".into()];
        sigs.raw_offsets.gobjects_rva = Some(GOBJ_RVA as u64);
        sigs.raw_offsets.gnames_rva = Some(POOL_RVA as u64);
        let mut f = Fake::new(sigs.raw_offsets.uobject);
        let class = f.object("Class", 0, 0);
        f.m.put_u64(class + f.l.class, class as u64);
        let object = f.class("Object", class, 0);
        let actor = f.class("Actor", class, object);
        let pawn = f.class("Pawn", class, actor);
        let wvp = f.class("WheeledVehiclePawn", class, pawn);
        let car = f.class("MyRallyCar", class, wvp);
        let func = f.class("Function", class, object);
        f.function(car, func, "ResetCarTransform");
        f.property(car, "SimState", 0x500, 0x200);
        f.object("Default__MyRallyCar", car, acr_ue::reflection::RF_CLASS_DEFAULT_OBJECT);
        let pawn = f.object("MyRallyCar_0", car, 0);
        // Sim body behind pawn+0x300; UE cm, position stored as (z, x, y) of shm (120, 3, -45).
        let sim = f.alloc(0x100);
        f.m.put_u64(pawn + 0x300, sim as u64);
        for (k, v) in [-4500.0f64, 12000.0, 300.0].iter().enumerate() {
            f.m.put(sim + 0x10 + k * 8, &v.to_le_bytes());
        }
        sigs.reflection.vehicle_pawn_class_candidates.insert(0, "MyRallyCar".into());
        let sc = &mut sigs.raw_offsets.sim_car;
        sc.locator = SimCarLocator::Chain;
        sc.chain_root = ChainRoot::Pawn;
        sc.chain = vec![0x300];
        sc.value_type = ValueType::F64;
        sc.length_scale = 100.0;
        sc.position = Some(0x10);
        sc.rotation_matrix = Some(0x40);
        let mods = vec![ModuleEntry { name: "fake.exe".into(), base: IMG, size: 0x4000, path: String::new() }];
        (f, sigs, mods)
    }

    #[test]
    fn sig_test_resolves_synthetic_engine() {
        let (f, sigs, mods) = setup();
        let pc = PageCache::new(&f.m, 256);
        let shm = ShmSample { pos: [120.0, 3.0, -45.0], ..Default::default() };
        let rep = sig_test(&pc, &mods, &sigs, Some(&shm)).unwrap();
        let sc = rep.items.iter().find(|i| i.section == "sim_car").expect("sim_car checked");
        assert_eq!(sc.plausible, Some(true), "{sc:?}");
        assert!(sc.detail.contains("(+z, +x, +y)"), "{}", sc.detail);
        let cfg =
            |sec: &str| rep.items.iter().find(|i| i.section == sec && i.source.starts_with("configured")).unwrap();
        assert_eq!(cfg("gobjects").resolved.as_deref(), Some("0x140002100"));
        assert_eq!(cfg("gobjects").rva.as_deref(), Some("0x2100"));
        assert!(cfg("gobjects").detail.contains("NumElements"), "{:?}", cfg("gobjects"));
        assert_eq!(cfg("fnamepool").plausible, Some(true), "{:?}", cfg("fnamepool"));
        // Missing required functions are reported by key (not "required_functions (a::b)").
        assert!(
            rep.required_functions_missing.contains(&"k2_get_actor_location".to_string()),
            "{:?}",
            rep.required_functions_missing
        );
        assert!(!rep.functions_missing.iter().any(|f| f.starts_with("required_functions")));
        assert!(rep.classes.unwrap() >= 7);
        assert!(rep.missing_core_classes.contains(&"World".to_string()));
        assert!(!rep.missing_core_classes.contains(&"Actor".to_string()));
        if sigs.reflection.vehicle_pawn_class_candidates.iter().any(|c| c == "WheeledVehiclePawn") {
            assert!(rep.vehicle_pawn_classes_present.contains(&"WheeledVehiclePawn".to_string()));
        }
    }

    #[test]
    fn sig_test_wrong_gnames_rva_falls_back_like_the_hook() {
        let (f, mut sigs, mods) = setup();
        sigs.raw_offsets.gnames_rva = Some(0x2800); // a Dumper-7 "GNames" that is not the pool
        let pc = PageCache::new(&f.m, 256);
        let rep = sig_test(&pc, &mods, &sigs, None).unwrap();
        let names = rep.items.iter().find(|i| i.section == "fnamepool" && i.source.starts_with("configured")).unwrap();
        assert_eq!(names.resolved.as_deref(), Some("0x140002400"), "{names:?}");
        assert!(names.source.contains("data scan"), "{names:?}");
        assert_eq!(names.plausible, Some(true), "{names:?}");
        assert!(rep.objects.is_some(), "reflection checks ran");
    }

    #[test]
    fn dumps_filtered_classes_with_props_funcs_instances() {
        let (f, sigs, mods) = setup();
        let pc = PageCache::new(&f.m, 256);
        let module = LoadedModule::load(&pc, &mods, &sigs).unwrap();
        let dm: &dyn Memory = &pc;
        let ue = open_ue(dm, &module, &sigs).unwrap();
        let opts = DumpOptions {
            filters: vec!["rallycar".into()],
            props: true,
            funcs: true,
            instances: true,
            max_instances: 5,
        };
        let out = dump_classes(&ue, &opts).unwrap();
        assert_eq!(out.len(), 1, "{:?}", out.iter().map(|c| &c.name).collect::<Vec<_>>());
        let c = &out[0];
        assert_eq!(c.name, "MyRallyCar");
        assert_eq!(c.kind, "Class");
        assert!(c.hierarchy.iter().any(|h| h == "Pawn"), "{:?}", c.hierarchy);
        assert_eq!(c.functions, ["ResetCarTransform"]);
        assert_eq!(c.properties.len(), 1);
        assert_eq!(c.properties[0].name, "SimState");
        assert_eq!(c.properties[0].offset, "0x500");
        assert_eq!(c.instance_count, 1, "CDO excluded");
        let all = dump_classes(&ue, &DumpOptions { filters: Vec::new(), ..opts }).unwrap();
        assert!(all.len() >= 7);
    }
}
