//! The resolved live game: engine globals, object index, UFunctions, pawn lookup, and the
//! reflection-based online-guard signals.

use crate::mem::{Memory, ProcessMemory};
use crate::pattern::Region;
use crate::pe::ModuleInfo;
use crate::reflection::{FunctionInfo, ObjectIndex, Ue};
use crate::resolve::{resolve_all, Resolution, Scan};
use crate::sigs::Signatures;
use crate::ue_call::{on_game_thread, Params};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

pub type LiveUe = Ue<ProcessMemory>;

/// `[reflection.functions]` resolved lazily: early in startup the classes exist but their
/// UFunctions are not linked yet, so unresolved keys are retried later. A resolved entry
/// never changes, which keeps `&FunctionInfo` borrows valid while others are still filled in.
struct FunctionTable {
    slots: Vec<(String, OnceLock<FunctionInfo>)>,
}

impl FunctionTable {
    fn new(sigs: &Signatures) -> Self {
        Self { slots: sigs.reflection.functions.keys().map(|k| (k.clone(), OnceLock::new())).collect() }
    }

    fn get(&self, key: &str) -> Option<&FunctionInfo> {
        self.slots.iter().find(|(k, _)| k == key)?.1.get()
    }
}

/// Everything resolved at init. Shared (`Arc`) between the tick and the guard thread.
pub struct Game {
    pub ue: LiveUe,
    pub module: ModuleInfo,
    pub module_name: String,
    pub sigs: Signatures,
    pub resolution: Resolution,
    functions: FunctionTable,
    index: RwLock<ObjectIndex>,
    last_index: Mutex<Instant>,
    /// Address of the `GWorld` slot (0 = not located yet). The slot itself is re-read on
    /// every [`Game::world`] call: it is null early in startup and changes per level.
    gworld_slot: AtomicUsize,
    /// Last slot value confirmed to be a `World` (skips the class check while unchanged).
    world_checked: AtomicUsize,
}

/// Why init failed, and whether retrying later may help (engine not initialised yet).
#[derive(Debug, Clone)]
pub struct InitError {
    pub message: String,
    pub retryable: bool,
}

impl std::fmt::Display for InitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn fatal(m: impl Into<String>) -> InitError {
    InitError { message: m.into(), retryable: false }
}

fn retry(m: impl Into<String>) -> InitError {
    InitError { message: m.into(), retryable: true }
}

