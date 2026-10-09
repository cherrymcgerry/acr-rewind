use crate::{CarSnapshot, Config, RingBuffer};

/// Largest frame delta the controller accepts; longer hitches (loading, breakpoints) are clamped.
const MAX_DT: f64 = 0.25;

#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum RewindState {
    /// Mod switched off: nothing is recorded, nothing is applied.
    Disabled,
    /// Normal driving; snapshots are captured at `tick_hz`.
    Recording,
    /// Rewind mode: the game is held frozen at `cursor`, which the scrub inputs move through
    /// the history.
    Rewinding {
        /// Timeline position being displayed.
        cursor: f64,
        /// Current scrub velocity (history seconds per real second; negative = backwards).
        speed: f64,
        /// Seconds spent in the mode.
        elapsed: f64,
        /// Resume was pressed while a scrub input was held and `mode.resume_requires_release`
        /// is set: resumes as soon as every scrub input is released.
        resume_pending: bool,
        /// The rewind binding that entered the mode is still held (scrubs back when
        /// `mode.hold_rewind_scrubs`).
        entry_hold: bool,
    },
    /// Leaving the mode: the car is held at `snapshot` for `remaining` seconds, then released.
    Resuming {
        snapshot: CarSnapshot,
        remaining: f64,
        /// Left by cancel (newest snapshot, nothing discarded) rather than resume.
        cancelled: bool,
    },
}

/// What the hook must do this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    /// Nothing to do.
    None,
    /// A snapshot was captured and stored.
    Recorded,
    /// Write this state to the car and keep the simulation frozen.
    Apply(CarSnapshot),
    /// Write this state to the car one last time and unfreeze the simulation.
    Release(CarSnapshot),
}

/// Mode transitions, for logging and the overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeEvent {
    Entered,
    /// Resume from the cursor: history after it was discarded.
    Resumed,
    Cancelled(CancelReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelReason {
    /// Cancel binding, or the rewind binding pressed again.
    User,
    /// `mode.max_time_s` elapsed.
    Timeout,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TickOutput {
    pub action: Action,
    /// Whether game physics/time should be frozen after this frame.
    pub frozen: bool,
    pub event: Option<ModeEvent>,
}

/// One scrub direction: the strongest analog binding (raw travel `0..=1`, before deadzone)
/// and whether any digital binding is held.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrubInput {
    pub analog: f64,
    pub digital: bool,
}

/// Per-frame action inputs. `*_pressed` are rising edges (the hook latches short taps).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ModeInput {
    pub rewind_pressed: bool,
    pub rewind_held: bool,
    pub resume_pressed: bool,
    pub cancel_pressed: bool,
    pub back: ScrubInput,
    pub forward: ScrubInput,
}

/// Recorded history extent and current cursor, for the overlay timeline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timeline {
    pub oldest: f64,
    pub newest: f64,
    /// Scrub position while rewinding/resuming, otherwise `newest`.
    pub cursor: f64,
}

impl Timeline {
    /// Cursor position in `[0, 1]` (1 = live edge).
    pub fn fraction(&self) -> f64 {
        let span = self.newest - self.oldest;
        if span > 0.0 {
            ((self.cursor - self.oldest) / span).clamp(0.0, 1.0)
        } else {
            1.0
        }
    }

    /// Seconds behind the newest snapshot (>= 0).
    pub fn offset(&self) -> f64 {
        (self.newest - self.cursor).max(0.0)
    }
}

/// Rewind-mode details for the overlay.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ModeView {
    pub speed: f64,
    pub elapsed: f64,
    pub resume_pending: bool,
    pub at_oldest: bool,
    pub at_newest: bool,
}

/// Drives recording and the rewind mode. Call [`RewindController::update`] once per frame.
///
/// The controller owns its own timeline clock: it advances only while recording and jumps back
/// to the resume point on release, so snapshot times always increase within the buffer.
#[derive(Clone, Debug)]
pub struct RewindController {
    cfg: Config,
    buffer: RingBuffer,
    state: RewindState,
    clock: f64,
    accum: f64,
    back_held: f64,
    fwd_held: f64,
}

