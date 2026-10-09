//! Startup sequence, run on a dedicated thread spawned from `DllMain`.
//!
//! Every failure path logs why, shows it on the overlay status line, and leaves the game
//! untouched (no write path is installed).

use crate::config::{steam_build_id, HookConfig};
use crate::driver::{Driver, DriverOptions, Upgrade};
use crate::engine;
use crate::hooks::{self, TickSource};
use crate::input::ActionTracker;
use crate::overlay::{self, StatusKind};
use crate::runtime::{self, LiveShm, Runtime};
use crate::settings;
use acr_ue::freeze::{CustomTimeDilation, Freezer, GlobalTimeDilation, NoFreeze, PhysicsAvatarPause};
use acr_ue::mem::ProcessMemory;
use acr_ue::pattern::resolve_first;
use acr_ue::sigs::BackendKind;
use acr_ue::sim_car::{BodyTracker, PawnFn};
use acr_ue::{CarAvatarAdapter, CarBackend, Game, PawnLocator, Signatures, SimCarAdapter, UeActorAdapter};
use rewind_core::Config;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const LOG_FILE: &str = "acr-rewind.log";
pub const CONFIG_FILE: &str = "acr-rewind.toml";
pub const SIGNATURES_FILE: &str = "signatures.toml";

fn disable(reason: impl Into<String>) {
    let r = reason.into();
    tracing::error!("rewind disabled: {r}");
    overlay::set_status(StatusKind::Disabled, r);
}

fn init_logging(path: &Path, level: &str) {
    use tracing_subscriber::EnvFilter;
    let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .with_thread_ids(true)
        .with_env_filter(filter)
        .try_init();
    std::panic::set_hook(Box::new(|info| {
        tracing::error!("panic: {info}");
    }));
}

fn load_configs(dir: &Path) -> (Result<(Config, HookConfig), String>, String) {
    let path = dir.join(CONFIG_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return (Err(format!("{} not readable: {e}", path.display())), "info".into()),
    };
    let hook = HookConfig::from_toml_str(&text);
    let level = hook.as_ref().map_or_else(|_| "info".to_owned(), |h| h.log_level.clone());
    let core = Config::from_toml_str(&text).map_err(|e| format!("{CONFIG_FILE}: {e}"));
    let result = match (core, hook) {
        (Ok(c), Ok(h)) => Ok((c, h)),
        (Err(e), _) => Err(e),
        (_, Err(e)) => Err(format!("{CONFIG_FILE} [hook]: {e}")),
    };
    (result, level)
}

/// Picks and installs the world-tick hook per `[world_tick]`. Falls back to the timer.
fn install_tick_source(game: &Game, sigs: &Signatures) -> TickSource {
    let wt = &sigs.world_tick;
    let pe = game.resolution.globals.process_event;
    let attempt: Result<TickSource, String> = match wt.strategy.as_str() {
        "process_event" => pe
            .ok_or_else(|| "ProcessEvent not resolved".to_owned())
            .and_then(hooks::install_process_event)
            .map(|_| TickSource::ProcessEvent),
        "sim_step" => sigs
            .raw_offsets
            .sim_step_rva
            .ok_or_else(|| "raw_offsets.sim_step_rva not set".to_owned())
            .and_then(|rva| hooks::install_tick_thunk(game.module.rva(rva), true))
            .map(|_| TickSource::SimStep),
        "pattern" => resolve_first(&wt.candidates, &game.code_regions(), &game.ue.mem)
            .map_err(|e| format!("world_tick candidates: {e:?}"))
            .and_then(|addr| hooks::install_tick_thunk(addr, false))
            .map(|_| TickSource::Pattern),
        "vtable" => (|| {
            let idx = wt.vtable_index.ok_or("world_tick.vtable_index not set")?;
            let class = game.class(&wt.vtable_class).ok_or_else(|| format!("class {} not found", wt.vtable_class))?;
            let inst =
                game.ue.find_live_instance_of(class).ok_or_else(|| format!("no live {} instance", wt.vtable_class))?;
            let ue = &game.ue;
            use acr_ue::mem::Memory;
            let vft = ue.mem.read_valid_ptr(inst).ok_or("vtable unreadable")?;
            let f = ue.mem.read_valid_ptr(vft + idx * 8).ok_or("vtable slot unreadable")?;
            if !game.module.contains(f) {
                return Err(format!("vtable[{idx:#x}] = {f:#x} outside the module"));
            }
            hooks::install_tick_thunk(f, false).map(|_| TickSource::Vtable)
        })(),
        other => Err(format!("unknown world_tick.strategy '{other}'")),
    };
    if let Some(pe) = pe {
        if !matches!(attempt, Ok(TickSource::ProcessEvent)) {
            hooks::use_process_event_unhooked(pe);
        }
    }
    match attempt {
        Ok(src) => {
            tracing::info!("tick source: {src:?} (world_tick.strategy = {})", wt.strategy);
            src
        }
        Err(e) => {
            tracing::warn!(
                "world_tick '{}' unavailable ({e}); falling back to a 16 ms timer thread \
                 (raw-memory paths only, no UFunction calls)",
                wt.strategy
            );
            hooks::spawn_timer();
            TickSource::Timer
        }
    }
}