#[cfg(windows)]
fn srwlock_fns() -> Vec<usize> {
    use windows::core::{s, w};
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    let mut out = Vec::new();
    for (module, func) in [
        (w!("kernel32.dll"), s!("InitializeSRWLock")),
        (w!("kernelbase.dll"), s!("InitializeSRWLock")),
        (w!("ntdll.dll"), s!("RtlInitializeSRWLock")),
    ] {
        // SAFETY: plain lookups of already-loaded system modules.
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
fn srwlock_fns() -> Vec<usize> {
    Vec::new()
}

/// The named sections of a loaded module as scannable regions (unreadable ones skipped).
fn code_regions(module: &ModuleInfo, sections: &[String]) -> Vec<Region<'static>> {
    let mut code = Vec::new();
    for name in sections {
        let Some((addr, len)) = module.section_range(name) else {
            tracing::warn!("scan section {name} not present");
            continue;
        };
        if !crate::mem::is_readable_range(addr, len) {
            tracing::warn!("scan section {name} not fully readable; skipped");
            continue;
        }
        // SAFETY: the range is a committed, readable image section of a module that stays
        // loaded for the life of the process.
        let bytes = unsafe { std::slice::from_raw_parts(addr as *const u8, len) };
        code.push(Region { base: addr, bytes });
    }
    code
}

impl Game {
    /// `[module].scan_sections` of the game module.
    pub fn code_regions(&self) -> Vec<Region<'static>> {
        code_regions(&self.module, &self.sigs.module.scan_sections)
    }

    /// Locates the module, resolves the engine globals and indexes reflection data.
    pub fn init(sigs: Signatures) -> Result<Self, InitError> {
        let mem = ProcessMemory;
        let (base, module_name, exact) =
            crate::pe::find_module(&sigs.module.name).ok_or_else(|| fatal("no main module"))?;
        if !exact {
            tracing::warn!(
                "module '{}' not loaded; scanning main executable '{}' instead",
                sigs.module.name,
                module_name
            );
        }
        let module = ModuleInfo::parse(&mem, base).ok_or_else(|| fatal("failed to parse PE headers"))?;

        let code = code_regions(&module, &sigs.module.scan_sections);
        if code.is_empty() {
            return Err(fatal("no scannable code sections"));
        }

        // Init is retried every few seconds until the engine is up; check the binary once.
        static FILENAME_CHECKED: std::sync::Once = std::sync::Once::new();
        let mut check_filename = false;
        FILENAME_CHECKED.call_once(|| check_filename = true);
        if check_filename && !sigs.module.original_filename.is_empty() {
            let ok = module
                .section_range(".rsrc")
                .and_then(|(a, l)| mem.read_vec(a, l))
                .map(|rsrc| crate::pe::contains_utf16(&rsrc, &sigs.module.original_filename));
            match ok {
                Some(true) => tracing::info!("OriginalFilename '{}' confirmed", sigs.module.original_filename),
                Some(false) => tracing::warn!(
                    "OriginalFilename '{}' not found in .rsrc: this may not be the expected game binary",
                    sigs.module.original_filename
                ),
                None => tracing::warn!("could not read .rsrc to check OriginalFilename"),
            }
        }

        let data_copy = module.section_range(".data").and_then(|(a, l)| mem.read_vec(a, l).map(|v| (a, v)));
        let data = data_copy.as_ref().map(|(a, v)| Region { base: *a, bytes: v });
        let scan = Scan { mem: &mem, module: &module, code, data, srwlock_fns: srwlock_fns(), sigs: &sigs };
        let resolution = resolve_all(&scan).map_err(retry)?;
        let ue = Ue::new(
            mem,
            resolution.globals,
            resolution.gobjects_layout,
            sigs.fnamepool.layout,
            sigs.raw_offsets.uobject,
        );
        let index = ue.build_index().ok_or_else(|| retry("GObjects iteration failed (no 'Class' UClass found yet)"))?;
        if index.class("Actor").is_none() || index.class("Object").is_none() {
            return Err(retry(format!(
                "object index has {} objects / {} classes but no Actor/Object class (names wrong or engine not ready)",
                index.object_count,
                index.classes.len()
            )));
        }
        let gworld_slot = AtomicUsize::new(resolution.globals.gworld.unwrap_or(0));
        let game = Self {
            functions: FunctionTable::new(&sigs),
            ue,
            module,
            module_name,
            sigs,
            resolution,
            index: RwLock::new(index),
            last_index: Mutex::new(Instant::now()),
            gworld_slot,
            world_checked: AtomicUsize::new(0),
        };
        game.resolve_functions();
        let still = game.missing_functions();
        if !still.is_empty() {
            let total = game.functions.slots.len();
            tracing::info!(
                "UFunctions not linked yet: {still:?} ({} of {total} resolved; retried in the background)",
                total - still.len()
            );
        }
        Ok(game)
    }

    /// Looks up every still-unresolved `[reflection.functions]` key in the current object
    /// index. Returns the keys resolved by this call.
    pub fn resolve_functions(&self) -> Vec<String> {
        let index = self.index.read().map(|i| i.clone()).unwrap_or_default();
        let mut newly = Vec::new();
        for (key, slot) in &self.functions.slots {
            if slot.get().is_some() {
                continue;
            }
            let Some(f) = self.sigs.reflection.functions.get(key) else {
                continue;
            };
            let info = index
                .class(&f.class)
                .and_then(|c| self.ue.find_function_in(c, &f.name))
                .and_then(|func| self.ue.function_info(func));
            if let Some(i) = info {
                tracing::debug!(
                    "UFunction {}::{} at {:#x}, parms {} bytes, params {:?}",
                    f.class,
                    f.name,
                    i.addr,
                    i.parms_size,
                    i.params.iter().map(|p| (&p.name, p.offset, p.size)).collect::<Vec<_>>()
                );
                if slot.set(i).is_ok() {
                    newly.push(key.clone());
                }
            }
        }
        newly
    }

    /// Re-indexes GObjects (at most every `min_gap`) and retries the unresolved UFunctions.
    /// `None` = nothing to do (all resolved, or throttled).
    pub fn retry_functions(&self, min_gap: Duration) -> Option<Vec<String>> {
        if self.missing_functions().is_empty() || !self.refresh_index(min_gap) {
            return None;
        }
        Some(self.resolve_functions())
    }

    pub fn func(&self, key: &str) -> Option<&FunctionInfo> {
        self.functions.get(key)
    }

    pub fn resolved_functions(&self) -> Vec<String> {
        self.functions.slots.iter().filter(|(_, s)| s.get().is_some()).map(|(k, _)| k.clone()).collect()
    }

    pub fn missing_functions(&self) -> Vec<String> {
        self.functions.slots.iter().filter(|(_, s)| s.get().is_none()).map(|(k, _)| k.clone()).collect()
    }

    pub fn missing_required_functions(&self) -> Vec<String> {
        self.sigs.reflection.required_functions.iter().filter(|k| self.func(k).is_none()).cloned().collect()
    }

    /// Live object count (GObjects `NumElements`).
    pub fn object_count(&self) -> Option<usize> {
        self.ue.num_objects()
    }

    /// Whether the GWorld slot address is known (its value may still be null).
    pub fn gworld_located(&self) -> bool {
        self.gworld_slot.load(Ordering::Relaxed) != 0
    }

    /// Tries to locate the GWorld slot if init could not (strategies that need a live world).
    /// Returns how it was found.
    pub fn retry_gworld(&self) -> Option<String> {
        if self.gworld_located() {
            return None;
        }
        let data_copy =
            self.module.section_range(".data").and_then(|(a, l)| self.ue.mem.read_vec(a, l).map(|v| (a, v)));
        let scan = Scan {
            mem: &self.ue.mem,
            module: &self.module,
            code: self.code_regions(),
            data: data_copy.as_ref().map(|(a, v)| Region { base: *a, bytes: v }),
            srwlock_fns: Vec::new(),
            sigs: &self.sigs,
        };
        let f = crate::resolve::resolve_gworld(&scan, &self.ue).ok()?;
        self.gworld_slot.store(f.addr, Ordering::Relaxed);
        Some(format!("{:#x} via {}", f.addr, f.how))
    }

    pub fn class(&self, name: &str) -> Option<usize> {
        self.index.read().ok()?.class(name)
    }

    pub fn cdo(&self, class_name: &str) -> Option<usize> {
        self.index.read().ok()?.cdo_of(class_name)
    }

    /// Rebuilds the object index (newly loaded Blueprint classes), at most every `min_gap`.
    pub fn refresh_index(&self, min_gap: Duration) -> bool {
        {
            let Ok(mut last) = self.last_index.lock() else {
                return false;
            };
            if last.elapsed() < min_gap {
                return false;
            }
            *last = Instant::now();
        }
        match self.ue.build_index() {
            Some(i) => {
                if let Ok(mut w) = self.index.write() {
                    *w = i;
                }
                true
            }
            None => false,
        }
    }

    /// The current `UWorld`, read fresh from the GWorld slot (null before the first level
    /// loads; changes on every level change).
    pub fn world(&self) -> Option<usize> {
        let slot = self.gworld_slot.load(Ordering::Relaxed);
        if slot == 0 {
            return None;
        }
        let w = self.ue.mem.read_valid_ptr(slot)?;
        if self.world_checked.load(Ordering::Relaxed) == w {
            return Some(w);
        }
        if self.ue.class_name(w).as_deref() != Some("World") {
            return None;
        }
        self.world_checked.store(w, Ordering::Relaxed);
        Some(w)
    }

    /// Short name of the current world (e.g. the stage map), for logs.
    pub fn world_name(&self) -> Option<String> {
        self.ue.name(self.world()?)
    }

    /// Calls a resolved static `GameplayStatics`-style function on its class CDO.
    pub fn call_static(
        &self,
        key: &str,
        fill: impl FnOnce(&mut Params<'_>) -> Result<(), crate::ue_call::CallError>,
    ) -> Result<Vec<u8>, String> {
        let info = self.func(key).ok_or_else(|| format!("{key} not resolved"))?;
        let class = &self.sigs.reflection.functions[key].class;
        let cdo = self.cdo(class).ok_or_else(|| format!("Default__{class} not found"))?;
        let mut p = Params::new(info);
        fill(&mut p).map_err(|e| e.to_string())?;
        p.call(cdo).map_err(|e| e.to_string())?;
        Ok(p.bytes().to_vec())
    }

    /// `UGameplayStatics::GetPlayerPawn(GWorld, index)`; game thread only.
    pub fn player_pawn(&self, index: i32) -> Option<usize> {
        let world = self.world()?;
        let info = self.func("get_player_pawn")?;
        let class = &self.sigs.reflection.functions["get_player_pawn"].class;
        let cdo = self.cdo(class)?;
        let mut p = Params::new(info);
        p.set_ptr("WorldContextObject", world).ok()?;
        p.set_i32("PlayerIndex", index).ok()?;
        p.call(cdo).ok()?;
        p.get_ptr("ReturnValue").ok().filter(|&x| crate::mem::is_plausible_ptr(x))
    }
}

// ---- Online-guard signals (generic so they can be tested on a fake object graph) --------

/// `UWorld::NetDriver` / `DemoNetDriver` offsets in the UE 5.6.1 SDK dump for build 25170642,
/// used only if reflection cannot find the properties.
const SDK_WORLD_NET_DRIVER: usize = 0x38;
const SDK_WORLD_DEMO_NET_DRIVER: usize = 0x140;

fn world_ptr_member<M: Memory>(ue: &Ue<M>, world: usize, class: usize, prop: &str, sdk: usize) -> Option<usize> {
    let offset = match ue.find_property(class, prop) {
        Some(p) => p.offset,
        None if ue.name(class).as_deref() == Some("World") => sdk,
        None => return None,
    };
    ue.mem.read_ptr(world + offset)
}

/// Mirrors `UWorld::GetNetMode`: standalone iff there is no `NetDriver` (server / client)
/// and no `DemoNetDriver` (replay). `None` if `NetDriver` is not evaluable; an unreadable
/// `DemoNetDriver` is ignored (a replay is not an online session).
pub fn world_is_standalone<M: Memory>(ue: &Ue<M>, world: usize) -> Option<bool> {
    let class = ue.class_of(world)?;
    let net = world_ptr_member(ue, world, class, "NetDriver", SDK_WORLD_NET_DRIVER)?;
    let demo = world_ptr_member(ue, world, class, "DemoNetDriver", SDK_WORLD_DEMO_NET_DRIVER).unwrap_or(0);
    Some(net == 0 && demo == 0)
}

/// Class name of an object-pointer property of the world (e.g. "AuthorityGameMode").
/// `Some(None)` if the property exists but is null.
pub fn world_member_class<M: Memory>(ue: &Ue<M>, world: usize, prop: &str) -> Option<Option<String>> {
    let class = ue.class_of(world)?;
    let p = ue.find_property(class, prop)?;
    let obj = ue.mem.read_ptr(world + p.offset)?;
    if obj == 0 {
        return Some(None);
    }
    Some(Some(ue.class_name(obj)?))
}

/// Scans GObjects for a live (non-CDO) instance whose class name is in `names`.
/// `Some(Some(class))` = found, `Some(None)` = none, `None` = GObjects unreadable.
pub fn find_live_blocked_class<M: Memory>(
    ue: &Ue<M>,
    names: &[String],
    cache: &mut HashMap<usize, bool>,
) -> Option<Option<String>> {
    if names.is_empty() {
        return Some(None);
    }
    let set: HashSet<&str> = names.iter().map(String::as_str).collect();
    for h in ue.object_headers()? {
        if h.flags & crate::reflection::RF_CLASS_DEFAULT_OBJECT != 0 {
            continue;
        }
        let blocked =
            *cache.entry(h.class).or_insert_with(|| ue.name(h.class).is_some_and(|n| set.contains(n.as_str())));
        if blocked {
            return Some(ue.name(h.class));
        }
    }
    Some(None)
}

// ---- Pawn lookup ------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalPawn {
    Pawn(usize),
    /// The chain resolves but the controller has no pawn right now (menus, loading).
    NoPawn,
    /// A reflected property is missing / unreadable.
    Unresolved,
}

