//! The installed driver and the per-tick entry point called by every tick source.

use crate::driver::{Driver, Frame, ShmSource};
use crate::input::{hub, ActionTracker};
use crate::overlay::{self, StatusKind};
use crate::settings;
use crate::validate::{physics_page_dead, ShmSample};
use acr_shm::SharedMemory;
use acr_ue::bodies::HintFn;
use rewind_core::DVec3;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct Runtime {
    pub driver: Driver,
    pub input: ActionTracker,
    /// Last [`settings::version`] handed to the driver.
    pub settings_version: u64,
    pub guard_interval: Duration,
    pub last: Option<Instant>,
}

static RUNTIME: Mutex<Option<Runtime>> = Mutex::new(None);
static PANICKED: AtomicBool = AtomicBool::new(false);

pub fn install(rt: Runtime) {
    if let Ok(mut g) = RUNTIME.lock() {
        *g = Some(rt);
    }
}

/// One frame. Never blocks (skips if another thread is mid-tick) and never unwinds.
pub fn tick(on_game_thread: bool) {
    if PANICKED.load(Ordering::Relaxed) {
        return;
    }
    let Ok(mut guard) = RUNTIME.try_lock() else {
        return;
    };
    let Some(rt) = guard.as_mut() else {
        return;
    };
    acr_ue::ue_call::set_game_thread(on_game_thread);
    let result = catch_unwind(AssertUnwindSafe(|| {
        let now = Instant::now();
        let dt = rt.last.map_or(0.0, |l| now.duration_since(l).as_secs_f64());
        rt.last = Some(now);
        let v = settings::version();
        if v != rt.settings_version {
            rt.settings_version = v;
            if let Some(cfg) = settings::current() {
                tracing::info!("settings changed: scrub {:?} mode {:?}", cfg.scrub, cfg.mode);
                rt.driver.set_config(cfg);
            }
        }
        let raw = hub::take(hub::CONSUMER_TICK);
        let input = settings::current()
            .map(|cfg| rt.input.update(&cfg.bindings, &raw, !settings::panel_open()).mode)
            .unwrap_or_default();
        let verdict = crate::guard::monitor::current(rt.guard_interval);
        let st = rt.driver.tick(Frame { dt, input, verdict });
        overlay::publish(st);
    }));
    if result.is_err() {
        PANICKED.store(true, Ordering::SeqCst);
        tracing::error!("panic in tick; rewind permanently disabled for this session");
        let _ = catch_unwind(AssertUnwindSafe(|| rt.driver.emergency_stop()));
        overlay::set_status(StatusKind::Disabled, "internal error (see acr-rewind.log)");
    }
    acr_ue::ue_call::set_game_thread(false);
}

/// Shared-memory sampler for validation, reopening the pages if the game recreates them.
pub struct LiveShm {
    shm: Option<SharedMemory>,
    last_try: Option<Instant>,
}

impl LiveShm {
    pub fn new() -> Self {
        Self { shm: None, last_try: None }
    }
}

impl ShmSource for LiveShm {
    fn sample(&mut self) -> Option<ShmSample> {
        if self.shm.is_none() && self.last_try.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
            self.last_try = Some(Instant::now());
            self.shm = SharedMemory::open().ok();
        }
        let shm = self.shm.as_ref()?;
        let p = shm.physics();
        Some(ShmSample {
            packet_id: p.packet_id,
            velocity: p.velocity,
            rpm: p.rpm,
            gear: p.gear,
            wheel_speed: std::array::from_fn(|i| p.wheels[i].angular_speed),
            position: car_position(shm),
            physics_dead: physics_page_dead(&p),
        })
    }
}

/// How often the "no car position" diagnostic is repeated.
const NO_CAR_LOG_PERIOD: Duration = Duration::from_secs(10);
static NO_CAR_LOGGED: Mutex<Option<Instant>> = Mutex::new(None);

/// Player car position from the graphics page (carCoordinates), which stays valid when the
/// physics page is dead. Same selection as acr-probe ([`acr_shm::car_position`]).
fn car_position(shm: &SharedMemory) -> Option<[f32; 3]> {
    let raw = shm.read_raw_graphics();
    if let Some((p, _)) = acr_shm::car_position(&raw) {
        return Some(p);
    }
    let due = NO_CAR_LOGGED
        .lock()
        .map(|mut t| {
            let due = t.is_none_or(|t| t.elapsed() >= NO_CAR_LOG_PERIOD);
            if due {
                *t = Some(Instant::now());
            }
            due
        })
        .unwrap_or(false);
    if due {
        let p = shm.physics();
        let (ids, coords) = ({ raw.carID }, { raw.carCoordinates });
        tracing::info!(
            "shm: no car position (waiting): graphics packetId {} status {} activeCars {} playerCarID {} \
             carID[0..3] {:?} carCoordinates[0] {:?} carCoordinates[1] {:?} | physics packetId {} rpm {} speed {:.1} km/h",
            { raw.packetId },
            { raw.status },
            { raw.activeCars },
            { raw.playerCarID },
            &ids[..3],
            coords[0],
            coords[1],
            p.packet_id,
            p.rpm,
            p.speed_kmh
        );
    }
    None
}

/// Thread-safe shm car position for the sim-car locator (tick and scan thread).
pub fn shm_position_hint() -> HintFn {
    let state: Mutex<(Option<SharedMemory>, Option<Instant>)> = Mutex::new((None, None));
    Arc::new(move || {
        let mut g = state.lock().ok()?;
        if g.0.is_none() && g.1.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
            g.1 = Some(Instant::now());
            g.0 = SharedMemory::open().ok();
        }
        let p = car_position(g.0.as_ref()?)?;
        Some(DVec3::new(p[0].into(), p[1].into(), p[2].into()))
    })
}
