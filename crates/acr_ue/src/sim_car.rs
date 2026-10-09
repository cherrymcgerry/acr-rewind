//! Primary backend for the custom (Kunos-derived) solver: reads and writes the simulated car
//! state directly (re-notes §6.B, §8).
//!
//! The car is located per `[raw_offsets.sim_car] locator`:
//! - `vtable_scan` (default): the rigid bodies found by [`crate::bodies`] on a background
//!   thread. The main chassis body is the snapshot source; every body of the car is restored.
//! - `chain`: a pointer chain from the module or the player pawn.
//!
//! The snapshot is kept in the sim's own coordinate frame (only lengths are scaled to metres),
//! so restore writes back exactly what was read.

use crate::backend::{BackendError, BlobHistory, CarBackend, CarChange, WriteMode};
use crate::bodies::{
    check_body_tol, header_ok, read_body, write_body, BackgroundScanner, BodyHistory, BodyLayout, BodyState, CarBodies,
    HintFn, ScanConfig, LOCKED_ORTHO_TOL,
};
use crate::freeze::Freezer;
use crate::mem::{is_plausible_ptr, Memory, ScanSource};
use crate::sigs::{ChainRoot, MatrixLayout, SimCarLocator, SimCarSpec, ValueType};
use glam::{DQuat, DVec3};
use rewind_core::CarSnapshot;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Follows `chain` from `root`: each step is `addr = *(addr + off)`.
pub fn resolve_chain(mem: &dyn Memory, root: usize, chain: &[u64]) -> Option<usize> {
    let mut addr = root;
    for &off in chain {
        addr = mem.read_ptr(addr.checked_add(off as usize)?)?;
        if !is_plausible_ptr(addr) {
            return None;
        }
    }
    Some(addr)
}

pub type PawnFn = Box<dyn FnMut() -> Option<usize> + Send>;

/// How often the cached car is compared with the shm car position.
const HINT_CHECK_PERIOD: Duration = Duration::from_millis(100);
/// Our own writes move the car away from what shm shows; ignore the hint this long after one.
const WRITE_QUIET: Duration = Duration::from_secs(1);
/// Minimum time between two "pinned car not re-adopted" log lines.
const PIN_LOG_PERIOD: Duration = Duration::from_secs(2);
/// A pinned body whose distance to the main body changed by more than this (m) since it was
/// pinned has come off the car (crash damage: hood, bumper); suspension travel stays well below.
pub const DETACH_M: f64 = 0.75;

/// `vtable_scan` state: the cached car, a body history and the background scanner. Shared by
/// [`SimCarAdapter`] and [`crate::car_avatar::CarAvatarAdapter`]; what goes into `history`
/// is up to the owner.
pub struct BodyTracker {
    cfg: ScanConfig,
    scanner: BackgroundScanner,
    car: Option<CarBodies>,
    pub history: BodyHistory,
    last_pos: Option<DVec3>,
    last_write: Option<Instant>,
    last_hint_check: Option<Instant>,
    mismatch_since: Option<Instant>,
    max_jump_m: f64,
    mismatch_m: f64,
    mismatch_s: f64,
    last_failure: String,
    /// When shm first showed a car while none is locked (scan start, for the timing log).
    seek_since: Option<Instant>,
    idle_logged: bool,
    /// The owner is writing the car (rewind mode): never adopt a new body set or sweep.
    held: bool,
    /// Pin the first body set confirmed against shm and only ever re-adopt (a subset of) it.
    pinning: bool,
    /// The car is static while the owner's pawn lives, but transient world bodies (vegetation
    /// hit off-road) share its vtable and parent, sit within `car_radius_m` and are freed later:
    /// a rescan of a stopped car would adopt them and writes would land in freed heap memory.
    pin: Option<CarBodies>,
    /// Distance of every pinned body to the main body when pinned.
    pin_rest: Vec<(usize, f64)>,
    last_pin_log: Option<Instant>,
    /// The bodies `history`'s columns belong to (kept across drops of a pinned car).
    hist_car: Option<CarBodies>,
    /// History columns are `[primary, others..]` (else `others` only).
    history_primary_first: bool,
}