/// `World.OwningGameInstance.LocalPlayers[index].PlayerController.AcknowledgedPawn` (falls back
/// to `Controller.Pawn`), all through reflected property offsets. Plain reads: any thread.
pub fn local_player_pawn<M: Memory>(ue: &Ue<M>, world: usize, index: usize) -> LocalPawn {
    let chain = || -> Option<Option<usize>> {
        let gi = ue.read_object_property(world, "OwningGameInstance")?;
        let lp = ue.find_property(ue.class_of(gi)?, "LocalPlayers")?;
        let data = ue.mem.read_ptr(gi + lp.offset)?;
        let num = ue.mem.read_i32(gi + lp.offset + 8)?;
        if !(0..=64).contains(&num) {
            return None;
        }
        if index >= num as usize || data == 0 {
            return Some(None);
        }
        let Some(player) = ue.mem.read_valid_ptr(data + index * 8) else {
            return Some(None);
        };
        let pc_prop = ue.find_property(ue.class_of(player)?, "PlayerController")?;
        let Some(pc) = ue.mem.read_valid_ptr(player + pc_prop.offset) else {
            return Some(None);
        };
        let pc_class = ue.class_of(pc)?;
        let prop = ue.find_property(pc_class, "AcknowledgedPawn").or_else(|| ue.find_property(pc_class, "Pawn"))?;
        Some(ue.mem.read_valid_ptr(pc + prop.offset))
    };
    match chain() {
        Some(Some(p)) => LocalPawn::Pawn(p),
        Some(None) => LocalPawn::NoPawn,
        None => LocalPawn::Unresolved,
    }
}