impl RewindController {
    pub fn new(cfg: Config) -> Self {
        let buffer = RingBuffer::new(cfg.capacity());
        let state = if cfg.enabled { RewindState::Recording } else { RewindState::Disabled };
        Self { cfg, buffer, state, clock: 0.0, accum: 0.0, back_held: 0.0, fwd_held: 0.0 }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Replaces the configuration. History is kept unless the buffer capacity changes.
    /// Returns a snapshot to release if this ends a rewind in progress.
    pub fn set_config(&mut self, cfg: Config) -> Option<CarSnapshot> {
        if cfg.capacity() != self.buffer.capacity() {
            let release = self.abort();
            self.buffer = RingBuffer::new(cfg.capacity());
            self.cfg = cfg;
            if !self.cfg.enabled {
                self.state = RewindState::Disabled;
            }
            return release;
        }
        let enabled = cfg.enabled;
        self.cfg = cfg;
        self.set_enabled(enabled)
    }

    pub fn state(&self) -> &RewindState {
        &self.state
    }

    pub fn buffer(&self) -> &RingBuffer {
        &self.buffer
    }

    /// Current recording timeline time (s).
    pub fn clock(&self) -> f64 {
        self.clock
    }

    pub fn is_enabled(&self) -> bool {
        !matches!(self.state, RewindState::Disabled)
    }

    /// In the rewind mode or holding the car before release.
    pub fn is_frozen(&self) -> bool {
        matches!(self.state, RewindState::Rewinding { .. } | RewindState::Resuming { .. })
    }

    pub fn in_mode(&self) -> bool {
        matches!(self.state, RewindState::Rewinding { .. })
    }

    pub fn timeline(&self) -> Option<Timeline> {
        let (oldest, newest) = self.buffer.time_range()?;
        let cursor = match self.state {
            RewindState::Rewinding { cursor, .. } => cursor,
            RewindState::Resuming { snapshot, .. } => snapshot.t,
            _ => newest,
        };
        Some(Timeline { oldest, newest, cursor })
    }

    pub fn mode_view(&self) -> Option<ModeView> {
        let RewindState::Rewinding { cursor, speed, elapsed, resume_pending, .. } = self.state else {
            return None;
        };
        let (oldest, newest) = self.buffer.time_range()?;
        Some(ModeView { speed, elapsed, resume_pending, at_oldest: cursor <= oldest, at_newest: cursor >= newest })
    }

    /// Enables or disables the controller. Disabling mid-rewind returns the snapshot the car
    /// should be released at (the newest one: nothing is discarded), which the hook should
    /// apply before unfreezing.
    pub fn set_enabled(&mut self, enabled: bool) -> Option<CarSnapshot> {
        match (enabled, self.is_enabled()) {
            (true, false) => {
                self.state = RewindState::Recording;
                self.reset();
                None
            }
            (false, true) => {
                let release = self.abort();
                self.state = RewindState::Disabled;
                release
            }
            _ => None,
        }
    }

    /// Clears all history (e.g. stage restart or teleport) and returns to recording.
    /// A disabled controller stays disabled.
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.clock = 0.0;
        self.accum = 0.0;
        self.back_held = 0.0;
        self.fwd_held = 0.0;
        if self.is_enabled() {
            self.state = RewindState::Recording;
        }
    }

    /// Leaves the mode as a cancel would (newest snapshot, nothing discarded), or finishes a
    /// release in progress immediately. Returns the state to write before unfreezing.
    pub fn abort(&mut self) -> Option<CarSnapshot> {
        let release = match self.state {
            RewindState::Rewinding { .. } => self.buffer.newest().copied(),
            RewindState::Resuming { snapshot, .. } => Some(snapshot),
            _ => None,
        };
        if self.is_frozen() {
            if let Some(s) = release {
                self.clock = s.t;
            }
            self.accum = 0.0;
            self.state = RewindState::Recording;
        }
        release
    }

    /// Advances one frame.
    ///
    /// - `dt`: real frame time in seconds (clamped to `[0, 0.25]`; non-finite counts as 0).
    /// - `input`: this frame's action inputs.
    /// - `capture`: reads the live car state; called only when a snapshot is due. Its `t` is
    ///   overwritten with the controller clock. Returning `None` skips this capture.
    pub fn update<F>(&mut self, dt: f64, input: &ModeInput, capture: F) -> TickOutput
    where
        F: FnOnce() -> Option<CarSnapshot>,
    {
        let dt = if dt.is_finite() { dt.clamp(0.0, MAX_DT) } else { 0.0 };
        let mut event = None;
        let action = match self.state {
            RewindState::Disabled => Action::None,
            RewindState::Recording => {
                if input.rewind_pressed && self.can_rewind() {
                    event = Some(ModeEvent::Entered);
                    self.enter(self.buffer.newest().map_or(self.clock, |s| s.t))
                } else {
                    self.record(dt, capture)
                }
            }
            RewindState::Rewinding { cursor, elapsed, resume_pending, entry_hold, .. } => {
                let elapsed = elapsed + dt;
                let max = self.cfg.mode.max_time_s;
                if input.rewind_pressed || input.cancel_pressed {
                    event = Some(ModeEvent::Cancelled(CancelReason::User));
                    self.cancel()
                } else if max > 0.0 && elapsed >= max {
                    event = Some(ModeEvent::Cancelled(CancelReason::Timeout));
                    self.cancel()
                } else {
                    let entry_hold = entry_hold && input.rewind_held;
                    let (cursor, speed, scrubbing) = self.scrub(cursor, dt, input, entry_hold);
                    let wait = self.cfg.mode.resume_requires_release && scrubbing;
                    let resume_now = (input.resume_pressed || resume_pending) && !wait;
                    if resume_now {
                        event = Some(ModeEvent::Resumed);
                        self.begin_release(cursor, false)
                    } else {
                        self.state = RewindState::Rewinding {
                            cursor,
                            speed,
                            elapsed,
                            resume_pending: resume_pending || (input.resume_pressed && wait),
                            entry_hold,
                        };
                        self.buffer.sample(cursor).map_or(Action::None, Action::Apply)
                    }
                }
            }
            RewindState::Resuming { snapshot, remaining, cancelled } => {
                if input.rewind_pressed {
                    event = Some(ModeEvent::Entered);
                    self.enter(snapshot.t)
                } else {
                    let remaining = remaining - dt;
                    if remaining <= 0.0 {
                        self.state = RewindState::Recording;
                        Action::Release(snapshot)
                    } else {
                        self.state = RewindState::Resuming { snapshot, remaining, cancelled };
                        Action::Apply(snapshot)
                    }
                }
            }
        };
        TickOutput { action, frozen: self.is_frozen(), event }
    }