fn make_freezer(game: &Arc<Game>, sigs: &Signatures, tick: TickSource) -> Box<dyn Freezer> {
    for lever in &sigs.freeze.levers {
        let f: Option<Box<dyn Freezer>> = match lever.as_str() {
            "sim_step" if tick == TickSource::SimStep => Some(Box::new(hooks::SimStepFreezer)),
            "custom_time_dilation" => CustomTimeDilation::new(game.clone()).map(|f| Box::new(f) as _),
            "global_time_dilation" if tick != TickSource::Timer => {
                GlobalTimeDilation::new(game.clone()).map(|f| Box::new(f) as _)
            }
            "physics_avatar_pause" if tick != TickSource::Timer => {
                PhysicsAvatarPause::new(game.clone()).map(|f| Box::new(f) as _)
            }
            "none" => Some(Box::new(NoFreeze)),
            _ => None,
        };
        if let Some(f) = f {
            tracing::info!("freeze lever: {}", f.name());
            return f;
        }
        tracing::info!("freeze lever '{lever}' not available");
    }
    tracing::warn!("no freeze lever available; the car is held by re-writing its pose every frame");
    Box::new(NoFreeze)
}

fn try_backend(
    kind: BackendKind,
    game: &Arc<Game>,
    sigs: &Signatures,
    cfg: &Config,
    tick: TickSource,
    reuse_bodies: Option<BodyTracker>,
) -> Result<Box<dyn CarBackend>, String> {
    let spec = &sigs.raw_offsets.sim_car;
    let game_thread = tick != TickSource::Timer;
    match kind {
        BackendKind::CarAvatar => {
            if !game_thread {
                return Err("needs a game-thread tick for ProcessEvent (tick source is the timer thread)".into());
            }
            if game.resolution.globals.process_event.is_none() {
                return Err("ProcessEvent not resolved".into());
            }
            let bodies = reuse_bodies.or_else(|| {
                BodyTracker::spawn(
                    Arc::new(ProcessMemory),
                    spec,
                    game.module.base,
                    runtime::shm_position_hint(),
                    cfg.capacity(),
                )
            });
            if bodies.is_none() {
                tracing::warn!(
                    "car_avatar: [raw_offsets.sim_car] vtable_scan not configured; angular velocity is not restored"
                );
            }
            let freezer = make_freezer(game, sigs, tick);
            CarAvatarAdapter::new(
                game.clone(),
                sigs.backend.car_avatar.clone(),
                bodies,
                runtime::shm_position_hint(),
                freezer,
            )
            .map(|b| Box::new(b) as Box<dyn CarBackend>)
        }
        BackendKind::SimCar => {
            if !spec.is_configured() {
                return Err("[raw_offsets.sim_car] not configured".into());
            }
            let missing = spec.missing_for_full_restore();
            if !missing.is_empty() {
                tracing::warn!("sim_car: resume will be incomplete, offsets missing for {missing:?}");
            }
            tracing::info!(
                "sim_car locator {:?} (vtable_rva {:?}); the car position hint comes from shm graphics carCoordinates",
                spec.locator,
                spec.vtable_rva
            );
            let mut locator = PawnLocator::new(game.clone());
            let pawn: PawnFn = Box::new(move || locator.locate());
            Ok(Box::new(SimCarAdapter::new(
                ProcessMemory,
                spec.clone(),
                game.module.base,
                pawn,
                runtime::shm_position_hint(),
                make_freezer(game, sigs, tick),
                cfg.capacity(),
            )))
        }
        BackendKind::UeActor => {
            if !game_thread {
                return Err("needs a game-thread tick for ProcessEvent".into());
            }
            tracing::warn!(
                "using the generic UE actor fallback (ProcessEvent teleport); with ACR's dmphysics solver \
                 this is expected NOT to move the simulated car"
            );
            UeActorAdapter::new(game.clone(), make_freezer(game, sigs, tick))
                .map(|b| Box::new(b) as Box<dyn CarBackend>)
        }
    }
}

