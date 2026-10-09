//! Per-frame glue between the online guard, the input, the [`RewindController`] and the
//! [`CarBackend`]. Platform-independent; the hook supplies the frame inputs.
//!
//! Fail-safe rules: a blocked guard verdict disables writes and the rewind input and clears
//! history (read-only mode keeps recording and validating, it never writes); any write failure
//! or unfreeze failure latches a fault that keeps the mod off until the game restarts. A failed
//! post-resume validation is a warning; [`MAX_VALIDATION_FAILURES`] in a row disable rewind
//! until the player car changes (respawn, stage restart).

use crate::guard::Verdict;
use crate::overlay::{OverlayState, StatusKind};
use crate::validate::{check_resume, compare, ResumeCheck, ShmSample, RESUME_CHECK_DELAY};
use acr_ue::{BackendHealth, CarBackend, CarChange, WriteMode};
use rewind_core::{Action, CancelReason, CarSnapshot, Config, ModeEvent, ModeInput, RewindController, RewindState};
use std::time::{Duration, Instant};

/// How often a pending backend upgrade is tried (only at a safe point).
const UPGRADE_PERIOD_S: f64 = 2.0;
/// A new rewind started this soon after a resume: the sim has not stepped enough to judge it.
const MIN_EARLY_CHECK_S: f64 = 0.05;
/// Give up on a post-resume check when shared memory stays unavailable this long.
const RESUME_CHECK_TIMEOUT_S: f64 = 1.0;
/// Status heartbeat period.
pub const HEARTBEAT_PERIOD: Duration = Duration::from_secs(10);
/// A single car write slower than this is logged (once per rewind).
const SLOW_WRITE: Duration = Duration::from_millis(8);
/// Consecutive failed post-resume validations that disable rewind (until the car changes).
pub const MAX_VALIDATION_FAILURES: u32 = 3;
/// How long a failed validation stays visible in the overlay status.
const NOTICE_DURATION: Duration = Duration::from_secs(8);

/// Car writes of one rewind (mode + resume), summarised when it ends.
#[derive(Debug, Default)]
struct WriteStats {
    written: u32,
    total: Duration,
    max: Duration,
    slow_logged: bool,
}

impl WriteStats {
    fn add(&mut self, took: Duration, mode: WriteMode) {
        self.written += 1;
        self.total += took;
        self.max = self.max.max(took);
        if took >= SLOW_WRITE && !self.slow_logged {
            self.slow_logged = true;
            tracing::info!("slow car write: {:.1} ms ({mode:?})", took.as_secs_f64() * 1e3);
        }
    }

    fn summary(&self) -> Option<String> {
        if self.written == 0 {
            return None;
        }
        Some(format!(
            "rewind writes: {} written, avg {:.2} ms, max {:.2} ms",
            self.written,
            self.total.as_secs_f64() * 1e3 / f64::from(self.written),
            self.max.as_secs_f64() * 1e3
        ))
    }
}

/// Called with the current backend; returns a better backend once one can be built (e.g.
/// car_avatar after its UFunctions resolved), possibly taking over the current one's body
/// tracker. Runs on the tick (game) thread.
pub type Upgrade = Box<dyn FnMut(&mut dyn CarBackend) -> Option<Box<dyn CarBackend>> + Send>;

pub fn heartbeat_line(
    backend: &str,
    verdict: &Verdict,
    health: &BackendHealth,
    ticks_per_s: f64,
    recorded_s: f64,
    last_error: Option<&str>,
) -> String {
    let guard = match verdict {
        Verdict::Allowed => "allowed".to_owned(),
        Verdict::Inactive(r) => format!("inactive ({r})"),
        Verdict::Blocked(r) => format!("blocked ({r})"),
    };
    format!(
        "heartbeat: backend {backend} | guard {guard} | car locked {} | bodies {} | {ticks_per_s:.1} ticks/s | \
         {recorded_s:.1} s recorded | last error: {}",
        if health.car_locked { "yes" } else { "no" },
        health.bodies,
        last_error.unwrap_or("none")
    )
}

pub trait ShmSource: Send {
    fn sample(&mut self) -> Option<ShmSample>;
}

pub struct NoShm;

impl ShmSource for NoShm {
    fn sample(&mut self) -> Option<ShmSample> {
        None
    }
}

#[derive(Clone, Debug)]
pub struct Frame {
    pub dt: f64,
    /// Action inputs (all released while the settings panel is open).
    pub input: ModeInput,
    pub verdict: Verdict,
}

pub struct DriverOptions {
    pub read_only: bool,
    pub post_resume_check: bool,
    pub validation_interval_s: f64,
}

pub struct Driver {
    ctrl: RewindController,
    backend: Box<dyn CarBackend>,
    shm: Box<dyn ShmSource>,
    opts: DriverOptions,
    enabled_cfg: bool,
    frozen: bool,
    fault: Option<String>,
    validation_acc: f64,
    prev_validation: Option<(CarSnapshot, ShmSample)>,
    rewind_start: Option<(CarSnapshot, Option<ShmSample>)>,
    pending_check: Option<ResumeCheck>,
    freeze_warned: bool,
    last_status: Option<(StatusKind, String)>,
    upgrade: Option<Upgrade>,
    upgrade_acc: f64,
    hb_ticks: u32,
    hb_since: Option<Instant>,
    write_stats: WriteStats,
    /// Post-resume validations failed in a row.
    validation_failures: u32,
    /// `fault` came from repeated validation failures: cleared when the car changes.
    fault_recoverable: bool,
    notice: Option<(String, Instant)>,
}