/// Finds the player car pawn per `[reflection]`, caching it between frames.
pub struct PawnLocator {
    game: Arc<Game>,
    candidates: Vec<usize>,
    accepted: HashMap<usize, bool>,
    pawn: Option<(usize, usize)>,
    last_scan: Option<Instant>,
    warned: HashSet<usize>,
}

const RESCAN_GAP: Duration = Duration::from_secs(2);
const REINDEX_GAP: Duration = Duration::from_secs(5);

impl PawnLocator {
    pub fn new(game: Arc<Game>) -> Self {
        let mut s = Self {
            game,
            candidates: Vec::new(),
            accepted: HashMap::new(),
            pawn: None,
            last_scan: None,
            warned: HashSet::new(),
        };
        s.resolve_candidates();
        s
    }

    fn resolve_candidates(&mut self) {
        let names = &self.game.sigs.reflection.vehicle_pawn_class_candidates;
        let found: Vec<usize> = names.iter().filter_map(|n| self.game.class(n)).collect();
        if found != self.candidates {
            let found_names: Vec<&String> = names.iter().filter(|n| self.game.class(n).is_some()).collect();
            tracing::info!("vehicle pawn classes present: {found_names:?}");
            self.candidates = found;
            self.accepted.clear();
        }
    }

    fn is_vehicle_class(&mut self, class: usize) -> bool {
        if let Some(&a) = self.accepted.get(&class) {
            return a;
        }
        let ue = &self.game.ue;
        let ok = self.candidates.iter().any(|&c| ue.class_is_a(class, c));
        self.accepted.insert(class, ok);
        ok
    }