    fn can_rewind(&self) -> bool {
        self.buffer.len() >= 2 && self.buffer.duration() >= self.cfg.scrub.min_history
    }

    fn record<F>(&mut self, dt: f64, capture: F) -> Action
    where
        F: FnOnce() -> Option<CarSnapshot>,
    {
        self.clock += dt;
        self.accum += dt;
        let period = 1.0 / f64::from(self.cfg.buffer.tick_hz);
        if !self.buffer.is_empty() && self.accum < period {
            return Action::None;
        }
        self.accum = if self.accum >= period { (self.accum - period) % period } else { 0.0 };
        match capture() {
            Some(mut snap) => {
                snap.t = self.clock;
                if !snap.is_valid() {
                    return Action::None;
                }
                self.buffer.push(snap);
                Action::Recorded
            }
            None => Action::None,
        }
    }

    fn enter(&mut self, cursor: f64) -> Action {
        self.back_held = 0.0;
        self.fwd_held = 0.0;
        self.state =
            RewindState::Rewinding { cursor, speed: 0.0, elapsed: 0.0, resume_pending: false, entry_hold: true };
        self.buffer.sample(cursor).map_or(Action::None, Action::Apply)
    }

    fn analog_speed(&self, raw: f64, held: f64) -> f64 {
        let s = &self.cfg.scrub;
        if !(raw.is_finite() && raw > s.analog_deadzone) {
            return 0.0;
        }
        let a = ((raw - s.analog_deadzone) / (1.0 - s.analog_deadzone)).clamp(0.0, 1.0);
        let boost = (1.0 + s.analog_hold_boost * held).min(s.analog_boost_max);
        s.analog_max_speed * a.powf(s.analog_curve) * boost
    }

    fn digital_speed(&self, held: f64) -> f64 {
        let s = &self.cfg.scrub;
        (s.initial_speed + s.acceleration * held).min(s.max_speed)
    }

    /// Speed of one direction; updates its hold timer. Returns (speed, active).
    fn direction_speed(&self, input: ScrubInput, extra_digital: bool, held: &mut f64, dt: f64) -> (f64, bool) {
        let digital = input.digital || extra_digital;
        let analog_active = input.analog.is_finite() && input.analog > self.cfg.scrub.analog_deadzone;
        if !(digital || analog_active) {
            *held = 0.0;
            return (0.0, false);
        }
        *held += dt;
        let d = if digital { self.digital_speed(*held) } else { 0.0 };
        (d.max(self.analog_speed(input.analog, *held)), true)
    }

    /// Moves the cursor; returns (cursor, signed speed, any scrub input active).
    fn scrub(&mut self, cursor: f64, dt: f64, input: &ModeInput, entry_hold: bool) -> (f64, f64, bool) {
        let hold_back = entry_hold && self.cfg.mode.hold_rewind_scrubs;
        let (mut back_held, mut fwd_held) = (self.back_held, self.fwd_held);
        let (back, back_on) = self.direction_speed(input.back, hold_back, &mut back_held, dt);
        let (fwd, fwd_on) = self.direction_speed(input.forward, false, &mut fwd_held, dt);
        self.back_held = back_held;
        self.fwd_held = fwd_held;
        let speed = fwd - back;
        let (oldest, newest) = self.buffer.time_range().unwrap_or((cursor, cursor));
        let cursor = (cursor + speed * dt).clamp(oldest, newest);
        (cursor, speed, back_on || fwd_on)
    }

    /// Resume (`cancelled == false`): discard history after `t`. Cancel: `t` is the newest.
    fn begin_release(&mut self, t: f64, cancelled: bool) -> Action {
        let Some(snapshot) = self.buffer.sample(t) else {
            self.state = RewindState::Recording;
            return Action::None;
        };
        self.buffer.truncate_after(snapshot.t);
        self.buffer.push(snapshot);
        self.clock = snapshot.t;
        self.accum = 0.0;
        self.back_held = 0.0;
        self.fwd_held = 0.0;
        let delay = self.cfg.scrub.resume_delay;
        if delay <= 0.0 {
            self.state = RewindState::Recording;
            Action::Release(snapshot)
        } else {
            self.state = RewindState::Resuming { snapshot, remaining: delay, cancelled };
            Action::Apply(snapshot)
        }
    }