impl Driver {
    pub fn new(cfg: Config, backend: Box<dyn CarBackend>, shm: Box<dyn ShmSource>, opts: DriverOptions) -> Self {
        let enabled_cfg = cfg.enabled;
        let mut ctrl = RewindController::new(cfg);
        // Recording starts on the first tick that may record (guard allowed, or read-only).
        ctrl.set_enabled(false);
        Self {
            ctrl,
            backend,
            shm,
            opts,
            enabled_cfg,
            frozen: false,
            fault: None,
            validation_acc: 0.0,
            prev_validation: None,
            rewind_start: None,
            pending_check: None,
            freeze_warned: false,
            last_status: None,
            upgrade: None,
            upgrade_acc: 0.0,
            hb_ticks: 0,
            hb_since: None,
            write_stats: WriteStats::default(),
            validation_failures: 0,
            fault_recoverable: false,
            notice: None,
        }
    }

    pub fn validation_failures(&self) -> u32 {
        self.validation_failures
    }

    fn validation_passed(&mut self, how: &str) {
        tracing::info!("post-resume validation ok{how}");
        self.validation_failures = 0;
    }

    /// A failed post-resume validation: a warning, unless it keeps happening.
    fn validation_failed(&mut self, reason: String) -> Option<OverlayState> {
        self.validation_failures += 1;
        let n = self.validation_failures;
        if n >= MAX_VALIDATION_FAILURES {
            let st = self.fail(format!(
                "post-resume validation failed {n} times in a row ({reason}); rewind re-enables after a respawn or restart"
            ));
            self.fault_recoverable = true;
            return Some(st);
        }
        tracing::warn!(
            "post-resume validation failed ({n}/{MAX_VALIDATION_FAILURES} in a row): {reason}; rewind stays enabled"
        );
        self.notice = Some((format!("last resume looked off: {reason}"), Instant::now()));
        None
    }

    fn timed_write(&mut self, s: &CarSnapshot, mode: WriteMode) -> Result<(), acr_ue::BackendError> {
        let start = Instant::now();
        let r = self.backend.write_state(s, mode);
        self.write_stats.add(start.elapsed(), mode);
        r
    }

    fn end_write_stats(&mut self) {
        if let Some(s) = self.write_stats.summary() {
            tracing::info!("{s}");
        }
        self.write_stats = WriteStats::default();
    }