/// First buildable backend of `[backend] order`.
fn make_backend(
    game: &Arc<Game>,
    sigs: &Signatures,
    cfg: &Config,
    tick: TickSource,
) -> Result<Box<dyn CarBackend>, String> {
    let mut why = Vec::new();
    for &kind in &sigs.backend.order {
        match try_backend(kind, game, sigs, cfg, tick, None) {
            Ok(b) => {
                if !why.is_empty() {
                    tracing::warn!("backend {kind:?} chosen after skipping: {}", why.join("; "));
                }
                return Ok(b);
            }
            Err(e) => {
                tracing::info!("backend {kind:?} unavailable: {e}");
                why.push(format!("{kind:?}: {e}"));
            }
        }
    }
    Err(format!("no write path ({}); see docs/re-notes.md §8-9", why.join("; ")))
}

/// UFunction keys some configured backend needs: retried fast until they resolve.
fn backend_functions(sigs: &Signatures) -> Vec<String> {
    let mut keys: Vec<String> = sigs.reflection.required_functions.clone();
    if sigs.backend.order.contains(&BackendKind::CarAvatar) {
        keys.extend(acr_ue::car_avatar::CORE_FUNCTIONS.iter().map(|k| (*k).to_owned()));
        keys.extend(sigs.backend.car_avatar.required_functions.iter().cloned());
    }
    keys.sort();
    keys.dedup();
    keys
}

/// Upgrade to car_avatar once its UFunctions resolve (init may run before they are linked),
/// if it ranks above the backend chosen at init. Called from the tick at safe points.
fn make_upgrade(game: &Arc<Game>, sigs: &Signatures, cfg: &Config, tick: TickSource, current: &str) -> Option<Upgrade> {
    let order = &sigs.backend.order;
    let avatar = order.iter().position(|k| *k == BackendKind::CarAvatar)?;
    let chosen = order.iter().position(|k| backend_name(*k) == current)?;
    if avatar >= chosen || tick == TickSource::Timer || game.resolution.globals.process_event.is_none() {
        return None;
    }
    tracing::info!("backend {current}: car_avatar ranks higher; upgrading once its UFunctions resolve");
    let (game, sigs, cfg) = (game.clone(), sigs.clone(), cfg.clone());
    let mut last_reason = String::new();
    let mut done = false;
    Some(Box::new(move |current: &mut dyn CarBackend| {
        if done || current.name() == backend_name(BackendKind::CarAvatar) {
            return None;
        }
        if let Err(why) = CarAvatarAdapter::check_requirements(&game, &sigs.backend.car_avatar) {
            if why != last_reason {
                tracing::info!("backend upgrade to car_avatar pending: {why}");
                last_reason = why;
            }
            return None;
        }
        // Keep the locked body set if it still checks out; otherwise car_avatar scans afresh.
        let hint = runtime::shm_position_hint();
        let reuse = current.take_body_tracker().filter(|tr| {
            let ok = tr.still_valid(&ProcessMemory, &hint);
            tracing::info!(
                "backend upgrade: {} the {} locked sim bodies",
                if ok { "reusing" } else { "rescanning; not reusing" },
                tr.car().map_or(0, |c| c.bodies.len())
            );
            ok
        });
        let current = current.name();
        match try_backend(BackendKind::CarAvatar, &game, &sigs, &cfg, tick, reuse) {
            Ok(b) => {
                done = true;
                Some(b)
            }
            Err(e) => {
                tracing::warn!("backend upgrade to car_avatar failed: {e}; staying on {current}");
                done = true;
                None
            }
        }
    }))
}