    fn cancel(&mut self) -> Action {
        match self.buffer.newest().map(|s| s.t) {
            Some(t) => self.begin_release(t, true),
            None => {
                self.state = RewindState::Recording;
                Action::None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec3;

    const DT: f64 = 1.0 / 60.0;

    /// Simulated game whose car x-position equals simulated time.
    struct Sim {
        ctrl: RewindController,
        sim_t: f64,
        captures: usize,
        prev_rewind: bool,
    }

    /// Held inputs for one frame; edges are derived by the sim like the hook does.
    #[derive(Clone, Copy, Default)]
    struct Held {
        rewind: bool,
        resume: bool,
        cancel: bool,
        back: f64,
        fwd: f64,
        back_key: bool,
        fwd_key: bool,
    }

    const IDLE: Held =
        Held { rewind: false, resume: false, cancel: false, back: 0.0, fwd: 0.0, back_key: false, fwd_key: false };

    fn rewind() -> Held {
        Held { rewind: true, ..IDLE }
    }
    fn resume() -> Held {
        Held { resume: true, ..IDLE }
    }
    fn cancel() -> Held {
        Held { cancel: true, ..IDLE }
    }
    fn back(v: f64) -> Held {
        Held { back: v, ..IDLE }
    }
    fn fwd(v: f64) -> Held {
        Held { fwd: v, ..IDLE }
    }
    fn back_key() -> Held {
        Held { back_key: true, ..IDLE }
    }

    impl Sim {
        fn new(cfg: Config) -> Self {
            Self { ctrl: RewindController::new(cfg), sim_t: 0.0, captures: 0, prev_rewind: false }
        }

        /// `resume` / `cancel` in `h` count as a fresh press on this frame.
        fn step(&mut self, h: Held) -> TickOutput {
            if !self.ctrl.is_frozen() {
                self.sim_t += DT;
            }
            let input = ModeInput {
                rewind_pressed: h.rewind && !self.prev_rewind,
                rewind_held: h.rewind,
                resume_pressed: h.resume,
                cancel_pressed: h.cancel,
                back: ScrubInput { analog: h.back, digital: h.back_key },
                forward: ScrubInput { analog: h.fwd, digital: h.fwd_key },
            };
            self.prev_rewind = h.rewind;
            let x = self.sim_t;
            let captures = &mut self.captures;
            let out = self.ctrl.update(DT, &input, || {
                *captures += 1;
                Some(CarSnapshot { pos: DVec3::new(x, 0.0, 0.0), ..Default::default() })
            });
            if let Action::Apply(s) | Action::Release(s) = out.action {
                self.sim_t = s.pos.x;
            }
            out
        }

        fn run(&mut self, frames: usize, h: Held) -> TickOutput {
            let mut out = None;
            for _ in 0..frames {
                out = Some(self.step(h));
            }
            out.unwrap()
        }

        /// Drive `secs`, then tap rewind (press + release) to enter the mode.
        fn drive_and_enter(&mut self, secs: f64) {
            self.run((secs * 60.0).round() as usize, IDLE);
            let out = self.step(rewind());
            assert_eq!(out.event, Some(ModeEvent::Entered));
            self.step(IDLE);
        }

        fn cursor(&self) -> f64 {
            match self.ctrl.state() {
                RewindState::Rewinding { cursor, .. } => *cursor,
                s => panic!("not in the mode: {s:?}"),
            }
        }

        fn newest(&self) -> f64 {
            self.ctrl.buffer().newest().unwrap().t
        }

        fn release_frames(&mut self) -> (usize, CarSnapshot) {
            for n in 1..60 {
                let out = self.step(IDLE);
                if let Action::Release(s) = out.action {
                    assert!(!out.frozen);
                    return (n, s);
                }
                assert!(out.frozen);
            }
            panic!("never released");
        }
    }

    fn cfg() -> Config {
        let mut c = Config::default();
        c.buffer.seconds = 10.0;
        c.buffer.tick_hz = 60;
        let s = &mut c.scrub;
        s.initial_speed = 1.0;
        s.max_speed = 4.0;
        s.acceleration = 2.0;
        s.analog_deadzone = 0.1;
        s.analog_curve = 1.0;
        s.analog_max_speed = 3.0;
        s.analog_hold_boost = 0.0;
        s.analog_boost_max = 1.0;
        s.resume_delay = 0.25;
        s.min_history = 0.5;
        c.mode.hold_rewind_scrubs = false;
        c
    }

    #[test]
    fn records_at_tick_rate() {
        let mut c = cfg();
        c.buffer.tick_hz = 30;
        let mut sim = Sim::new(c);
        sim.run(120, IDLE); // 2 s at 60 fps
        let n = sim.ctrl.buffer().len();
        assert!((59..=62).contains(&n), "len {n}");
        assert_eq!(sim.captures, n);
        let newest = sim.ctrl.buffer().newest().unwrap();
        assert!((newest.t - newest.pos.x).abs() < 1e-9);
        assert!((sim.ctrl.clock() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn first_frame_always_captures() {
        let mut c = cfg();
        c.buffer.tick_hz = 1;
        let mut ctrl = RewindController::new(c);
        let idle = ModeInput::default();
        let out = ctrl.update(0.001, &idle, || Some(CarSnapshot::default()));
        assert_eq!(out.action, Action::Recorded);
        let out = ctrl.update(0.001, &idle, || panic!("capture should not be due"));
        assert_eq!(out.action, Action::None);
    }

    #[test]
    fn capture_none_or_invalid_is_skipped() {
        let mut ctrl = RewindController::new(cfg());
        let idle = ModeInput::default();
        assert_eq!(ctrl.update(DT, &idle, || None).action, Action::None);
        let bad = CarSnapshot { rpm: f64::NAN, ..Default::default() };
        assert_eq!(ctrl.update(DT, &idle, || Some(bad)).action, Action::None);
        assert!(ctrl.buffer().is_empty());
    }

    #[test]
    fn enter_requires_min_history() {
        let mut sim = Sim::new(cfg());
        sim.run(10, IDLE); // ~0.17 s
        let out = sim.step(rewind());
        assert!(!out.frozen);
        assert_eq!(out.event, None);
        assert_eq!(*sim.ctrl.state(), RewindState::Recording);
    }

    #[test]
    fn enter_requires_a_press_not_a_hold() {
        let mut sim = Sim::new(cfg());
        sim.run(10, rewind()); // held while history is too short
        sim.run(60, rewind()); // still held: no new press
        assert_eq!(*sim.ctrl.state(), RewindState::Recording);
        sim.step(IDLE);
        let out = sim.step(rewind());
        assert!(out.frozen);
        assert!(sim.ctrl.in_mode());
    }

    #[test]
    fn enter_freezes_at_newest_and_holds_without_input() {
        let mut sim = Sim::new(cfg());
        sim.run(120, IDLE);
        let newest = sim.newest();
        let out = sim.step(rewind());
        assert!(out.frozen);
        assert_eq!(out.event, Some(ModeEvent::Entered));
        match out.action {
            Action::Apply(s) => assert!((s.t - newest).abs() < 1e-12),
            a => panic!("expected Apply, got {a:?}"),
        }
        // Idle in the mode: frozen at the same moment, nothing recorded.
        let before = sim.captures;
        for _ in 0..300 {
            let out = sim.step(IDLE);
            assert!(out.frozen);
            assert!(matches!(out.action, Action::Apply(s) if (s.t - newest).abs() < 1e-12));
        }
        assert_eq!(sim.captures, before);
        assert!((sim.cursor() - newest).abs() < 1e-12);
        let tl = sim.ctrl.timeline().unwrap();
        assert_eq!(tl.fraction(), 1.0);
        assert_eq!(tl.offset(), 0.0);
        let v = sim.ctrl.mode_view().unwrap();
        assert!(v.at_newest && !v.at_oldest && v.speed == 0.0);
        assert!((v.elapsed - 300.0 * DT).abs() < 1e-9);
    }

    #[test]
    fn analog_scrub_is_proportional_with_deadzone() {
        let mut sim = Sim::new(cfg());
        sim.drive_and_enter(8.0);
        let c0 = sim.cursor();
        sim.step(back(0.05)); // inside the deadzone
        assert_eq!(sim.cursor(), c0);
        // 0.55 raw -> (0.55 - 0.1) / 0.9 = 0.5 of 3 s/s = 1.5 s/s.
        sim.run(60, back(0.55));
        assert!((c0 - sim.cursor() - 1.5).abs() < 1e-6, "moved {}", c0 - sim.cursor());
        // Full trigger: 3 s/s.
        let c1 = sim.cursor();
        sim.run(30, back(1.0));
        assert!((c1 - sim.cursor() - 1.5).abs() < 1e-6);
        match sim.ctrl.state() {
            RewindState::Rewinding { speed, .. } => assert!((speed + 3.0).abs() < 1e-9),
            _ => unreachable!(),
        }
        // Forward at half travel brings it back.
        let c2 = sim.cursor();
        sim.run(60, fwd(0.55));
        assert!((sim.cursor() - c2 - 1.5).abs() < 1e-6);
    }

    #[test]
    fn analog_curve_and_hold_boost() {
        let mut c = cfg();
        c.scrub.analog_deadzone = 0.0;
        c.scrub.analog_curve = 2.0;
        c.scrub.analog_hold_boost = 1.0;
        c.scrub.analog_boost_max = 2.0;
        let mut sim = Sim::new(c);
        sim.drive_and_enter(9.0);
        sim.step(back(0.5));
        let speed = |sim: &Sim| match sim.ctrl.state() {
            RewindState::Rewinding { speed, .. } => -speed,
            _ => unreachable!(),
        };
        // 3 * 0.5^2 * (1 + DT).
        assert!((speed(&sim) - 0.75 * (1.0 + DT)).abs() < 1e-9);
        sim.run(119, back(0.5)); // 2 s held: boost capped at 2
        assert!((speed(&sim) - 1.5).abs() < 1e-9);
        sim.step(IDLE); // released: boost resets
        sim.step(back(0.5));
        assert!((speed(&sim) - 0.75 * (1.0 + DT)).abs() < 1e-9);
    }

    #[test]
    fn digital_scrub_accelerates_and_caps() {
        let mut sim = Sim::new(cfg());
        sim.drive_and_enter(9.0);
        let mut prev = sim.cursor();
        let mut deltas = Vec::new();
        for _ in 0..120 {
            sim.step(back_key());
            let t = sim.cursor();
            deltas.push(prev - t);
            prev = t;
        }
        assert!((deltas[0] - (1.0 + 2.0 * DT) * DT).abs() < 1e-9);
        assert!(deltas.windows(2).all(|w| w[1] >= w[0] - 1e-12), "monotonic speed-up");
        assert!((deltas[119] - 4.0 * DT).abs() < 1e-9, "capped at max_speed");
    }

    #[test]
    fn digital_and_analog_take_the_faster() {
        let mut sim = Sim::new(cfg());
        sim.drive_and_enter(9.0);
        let c0 = sim.cursor();
        sim.step(Held { back: 1.0, back_key: true, ..IDLE });
        assert!((c0 - sim.cursor() - 3.0 * DT).abs() < 1e-9, "analog 3 s/s beats digital ~1 s/s");
    }

    #[test]
    fn both_directions_cancel_out() {
        let mut sim = Sim::new(cfg());
        sim.drive_and_enter(5.0);
        sim.run(60, back(1.0));
        let c = sim.cursor();
        sim.run(30, Held { back: 1.0, fwd: 1.0, ..IDLE });
        assert!((sim.cursor() - c).abs() < 1e-9);
    }

    #[test]
    fn cursor_clamps_to_buffer() {
        let mut sim = Sim::new(cfg());
        sim.drive_and_enter(2.0);
        let (oldest, newest) = sim.ctrl.buffer().time_range().unwrap();
        sim.run(600, back(1.0));
        assert_eq!(sim.cursor(), oldest);
        assert!(sim.ctrl.mode_view().unwrap().at_oldest);
        sim.run(600, fwd(1.0));
        assert_eq!(sim.cursor(), newest);
        assert!(sim.step(fwd(1.0)).frozen);
        // Applied snapshot matches the cursor (x == t).
        sim.run(20, back(0.7));
        if let Action::Apply(s) = sim.step(back(0.7)).action {
            assert!((s.pos.x - s.t).abs() < 1e-9);
            assert!((s.t - sim.cursor()).abs() < 1e-12);
        } else {
            panic!("expected Apply");
        }
    }

    #[test]
    fn resume_truncates_and_releases_from_cursor() {
        let mut sim = Sim::new(cfg());
        sim.drive_and_enter(5.0);
        sim.run(60, back(1.0)); // 3 s back
        let at = sim.cursor();
        let out = sim.step(resume());
        assert_eq!(out.event, Some(ModeEvent::Resumed));
        assert!(out.frozen);
        assert!(matches!(out.action, Action::Apply(s) if (s.t - at).abs() < 1e-12));
        assert!((sim.newest() - at).abs() < 1e-12, "future discarded");
        assert!((sim.ctrl.clock() - at).abs() < 1e-12);
        assert!(matches!(
            sim.ctrl.state(),
            RewindState::Resuming { cancelled: false, remaining, .. } if *remaining == 0.25
        ));
        let (frames, s) = sim.release_frames();
        assert!((14..=16).contains(&frames), "released after {frames} frames");
        assert!((s.t - at).abs() < 1e-12);
        assert_eq!(*sim.ctrl.state(), RewindState::Recording);
        sim.run(60, IDLE);
        let ts: Vec<f64> = sim.ctrl.buffer().iter().map(|s| s.t).collect();
        assert!(ts.windows(2).all(|w| w[1] > w[0]), "times strictly increasing");
        assert!((sim.newest() - (at + 1.0)).abs() < 1e-6);
        assert!(sim.ctrl.buffer().iter().all(|s| (s.pos.x - s.t).abs() < 1e-6));
    }

    #[test]
    fn cancel_returns_to_newest_without_truncating() {
        for how in [cancel(), rewind()] {
            let mut sim = Sim::new(cfg());
            sim.drive_and_enter(5.0);
            let newest = sim.newest();
            let len = sim.ctrl.buffer().len();
            sim.run(90, back(1.0));
            assert!(sim.cursor() < newest - 3.0);
            let out = sim.step(how);
            assert_eq!(out.event, Some(ModeEvent::Cancelled(CancelReason::User)));
            assert!(matches!(out.action, Action::Apply(s) if (s.t - newest).abs() < 1e-12));
            assert_eq!(sim.ctrl.buffer().len(), len, "nothing discarded");
            assert!(matches!(sim.ctrl.state(), RewindState::Resuming { cancelled: true, .. }));
            sim.step(IDLE);
            let (_, s) = sim.release_frames();
            assert!((s.t - newest).abs() < 1e-12);
            assert!((s.pos.x - newest).abs() < 1e-12);
        }
    }

    #[test]
    fn rewind_press_in_mode_cancels_but_entry_hold_does_not() {
        let mut c = cfg();
        c.mode.hold_rewind_scrubs = true;
        let mut sim = Sim::new(c);
        sim.run(300, IDLE);
        sim.step(rewind());
        // Keep holding the button that entered: scrubs back at digital speed, no cancel.
        let c0 = sim.cursor();
        sim.run(60, rewind());
        assert!(sim.ctrl.in_mode());
        assert!(c0 - sim.cursor() > 1.0, "held rewind scrubs back");
        // Release: stays in the mode and stops moving.
        sim.step(IDLE);
        let c1 = sim.cursor();
        sim.run(30, IDLE);
        assert_eq!(sim.cursor(), c1);
        // Press again: cancel.
        assert_eq!(sim.step(rewind()).event, Some(ModeEvent::Cancelled(CancelReason::User)));
    }

    #[test]
    fn hold_rewind_scrub_can_be_disabled() {
        let mut sim = Sim::new(cfg()); // hold_rewind_scrubs = false
        sim.run(300, IDLE);
        sim.step(rewind());
        let c0 = sim.cursor();
        sim.run(60, rewind());
        assert_eq!(sim.cursor(), c0);
    }

    #[test]
    fn resume_can_wait_for_triggers_released() {
        let mut c = cfg();
        c.mode.resume_requires_release = true;
        let mut sim = Sim::new(c);
        sim.drive_and_enter(5.0);
        sim.run(30, fwd(0.0));
        sim.run(30, back(1.0));
        // Resume pressed while still on the trigger: deferred.
        let out = sim.step(Held { resume: true, back: 1.0, ..IDLE });
        assert_eq!(out.event, None);
        assert!(sim.ctrl.mode_view().unwrap().resume_pending);
        let c = {
            sim.run(10, back(1.0));
            sim.cursor()
        };
        // Released: resumes on the next frame, from where the cursor ended up.
        let out = sim.step(IDLE);
        assert_eq!(out.event, Some(ModeEvent::Resumed));
        assert!(matches!(out.action, Action::Apply(s) if (s.t - c).abs() < 1e-12));
    }

    #[test]
    fn resume_while_scrubbing_is_immediate_by_default() {
        let mut sim = Sim::new(cfg());
        sim.drive_and_enter(5.0);
        sim.run(30, back(1.0));
        let out = sim.step(Held { resume: true, back: 1.0, ..IDLE });
        assert_eq!(out.event, Some(ModeEvent::Resumed));
    }

    #[test]
    fn max_time_in_mode_auto_cancels() {
        let mut c = cfg();
        c.mode.max_time_s = 1.0;
        let mut sim = Sim::new(c);
        sim.drive_and_enter(5.0);
        let newest = sim.newest();
        sim.run(30, back(1.0));
        let mut ev = None;
        for _ in 0..40 {
            if let Some(e) = sim.step(IDLE).event {
                ev = Some(e);
                break;
            }
        }
        assert_eq!(ev, Some(ModeEvent::Cancelled(CancelReason::Timeout)));
        let (_, s) = sim.release_frames();
        assert!((s.t - newest).abs() < 1e-12);
    }

    #[test]
    fn capture_not_called_while_frozen() {
        let mut sim = Sim::new(cfg());
        sim.run(120, IDLE);
        let before = sim.captures;
        sim.step(rewind());
        sim.run(30, back(1.0));
        sim.step(resume());
        sim.run(5, IDLE); // still in the resume delay
        assert_eq!(sim.captures, before);
    }

    #[test]
    fn rewind_press_during_release_reenters_at_resume_point() {
        let mut sim = Sim::new(cfg());
        sim.drive_and_enter(5.0);
        sim.run(60, back(1.0));
        let at = sim.cursor();
        sim.step(resume());
        sim.run(3, IDLE);
        let out = sim.step(rewind());
        assert_eq!(out.event, Some(ModeEvent::Entered));
        assert!((sim.cursor() - at).abs() < 1e-12);
        sim.step(IDLE);
        sim.run(30, back(1.0));
        assert!(sim.cursor() < at);
    }

    #[test]
    fn zero_resume_delay_releases_immediately() {
        let mut c = cfg();
        c.scrub.resume_delay = 0.0;
        let mut sim = Sim::new(c);
        sim.drive_and_enter(2.0);
        sim.run(20, back(1.0));
        let out = sim.step(resume());
        assert!(!out.frozen);
        assert!(matches!(out.action, Action::Release(_)));
        assert_eq!(*sim.ctrl.state(), RewindState::Recording);
    }

    #[test]
    fn rewind_after_buffer_wraparound() {
        let mut c = cfg();
        c.buffer.seconds = 2.0;
        let mut sim = Sim::new(c);
        sim.drive_and_enter(10.0); // buffer keeps ~2 s
        let (oldest, newest) = sim.ctrl.buffer().time_range().unwrap();
        assert!((newest - oldest - 2.0).abs() < 0.05);
        assert!(oldest > 7.9);
        sim.run(600, back(1.0));
        assert_eq!(sim.cursor(), oldest);
        let out = sim.step(resume());
        assert!(matches!(out.action, Action::Apply(s) if (s.pos.x - oldest).abs() < 1e-9));
    }

    #[test]
    fn disable_mid_mode_releases_at_newest() {
        let mut sim = Sim::new(cfg());
        sim.drive_and_enter(2.0);
        let newest = sim.newest();
        sim.run(20, back(1.0));
        let release = sim.ctrl.set_enabled(false).expect("release snapshot");
        assert!((release.t - newest).abs() < 1e-12, "auto-cancel goes back to live");
        assert_eq!(*sim.ctrl.state(), RewindState::Disabled);
        assert!(!sim.ctrl.is_frozen());
        let out = sim.ctrl.update(DT, &ModeInput { rewind_pressed: true, ..Default::default() }, || {
            panic!("no capture while disabled")
        });
        assert_eq!(out, TickOutput { action: Action::None, frozen: false, event: None });
        assert_eq!(sim.ctrl.set_enabled(true), None);
        assert_eq!(*sim.ctrl.state(), RewindState::Recording);
        assert!(sim.ctrl.buffer().is_empty());
    }

    #[test]
    fn abort_during_release_returns_that_snapshot() {
        let mut sim = Sim::new(cfg());
        sim.drive_and_enter(3.0);
        sim.run(30, back(1.0));
        let at = sim.cursor();
        sim.step(resume());
        let s = sim.ctrl.abort().unwrap();
        assert!((s.t - at).abs() < 1e-12);
        assert_eq!(*sim.ctrl.state(), RewindState::Recording);
        assert_eq!(sim.ctrl.abort(), None, "nothing to abort while recording");
    }

    #[test]
    fn starts_disabled_from_config() {
        let mut c = cfg();
        c.enabled = false;
        let mut ctrl = RewindController::new(c);
        assert_eq!(*ctrl.state(), RewindState::Disabled);
        ctrl.reset();
        assert_eq!(*ctrl.state(), RewindState::Disabled);
    }

    #[test]
    fn set_config_resizes_buffer_and_toggles() {
        let mut sim = Sim::new(cfg());
        sim.run(120, IDLE);
        let mut c = cfg();
        c.scrub.max_speed = 8.0;
        assert_eq!(sim.ctrl.set_config(c.clone()), None);
        assert!(!sim.ctrl.buffer().is_empty(), "same capacity keeps history");
        c.buffer.seconds = 5.0;
        sim.ctrl.set_config(c.clone());
        assert!(sim.ctrl.buffer().is_empty());
        assert_eq!(sim.ctrl.buffer().capacity(), 301);
        c.enabled = false;
        sim.ctrl.set_config(c);
        assert_eq!(*sim.ctrl.state(), RewindState::Disabled);
    }

    #[test]
    fn set_config_mid_mode_keeps_the_mode() {
        let mut sim = Sim::new(cfg());
        sim.drive_and_enter(3.0);
        sim.run(10, back(1.0));
        let mut c = cfg();
        c.scrub.analog_max_speed = 9.0;
        assert_eq!(sim.ctrl.set_config(c), None);
        assert!(sim.ctrl.in_mode());
        let c0 = sim.cursor();
        sim.step(back(1.0));
        assert!((c0 - sim.cursor() - 9.0 * DT).abs() < 1e-9, "new speed applies at once");
    }

    #[test]
    fn timeline_reports_cursor() {
        let mut sim = Sim::new(cfg());
        assert_eq!(sim.ctrl.timeline(), None);
        sim.run(240, IDLE);
        let tl = sim.ctrl.timeline().unwrap();
        assert_eq!(tl.cursor, tl.newest);
        assert_eq!(tl.fraction(), 1.0);
        sim.step(rewind());
        sim.run(60, back(1.0));
        let tl = sim.ctrl.timeline().unwrap();
        assert!(tl.fraction() < 1.0 && tl.fraction() > 0.0);
        assert_eq!(tl.cursor, sim.cursor());
        assert!((tl.offset() - 3.0).abs() < 1e-6);
        assert_eq!(sim.ctrl.mode_view().map(|v| v.at_newest), Some(false));
    }

    #[test]
    fn bad_dt_and_bad_analog_are_sanitized() {
        let mut sim = Sim::new(cfg());
        sim.run(60, IDLE);
        let clock = sim.ctrl.clock();
        let idle = ModeInput::default();
        sim.ctrl.update(f64::NAN, &idle, || None);
        sim.ctrl.update(-1.0, &idle, || None);
        assert_eq!(sim.ctrl.clock(), clock);
        sim.ctrl.update(10.0, &idle, || None);
        assert!((sim.ctrl.clock() - clock - MAX_DT).abs() < 1e-12);
        let mut sim = Sim::new(cfg());
        sim.drive_and_enter(3.0);
        let c0 = sim.cursor();
        sim.run(10, back(f64::NAN));
        assert_eq!(sim.cursor(), c0);
    }

    #[test]
    fn reset_clears_history() {
        let mut sim = Sim::new(cfg());
        sim.drive_and_enter(2.0);
        sim.run(10, back(1.0));
        sim.ctrl.reset();
        assert!(sim.ctrl.buffer().is_empty());
        assert_eq!(*sim.ctrl.state(), RewindState::Recording);
        assert_eq!(sim.ctrl.clock(), 0.0);
    }
}