    pub fn set_upgrade(&mut self, up: Upgrade) {
        self.upgrade = Some(up);
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    pub fn fault(&self) -> Option<&str> {
        self.fault.as_deref()
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen
    }

    pub fn controller(&self) -> &RewindController {
        &self.ctrl
    }

    pub fn backend_description(&self) -> String {
        self.backend.describe()
    }

    /// Applies edited settings (scrub speeds, mode options, bindings). The enabled state is
    /// left to the guard logic in [`Driver::tick`].
    pub fn set_config(&mut self, mut cfg: Config) {
        self.enabled_cfg = cfg.enabled;
        cfg.enabled = self.ctrl.is_enabled();
        if let Some(s) = self.ctrl.set_config(cfg) {
            if !self.opts.read_only {
                if let Err(e) = self.backend.write_state(&s, WriteMode::Full) {
                    tracing::warn!("release on config change failed: {e}");
                }
            }
            if let Err(e) = self.unfreeze() {
                tracing::error!("{e}");
            }
        }
    }

    /// Best-effort stop after an internal error: unfreeze and latch a fault.
    pub fn emergency_stop(&mut self) {
        self.fault = Some("internal error".into());
        self.ctrl.set_enabled(false);
        if self.frozen {
            self.frozen = false;
            let _ = self.backend.freeze(false);
        }
    }

    fn state(&mut self, kind: StatusKind, status: String) -> OverlayState {
        let key = (kind, status.clone());
        if self.last_status.as_ref() != Some(&key) {
            match kind {
                StatusKind::Disabled | StatusKind::Blocked => tracing::warn!("status: {kind:?} - {status}"),
                _ => tracing::info!("status: {kind:?} - {status}"),
            }
            self.last_status = Some(key);
        }
        let st = self.ctrl.state();
        OverlayState {
            kind,
            status,
            rewinding: matches!(st, RewindState::Rewinding { .. }),
            resuming: matches!(st, RewindState::Resuming { .. }),
            cancelling: matches!(st, RewindState::Resuming { cancelled: true, .. }),
            mode: self.ctrl.mode_view(),
            timeline: self.ctrl.timeline(),
        }
    }

    fn unfreeze(&mut self) -> Result<(), String> {
        if !self.frozen {
            return Ok(());
        }
        self.frozen = false;
        if !matches!(self.ctrl.state(), RewindState::Resuming { .. }) {
            self.end_write_stats();
        }
        self.backend.freeze(false).map_err(|e| format!("unfreeze failed: {e}"))
    }

    /// Guard blocked: leave the rewind mode as a cancel would (newest snapshot), unfreeze,
    /// drop history.
    fn shutdown(&mut self) {
        if self.ctrl.is_enabled() {
            let in_mode = self.ctrl.is_frozen();
            if let Some(s) = self.ctrl.set_enabled(false) {
                if in_mode {
                    tracing::warn!("rewind mode cancelled: online guard blocked (car released at t={:.2})", s.t);
                }
                if !self.opts.read_only {
                    if let Err(e) = self.backend.write_state(&s, WriteMode::Full) {
                        tracing::warn!("release on shutdown failed: {e}");
                    }
                }
            }
        }
        if let Err(e) = self.unfreeze() {
            tracing::error!("{e}");
        }
        self.ctrl.reset(); // drop recorded history so nothing survives into an online session
        self.backend.on_truncate(None);
        self.pending_check = None;
        self.rewind_start = None;
        self.prev_validation = None;
    }

    fn fail(&mut self, reason: String) -> OverlayState {
        tracing::error!("disabling rewind: {reason}");
        self.ctrl.set_enabled(false);
        if let Err(e) = self.unfreeze() {
            tracing::error!("{e}");
        }
        self.backend.on_truncate(None);
        self.pending_check = None;
        self.fault = Some(reason.clone());
        self.state(StatusKind::Disabled, reason)
    }

    fn abort_rewind(&mut self, why: &str) -> Result<(), String> {
        if self.ctrl.is_frozen() {
            tracing::warn!("rewind mode cancelled: {why}");
            self.ctrl.reset();
        }
        self.pending_check = None;
        self.rewind_start = None;
        self.unfreeze()
    }

    fn set_frozen(&mut self, frozen: bool) -> Result<(), String> {
        if frozen == self.frozen {
            return Ok(());
        }
        if !frozen {
            return self.unfreeze();
        }
        self.frozen = true;
        if let Err(e) = self.backend.freeze(true) {
            if !self.freeze_warned {
                self.freeze_warned = true;
                tracing::warn!("freeze lever failed ({e}); holding the car by re-writing the pose each frame");
            }
        }
        Ok(())
    }

    fn validation_log(&mut self, dt: f64) {
        self.validation_acc += dt;
        if self.validation_acc < self.opts.validation_interval_s {
            return;
        }
        self.validation_acc = 0.0;
        let Some(snap) = self.ctrl.buffer().newest().copied() else {
            return;
        };
        let Some(shm) = self.shm.sample() else {
            tracing::info!(
                "validation: snapshot pos {:.2?} speed {:.2} m/s rpm {:.0} gear {} (shared memory unavailable)",
                snap.pos,
                snap.lin_vel.length(),
                snap.rpm,
                snap.gear
            );
            return;
        };
        let prev = self.prev_validation.as_ref().map(|(s, m)| (s, m));
        let c = compare(&snap, &shm, prev);
        tracing::info!("validation: pos {:.2?} | {}", snap.pos, c.summary());
        self.prev_validation = Some((snap, shm));
    }

    fn log_event(&self, ev: ModeEvent) {
        let tl = self.ctrl.timeline();
        let at = tl.map_or(0.0, |t| t.cursor);
        match ev {
            ModeEvent::Entered => tracing::info!(
                "rewind mode entered at t={at:.2} ({:.1} s of history)",
                tl.map_or(0.0, |t| t.newest - t.oldest)
            ),
            ModeEvent::Resumed => tracing::info!("rewind mode: resume from t={at:.2} (later history discarded)"),
            ModeEvent::Cancelled(CancelReason::User) => {
                tracing::info!("rewind mode cancelled: back to t={at:.2} (nothing discarded)")
            }
            ModeEvent::Cancelled(CancelReason::Timeout) => {
                tracing::warn!("rewind mode cancelled: mode.max_time_s reached (back to t={at:.2})")
            }
        }
    }

    /// A new rewind starts while the previous resume's check is still pending (rewind pressed
    /// again within [`RESUME_CHECK_DELAY`]): check now if the sim had time to move, else skip.
    fn check_before_new_rewind(&mut self, c: &ResumeCheck) -> Result<(), String> {
        if c.elapsed < MIN_EARLY_CHECK_S {
            tracing::info!("post-resume validation skipped: a new rewind started {:.2} s after the resume", c.elapsed);
            return Ok(());
        }
        let Some(now) = self.shm.sample() else {
            tracing::warn!("post-resume validation skipped: shared memory unavailable");
            return Ok(());
        };
        check_resume(c, &now)?;
        self.validation_passed(&format!(" (early: a new rewind started {:.2} s after the resume)", c.elapsed));
        Ok(())
    }

    /// No rewind in progress, nothing frozen, no resume check pending: the backend may be
    /// swapped.
    fn at_safe_point(&self) -> bool {
        !self.frozen && !self.ctrl.is_frozen() && self.pending_check.is_none() && self.rewind_start.is_none()
    }

    fn try_upgrade(&mut self, dt: f64) {
        if self.upgrade.is_none() {
            return;
        }
        self.upgrade_acc += dt;
        if self.upgrade_acc < UPGRADE_PERIOD_S || !self.at_safe_point() {
            return;
        }
        self.upgrade_acc = 0.0;
        let backend = &mut self.backend;
        let Some(next) = self.upgrade.as_mut().and_then(|up| up(backend.as_mut())) else {
            return;
        };
        let old = self.backend.name();
        tracing::info!("backend upgraded: {old} -> {} ({})", next.name(), next.describe());
        self.backend.on_truncate(None);
        self.backend = next;
        self.ctrl.reset();
        self.prev_validation = None;
        self.freeze_warned = false;
    }

    fn heartbeat(&mut self, verdict: &Verdict) {
        self.hb_ticks += 1;
        let now = Instant::now();
        let since = *self.hb_since.get_or_insert(now);
        let elapsed = now.duration_since(since);
        if elapsed < HEARTBEAT_PERIOD {
            return;
        }
        let rate = f64::from(self.hb_ticks) / elapsed.as_secs_f64();
        self.hb_ticks = 0;
        self.hb_since = Some(now);
        let health = self.backend.health();
        let last_error = self.fault.clone().or(health.last_error.clone());
        tracing::info!(
            "{}",
            heartbeat_line(
                self.backend.name(),
                verdict,
                &health,
                rate,
                self.ctrl.buffer().duration(),
                last_error.as_deref()
            )
        );
    }

    /// Runs one frame.
    ///
    /// The guard gates writes and the rewind input only: in read-only mode the car is
    /// located, recorded and validated whatever the verdict (nothing is ever written).
    pub fn tick(&mut self, f: Frame) -> OverlayState {
        self.heartbeat(&f.verdict);
        if let Some(r) = self.fault.clone() {
            let recovered = self.fault_recoverable
                && f.verdict == Verdict::Allowed
                && !self.opts.read_only
                && self.backend.refresh() == CarChange::Changed;
            if !recovered {
                return self.state(StatusKind::Disabled, r);
            }
            tracing::info!("rewind re-enabled: the player car changed (respawn / restart)");
            self.fault = None;
            self.fault_recoverable = false;
            self.validation_failures = 0;
            self.notice = None;
        }
        self.try_upgrade(f.dt);
        let allowed = f.verdict == Verdict::Allowed;
        if !self.opts.read_only {
            match &f.verdict {
                Verdict::Blocked(r) => {
                    self.shutdown();
                    // Keep locating the car so rewind is ready the moment the guard allows it.
                    if self.backend.refresh() == CarChange::Changed {
                        tracing::info!("player car found: {}", self.backend.describe());
                    }
                    return self.state(StatusKind::Blocked, r.clone());
                }
                Verdict::Inactive(r) => return self.state(StatusKind::Inactive, r.clone()),
                Verdict::Allowed => {}
            }
        }
        if !self.ctrl.is_enabled() {
            if !self.enabled_cfg {
                return self.state(StatusKind::Disabled, "enabled = false in acr-rewind.toml".into());
            }
            self.ctrl.set_enabled(true);
            self.backend.on_truncate(None);
        }
        match self.backend.refresh() {
            CarChange::Lost => {
                if let Err(e) = self.abort_rewind("player car lost") {
                    return self.fail(e);
                }
                return self.state(StatusKind::Inactive, "waiting for the player car".into());
            }
            CarChange::Changed => {
                if let Err(e) = self.abort_rewind("player car changed") {
                    return self.fail(e);
                }
                self.ctrl.reset();
                self.backend.on_truncate(None);
                self.prev_validation = None;
                tracing::info!("player car found: {}", self.backend.describe());
            }
            CarChange::Same => {}
        }

        let was_recording = !self.ctrl.is_frozen();
        let input = if !self.opts.read_only && allowed { f.input } else { ModeInput::default() };
        let live_before = self.ctrl.buffer().newest().copied();
        let backend = &mut self.backend;
        let out = self.ctrl.update(f.dt, &input, || backend.read_state());
        if let Some(ev) = out.event {
            self.log_event(ev);
        }
        // Resume run-in: for the resume delay the sim runs while the car is held at the
        // release state with its velocities, so state we can't write (engine rpm, tyres) spins
        // back up. Live: released straight from the freeze, the engine sat at ~50 rpm at 20 m/s
        // in 1st and dragged the wheels into a slide.
        let run_in = matches!(self.ctrl.state(), RewindState::Resuming { .. });
        let result = match out.action {
            Action::None => Ok(()),
            Action::Recorded => {
                if let Some(t) = self.ctrl.buffer().newest().map(|s| s.t) {
                    self.backend.on_recorded(t);
                }
                if self.opts.read_only {
                    self.validation_log(f.dt);
                }
                Ok(())
            }
            Action::Apply(s) => {
                if was_recording {
                    // The previous resume's check must see the sim before this rewind moves it.
                    if let Some(c) = self.pending_check.take() {
                        if let Err(r) = self.check_before_new_rewind(&c) {
                            if let Some(st) = self.validation_failed(r) {
                                return st;
                            }
                        }
                    }
                    let start = live_before.unwrap_or(s);
                    self.rewind_start = Some((start, self.shm.sample()));
                    tracing::debug!("rewind started at t={:.2}", start.t);
                }
                // Every frame, even if unchanged: the freeze lever doesn't stop the whole sim
                // (live: the car bobbed between writes spaced 0.25 s apart).
                self.timed_write(&s, if run_in { WriteMode::Full } else { WriteMode::Pose })
            }
            Action::Release(s) => {
                let r = self.timed_write(&s, WriteMode::Full);
                self.end_write_stats();
                self.backend.on_truncate(Some(s.t));
                if r.is_ok() {
                    tracing::info!("resumed at t={:.2} (speed {:.1} m/s, gear {})", s.t, s.lin_vel.length(), s.gear);
                    if self.opts.post_resume_check {
                        if let Some((start_snap, start_shm)) = self.rewind_start.take() {
                            self.pending_check = Some(ResumeCheck { start_snap, start_shm, resumed: s, elapsed: 0.0 });
                        }
                    }
                }
                r
            }
        };
        if let Err(e) = result {
            return self.fail(format!("car state write failed: {e}"));
        }
        if let Err(e) = self.set_frozen(out.frozen && !run_in) {
            return self.fail(e);
        }

        if let Some(mut c) = self.pending_check.take() {
            c.elapsed += f.dt;
            if c.elapsed >= RESUME_CHECK_DELAY {
                match self.shm.sample() {
                    Some(now) => match check_resume(&c, &now) {
                        Ok(()) => self.validation_passed(""),
                        Err(r) => {
                            if let Some(st) = self.validation_failed(r) {
                                return st;
                            }
                        }
                    },
                    None if c.elapsed >= RESUME_CHECK_TIMEOUT_S => {
                        tracing::warn!("post-resume validation skipped: shared memory unavailable");
                    }
                    None => self.pending_check = Some(c),
                }
            } else {
                self.pending_check = Some(c);
            }
        }

        if self.opts.read_only {
            let guard = match &f.verdict {
                Verdict::Allowed => String::new(),
                Verdict::Inactive(r) => format!(" | guard: inactive ({r})"),
                Verdict::Blocked(r) => format!(" | guard: blocked ({r})"),
            };
            self.state(StatusKind::ReadOnly, format!("{} | logging snapshots, no writes{guard}", self.backend.name()))
        } else {
            let buffered = self.ctrl.buffer().duration();
            let freeze = if self.backend.can_freeze() { "" } else { " | no freeze lever" };
            if self.notice.as_ref().is_some_and(|(_, at)| at.elapsed() >= NOTICE_DURATION) {
                self.notice = None;
            }
            let notice = self.notice.as_ref().map_or(String::new(), |(n, _)| format!(" | {n}"));
            self.state(
                StatusKind::Active,
                format!("{} | {buffered:.0} s buffered{freeze}{notice}", self.backend.name()),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acr_ue::BackendError;
    use rewind_core::DVec3;
    use std::sync::{Arc, Mutex};

    const DT: f64 = 1.0 / 60.0;

    /// Car moving along +x at 20 m/s while unfrozen; shm mirrors the sim (optionally ignoring
    /// writes, like a visual proxy overwritten by the real sim).
    #[derive(Default)]
    struct World {
        x: f64,
        speed: f64,
        frozen: bool,
        writes: Vec<(WriteMode, f64)>,
        freeze_calls: Vec<bool>,
        car_present: bool,
        car_id: u32,
        ignore_writes: bool,
        fail_writes: bool,
        fail_freeze: bool,
        sim_x: f64,
        refreshes: u32,
    }

    type Shared = Arc<Mutex<World>>;

    struct MockBackend {
        w: Shared,
        last_id: Option<u32>,
        name: &'static str,
    }

    fn mock(w: &Shared) -> Box<MockBackend> {
        Box::new(MockBackend { w: w.clone(), last_id: None, name: "mock" })
    }

    impl CarBackend for MockBackend {
        fn name(&self) -> &'static str {
            self.name
        }
        fn refresh(&mut self) -> CarChange {
            let mut w = self.w.lock().unwrap();
            w.refreshes += 1;
            if !w.car_present {
                self.last_id = None;
                return CarChange::Lost;
            }
            let c = if self.last_id == Some(w.car_id) { CarChange::Same } else { CarChange::Changed };
            self.last_id = Some(w.car_id);
            c
        }
        fn read_state(&mut self) -> Option<CarSnapshot> {
            let w = self.w.lock().unwrap();
            Some(CarSnapshot {
                pos: DVec3::new(w.x, 0.0, 0.0),
                lin_vel: DVec3::new(w.speed, 0.0, 0.0),
                ..Default::default()
            })
        }
        fn write_state(&mut self, s: &CarSnapshot, mode: WriteMode) -> Result<(), BackendError> {
            let mut w = self.w.lock().unwrap();
            if w.fail_writes {
                return Err(BackendError::Write(0x1234));
            }
            w.writes.push((mode, s.pos.x));
            w.x = s.pos.x;
            if !w.ignore_writes {
                w.sim_x = s.pos.x;
                if mode == WriteMode::Full {
                    w.speed = s.lin_vel.x;
                }
            }
            Ok(())
        }
        fn freeze(&mut self, frozen: bool) -> Result<(), BackendError> {
            let mut w = self.w.lock().unwrap();
            w.freeze_calls.push(frozen);
            if w.fail_freeze {
                return Err(BackendError::Unsupported("no lever".into()));
            }
            w.frozen = frozen;
            Ok(())
        }
        fn can_freeze(&self) -> bool {
            true
        }
    }

    struct MockShm(Shared);

    impl ShmSource for MockShm {
        fn sample(&mut self) -> Option<ShmSample> {
            let w = self.0.lock().unwrap();
            Some(ShmSample {
                velocity: [w.speed as f32, 0.0, 0.0],
                position: Some([w.sim_x as f32, 0.0, 0.0]),
                ..Default::default()
            })
        }
    }

    fn setup(read_only: bool) -> (Driver, Shared) {
        let w: Shared = Arc::new(Mutex::new(World { speed: 20.0, car_present: true, ..Default::default() }));
        let mut cfg = Config::default();
        cfg.scrub.resume_delay = 0.1;
        let d = Driver::new(
            cfg,
            mock(&w),
            Box::new(MockShm(w.clone())),
            DriverOptions { read_only, post_resume_check: true, validation_interval_s: 0.5 },
        );
        (d, w)
    }

    const IDLE: ModeInput = ModeInput {
        rewind_pressed: false,
        rewind_held: false,
        resume_pressed: false,
        cancel_pressed: false,
        back: rewind_core::ScrubInput { analog: 0.0, digital: false },
        forward: rewind_core::ScrubInput { analog: 0.0, digital: false },
    };

    fn enter_input() -> ModeInput {
        ModeInput { rewind_pressed: true, rewind_held: true, ..IDLE }
    }
    fn back_input() -> ModeInput {
        ModeInput { back: rewind_core::ScrubInput { analog: 1.0, digital: false }, ..IDLE }
    }
    fn resume_input() -> ModeInput {
        ModeInput { resume_pressed: true, ..IDLE }
    }
    fn cancel_input() -> ModeInput {
        ModeInput { cancel_pressed: true, ..IDLE }
    }

    fn step(d: &mut Driver, w: &Shared, input: ModeInput) -> OverlayState {
        step_v(d, w, input, Verdict::Allowed)
    }

    fn step_v(d: &mut Driver, w: &Shared, input: ModeInput, verdict: Verdict) -> OverlayState {
        {
            let mut g = w.lock().unwrap();
            if !g.frozen && !g.ignore_writes {
                g.x += g.speed * DT;
                g.sim_x = g.x;
            } else if g.ignore_writes {
                // The real sim keeps going regardless of what we wrote.
                g.sim_x += g.speed * DT;
                g.x = g.sim_x;
            }
        }
        d.tick(Frame { dt: DT, input, verdict })
    }

    fn run(d: &mut Driver, w: &Shared, n: usize, input: ModeInput) -> OverlayState {
        let mut s = None;
        for _ in 0..n {
            s = Some(step(d, w, input));
        }
        s.unwrap()
    }

    /// Enter the mode and scrub back for `frames` at full trigger.
    fn rewind(d: &mut Driver, w: &Shared, frames: usize) -> OverlayState {
        step(d, w, enter_input());
        run(d, w, frames, back_input())
    }

    #[test]
    fn full_rewind_cycle_freezes_writes_and_validates() {
        let (mut d, w) = setup(false);
        let s = run(&mut d, &w, 300, IDLE); // 5 s driving
        assert_eq!(s.kind, StatusKind::Active);
        let x_before = w.lock().unwrap().x;
        let s = rewind(&mut d, &w, 60);
        assert!(s.rewinding);
        assert!(s.mode.is_some());
        assert!(s.timeline.unwrap().fraction() < 1.0);
        {
            let g = w.lock().unwrap();
            assert!(g.frozen);
            assert_eq!(g.freeze_calls, vec![true]);
            assert!(g.writes.iter().all(|(m, _)| *m == WriteMode::Pose));
            assert!(g.x < x_before - 20.0, "car scrubbed back");
        }
        // Idle in the mode: still frozen, nothing released.
        let s = run(&mut d, &w, 120, IDLE);
        assert!(s.rewinding && d.is_frozen());
        step(&mut d, &w, resume_input());
        let s = run(&mut d, &w, 30, IDLE); // resume delay 0.1 s, then release + check
        assert!(!s.rewinding && !s.resuming);
        assert_eq!(s.kind, StatusKind::Active, "{s:?}");
        let g = w.lock().unwrap();
        assert!(!g.frozen);
        assert_eq!(g.freeze_calls, vec![true, false]);
        assert_eq!(g.writes.last().unwrap().0, WriteMode::Full);
        drop(g);
        assert!(d.fault().is_none());
    }

    #[test]
    fn resume_runs_the_sim_in_while_holding_the_release_state() {
        let (mut d, w) = setup(false);
        run(&mut d, &w, 300, IDLE);
        rewind(&mut d, &w, 60);
        let n = w.lock().unwrap().writes.len();
        assert!(step(&mut d, &w, resume_input()).resuming);
        assert!(!w.lock().unwrap().frozen, "the sim runs during the run-in");
        let mut frames = 0;
        while step(&mut d, &w, IDLE).resuming {
            frames += 1;
            assert!(frames < 60, "never released");
        }
        let g = w.lock().unwrap();
        let run_in = &g.writes[n..];
        assert!(run_in.len() >= 3, "{run_in:?}");
        assert!(run_in.iter().all(|(m, _)| *m == WriteMode::Full), "with velocities: {run_in:?}");
        assert!(run_in.iter().all(|(_, x)| (*x - run_in[0].1).abs() < 1e-9), "held at the release state");
        assert_eq!(g.freeze_calls, vec![true, false]);
        drop(g);
        run(&mut d, &w, 30, IDLE);
        assert!(d.fault().is_none() && d.validation_failures() == 0, "{:?}", d.fault());
    }

    fn pose_writes(w: &Shared) -> usize {
        w.lock().unwrap().writes.iter().filter(|(m, _)| *m == WriteMode::Pose).count()
    }

    #[test]
    fn idle_mode_holds_the_pose_every_frame() {
        for fail_freeze in [false, true] {
            let (mut d, w) = setup(false);
            run(&mut d, &w, 300, IDLE);
            w.lock().unwrap().fail_freeze = fail_freeze;
            rewind(&mut d, &w, 30);
            let scrubbed = pose_writes(&w);
            run(&mut d, &w, 60, IDLE);
            assert_eq!(pose_writes(&w) - scrubbed, 60, "lever failing: {fail_freeze}");
        }
    }

    #[test]
    fn cancel_releases_at_the_newest_snapshot() {
        let (mut d, w) = setup(false);
        run(&mut d, &w, 300, IDLE);
        let x_live = w.lock().unwrap().x;
        let len = d.controller().buffer().len();
        rewind(&mut d, &w, 60);
        let s = step(&mut d, &w, cancel_input());
        assert!(s.resuming && s.cancelling);
        let s = run(&mut d, &w, 20, IDLE);
        assert!(!s.resuming);
        let g = w.lock().unwrap();
        let (mode, x) = *g.writes.last().unwrap();
        assert_eq!(mode, WriteMode::Full);
        assert!((x - x_live).abs() < 0.5, "released at the live moment: {x} vs {x_live}");
        drop(g);
        assert!(d.controller().buffer().len() >= len, "nothing discarded");
        assert!(d.fault().is_none(), "{:?}", d.fault());
    }

    #[test]
    fn read_only_never_writes_or_freezes() {
        let (mut d, w) = setup(true);
        let s = run(&mut d, &w, 300, IDLE);
        assert_eq!(s.kind, StatusKind::ReadOnly);
        let s = rewind(&mut d, &w, 60);
        assert!(!s.rewinding);
        let g = w.lock().unwrap();
        assert!(g.writes.is_empty());
        assert!(g.freeze_calls.is_empty());
        assert!(d.controller().buffer().len() > 100, "still recording");
    }

    #[test]
    fn blocked_verdict_cancels_to_newest_unfreezes_and_clears() {
        let (mut d, w) = setup(false);
        run(&mut d, &w, 300, IDLE);
        let x_live = w.lock().unwrap().x;
        rewind(&mut d, &w, 30);
        assert!(d.is_frozen());
        let s = d.tick(Frame { dt: DT, input: back_input(), verdict: Verdict::Blocked("online server 'X'".into()) });
        assert_eq!(s.kind, StatusKind::Blocked);
        assert!(!d.is_frozen());
        assert!(d.controller().buffer().is_empty());
        let g = w.lock().unwrap();
        let (mode, x) = *g.writes.last().unwrap();
        assert_eq!(mode, WriteMode::Full, "car released");
        assert!((x - x_live).abs() < 0.5, "at the newest snapshot, as a cancel");
        assert!(!g.frozen);
        drop(g);
        // Back offline: recording restarts from scratch.
        let s = run(&mut d, &w, 10, IDLE);
        assert_eq!(s.kind, StatusKind::Active);
    }

    #[test]
    fn inactive_keeps_history() {
        let (mut d, w) = setup(false);
        run(&mut d, &w, 120, IDLE);
        let n = d.controller().buffer().len();
        let s = d.tick(Frame { dt: DT, input: IDLE, verdict: Verdict::Inactive("game status Pause".into()) });
        assert_eq!(s.kind, StatusKind::Inactive);
        assert_eq!(d.controller().buffer().len(), n);
    }

    #[test]
    fn write_failure_latches_fault() {
        let (mut d, w) = setup(false);
        run(&mut d, &w, 120, IDLE);
        w.lock().unwrap().fail_writes = true;
        let s = rewind(&mut d, &w, 5);
        assert_eq!(s.kind, StatusKind::Disabled);
        assert!(d.fault().unwrap().contains("write failed"));
        assert!(!d.is_frozen());
        w.lock().unwrap().fail_writes = false;
        let s = run(&mut d, &w, 60, IDLE);
        assert_eq!(s.kind, StatusKind::Disabled, "fault is latched");
    }

    /// Scrub back > 5 m, resume, let the post-resume check run.
    fn rewind_and_resume(d: &mut Driver, w: &Shared) -> OverlayState {
        rewind(d, w, 60);
        step(d, w, resume_input());
        run(d, w, 40, IDLE)
    }

    #[test]
    fn post_resume_check_warns_then_disables_after_repeated_failures() {
        let (mut d, w) = setup(false);
        run(&mut d, &w, 300, IDLE);
        w.lock().unwrap().ignore_writes = true;
        for n in 1..MAX_VALIDATION_FAILURES {
            let s = rewind_and_resume(&mut d, &w);
            assert_eq!(s.kind, StatusKind::Active, "failure {n} is only a warning: {s:?}");
            assert!(s.status.contains("last resume looked off"), "{}", s.status);
            assert_eq!(d.validation_failures(), n);
            assert!(d.fault().is_none());
            run(&mut d, &w, 240, IDLE); // rebuild history
        }
        let s = rewind_and_resume(&mut d, &w);
        assert_eq!(s.kind, StatusKind::Disabled, "{s:?}");
        assert!(d.fault().unwrap().contains("re-enables after a respawn"), "{:?}", d.fault());
        assert_eq!(run(&mut d, &w, 30, IDLE).kind, StatusKind::Disabled);
        // Respawn: a new car object re-enables rewind.
        {
            let mut g = w.lock().unwrap();
            g.car_id += 1;
            g.ignore_writes = false;
        }
        let s = run(&mut d, &w, 300, IDLE);
        assert_eq!(s.kind, StatusKind::Active, "{s:?}");
        assert!(d.fault().is_none() && d.validation_failures() == 0);
        let s = rewind_and_resume(&mut d, &w);
        assert_eq!(s.kind, StatusKind::Active);
    }

    #[test]
    fn passing_validation_resets_the_failure_count() {
        let (mut d, w) = setup(false);
        run(&mut d, &w, 300, IDLE);
        w.lock().unwrap().ignore_writes = true;
        rewind_and_resume(&mut d, &w);
        assert_eq!(d.validation_failures(), 1);
        w.lock().unwrap().ignore_writes = false;
        run(&mut d, &w, 240, IDLE);
        rewind_and_resume(&mut d, &w);
        assert_eq!(d.validation_failures(), 0);
    }

    #[test]
    fn write_failures_still_latch_for_good() {
        let (mut d, w) = setup(false);
        run(&mut d, &w, 120, IDLE);
        w.lock().unwrap().fail_writes = true;
        rewind(&mut d, &w, 5);
        w.lock().unwrap().fail_writes = false;
        w.lock().unwrap().car_id += 1;
        assert_eq!(run(&mut d, &w, 60, IDLE).kind, StatusKind::Disabled, "a respawn does not clear it");
    }

    /// Release, then `frames_after` idle frames, then enter the mode again.
    fn quick_rerewind(ignore_writes: bool, frames_after: usize) -> Driver {
        let (mut d, w) = setup(false);
        run(&mut d, &w, 300, IDLE);
        w.lock().unwrap().ignore_writes = ignore_writes;
        rewind(&mut d, &w, 30); // keeps enough history to enter the mode again
        step(&mut d, &w, resume_input());
        // Resume delay 0.1 s = 6 frames until the release.
        loop {
            let s = step(&mut d, &w, IDLE);
            if !s.resuming {
                break;
            }
        }
        run(&mut d, &w, frames_after.max(1), IDLE);
        step(&mut d, &w, enter_input());
        d
    }

    #[test]
    fn resume_check_runs_early_when_a_new_rewind_starts() {
        // 4 frames (~0.07 s) after the release, before the 0.15 s check: still validated.
        let d = quick_rerewind(true, 4);
        assert_eq!(d.validation_failures(), 1);
        assert!(d.fault().is_none() && d.controller().in_mode(), "a warning: the new rewind proceeds");
        let d = quick_rerewind(false, 4);
        assert_eq!(d.validation_failures(), 0);
        assert!(d.fault().is_none(), "{:?}", d.fault());
        assert!(d.controller().in_mode());
    }

    #[test]
    fn resume_check_skipped_when_rewinding_again_immediately() {
        // 1 frame (~0.017 s): too early to judge; skipped instead of failing.
        let d = quick_rerewind(true, 1);
        assert!(d.fault().is_none(), "{:?}", d.fault());
        assert!(d.controller().in_mode());
    }

    #[test]
    fn car_change_and_loss_reset_history() {
        let (mut d, w) = setup(false);
        run(&mut d, &w, 120, IDLE);
        rewind(&mut d, &w, 10);
        assert!(d.is_frozen());
        w.lock().unwrap().car_present = false;
        let s = step(&mut d, &w, back_input());
        assert_eq!(s.kind, StatusKind::Inactive);
        assert!(!d.is_frozen());
        {
            let mut g = w.lock().unwrap();
            g.car_present = true;
            g.car_id = 2;
        }
        run(&mut d, &w, 5, IDLE);
        assert!(d.controller().buffer().len() <= 5, "history reset for the new car");
    }

    #[test]
    fn disabled_in_config() {
        let w: Shared = Arc::new(Mutex::new(World { car_present: true, ..Default::default() }));
        let cfg = Config { enabled: false, ..Default::default() };
        let mut d = Driver::new(
            cfg,
            mock(&w),
            Box::new(NoShm),
            DriverOptions { read_only: false, post_resume_check: true, validation_interval_s: 1.0 },
        );
        let s = d.tick(Frame { dt: DT, input: IDLE, verdict: Verdict::Allowed });
        assert_eq!(s.kind, StatusKind::Disabled);
    }

    #[test]
    fn set_config_applies_live_without_dropping_history() {
        let (mut d, w) = setup(false);
        run(&mut d, &w, 300, IDLE);
        let n = d.controller().buffer().len();
        let mut cfg = Config::default();
        cfg.scrub.analog_max_speed = 1.0;
        cfg.scrub.analog_curve = 1.0;
        cfg.scrub.analog_boost_max = 1.0;
        d.set_config(cfg);
        assert_eq!(d.controller().buffer().len(), n);
        assert!(d.controller().is_enabled());
        step(&mut d, &w, enter_input());
        let c0 = d.controller().timeline().unwrap().cursor;
        run(&mut d, &w, 60, back_input());
        let moved = c0 - d.controller().timeline().unwrap().cursor;
        assert!((moved - (1.0 - 0.08) / 0.92).abs() < 1e-6, "1 s/s after the deadzone mapping: {moved}");
    }

    fn blocked() -> Verdict {
        Verdict::Blocked("UE net mode unavailable (fail closed)".into())
    }

    #[test]
    fn read_only_records_and_validates_while_blocked() {
        let (mut d, w) = setup(true);
        let mut s = None;
        for i in 0..300 {
            let input = if i % 2 == 0 { enter_input() } else { IDLE };
            s = Some(step_v(&mut d, &w, input, blocked()));
        }
        let s = s.unwrap();
        assert_eq!(s.kind, StatusKind::ReadOnly, "{s:?}");
        assert!(s.status.contains("guard: blocked"), "{}", s.status);
        assert!(d.controller().buffer().len() > 100, "recording regardless of the guard");
        let g = w.lock().unwrap();
        assert_eq!(g.refreshes, 300, "car located every tick");
        assert!(g.writes.is_empty() && g.freeze_calls.is_empty());
    }

    #[test]
    fn blocked_write_mode_keeps_locating_but_never_records_or_rewinds() {
        let (mut d, w) = setup(false);
        for i in 0..120 {
            let input = if i % 2 == 0 { enter_input() } else { IDLE };
            let s = step_v(&mut d, &w, input, blocked());
            assert_eq!(s.kind, StatusKind::Blocked);
        }
        assert!(d.controller().buffer().is_empty());
        let g = w.lock().unwrap();
        assert_eq!(g.refreshes, 120);
        assert!(g.writes.is_empty() && g.freeze_calls.is_empty());
    }

    #[test]
    fn upgrade_waits_for_a_safe_point() {
        let (mut d, w) = setup(false);
        let w2 = w.clone();
        let tries = Arc::new(Mutex::new(Vec::new()));
        let t2 = tries.clone();
        d.set_upgrade(Box::new(move |current: &mut dyn CarBackend| {
            let current = current.name();
            t2.lock().unwrap().push(current.to_owned());
            (current == "mock")
                .then(|| Box::new(MockBackend { w: w2.clone(), last_id: None, name: "better" }) as Box<dyn CarBackend>)
        }));
        run(&mut d, &w, 60, IDLE); // 1 s: before the first upgrade attempt
        assert_eq!(d.backend_name(), "mock");
        run(&mut d, &w, 10, IDLE);
        // In the mode past the upgrade period: never swapped mid-rewind.
        step(&mut d, &w, enter_input());
        let s = run(&mut d, &w, 150, IDLE);
        assert!(s.rewinding);
        assert_eq!(d.backend_name(), "mock");
        assert!(tries.lock().unwrap().is_empty(), "not even tried while frozen");
        // Released (resume check done): the next attempt upgrades and history restarts.
        step(&mut d, &w, resume_input());
        run(&mut d, &w, 200, IDLE);
        assert_eq!(d.backend_name(), "better");
        assert!(d.fault().is_none(), "{:?}", d.fault());
        assert!(d.controller().buffer().duration() < 3.5);
        let n = tries.lock().unwrap().len();
        run(&mut d, &w, 130, IDLE);
        assert!(tries.lock().unwrap().iter().skip(n).all(|c| c == "better"));
    }

    #[test]
    fn heartbeat_line_reports_everything() {
        let h = BackendHealth { car_locked: true, bodies: 6, last_error: None };
        let l = heartbeat_line("car_avatar", &Verdict::Allowed, &h, 59.94, 12.0, None);
        assert_eq!(
            l,
            "heartbeat: backend car_avatar | guard allowed | car locked yes | bodies 6 | 59.9 ticks/s | \
             12.0 s recorded | last error: none"
        );
        let l = heartbeat_line("sim_car", &blocked(), &BackendHealth::default(), 0.0, 0.0, Some("scan: no hint"));
        assert!(l.contains("guard blocked (UE net mode unavailable (fail closed))"), "{l}");
        assert!(l.contains("car locked no | bodies 0") && l.ends_with("last error: scan: no hint"), "{l}");
    }
}