impl BodyTracker {
    /// `None` unless `spec` is a configured `vtable_scan` locator. Starts the scan thread.
    pub fn spawn<M: ScanSource + Send + Sync + 'static>(
        mem: Arc<M>,
        spec: &SimCarSpec,
        module_base: usize,
        hint: HintFn,
        history_capacity: usize,
    ) -> Option<Self> {
        if spec.locator != SimCarLocator::VtableScan {
            return None;
        }
        let cfg = ScanConfig::from_spec(spec, module_base)?;
        Some(Self {
            cfg,
            scanner: BackgroundScanner::spawn(mem, cfg, hint, Duration::from_millis(spec.rescan_interval_ms)),
            car: None,
            history: BodyHistory::new(history_capacity),
            last_pos: None,
            last_write: None,
            last_hint_check: None,
            mismatch_since: None,
            max_jump_m: spec.max_jump_m,
            mismatch_m: spec.hint_mismatch_m,
            mismatch_s: spec.hint_mismatch_s,
            last_failure: String::new(),
            seek_since: None,
            idle_logged: false,
            held: false,
            pinning: false,
            pin: None,
            pin_rest: Vec::new(),
            last_pin_log: None,
            hist_car: None,
            history_primary_first: false,
        })
    }

    /// The owner records `history` as `[primary, others..]` instead of `others` only.
    pub fn set_history_primary_first(&mut self, on: bool) {
        self.history_primary_first = on;
        self.history.clear();
    }

    fn history_order(&self, car: &CarBodies) -> Vec<usize> {
        let first = self.history_primary_first.then_some(car.primary);
        first.into_iter().chain(car.others()).collect()
    }

    /// Switches to `new`, keeping the history columns of bodies it shares with the previous
    /// set (same objects), clearing the history otherwise.
    fn set_car_keeping_history(&mut self, new: CarBodies) {
        let keep = self.hist_car.as_ref().and_then(|old| {
            let old_order = self.history_order(old);
            let keep: Option<Vec<usize>> =
                self.history_order(&new).iter().map(|a| old_order.iter().position(|b| b == a)).collect();
            keep.map(|k| (k, old_order.len()))
        });
        match keep {
            Some((k, old_len)) => self.history.remap(&k, old_len),
            None => self.history.clear(),
        }
        self.hist_car = Some(new.clone());
        self.car = Some(new);
    }

    /// Current distance of `b` to the main body, if both read as bodies of the car.
    fn dist_to_main(&self, mem: &dyn Memory, main: DVec3, b: usize) -> Option<f64> {
        check_body_tol(mem, self.layout(), b, LOCKED_ORTHO_TOL).ok().map(|x| (x.state.pos - main).length())
    }

    /// Pinned bodies whose distance to the main body (at `main`) moved by more than
    /// [`DETACH_M`] since pinning, with (rest, now) distances.
    fn detached(&self, mem: &dyn Memory, main: DVec3, bodies: &[usize]) -> Vec<(usize, f64, f64)> {
        self.pin_rest
            .iter()
            .filter(|(b, _)| bodies.contains(b))
            .filter_map(|&(b, rest)| {
                let now = self.dist_to_main(mem, main, b)?;
                ((now - rest).abs() > DETACH_M).then_some((b, rest, now))
            })
            .collect()
    }

    /// Removes detached bodies from the car, the pin and the history columns.
    fn drop_detached(&mut self, mem: &dyn Memory, main: DVec3) {
        let Some(car) = self.car.clone() else {
            return;
        };
        let gone = self.detached(mem, main, &car.bodies);
        if gone.is_empty() {
            return;
        }
        for (b, rest, now) in &gone {
            tracing::info!(
                "sim bodies: body {b:#x} came off the car ({rest:.2} m -> {now:.2} m from the main body); no longer restored"
            );
        }
        let is_gone = |a: &usize| gone.iter().any(|(b, _, _)| b == a);
        if let Some(pin) = self.pin.as_mut() {
            pin.bodies.retain(|a| !is_gone(a));
        }
        self.pin_rest.retain(|(a, _)| !is_gone(a));
        let mut new = car;
        new.excluded += gone.len();
        new.bodies.retain(|a| !is_gone(a));
        self.set_car_keeping_history(new);
    }

    /// While held, a lost car stays lost: no scan is started and no result adopted.
    pub fn set_held(&mut self, held: bool) {
        self.held = held;
    }

    pub fn is_held(&self) -> bool {
        self.held
    }

    /// Enables pinning (see [`BodyTracker::pin`]). The owner must [`unpin`](Self::unpin) when
    /// its car object changes.
    pub fn set_pinning(&mut self, on: bool) {
        self.pinning = on;
        if !on {
            self.pin = None;
            self.pin_rest.clear();
        }
    }

    /// Forgets the pinned body set (new car / respawn); the next confirmed lock is pinned.
    pub fn unpin(&mut self) {
        self.pin_rest.clear();
        if let Some(p) = self.pin.take() {
            tracing::info!("sim bodies: unpinned the body set of main body {:#x}", p.primary);
        }
    }

    pub fn pinned(&self) -> Option<&CarBodies> {
        self.pin.as_ref()
    }

    fn pin_current(&mut self, mem: &dyn Memory) {
        if !self.pinning || self.pin.is_some() {
            return;
        }
        let Some(car) = self.car.clone() else {
            return;
        };
        let Some(main) = check_body_tol(mem, self.layout(), car.primary, LOCKED_ORTHO_TOL).ok().map(|b| b.state.pos)
        else {
            return;
        };
        let rest: Option<Vec<(usize, f64)>> =
            car.bodies.iter().map(|&b| self.dist_to_main(mem, main, b).map(|d| (b, d))).collect();
        let Some(rest) = rest else {
            return;
        };
        tracing::info!(
            "sim bodies: pinned {} bodies of main body {:#x}; rescans only re-adopt these",
            car.bodies.len(),
            car.primary
        );
        self.pin_rest = rest;
        self.pin = Some(car);
    }

    fn pin_log(&mut self, msg: impl FnOnce() -> String) {
        if self.last_pin_log.is_none_or(|t| t.elapsed() >= PIN_LOG_PERIOD) {
            self.last_pin_log = Some(Instant::now());
            tracing::info!("{}", msg());
        }
    }

    /// Re-adopts the pinned bodies that are valid and move with the pinned main body, if the
    /// main body is back within tolerance of `hint`. `Err((gone, why))`: `gone` = the pinned
    /// main body no longer is a body of that parent (freed / reused), so the pin is stale.
    fn readopt_pinned(&self, mem: &dyn Memory, hint: &HintFn) -> Result<CarBodies, (bool, String)> {
        let pin = self.pin.as_ref().ok_or((true, "nothing pinned".to_owned()))?;
        let l = &self.cfg.layout;
        if !header_ok(mem, l, pin.primary, self.cfg.vtable, pin.parent) {
            return Err((true, format!("pinned main body {:#x} lost its vtable or parent", pin.primary)));
        }
        let main = check_body_tol(mem, l, pin.primary, LOCKED_ORTHO_TOL)
            .map_err(|e| (false, format!("pinned main body {:#x} invalid: {e}", pin.primary)))?
            .state;
        let h = hint().ok_or((false, "shm car position unavailable".to_owned()))?;
        let d = (main.pos - h).length();
        let tol = self.cfg.select.position_tolerance(main.lin_vel.length());
        if d > tol {
            return Err((false, format!("pinned main body {d:.1} m from the shm car position (tol {tol:.1} m)")));
        }
        let detached = self.detached(mem, main.pos, &pin.bodies);
        let bodies: Vec<usize> = pin
            .bodies
            .iter()
            .copied()
            .filter(|&b| {
                b == pin.primary
                    || (header_ok(mem, l, b, self.cfg.vtable, pin.parent)
                        && !detached.iter().any(|(d, _, _)| *d == b)
                        && check_body_tol(mem, l, b, LOCKED_ORTHO_TOL)
                            .is_ok_and(|x| self.cfg.select.is_member(&main, &x.state)))
            })
            .collect();
        Ok(CarBodies { parent: pin.parent, primary: pin.primary, excluded: pin.bodies.len() - bodies.len(), bodies })
    }

    /// A scanned car: history restarts.
    fn adopt(&mut self, mem: &dyn Memory, car: CarBodies) {
        self.history.clear();
        self.hist_car = None;
        self.adopt_keeping_history(mem, car);
    }

    /// Re-adopted pinned bodies: history columns of the bodies still there survive.
    fn adopt_keeping_history(&mut self, mem: &dyn Memory, car: CarBodies) {
        self.last_pos = read_body(mem, self.layout(), car.primary).map(|b| b.state.pos);
        self.mismatch_since = None;
        self.last_failure.clear();
        self.set_car_keeping_history(car);
        self.pin_current(mem);
    }

    pub fn layout(&self) -> &BodyLayout {
        &self.cfg.layout
    }

    pub fn select(&self) -> &crate::bodies::CarSelect {
        &self.cfg.select
    }

    /// Whether the cached car still passes the per-tick checks (headers, main body readable
    /// and within the speed-aware tolerance of `hint`). Used before handing the tracker over.
    pub fn still_valid(&self, mem: &dyn Memory, hint: &HintFn) -> bool {
        let Some(car) = self.car.as_ref() else {
            return false;
        };
        if !self.headers_ok(mem) {
            return false;
        }
        let Some(main) = read_body(mem, self.layout(), car.primary).map(|b| b.state) else {
            return false;
        };
        hint().is_none_or(|h| {
            (main.pos - h).length()
                <= crate::bodies::shm_tolerance(self.mismatch_m, self.cfg.select.shm_lag_s, main.lin_vel.length())
        })
    }

    pub fn vtable(&self) -> usize {
        self.cfg.vtable
    }

    pub fn car(&self) -> Option<&CarBodies> {
        self.car.as_ref()
    }

    fn drop_car(&mut self, why: &str) {
        let next = if self.pin.is_some() {
            "waiting for the pinned bodies"
        } else if self.held {
            "no rescan until the rewind ends"
        } else {
            "rescanning"
        };
        tracing::info!("sim bodies: dropping cached car ({why}); {next}");
        self.car = None;
        if self.pin.is_none() {
            self.history.clear();
            self.hist_car = None;
        }
        self.last_pos = None;
        self.mismatch_since = None;
        self.seek_since = Some(Instant::now());
    }

    /// Per-tick: revalidates the cached car, or re-adopts the pinned bodies / a finished scan
    /// (`Changed`, history cleared), or requests a scan (`Lost`). Never blocks on a scan.
    /// While [held](Self::set_held) a lost car stays lost.
    pub fn refresh(&mut self, mem: &dyn Memory, hint: &HintFn) -> CarChange {
        if self.car.is_some() {
            return match self.revalidate(mem, hint) {
                Ok(pos) => {
                    self.last_pos = Some(pos);
                    CarChange::Same
                }
                Err(why) => {
                    self.drop_car(&why);
                    CarChange::Lost
                }
            };
        }
        if self.pin.is_some() {
            // The pinned bodies are the car's own objects: wait for them instead of sweeping
            // (also while held: re-adopting them is as safe as keeping them).
            match self.readopt_pinned(mem, hint) {
                Ok(car) => {
                    tracing::info!(
                        "sim bodies: re-adopted {} of {} pinned bodies (main body {:#x})",
                        car.bodies.len(),
                        car.bodies.len() + car.excluded,
                        car.primary
                    );
                    let _ = self.scanner.take();
                    self.seek_since = None;
                    self.adopt_keeping_history(mem, car);
                    return CarChange::Changed;
                }
                Err((false, why)) => {
                    self.pin_log(|| format!("sim bodies: pinned car not re-adopted yet: {why}"));
                    return CarChange::Lost;
                }
                Err((true, why)) => {
                    tracing::info!("sim bodies: pin released ({why})");
                    self.unpin();
                    self.history.clear();
                    self.hist_car = None;
                }
            }
        }
        if self.held {
            // Anything found now was swept before or during the hold: stale.
            let _ = self.scanner.take();
            return CarChange::Lost;
        }
        if let Some(rep) = self.scanner.take() {
            let summary = rep.summary();
            match rep.chosen {
                Ok((_, car)) => {
                    let after = self.seek_since.take().map_or(String::new(), |t| {
                        format!(" (locked {:.1} s after shm showed the car)", t.elapsed().as_secs_f64())
                    });
                    tracing::info!("sim bodies: scan finished: {}{after}", rep_summary_with_bodies(&summary, &car));
                    self.adopt(mem, car);
                    return CarChange::Changed;
                }
                Err(_) => {
                    if summary != self.last_failure {
                        tracing::info!("sim bodies: scan finished without a car: {summary}");
                        self.last_failure = summary;
                    }
                }
            }
        }
        // Scanning is pointless (and costs a full heap sweep) until shm shows a car on stage.
        match hint() {
            None => {
                if self.seek_since.take().is_some() || !self.idle_logged {
                    tracing::info!("sim bodies: waiting for shm to show a car on stage before scanning");
                    self.idle_logged = true;
                }
            }
            Some(p) => {
                if self.seek_since.is_none() {
                    tracing::info!(
                        "sim bodies: shm shows a car at ({:.1}, {:.1}, {:.1}); starting the body scan",
                        p.x,
                        p.y,
                        p.z
                    );
                    self.seek_since = Some(Instant::now());
                }
                if self.scanner.request() {
                    tracing::debug!("sim bodies: scan requested");
                }
            }
        }
        CarChange::Lost
    }

    /// Lock state for the status heartbeat.
    pub fn health(&self) -> crate::backend::BackendHealth {
        crate::backend::BackendHealth {
            car_locked: self.car.is_some(),
            bodies: self.car.as_ref().map_or(0, |c| c.bodies.len()),
            last_error: (!self.last_failure.is_empty()).then(|| self.last_failure.clone()),
        }
    }

    /// Every body still carries the vtable and the car's parent pointer.
    pub fn headers_ok(&self, mem: &dyn Memory) -> bool {
        self.car.as_ref().is_some_and(|car| {
            car.bodies.iter().all(|&b| header_ok(mem, self.layout(), b, self.cfg.vtable, car.parent))
        })
    }

    /// States of `addrs` (all must be valid bodies of the car).
    pub fn read_states(&self, mem: &dyn Memory, addrs: impl Iterator<Item = usize>) -> Option<Vec<BodyState>> {
        let car = self.car.as_ref()?;
        addrs
            .map(|b| {
                check_body_tol(mem, self.layout(), b, LOCKED_ORTHO_TOL)
                    .ok()
                    .filter(|x| x.parent == car.parent)
                    .map(|x| x.state)
            })
            .collect()
    }

    /// Records that the owner moved the car: the hint comparison pauses briefly and, with
    /// `primary_pos = None`, the next tick skips the jump check.
    pub fn note_write(&mut self, primary_pos: Option<DVec3>) {
        self.last_write = Some(Instant::now());
        self.last_pos = primary_pos;
    }

    /// Cheap per-tick check of the cached car. Returns the main body position.
    fn revalidate(&mut self, mem: &dyn Memory, hint: &HintFn) -> Result<DVec3, String> {
        let car = self.car.as_ref().ok_or("no car")?;
        let l = self.cfg.layout;
        if let Some(b) = car.bodies.iter().find(|&&b| !header_ok(mem, &l, b, self.cfg.vtable, car.parent)) {
            return Err(format!("body {b:#x} lost its vtable or parent"));
        }
        let main = check_body_tol(mem, &l, car.primary, LOCKED_ORTHO_TOL)
            .map_err(|e| format!("main body unreadable or invalid: {e}"))?
            .state;
        let pos = main.pos;
        if let Some(last) = self.last_pos {
            let d = (pos - last).length();
            if d > self.max_jump_m {
                return Err(format!("main body jumped {d:.1} m in one tick"));
            }
        }
        let now = Instant::now();
        if self.last_write.is_some_and(|t| now.duration_since(t) < WRITE_QUIET) {
            self.mismatch_since = None;
        } else if self.last_hint_check.is_none_or(|t| now.duration_since(t) >= HINT_CHECK_PERIOD) {
            self.last_hint_check = Some(now);
            if let Some(h) = hint() {
                let d = (pos - h).length();
                if d > crate::bodies::shm_tolerance(self.mismatch_m, self.cfg.select.shm_lag_s, main.lin_vel.length()) {
                    let since = *self.mismatch_since.get_or_insert(now);
                    let held = now.duration_since(since).as_secs_f64();
                    if held >= self.mismatch_s {
                        return Err(format!("main body {d:.1} m from the shm car position for {held:.1} s"));
                    }
                } else {
                    self.mismatch_since = None;
                    self.pin_current(mem);
                }
            }
            // Only the sim moves the bodies now (no write of ours within WRITE_QUIET).
            if !self.held {
                self.drop_detached(mem, pos);
            }
        }
        Ok(pos)
    }
}