    pub fn current(&self) -> Option<usize> {
        self.pawn.map(|(p, _)| p)
    }

    fn still_valid(&self, pawn: usize, class: usize) -> bool {
        let ue = &self.game.ue;
        ue.class_of(pawn) == Some(class) && !ue.is_cdo(pawn)
    }

    /// Accepts `p` (the local player's pawn) if its class is a vehicle candidate.
    fn accept_player_pawn(&mut self, p: usize) -> Option<usize> {
        if let Some((cached, class)) = self.pawn {
            if cached == p && self.still_valid(p, class) {
                return Some(p);
            }
        }
        let class = self.game.ue.class_of(p)?;
        if !self.is_vehicle_class(class) {
            if self.game.refresh_index(REINDEX_GAP) {
                self.resolve_candidates();
                if self.is_vehicle_class(class) {
                    self.pawn = Some((p, class));
                    return Some(p);
                }
            }
            if self.warned.insert(class) {
                tracing::warn!(
                    "player pawn class hierarchy {:?} matches none of vehicle_pawn_class_candidates; \
                     add the real car class to signatures.toml [reflection]",
                    self.game.ue.class_hierarchy(class)
                );
            }
            self.pawn = None;
            return None;
        }
        self.pawn = Some((p, class));
        Some(p)
    }