fn backend_name(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::CarAvatar => "car_avatar",
        BackendKind::SimCar => "sim_car",
        BackendKind::UeActor => "ue_actor",
    }
}

/// Upper bound on waiting for a loaded world after the engine globals resolve.
const ENGINE_SETTLE_MAX: Duration = Duration::from_secs(30);

/// Re-log an unchanged "waiting for the engine" reason at least this often.
const WAIT_LOG_REPEAT: Duration = Duration::from_secs(15);

/// Dedupes the init retry log: a new reason is logged at once, an unchanged one every
/// [`WAIT_LOG_REPEAT`] (with the attempt count and elapsed time).
#[derive(Default)]
struct WaitLog {
    last_msg: String,
    last_at: Option<Instant>,
}

impl WaitLog {
    fn line(&mut self, msg: &str, attempt: u32, elapsed: Duration, now: Instant) -> Option<String> {
        let changed = msg != self.last_msg;
        if !changed && self.last_at.is_some_and(|t| now.duration_since(t) < WAIT_LOG_REPEAT) {
            return None;
        }
        self.last_msg = msg.to_owned();
        self.last_at = Some(now);
        Some(if changed {
            format!("waiting for the engine (attempt {attempt}, {:.0} s): {msg}", elapsed.as_secs_f64())
        } else {
            format!("still waiting for the engine (attempt {attempt}, {:.0} s): {msg}", elapsed.as_secs_f64())
        })
    }
}

fn wait_for_game(sigs: &Signatures, timeout: Duration) -> Result<Game, String> {
    let start = Instant::now();
    let mut log = WaitLog::default();
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match Game::init(sigs.clone()) {
            Ok(g) => return Ok(g),
            Err(e) if e.retryable && start.elapsed() < timeout => {
                if let Some(line) = log.line(&e.message, attempt, start.elapsed(), Instant::now()) {
                    tracing::info!("{line}");
                    overlay::set_status(StatusKind::Starting, "waiting for the engine");
                }
                std::thread::sleep(Duration::from_secs(3));
            }
            Err(e) => {
                return Err(format!("{} (after {attempt} attempts, {:.0} s)", e.message, start.elapsed().as_secs_f64()))
            }
        }
    }
}