pub struct SimCarAdapter<M: ScanSource + Send + Sync + 'static> {
    mem: Arc<M>,
    spec: SimCarSpec,
    module_base: usize,
    pawn: PawnFn,
    hint: HintFn,
    freezer: Box<dyn Freezer>,
    car: Option<usize>,
    pawn_ptr: Option<usize>,
    blobs: BlobHistory,
    tracker: Option<BodyTracker>,
}

impl<M: ScanSource + Send + Sync + 'static> SimCarAdapter<M> {
    /// `hint` returns the shm car position (graphics carCoordinates); it is called from the
    /// tick and from the scan thread.
    pub fn new(
        mem: M,
        spec: SimCarSpec,
        module_base: usize,
        pawn: PawnFn,
        hint: HintFn,
        freezer: Box<dyn Freezer>,
        history_capacity: usize,
    ) -> Self {
        let mem = Arc::new(mem);
        let tracker = BodyTracker::spawn(mem.clone(), &spec, module_base, hint.clone(), history_capacity);
        Self {
            mem,
            spec,
            module_base,
            pawn,
            hint,
            freezer,
            car: None,
            pawn_ptr: None,
            blobs: BlobHistory::new(history_capacity),
            tracker,
        }
    }

    pub fn mem(&self) -> &M {
        &self.mem
    }

    pub fn car_address(&self) -> Option<usize> {
        self.car
    }

    /// The located car (`vtable_scan` only).
    pub fn car_bodies(&self) -> Option<&CarBodies> {
        self.tracker.as_ref()?.car()
    }

    fn vt(&self) -> ValueType {
        self.spec.value_type
    }

    fn read_val(&self, addr: usize) -> Option<f64> {
        match self.vt() {
            ValueType::F32 => self.mem.read_f32(addr).map(f64::from),
            ValueType::F64 => self.mem.read_f64(addr),
        }
    }

    fn write_val(&self, addr: usize, v: f64) -> Result<(), BackendError> {
        let ok = match self.vt() {
            ValueType::F32 => self.mem.write_f32(addr, v as f32),
            ValueType::F64 => self.mem.write_f64(addr, v),
        };
        ok.then_some(()).ok_or(BackendError::Write(addr))
    }

    fn read_vec3(&self, addr: usize) -> Option<DVec3> {
        let s = self.vt().size();
        Some(DVec3::new(self.read_val(addr)?, self.read_val(addr + s)?, self.read_val(addr + 2 * s)?))
    }

    fn write_vec3(&self, addr: usize, v: DVec3) -> Result<(), BackendError> {
        let s = self.vt().size();
        self.write_val(addr, v.x)?;
        self.write_val(addr + s, v.y)?;
        self.write_val(addr + 2 * s, v.z)
    }

    fn matrix_slot(&self, base: usize, i: usize) -> usize {
        let (row, col) = (i / 3, i % 3);
        base + (row * self.spec.rotation_matrix_row_stride + col) * self.vt().size()
    }

    fn read_rotation(&self, car: usize) -> Option<DQuat> {
        if let Some(off) = self.spec.orientation_quat {
            let s = self.vt().size();
            let a = car + off;
            let q = DQuat::from_xyzw(
                self.read_val(a)?,
                self.read_val(a + s)?,
                self.read_val(a + 2 * s)?,
                self.read_val(a + 3 * s)?,
            );
            return (q.is_finite() && (q.length() - 1.0).abs() < 0.05).then(|| q.normalize());
        }
        let base = car + self.spec.rotation_matrix?;
        let mut m = [0.0; 9];
        for (i, v) in m.iter_mut().enumerate() {
            *v = self.read_val(self.matrix_slot(base, i))?;
        }
        crate::math::quat_from_matrix(m, self.spec.rotation_matrix_layout == MatrixLayout::Rows)
    }

    fn write_rotation(&self, car: usize, q: DQuat) -> Result<(), BackendError> {
        if let Some(off) = self.spec.orientation_quat {
            let s = self.vt().size();
            let a = car + off;
            let q = q.normalize();
            for (i, v) in [q.x, q.y, q.z, q.w].iter().enumerate() {
                self.write_val(a + i * s, *v)?;
            }
            return Ok(());
        }
        let Some(off) = self.spec.rotation_matrix else {
            return Err(BackendError::Unsupported("no rotation field".into()));
        };
        let m = crate::math::quat_to_matrix(q, self.spec.rotation_matrix_layout == MatrixLayout::Rows);
        for (i, v) in m.iter().enumerate() {
            self.write_val(self.matrix_slot(car + off, i), *v)?;
        }
        Ok(())
    }

    fn read_blob(&self, car: usize) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        for [off, len] in &self.spec.blob_regions {
            out.extend(self.mem.read_vec(car + off, *len)?);
        }
        Some(out)
    }

    fn write_blob(&self, car: usize, blob: &[u8]) -> Result<(), BackendError> {
        let total: usize = self.spec.blob_regions.iter().map(|r| r[1]).sum();
        if blob.len() != total {
            return Err(BackendError::Unsupported("blob size mismatch".into()));
        }
        let mut at = 0;
        for [off, len] in &self.spec.blob_regions {
            if !self.mem.write(car + off, &blob[at..at + len]) {
                return Err(BackendError::Write(car + off));
            }
            at += len;
        }
        Ok(())
    }

    fn rpm_from_raw(&self, v: f64) -> f64 {
        if self.spec.engine_rpm_is_rad_s {
            v * 60.0 / std::f64::consts::TAU
        } else {
            v
        }
    }

    fn rpm_to_raw(&self, rpm: f64) -> f64 {
        if self.spec.engine_rpm_is_rad_s {
            rpm * std::f64::consts::TAU / 60.0
        } else {
            rpm
        }
    }

    fn refresh_chain(&mut self, pawn: Option<usize>) -> Option<usize> {
        let root = match self.spec.chain_root {
            ChainRoot::Module => Some(self.module_base),
            ChainRoot::Pawn => pawn,
        };
        root.and_then(|r| resolve_chain(&*self.mem, r, &self.spec.chain))
    }

    fn refresh_scan(&mut self) -> CarChange {
        let Some(tr) = self.tracker.as_mut() else {
            self.car = None;
            return CarChange::Lost;
        };
        let had_car = tr.car.is_some();
        let change = tr.refresh(&*self.mem, &self.hint);
        self.car = tr.car().map(|c| c.primary);
        if change == CarChange::Changed || (had_car && change == CarChange::Lost) {
            self.blobs.clear();
        }
        change
    }

    fn read_others(&self, tr: &BodyTracker) -> Option<Vec<BodyState>> {
        tr.read_states(&*self.mem, tr.car()?.others())
    }

    /// Writes every non-primary body from the body history at `t` (zero velocities for `Pose`).
    fn write_others(&self, tr: &BodyTracker, t: f64, mode: WriteMode) -> Result<(), BackendError> {
        let Some(car) = tr.car() else {
            return Err(BackendError::NoCar);
        };
        let l = tr.layout();
        if !tr.headers_ok(&*self.mem) {
            return Err(BackendError::NoCar);
        }
        let others: Vec<usize> = car.others().collect();
        let Some(states) = tr.history.sample(t).filter(|s| s.len() == others.len()) else {
            return Ok(());
        };
        for (addr, s) in others.iter().zip(&states) {
            let s = match mode {
                WriteMode::Pose => s.at_rest(),
                WriteMode::Full => *s,
            };
            write_body(&*self.mem, l, *addr, &s).map_err(BackendError::Write)?;
        }
        Ok(())
    }
}