    /// Returns the player car, re-resolving if needed.
    pub fn locate(&mut self) -> Option<usize> {
        let game = self.game.clone();
        let r = &game.sigs.reflection;
        if r.pawn_lookup == "local_player" {
            let index = usize::try_from(r.player_index).unwrap_or(0);
            let pawn = game.world().map(|w| local_player_pawn(&game.ue, w, index));
            match pawn {
                Some(LocalPawn::Pawn(p)) => return self.accept_player_pawn(p),
                Some(LocalPawn::NoPawn) => {
                    self.pawn = None;
                    return None;
                }
                // Reflection chain unavailable: fall through to the class scan.
                Some(LocalPawn::Unresolved) | None => {}
            }
        }
        let use_player_pawn =
            r.pawn_lookup == "player_pawn" && on_game_thread() && game.func("get_player_pawn").is_some();
        if use_player_pawn {
            let Some(p) = game.player_pawn(r.player_index) else {
                self.pawn = None;
                return None;
            };
            return self.accept_player_pawn(p);
        }

        if let Some((p, c)) = self.pawn {
            if self.still_valid(p, c) {
                return Some(p);
            }
            self.pawn = None;
        }
        if self.last_scan.is_some_and(|t| t.elapsed() < RESCAN_GAP) {
            return None;
        }
        self.last_scan = Some(Instant::now());
        if self.game.refresh_index(REINDEX_GAP) {
            self.resolve_candidates();
        }
        if self.candidates.is_empty() {
            return None;
        }
        let headers = self.game.ue.object_headers()?;
        for h in headers {
            if h.flags & crate::reflection::RF_CLASS_DEFAULT_OBJECT == 0 && self.is_vehicle_class(h.class) {
                self.pawn = Some((h.obj, h.class));
                return Some(h.obj);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reflection::tests::{graph, FakeUe};
    use crate::reflection::RF_CLASS_DEFAULT_OBJECT;

    #[test]
    fn standalone_and_game_mode_signals() {
        let mut f = FakeUe::new();
        let g = graph(&mut f);
        let world_cls = f.object("World", g.class, 0, 0);
        f.property(world_cls, "NetDriver", 0x80, 8);
        f.property(world_cls, "AuthorityGameMode", 0x88, 8);
        f.property(world_cls, "DemoNetDriver", 0x90, 8);
        let gm_cls = f.object("ACRTimeAttackGameMode", g.class, 0, 0);
        let gm = f.object("GM", gm_cls, 0, 0);
        let world = f.object("Stage", world_cls, 0, 0);
        let ue = f.ue();
        assert_eq!(world_is_standalone(&ue, world), Some(true));
        assert_eq!(world_member_class(&ue, world, "AuthorityGameMode"), Some(None));
        // A replay (DemoNetDriver) is not standalone either.
        f.w64(world + 0x90, 0x5555_1000);
        assert_eq!(world_is_standalone(&f.ue(), world), Some(false));
        f.w64(world + 0x90, 0);
        f.w64(world + 0x80, 0x5555_0000);
        f.w64(world + 0x88, gm as u64);
        let ue = f.ue();
        assert_eq!(world_is_standalone(&ue, world), Some(false));
        assert_eq!(world_member_class(&ue, world, "AuthorityGameMode"), Some(Some("ACRTimeAttackGameMode".into())));
        assert_eq!(world_member_class(&ue, world, "Missing"), None);
        // Object without the property -> not evaluable.
        assert_eq!(world_is_standalone(&ue, g.actor), None);
    }

    #[test]
    fn standalone_falls_back_to_sdk_offsets_for_world() {
        let mut f = FakeUe::new();
        let g = graph(&mut f);
        let world_cls = f.object("World", g.class, 0, 0);
        let world = f.object("Stage", world_cls, 0, 0);
        f.alloc(0x100);
        f.w64(world + SDK_WORLD_NET_DRIVER, 0);
        f.w64(world + SDK_WORLD_DEMO_NET_DRIVER, 0);
        assert_eq!(world_is_standalone(&f.ue(), world), Some(true));
        f.w64(world + SDK_WORLD_NET_DRIVER, 0x5555_0000);
        assert_eq!(world_is_standalone(&f.ue(), world), Some(false));
    }

    #[test]
    fn local_player_pawn_chain() {
        let mut f = FakeUe::new();
        let g = graph(&mut f);
        let world_cls = f.object("World", g.class, 0, 0);
        f.property(world_cls, "OwningGameInstance", 0x30, 8);
        let gi_cls = f.object("GameInstance", g.class, 0, 0);
        f.property(gi_cls, "LocalPlayers", 0x38, 0x10);
        let lp_cls = f.object("LocalPlayer", g.class, 0, 0);
        f.property(lp_cls, "PlayerController", 0x30, 8);
        let pc_cls = f.object("PlayerController", g.class, 0, 0);
        f.property(pc_cls, "Pawn", 0x40, 8);
        f.property(pc_cls, "AcknowledgedPawn", 0x48, 8);
        let world = f.object("World_0", world_cls, 0, 0);
        let gi = f.object("GI", gi_cls, 0, 0);
        let lp = f.object("LP", lp_cls, 0, 0);
        let pc = f.object("PC", pc_cls, 0, 0);
        let car = f.object("MyCar_0", g.car, 0, 0);
        let arr = f.alloc(16);
        {
            let ue = f.ue();
            assert_eq!(local_player_pawn(&ue, world, 0), LocalPawn::Unresolved, "no game instance");
        }
        f.w64(world + 0x30, gi as u64);
        f.w64(gi + 0x38, arr as u64);
        f.w32(gi + 0x40, 0);
        assert_eq!(local_player_pawn(&f.ue(), world, 0), LocalPawn::NoPawn, "no local players yet");
        f.w64(arr, lp as u64);
        f.w32(gi + 0x40, 1);
        f.w64(lp + 0x30, pc as u64);
        assert_eq!(local_player_pawn(&f.ue(), world, 0), LocalPawn::NoPawn, "controller without pawn");
        f.w64(pc + 0x48, car as u64);
        assert_eq!(local_player_pawn(&f.ue(), world, 0), LocalPawn::Pawn(car));
        assert_eq!(local_player_pawn(&f.ue(), world, 1), LocalPawn::NoPawn, "index out of range");
        // Garbage count -> not trusted.
        f.w32(gi + 0x40, 100_000);
        assert_eq!(local_player_pawn(&f.ue(), world, 0), LocalPawn::Unresolved);
    }

    #[test]
    fn blocked_live_class_scan() {
        let mut f = FakeUe::new();
        let g = graph(&mut f);
        let lobby = f.object("ACRLobbySubsystem", g.class, 0, 0);
        let _cdo = f.object("Default__ACRLobbySubsystem", lobby, 0, RF_CLASS_DEFAULT_OBJECT);
        let names = vec!["ACRLobbySubsystem".to_string()];
        let mut cache = HashMap::new();
        {
            let ue = f.ue();
            assert_eq!(find_live_blocked_class(&ue, &names, &mut cache), Some(None), "CDO ignored");
            assert_eq!(find_live_blocked_class(&ue, &[], &mut cache), Some(None));
        }
        f.object("LobbyInstance", lobby, 0, 0);
        let ue = f.ue();
        assert_eq!(find_live_blocked_class(&ue, &names, &mut cache), Some(Some("ACRLobbySubsystem".into())));
    }
}