pub fn run(hinstance: usize, dll_dir: PathBuf) {
    let (configs, level) = load_configs(&dll_dir);
    init_logging(&dll_dir.join(LOG_FILE), &level);
    let exe = std::env::current_exe().unwrap_or_default();
    tracing::info!(
        "acr-rewind {} loaded into {} (pid {}) from {}",
        env!("CARGO_PKG_VERSION"),
        exe.display(),
        std::process::id(),
        dll_dir.display()
    );

    let (cfg, hook_cfg) = match configs {
        Ok(c) => c,
        Err(e) => {
            if let Err(oe) = overlay::start(hinstance, "dx12") {
                tracing::warn!("overlay: {oe}");
            }
            return disable(format!("config error: {e}"));
        }
    };
    tracing::info!("config: {cfg:?}");
    tracing::info!("hook config: {hook_cfg:?}");
    for w in &cfg.bindings.warnings {
        tracing::warn!("bindings: {w}");
    }
    if cfg.bindings.rewind.is_empty() {
        tracing::warn!("no rewind binding configured (open the settings panel to add one)");
    }
    crate::input::hub::start();
    settings::init(cfg.clone(), Some(dll_dir.join(CONFIG_FILE)));

    if hook_cfg.overlay != "off" {
        match overlay::start(hinstance, &hook_cfg.overlay) {
            Ok(()) => tracing::info!("overlay hooks installed ({})", hook_cfg.overlay),
            Err(e) => tracing::warn!("overlay unavailable: {e}"),
        }
    }
    overlay::set_status(StatusKind::Starting, "loading signatures");

    let sigs_path = dll_dir.join(SIGNATURES_FILE);
    let sigs = match std::fs::read_to_string(&sigs_path)
        .map_err(|e| format!("{} not readable: {e}", sigs_path.display()))
        .and_then(|t| Signatures::from_toml_str(&t).map_err(|e| e.to_string()))
    {
        Ok(s) => s,
        Err(e) => return disable(e),
    };

    let running_build = steam_build_id(&exe);
    tracing::info!(
        "game build {:?}, signatures verified for {:?} (written for {})",
        running_build,
        sigs.verified_build,
        sigs.target_build
    );
    match sigs.build_matches(running_build.as_deref()) {
        Some(true) => {}
        Some(false) if sigs.require_build_match => {
            return disable(format!(
                "game build {} != verified_build {} (require_build_match = true)",
                running_build.unwrap_or_default(),
                sigs.verified_build
            ))
        }
        Some(false) => tracing::warn!("game build differs from verified_build: signatures may be stale"),
        None => tracing::warn!("signatures.toml has never been verified in-game (verified_build is empty)"),
    }

    if !cfg.enabled {
        return disable("enabled = false in acr-rewind.toml");
    }

    crate::guard::monitor::spawn(sigs.online_guard.clone());
    match hooks::detect_game_thread() {
        Some(tid) => tracing::info!("game (main) thread id {tid}"),
        None => tracing::warn!("could not determine the main thread; UFunction calls will be refused"),
    }

    let game = match wait_for_game(&sigs, Duration::from_secs(hook_cfg.init_timeout_s)) {
        Ok(g) => Arc::new(g),
        Err(e) => return disable(format!("engine globals unresolved: {e}")),
    };
    let r = &game.resolution;
    tracing::info!("module {} base {:#x} size {:#x}", game.module_name, game.module.base, game.module.size);
    tracing::info!("GObjects: {} ({:?})", r.gobjects_how, r.gobjects_layout);
    tracing::info!("FNamePool: {}", r.fnamepool_how);
    tracing::info!("GWorld: {:?}", r.gworld);
    tracing::info!("ProcessEvent: {:?}", r.process_event);
    crate::guard::monitor::set_game(game.clone());
    engine::live::settle(&game, ENGINE_SETTLE_MAX);
    tracing::info!("UFunctions resolved: {:?}", game.resolved_functions());
    engine::live::spawn(game.clone(), backend_functions(&sigs));

    hooks::init_gate(sigs.world_tick.frame_source == "present_counter" && hook_cfg.overlay != "off");
    let tick = install_tick_source(&game, &sigs);
    let backend = match make_backend(&game, &sigs, &cfg, tick) {
        Ok(b) => b,
        Err(e) => return disable(e),
    };
    tracing::info!("backend: {}", backend.describe());

    let opts = DriverOptions {
        read_only: hook_cfg.read_only,
        post_resume_check: hook_cfg.post_resume_check,
        validation_interval_s: hook_cfg.validation_log_interval_ms as f64 / 1000.0,
    };
    if opts.read_only {
        tracing::info!("READ-ONLY validation mode: snapshots are logged, nothing is written");
    }
    let mut driver = Driver::new(cfg.clone(), backend, Box::new(LiveShm::new()), opts);
    if let Some(up) = make_upgrade(&game, &sigs, &cfg, tick, driver.backend_name()) {
        driver.set_upgrade(up);
    }
    runtime::install(Runtime {
        driver,
        input: ActionTracker::default(),
        ffb: Default::default(),
        settings_version: settings::version(),
        guard_interval: Duration::from_millis(sigs.online_guard.recheck_interval_ms),
        last: None,
    });
    overlay::set_status(StatusKind::Starting, "waiting for the first frame");
    tracing::info!("initialisation complete");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_log_repeats_an_unchanged_reason_every_15_s() {
        let mut log = WaitLog::default();
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let first = log.line("GObjects: not yet", 1, Duration::ZERO, at(0)).unwrap();
        assert_eq!(first, "waiting for the engine (attempt 1, 0 s): GObjects: not yet");
        assert!(log.line("GObjects: not yet", 2, Duration::from_secs(3), at(3)).is_none());
        let changed = log.line("FNamePool: bad", 3, Duration::from_secs(6), at(6)).unwrap();
        assert!(changed.starts_with("waiting for the engine (attempt 3"), "{changed}");
        for (n, s) in [(4, 9), (5, 12), (6, 15), (7, 18)] {
            assert!(log.line("FNamePool: bad", n, Duration::from_secs(s), at(s)).is_none(), "attempt {n}");
        }
        let again = log.line("FNamePool: bad", 8, Duration::from_secs(21), at(21)).unwrap();
        assert_eq!(again, "still waiting for the engine (attempt 8, 21 s): FNamePool: bad");
    }
}