fn rep_summary_with_bodies(summary: &str, car: &CarBodies) -> String {
    let list: Vec<String> = car.bodies.iter().map(|b| format!("{b:#x}")).collect();
    format!("{summary}; bodies [{}]", list.join(", "))
}

impl<M: ScanSource + Send + Sync + 'static> CarBackend for SimCarAdapter<M> {
    fn name(&self) -> &'static str {
        "sim_car"
    }

    fn refresh(&mut self) -> CarChange {
        let pawn = (self.pawn)();
        self.pawn_ptr = pawn;
        if self.spec.locator == SimCarLocator::VtableScan {
            return self.refresh_scan();
        }
        let car = self.refresh_chain(pawn);
        let change = match (self.car, car) {
            (_, None) => CarChange::Lost,
            (Some(a), Some(b)) if a == b => CarChange::Same,
            (None, Some(_)) | (Some(_), Some(_)) => CarChange::Changed,
        };
        if change == CarChange::Changed {
            self.blobs.clear();
        }
        self.car = car;
        change
    }

    fn read_state(&mut self) -> Option<CarSnapshot> {
        let car = self.car?;
        let sp = &self.spec;
        let inv = 1.0 / sp.length_scale;
        let pos = self.read_vec3(car + sp.position?)? * inv;
        let rot = self.read_rotation(car)?;
        let lin_vel = match sp.linear_velocity {
            Some(o) => self.read_vec3(car + o)? * inv,
            None => DVec3::ZERO,
        };
        let ang_vel = match sp.angular_velocity {
            Some(o) => self.read_vec3(car + o)?,
            None => DVec3::ZERO,
        };
        let mut snap = CarSnapshot { pos, rot, lin_vel, ang_vel, ..Default::default() };
        if sp.wheel_angular_speed.len() == 4 {
            for (w, off) in snap.wheels.iter_mut().zip(&sp.wheel_angular_speed) {
                w.angular_vel = self.read_val(car + off)?;
            }
        }
        if let Some(o) = sp.engine_rpm {
            snap.rpm = self.rpm_from_raw(self.read_val(car + o)?);
        }
        if let Some(o) = sp.gear {
            snap.gear = self.mem.read_i32(car + o)? - sp.gear_neutral_value;
        }
        if !snap.is_valid() {
            return None;
        }
        let others = match &self.tracker {
            Some(tr) => Some(self.read_others(tr)?),
            None => None,
        };
        if !self.spec.blob_regions.is_empty() {
            let blob = self.read_blob(car)?;
            self.blobs.stage(blob);
        }
        if let (Some(tr), Some(o)) = (self.tracker.as_mut(), others) {
            tr.history.stage(o);
        }
        Some(snap)
    }

    fn write_state(&mut self, snap: &CarSnapshot, mode: WriteMode) -> Result<(), BackendError> {
        let car = self.car.ok_or(BackendError::NoCar)?;
        if !snap.is_valid() {
            return Err(BackendError::Unsupported("refusing to write an invalid snapshot".into()));
        }
        if let Some(tr) = &self.tracker {
            let parent = tr.car().map_or(0, |c| c.parent);
            if !header_ok(&*self.mem, tr.layout(), car, tr.vtable(), parent) {
                return Err(BackendError::NoCar);
            }
        }
        let sp = self.spec.clone();
        if mode == WriteMode::Full && !sp.blob_regions.is_empty() {
            if let Some(blob) = self.blobs.at_or_before(snap.t).map(<[u8]>::to_vec) {
                self.write_blob(car, &blob)?;
            }
        }
        let scale = sp.length_scale;
        if let Some(o) = sp.position {
            self.write_vec3(car + o, snap.pos * scale)?;
        }
        self.write_rotation(car, snap.rot)?;
        let (lin, ang) = match mode {
            WriteMode::Full => (snap.lin_vel, snap.ang_vel),
            WriteMode::Pose => (DVec3::ZERO, DVec3::ZERO),
        };
        if let Some(o) = sp.linear_velocity {
            self.write_vec3(car + o, lin * scale)?;
        }
        if let Some(o) = sp.angular_velocity {
            self.write_vec3(car + o, ang)?;
        }
        if mode == WriteMode::Full {
            if sp.wheel_angular_speed.len() == 4 {
                for (w, off) in snap.wheels.iter().zip(&sp.wheel_angular_speed) {
                    self.write_val(car + off, w.angular_vel)?;
                }
            }
            if let Some(o) = sp.engine_rpm {
                self.write_val(car + o, self.rpm_to_raw(snap.rpm))?;
            }
            if let Some(o) = sp.gear {
                if !self.mem.write_i32(car + o, snap.gear + sp.gear_neutral_value) {
                    return Err(BackendError::Write(car + o));
                }
            }
        }
        if let Some(tr) = &self.tracker {
            self.write_others(tr, snap.t, mode)?;
        }
        if let Some(tr) = self.tracker.as_mut() {
            tr.note_write(Some(snap.pos));
        }
        Ok(())
    }

    fn freeze(&mut self, frozen: bool) -> Result<(), BackendError> {
        if let Some(tr) = self.tracker.as_mut() {
            tr.set_held(frozen);
        }
        self.freezer.set(self.pawn_ptr, frozen)
    }

    fn can_freeze(&self) -> bool {
        self.freezer.freezes()
    }

    fn on_recorded(&mut self, t: f64) {
        self.blobs.commit(t);
        if let Some(tr) = self.tracker.as_mut() {
            tr.history.commit(t);
        }
    }

    fn on_truncate(&mut self, t: Option<f64>) {
        match t {
            Some(t) => self.blobs.truncate_after(t),
            None => self.blobs.clear(),
        }
        if let Some(tr) = self.tracker.as_mut() {
            match t {
                Some(t) => tr.history.resume_at(t),
                None => tr.history.clear(),
            }
        }
    }

    fn take_body_tracker(&mut self) -> Option<BodyTracker> {
        self.car = None;
        self.blobs.clear();
        let mut tr = self.tracker.take()?;
        tr.history.clear();
        tr.set_held(false);
        Some(tr)
    }

    fn health(&self) -> crate::backend::BackendHealth {
        match &self.tracker {
            Some(tr) => tr.health(),
            None => crate::backend::BackendHealth { car_locked: self.car.is_some(), ..Default::default() },
        }
    }

    fn describe(&self) -> String {
        let car = self.car.map_or("?".into(), |c| format!("{c:#x}"));
        match (&self.spec.locator, &self.tracker) {
            (SimCarLocator::VtableScan, Some(tr)) => format!(
                "sim_car (vtable_scan, vtable {:#x} = module+{:#x}, {} bodies, {:?}, freeze: {}, main body at {car})",
                tr.vtable(),
                tr.vtable() - self.module_base,
                tr.car().map_or(0, |c| c.bodies.len()),
                self.spec.value_type,
                self.freezer.name(),
            ),
            (SimCarLocator::VtableScan, None) => "sim_car (vtable_scan not configured)".into(),
            _ => format!(
                "sim_car (root {:?}, {} chain steps, {:?}, freeze: {}, car at {car})",
                self.spec.chain_root,
                self.spec.chain.len(),
                self.spec.value_type,
                self.freezer.name(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bodies::tests::{put_body, state, world, POOL, SLOT, VT};
    use crate::freeze::NoFreeze;
    use crate::mem::SliceMemory;
    use std::sync::Mutex;

    const MODULE: usize = 0x1_4000_0000;
    const HEAP: usize = 0x2_0000_0000;

    fn no_hint() -> HintFn {
        Arc::new(|| None)
    }

    /// module+0x100 -> obj (heap); obj+0x20 -> car (heap+0x1000).
    fn setup(value_type: ValueType, rows: bool) -> (SimCarAdapter<SliceMemory>, usize) {
        let mem = SliceMemory::new(MODULE, vec![0; 0x1000]);
        mem.add_region(HEAP, vec![0; 0x4000]);
        let obj = HEAP + 0x100;
        let car = HEAP + 0x1000;
        assert!(mem.write(MODULE + 0x100, &(obj as u64).to_le_bytes()));
        assert!(mem.write(obj + 0x20, &(car as u64).to_le_bytes()));
        let spec = SimCarSpec {
            locator: SimCarLocator::Chain,
            chain_root: ChainRoot::Module,
            chain: vec![0x100, 0x20],
            value_type,
            length_scale: 1.0,
            position: Some(0x00),
            rotation_matrix: Some(0x40),
            rotation_matrix_layout: if rows { MatrixLayout::Rows } else { MatrixLayout::Columns },
            rotation_matrix_row_stride: 4,
            linear_velocity: Some(0x100),
            angular_velocity: Some(0x140),
            wheel_angular_speed: vec![0x200, 0x208, 0x210, 0x218],
            engine_rpm: Some(0x240),
            engine_rpm_is_rad_s: true,
            gear: Some(0x260),
            gear_neutral_value: 1,
            blob_regions: vec![[0x300, 0x10], [0x400, 0x8]],
            ..Default::default()
        };
        let a = SimCarAdapter::new(mem, spec, MODULE, Box::new(|| None), no_hint(), Box::new(NoFreeze), 100);
        (a, car)
    }

    fn sample(t: f64) -> CarSnapshot {
        let mut s = CarSnapshot {
            t,
            pos: DVec3::new(10.5, -3.0, 250.25),
            rot: DQuat::from_euler(glam::EulerRot::YXZ, 0.4, 0.05, -0.02),
            lin_vel: DVec3::new(20.0, 0.5, -1.0),
            ang_vel: DVec3::new(0.0, 0.3, 0.01),
            gear: 3,
            rpm: 5200.0,
            ..Default::default()
        };
        for (i, w) in s.wheels.iter_mut().enumerate() {
            w.angular_vel = 60.0 + i as f64;
        }
        s
    }

    #[test]
    fn chain_resolution() {
        let (a, car) = setup(ValueType::F64, true);
        assert_eq!(resolve_chain(a.mem(), MODULE, &[0x100, 0x20]), Some(car));
        assert_eq!(resolve_chain(a.mem(), MODULE, &[]), Some(MODULE));
        assert_eq!(resolve_chain(a.mem(), MODULE, &[0x108]), None, "null pointer");
        assert_eq!(resolve_chain(a.mem(), MODULE, &[0x5000]), None, "unmapped");
    }

    #[test]
    fn write_then_read_full_roundtrip() {
        for vt in [ValueType::F64, ValueType::F32] {
            for rows in [true, false] {
                let (mut a, car) = setup(vt, rows);
                assert_eq!(a.refresh(), CarChange::Changed);
                assert_eq!(a.car_address(), Some(car));
                assert_eq!(a.refresh(), CarChange::Same);
                let s = sample(0.0);
                a.write_state(&s, WriteMode::Full).unwrap();
                let r = a.read_state().unwrap();
                let tol = if vt == ValueType::F32 { 1e-3 } else { 1e-9 };
                assert!((r.pos - s.pos).length() < tol * 300.0, "{:?} vs {:?}", r.pos, s.pos);
                assert!(r.rot.angle_between(s.rot) < 1e-5);
                assert!((r.lin_vel - s.lin_vel).length() < tol * 30.0);
                assert!((r.ang_vel - s.ang_vel).length() < tol);
                assert_eq!(r.gear, 3);
                assert!((r.rpm - 5200.0).abs() < 0.1);
                assert!((r.wheels[2].angular_vel - 62.0).abs() < tol * 100.0);
                assert_eq!(a.mem().read_i32(car + 0x260), Some(4), "gear stored with neutral offset");
                let raw_rpm = a.read_val(car + 0x240).unwrap();
                assert!((raw_rpm - 5200.0 * std::f64::consts::TAU / 60.0).abs() < 0.01);
            }
        }
    }

    #[test]
    fn pose_write_zeroes_velocity_and_keeps_drivetrain() {
        let (mut a, car) = setup(ValueType::F64, true);
        a.refresh();
        a.write_state(&sample(0.0), WriteMode::Full).unwrap();
        let mut s2 = sample(1.0);
        s2.pos.x = 99.0;
        s2.gear = 1;
        a.write_state(&s2, WriteMode::Pose).unwrap();
        let r = a.read_state().unwrap();
        assert_eq!(r.pos.x, 99.0);
        assert_eq!(r.lin_vel, DVec3::ZERO);
        assert_eq!(r.ang_vel, DVec3::ZERO);
        assert_eq!(r.gear, 3, "pose writes leave the gearbox alone");
        assert_eq!(a.mem().read_i32(car + 0x260), Some(4));
    }

    #[test]
    fn blobs_restored_on_full_write() {
        let (mut a, car) = setup(ValueType::F64, true);
        a.refresh();
        a.write_state(&sample(0.0), WriteMode::Full).unwrap();
        assert!(a.mem().write(car + 0x300, &[1u8; 0x10]));
        assert!(a.mem().write(car + 0x400, &[2u8; 0x8]));
        a.read_state().unwrap();
        a.on_recorded(0.5);
        // Sim state changes afterwards...
        assert!(a.mem().write(car + 0x300, &[9u8; 0x10]));
        assert!(a.mem().write(car + 0x400, &[9u8; 0x8]));
        // ...and resuming at t=0.6 restores the blob captured at 0.5.
        a.write_state(&sample(0.6), WriteMode::Full).unwrap();
        assert_eq!(a.mem().bytes(car + 0x300, 0x10), vec![1u8; 0x10]);
        assert_eq!(a.mem().bytes(car + 0x400, 0x8), vec![2u8; 0x8]);
        // Pose writes never touch blobs.
        assert!(a.mem().write(car + 0x300, &[7u8; 0x10]));
        a.write_state(&sample(0.6), WriteMode::Pose).unwrap();
        assert_eq!(a.mem().bytes(car + 0x300, 0x10), vec![7u8; 0x10]);
        a.on_truncate(None);
        assert!(a.blobs.is_empty());
    }

    #[test]
    fn lost_car_and_invalid_data_fail_safe() {
        let (mut a, car) = setup(ValueType::F64, true);
        a.refresh();
        // Garbage rotation (all zeros) -> no snapshot.
        assert!(a.read_state().is_none());
        // Break the chain.
        assert!(a.mem().write(MODULE + 0x100, &0u64.to_le_bytes()));
        assert_eq!(a.refresh(), CarChange::Lost);
        assert!(a.read_state().is_none());
        assert_eq!(a.write_state(&sample(0.0), WriteMode::Full), Err(BackendError::NoCar));
        let mut bad = sample(0.0);
        bad.pos.x = f64::NAN;
        assert!(a.mem().write(MODULE + 0x100, &((HEAP + 0x100) as u64).to_le_bytes()));
        assert_eq!(a.refresh(), CarChange::Changed);
        assert!(a.write_state(&bad, WriteMode::Full).is_err());
        let _ = car;
    }

    #[test]
    fn pawn_rooted_chain_and_length_scale() {
        let mem = SliceMemory::new(HEAP, vec![0; 0x4000]);
        let pawn = HEAP + 0x10;
        let car = HEAP + 0x800;
        assert!(mem.write(pawn + 0x2F8, &(car as u64).to_le_bytes()));
        let spec = SimCarSpec {
            locator: SimCarLocator::Chain,
            chain_root: ChainRoot::Pawn,
            chain: vec![0x2F8],
            value_type: ValueType::F32,
            length_scale: 100.0,
            position: Some(0),
            orientation_quat: Some(0x10),
            ..Default::default()
        };
        let mut a = SimCarAdapter::new(mem, spec, 0, Box::new(move || Some(pawn)), no_hint(), Box::new(NoFreeze), 10);
        a.refresh();
        let s = CarSnapshot { pos: DVec3::new(1.0, 2.0, 3.0), rot: DQuat::from_rotation_y(0.5), ..Default::default() };
        a.write_state(&s, WriteMode::Full).unwrap();
        assert_eq!(a.mem().read_f32(car), Some(100.0), "stored in centimetres");
        let r = a.read_state().unwrap();
        assert!((r.pos - s.pos).length() < 1e-5);
        assert!(r.rot.angle_between(s.rot) < 1e-5);
        assert!(!a.can_freeze());
    }

    // ---- vtable_scan ----

    type SharedHint = Arc<Mutex<Option<DVec3>>>;

    fn scan_adapter(hint: SharedHint) -> SimCarAdapter<SliceMemory> {
        let mut spec = crate::Signatures::from_toml_str(include_str!("../../../config/signatures.toml"))
            .unwrap()
            .raw_offsets
            .sim_car;
        spec.rescan_interval_ms = 1;
        spec.hint_mismatch_s = 0.0;
        let module_base = VT - spec.vtable_rva.unwrap() as usize;
        let h: HintFn = Arc::new(move || *hint.lock().unwrap());
        SimCarAdapter::new(world(), spec, module_base, Box::new(|| None), h, Box::new(NoFreeze), 100)
    }

    /// Refreshes until the background scan delivers (or panics after a timeout).
    fn acquire(a: &mut SimCarAdapter<SliceMemory>) -> CarChange {
        let start = Instant::now();
        loop {
            match a.refresh() {
                CarChange::Lost => {
                    assert!(start.elapsed() < Duration::from_secs(10), "scan never found the car");
                    std::thread::sleep(Duration::from_millis(5));
                }
                c => return c,
            }
        }
    }

    fn obj(i: usize) -> usize {
        POOL + i * SLOT + 8
    }

    fn car1() -> SharedHint {
        Arc::new(Mutex::new(Some(DVec3::new(100.0, 12.0, -300.0))))
    }

    #[test]
    fn vtable_scan_finds_car_and_reads_primary() {
        let mut a = scan_adapter(car1());
        assert_eq!(a.refresh(), CarChange::Lost, "first tick never blocks on the scan");
        assert_eq!(acquire(&mut a), CarChange::Changed);
        assert_eq!(a.car_address(), Some(obj(5)));
        assert_eq!(a.car_bodies().unwrap().bodies.len(), 6);
        assert_eq!(a.refresh(), CarChange::Same);
        let s = a.read_state().unwrap();
        assert_eq!(s.pos, DVec3::new(100.0, 12.0, -300.0));
        assert_eq!(s.lin_vel, DVec3::new(20.0, 0.0, 1.0));
        assert!(s.rot.angle_between(DQuat::from_rotation_z(0.5)) < 1e-6);
        assert!(a.describe().contains("6 bodies"), "{}", a.describe());
    }

    #[test]
    fn vtable_scan_restores_all_bodies() {
        let mut a = scan_adapter(car1());
        acquire(&mut a);
        let l = BodyLayout::from_spec(&a.spec).unwrap();
        let parent = a.car_bodies().unwrap().parent;
        // Record two captures: the whole car moves +10 m in x between them.
        let s0 = a.read_state().unwrap();
        a.on_recorded(0.0);
        for i in [1usize, 3, 5, 6, 9, 12] {
            let mut b = read_body(a.mem(), &l, obj(i)).unwrap().state;
            b.pos.x += 10.0;
            put_body(a.mem(), obj(i), parent, &b);
        }
        let s1 = CarSnapshot { t: 0.1, ..a.read_state().unwrap() };
        a.on_recorded(0.1);
        // Scrub to t = 0.05 (what the controller would apply): every body sits at +5 m, at rest.
        let mid = CarSnapshot { t: 0.05, ..s0.interpolate(&s1, 0.5) };
        a.write_state(&mid, WriteMode::Pose).unwrap();
        for i in [1usize, 3, 5, 6, 9, 12] {
            let b = read_body(a.mem(), &l, obj(i)).unwrap().state;
            assert_eq!(b.lin_vel, DVec3::ZERO, "body {i} at rest while scrubbing");
            assert_eq!(b.ang_vel, DVec3::ZERO);
        }
        let x = |i: usize| read_body(a.mem(), &l, obj(i)).unwrap().state.pos.x;
        assert!((x(5) - 105.0).abs() < 1e-9);
        assert!((x(6) - 105.0).abs() < 1e-9, "chassis twin restored too");
        assert!((x(1) - (98.2 + 5.0)).abs() < 1e-9, "suspension part follows");
        // Release at t = 0.05: velocities from the snapshot / body history.
        a.write_state(&mid, WriteMode::Full).unwrap();
        a.on_truncate(Some(0.05));
        for i in [1usize, 5, 6, 12] {
            assert_eq!(read_body(a.mem(), &l, obj(i)).unwrap().state.lin_vel, DVec3::new(20.0, 0.0, 1.0));
        }
        // Car 2 untouched.
        assert_eq!(read_body(a.mem(), &l, obj(20)).unwrap().state.pos.x, 500.0);
        assert_eq!(a.refresh(), CarChange::Same, "own writes don't trip revalidation");
    }

    #[test]
    fn vtable_scan_drops_and_rescans_on_invalidation() {
        let hint = car1();
        let mut a = scan_adapter(hint.clone());
        acquire(&mut a);
        // Freed slot (vtable gone) -> Lost, then the rescan finds the car again.
        assert!(a.mem().write(obj(9), &0u64.to_le_bytes()));
        assert_eq!(a.refresh(), CarChange::Lost);
        assert_eq!(a.write_state(&CarSnapshot::default(), WriteMode::Pose), Err(BackendError::NoCar));
        assert_eq!(acquire(&mut a), CarChange::Changed);
        assert_eq!(a.car_bodies().unwrap().bodies.len(), 5);
        // Teleport (stage restart) -> jump detected.
        let l = BodyLayout::from_spec(&a.spec).unwrap();
        let parent = a.car_bodies().unwrap().parent;
        let mut b = read_body(a.mem(), &l, obj(5)).unwrap().state;
        b.pos.x += 500.0;
        put_body(a.mem(), obj(5), parent, &b);
        assert_eq!(a.refresh(), CarChange::Lost);
        put_body(a.mem(), obj(5), parent, &state(100.0, 0.5));
        assert_eq!(acquire(&mut a), CarChange::Changed);
        // shm shows the player somewhere else (stale bodies of an old stage) -> rescan picks car 2.
        *hint.lock().unwrap() = Some(DVec3::new(500.0, 12.0, -300.0));
        std::thread::sleep(HINT_CHECK_PERIOD);
        assert_eq!(a.refresh(), CarChange::Lost);
        assert_eq!(acquire(&mut a), CarChange::Changed);
        assert_eq!(a.car_address(), Some(obj(20)));
    }

    #[test]
    fn revalidation_tolerance_grows_with_speed() {
        let hint = car1();
        let mut a = scan_adapter(hint.clone());
        acquire(&mut a);
        // Main body moves at |(20, 0, 1)| m/s: 5 + 20.02 * 0.06 = 6.2 m. A lagging shm 5.5 m
        // behind is fine; 7 m is not.
        *hint.lock().unwrap() = Some(DVec3::new(94.5, 12.0, -300.0));
        std::thread::sleep(HINT_CHECK_PERIOD);
        assert_eq!(a.refresh(), CarChange::Same);
        *hint.lock().unwrap() = Some(DVec3::new(93.0, 12.0, -300.0));
        std::thread::sleep(HINT_CHECK_PERIOD);
        assert_eq!(a.refresh(), CarChange::Lost);
    }

    #[test]
    fn body_tracker_handover_keeps_the_locked_car() {
        let hint = car1();
        let mut a = scan_adapter(hint.clone());
        acquire(&mut a);
        a.read_state().unwrap();
        a.on_recorded(0.0);
        let h: HintFn = Arc::new(move || *hint.lock().unwrap());
        let tr = a.take_body_tracker().expect("tracker");
        assert!(tr.still_valid(a.mem(), &h));
        assert_eq!(tr.car().unwrap().bodies.len(), 6);
        assert!(tr.history.is_empty(), "the new owner records its own history");
        assert!(a.car_address().is_none() && a.take_body_tracker().is_none());
        assert_eq!(a.refresh(), CarChange::Lost);
        // A freed body makes the handed-over set invalid.
        assert!(a.mem().write(obj(9), &0u64.to_le_bytes()));
        assert!(!tr.still_valid(a.mem(), &h));
    }

    fn tracker(hint: SharedHint, pinning: bool) -> (BodyTracker, Arc<SliceMemory>, HintFn) {
        let mut spec = crate::Signatures::from_toml_str(include_str!("../../../config/signatures.toml"))
            .unwrap()
            .raw_offsets
            .sim_car;
        spec.rescan_interval_ms = 1;
        spec.hint_mismatch_s = 0.0;
        let module_base = VT - spec.vtable_rva.unwrap() as usize;
        let mem = Arc::new(world());
        let h: HintFn = Arc::new(move || *hint.lock().unwrap());
        let mut tr = BodyTracker::spawn(mem.clone(), &spec, module_base, h.clone(), 100).unwrap();
        tr.set_pinning(pinning);
        (tr, mem, h)
    }

    fn acquire_tr(tr: &mut BodyTracker, mem: &SliceMemory, h: &HintFn) {
        let start = Instant::now();
        while tr.refresh(mem, h) != CarChange::Changed {
            assert!(start.elapsed() < Duration::from_secs(10), "scan never found the car");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Refreshes for `ms` and asserts the car stays lost the whole time.
    fn stays_lost(tr: &mut BodyTracker, mem: &SliceMemory, h: &HintFn, ms: u64) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(ms) {
            assert_eq!(tr.refresh(mem, h), CarChange::Lost);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    const CAR1: [usize; 6] = [1, 3, 5, 6, 9, 12];

    /// Car 1 stopped (no velocity test) with a transient world body ("bush") 1 m from it, and
    /// the main body's state garbage for a moment (as after the crash into vegetation).
    fn stop_car_in_a_bush(mem: &SliceMemory, tr: &BodyTracker) -> usize {
        let l = *tr.layout();
        let parent = tr.car().unwrap().parent;
        for i in CAR1 {
            let s = read_body(mem, &l, obj(i)).unwrap().state;
            put_body(mem, obj(i), parent, &BodyState { lin_vel: DVec3::ZERO, ang_vel: DVec3::ZERO, ..s });
        }
        let bush = obj(16);
        put_body(mem, bush, parent, &BodyState { lin_vel: DVec3::ZERO, ang_vel: DVec3::ZERO, ..state(101.0, 0.0) });
        assert!(mem.write_f64(obj(5) + l.position, f64::NAN));
        bush
    }

    #[test]
    fn unpinned_rescan_of_a_stopped_car_adopts_a_nearby_world_body() {
        // The hazard the pin exists for (live crash: heap corruption after writes to freed
        // vegetation bodies adopted by a rescan).
        let (mut tr, mem, h) = tracker(car1(), false);
        acquire_tr(&mut tr, &mem, &h);
        let bush = stop_car_in_a_bush(&mem, &tr);
        assert_eq!(tr.refresh(&*mem, &h), CarChange::Lost);
        acquire_tr(&mut tr, &mem, &h);
        assert!(tr.car().unwrap().bodies.contains(&bush));
    }

    #[test]
    fn pinned_car_waits_for_its_own_bodies_and_never_adopts_world_bodies() {
        let (mut tr, mem, h) = tracker(car1(), true);
        acquire_tr(&mut tr, &mem, &h);
        let pinned = tr.pinned().expect("pinned at the first lock").clone();
        assert_eq!(pinned.bodies.len(), 6);
        let bush = stop_car_in_a_bush(&mem, &tr);
        assert_eq!(tr.refresh(&*mem, &h), CarChange::Lost, "main body invalid");
        // The twin is still valid, so a sweep would find "a car" (with the bush): no sweep.
        stays_lost(&mut tr, &mem, &h, 150);
        assert!(!tr.scanner.is_busy());
        // The main body recovers: the pinned set comes back without a sweep, bush excluded.
        let l = *tr.layout();
        assert!(mem.write_f64(obj(5) + l.position, 100.0));
        assert_eq!(tr.refresh(&*mem, &h), CarChange::Changed);
        let car = tr.car().unwrap();
        assert_eq!(car.bodies, pinned.bodies);
        assert!(!car.bodies.contains(&bush));
        assert_eq!(tr.refresh(&*mem, &h), CarChange::Same);
    }

    #[test]
    fn pinned_readoption_drops_bodies_that_left_the_car() {
        let (mut tr, mem, h) = tracker(car1(), true);
        acquire_tr(&mut tr, &mem, &h);
        let l = *tr.layout();
        let parent = tr.car().unwrap().parent;
        assert!(mem.write_f64(obj(5) + l.position, f64::NAN));
        assert_eq!(tr.refresh(&*mem, &h), CarChange::Lost);
        // A part broke off and flew 50 m away; another slot was freed.
        put_body(&mem, obj(12), parent, &state(150.0, 0.5));
        assert!(mem.write(obj(9), &0u64.to_le_bytes()));
        assert!(mem.write_f64(obj(5) + l.position, 100.0));
        assert_eq!(tr.refresh(&*mem, &h), CarChange::Changed);
        assert_eq!(tr.car().unwrap().bodies, vec![obj(1), obj(3), obj(5), obj(6)]);
    }

    #[test]
    fn stale_pin_is_released_when_the_main_body_is_freed() {
        let (mut tr, mem, h) = tracker(car1(), true);
        acquire_tr(&mut tr, &mem, &h);
        assert_eq!(tr.pinned().unwrap().primary, obj(5));
        assert!(mem.write(obj(5), &0u64.to_le_bytes()));
        assert_eq!(tr.refresh(&*mem, &h), CarChange::Lost);
        acquire_tr(&mut tr, &mem, &h);
        assert_eq!(tr.car().unwrap().primary, obj(6), "the twin");
        assert_eq!(tr.pinned().unwrap().primary, obj(6), "re-pinned");
        tr.unpin();
        assert!(tr.pinned().is_none());
    }

    /// Records one history capture at `t` in car_avatar order (main body first).
    fn record(tr: &mut BodyTracker, mem: &SliceMemory, t: f64) {
        let car = tr.car().unwrap().clone();
        let addrs = std::iter::once(car.primary).chain(car.others());
        let states = tr.read_states(mem, addrs).expect("bodies readable");
        tr.history.stage(states);
        tr.history.commit(t);
    }

    #[test]
    fn slightly_sheared_main_body_of_a_damaged_car_stays_locked() {
        let (mut tr, mem, h) = tracker(car1(), true);
        acquire_tr(&mut tr, &mem, &h);
        let l = *tr.layout();
        let a = obj(5) + l.rotation + 8; // row 0, column 1
        let v = mem.read_f64(a).unwrap();
        assert!(mem.write_f64(a, v + 0.03), "live: 1.5 % shear after a crash");
        assert!(read_body(&*mem, &l, obj(5)).is_none(), "too sheared for a sweep");
        assert_eq!(tr.refresh(&*mem, &h), CarChange::Same, "but fine for the locked car");
        assert!(tr.read_states(&*mem, [obj(5)].into_iter()).is_some(), "and still recorded");
    }

    #[test]
    fn readopting_the_pinned_bodies_keeps_their_history() {
        let (mut tr, mem, h) = tracker(car1(), true);
        tr.set_history_primary_first(true);
        acquire_tr(&mut tr, &mem, &h);
        record(&mut tr, &mem, 0.0);
        let l = *tr.layout();
        assert!(mem.write_f64(obj(5) + l.position, f64::NAN));
        assert_eq!(tr.refresh(&*mem, &h), CarChange::Lost);
        assert!(tr.history.sample(0.0).is_some(), "kept while waiting for the pinned bodies");
        assert!(mem.write_f64(obj(5) + l.position, 100.0));
        assert_eq!(tr.refresh(&*mem, &h), CarChange::Changed);
        let s = tr.history.sample(0.0).expect("history survives the re-adoption");
        assert_eq!(s.len(), 6);
        assert_eq!((s[0].pos.x, s[1].pos.x), (100.0, 98.2), "main body first, then the others");
    }

    #[test]
    fn a_part_coming_off_is_dropped_from_the_car_pin_and_history() {
        let (mut tr, mem, h) = tracker(car1(), true);
        tr.set_history_primary_first(true);
        acquire_tr(&mut tr, &mem, &h);
        record(&mut tr, &mem, 0.0);
        let l = *tr.layout();
        let parent = tr.car().unwrap().parent;
        // The "hood" (slot 12, 1.8 m from the main body) moves 1.5 m away from the car, still
        // inside car_radius_m and with the car's velocity.
        let hood = read_body(&*mem, &l, obj(12)).unwrap().state;
        put_body(&mem, obj(12), parent, &BodyState { pos: hood.pos + DVec3::new(1.5, 0.0, 0.0), ..hood });
        assert_eq!(tr.refresh(&*mem, &h), CarChange::Same);
        let car = tr.car().unwrap();
        assert_eq!(car.bodies, vec![obj(1), obj(3), obj(5), obj(6), obj(9)]);
        assert!(!tr.pinned().unwrap().bodies.contains(&obj(12)));
        let s = tr.history.sample(0.0).expect("history kept for the remaining bodies");
        assert_eq!(s.len(), 5);
        assert_eq!(s[0].pos.x, 100.0, "main body column first");
        assert_eq!(s[1].pos.x, 98.2, "then the others in address order");
        // Suspension-sized movement is not a detach.
        let wheel = read_body(&*mem, &l, obj(1)).unwrap().state;
        put_body(&mem, obj(1), parent, &BodyState { pos: wheel.pos + DVec3::new(0.0, 0.3, 0.0), ..wheel });
        std::thread::sleep(HINT_CHECK_PERIOD);
        assert_eq!(tr.refresh(&*mem, &h), CarChange::Same);
        assert_eq!(tr.car().unwrap().bodies.len(), 5);
    }

    #[test]
    fn history_outside_its_recorded_range_is_not_sampled() {
        let (mut tr, mem, h) = tracker(car1(), true);
        tr.set_history_primary_first(true);
        acquire_tr(&mut tr, &mem, &h);
        record(&mut tr, &mem, 14.0);
        assert!(tr.history.sample(14.0).is_some());
        assert!(tr.history.sample(0.0).is_none(), "a resume 14 s before the history starts");
    }

    #[test]
    fn held_tracker_neither_sweeps_nor_adopts() {
        let (mut tr, mem, h) = tracker(car1(), false);
        acquire_tr(&mut tr, &mem, &h);
        tr.set_held(true);
        assert!(mem.write(obj(9), &0u64.to_le_bytes()));
        assert_eq!(tr.refresh(&*mem, &h), CarChange::Lost);
        stays_lost(&mut tr, &mem, &h, 150);
        assert!(!tr.scanner.is_busy());
        tr.set_held(false);
        acquire_tr(&mut tr, &mem, &h);
        assert_eq!(tr.car().unwrap().bodies.len(), 5);
    }

    #[test]
    fn vtable_scan_without_hint_stays_lost() {
        let hint: SharedHint = Arc::new(Mutex::new(None));
        let mut a = scan_adapter(hint);
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(200) {
            assert_eq!(a.refresh(), CarChange::Lost);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(a.read_state().is_none());
    }
}
