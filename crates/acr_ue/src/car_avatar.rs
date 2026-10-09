//! Default backend: the dmphysics `CarAvatar` pawn driven through its own UFunctions
//! (re-notes §9). Every call goes through ProcessEvent and therefore only runs on the game
//! thread (the tick hook); [`Params::call`] refuses anything else. The rigid bodies found by
//! the `vtable_scan` locator ([`BodyTracker`], background thread, plain memory reads only)
//! add what the UFunctions can't restore: angular velocity, and optionally every body's state.
//!
//! Snapshot convention: UE world space, lengths in metres (cm / 100), rotation = actor
//! quaternion, velocity = `GetVelocityCMS / 100`. Restores write back exactly what was read, so
//! no axis mapping is involved in a write; the UE -> sim axis map ([`AxisMap`], configured or
//! auto-detected) only feeds the validate log line and the UE-space angular velocity.

use crate::backend::{BackendError, CarBackend, CarChange, WriteMode};
use crate::bodies::{
    apply_body_states, rigid_point_velocity, write_body_velocities, BodyState, BodyWriteScope, HintFn,
};
use crate::freeze::Freezer;
use crate::game::{Game, PawnLocator};
use crate::math::{cm_to_m, m_to_cm, quat_to_rotator, rotator_to_quat, AxisFit, AxisMap, Rotator};
use crate::mem::Memory;
use crate::reflection::FunctionInfo;
use crate::sigs::{BodyWrites, CarAvatarSpec};
use crate::sim_car::BodyTracker;
use crate::ue_call::{CallError, Params, FTRANSFORM_SIZE};
use glam::{DQuat, DVec3};
use rewind_core::CarSnapshot;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `[reflection.functions]` keys the backend can't work without.
pub const CORE_FUNCTIONS: &[&str] = &[
    "k2_get_actor_location",
    "k2_get_actor_rotation",
    "car_set_physics_transform",
    "car_set_velocity_cms",
    "car_get_velocity_cms",
];

/// `CarAvatar::GetGear()` text -> shm-style gear (-1 R, 0 N, 1.. forward).
pub fn parse_gear(s: &str) -> Option<i32> {
    match s.trim() {
        "R" | "r" => Some(-1),
        "N" | "n" => Some(0),
        t => t.parse::<i32>().ok().filter(|g| (1..=12).contains(g)),
    }
}

/// Builds the snapshot from the raw UFunction results (UE units).
pub fn snapshot_from_ue(
    loc_cm: DVec3,
    rot: Rotator,
    vel_cms: DVec3,
    rpm: f64,
    gear: i32,
    ang_vel_ue: DVec3,
) -> CarSnapshot {
    CarSnapshot {
        pos: cm_to_m(loc_cm),
        rot: rotator_to_quat(rot),
        lin_vel: cm_to_m(vel_cms),
        ang_vel: ang_vel_ue,
        rpm,
        gear,
        ..Default::default()
    }
}

/// Inputs of one `car_avatar validate:` line.
#[derive(Clone, Debug, Default)]
pub struct Diag {
    pub actor_cm: DVec3,
    pub rot: DQuat,
    pub vel_ms: DVec3,
    pub rpm: f64,
    pub gear: i32,
    pub respawn_seq: Option<i32>,
    pub axes: Option<AxisMap>,
    pub axes_note: String,
    /// Main sim body (sim frame, metres).
    pub body: Option<BodyState>,
    /// shm graphics carCoordinates (sim frame, metres).
    pub shm: Option<DVec3>,
    /// `(match_tolerance_m, shm_lag_s)`: flags shm residuals above the speed-aware tolerance.
    pub shm_tol: Option<(f64, f64)>,
}

fn shm_residual(what: &str, d: f64, speed: f64, tol: Option<(f64, f64)>) -> String {
    match tol {
        None => format!(" [{what} {d:.3} m]"),
        Some((base, lag)) => {
            let t = crate::bodies::shm_tolerance(base, lag, speed);
            format!(" [{what} {d:.3} m, tol {t:.2} m{}]", if d > t { " MISMATCH" } else { "" })
        }
    }
}

pub fn diag_line(d: &Diag) -> String {
    let r = quat_to_rotator(d.rot);
    let mut s = format!(
        "car_avatar validate: actor UE ({:.1}, {:.1}, {:.1}) cm rot (p {:.1}, y {:.1}, r {:.1}) deg | {:.2} m/s rpm {:.0} gear {} respawn_seq {}",
        d.actor_cm.x,
        d.actor_cm.y,
        d.actor_cm.z,
        r.pitch,
        r.yaw,
        r.roll,
        d.vel_ms.length(),
        d.rpm,
        d.gear,
        d.respawn_seq.map_or("?".into(), |v| v.to_string()),
    );
    let ue_m = cm_to_m(d.actor_cm);
    let mapped = d.axes.map(|m| m.ue_to_sim(ue_m));
    match (d.axes, mapped) {
        (Some(m), Some(p)) => {
            s += &format!(" | axes {} ({}) -> sim ({:.3}, {:.3}, {:.3}) m", m.describe(), d.axes_note, p.x, p.y, p.z)
        }
        _ => s += &format!(" | axes unknown ({})", d.axes_note),
    }
    match &d.body {
        Some(b) => {
            s += &format!(" | main body ({:.3}, {:.3}, {:.3}) m", b.pos.x, b.pos.y, b.pos.z);
            if let (Some(m), Some(p)) = (d.axes, mapped) {
                let rot_err = m.ue_to_sim_rot(d.rot).angle_between(b.rot).to_degrees();
                let vel_err = (m.ue_to_sim(d.vel_ms) - b.lin_vel).length();
                s += &format!(
                    " [actor-body {:.3} m, rot {:.2} deg, vel {:.2} m/s]",
                    (p - b.pos).length(),
                    rot_err,
                    vel_err
                );
            }
        }
        None => s += " | main body n/a",
    }
    match d.shm {
        Some(h) => {
            s += &format!(" | shm ({:.3}, {:.3}, {:.3}) m", h.x, h.y, h.z);
            if let Some(p) = mapped {
                s += &shm_residual("actor-shm", (p - h).length(), d.vel_ms.length(), d.shm_tol);
            }
            if let Some(b) = &d.body {
                s += &shm_residual("body-shm", (b.pos - h).length(), b.lin_vel.length(), d.shm_tol);
            }
        }
        None => s += " | shm n/a",
    }
    s
}

/// Speed (m/s), slip angle (deg, + = moving towards the car's right) and yaw rate (deg/s) of
/// a UE-frame state (X forward, Y right, Z up).
pub fn motion(rot: DQuat, vel: DVec3, ang_vel: DVec3) -> (f64, f64, f64) {
    let local = rot.inverse() * vel;
    let slip = if local.truncate().length() < 0.5 { 0.0 } else { local.y.atan2(local.x).to_degrees() };
    (vel.length(), slip, ang_vel.z.to_degrees())
}

/// Fastest-spinning non-main body (rad/s): the wheels, if they are among the bodies.
fn max_spin(states: &[BodyState]) -> f64 {
    states.iter().skip(1).map(|s| s.ang_vel.length()).fold(0.0, f64::max)
}

/// The car after a release, compared with what was released.
#[derive(Clone, Copy, Debug)]
struct ResumeWatch {
    target: CarSnapshot,
    target_spin: f64,
    /// rpm when the run-in started and at the release.
    rpm_first: f64,
    rpm_last: f64,
    ticks: u32,
}

fn resume_line(w: &ResumeWatch, now: &CarSnapshot, spin: f64) -> String {
    let (ts, tslip, tyaw) = motion(w.target.rot, w.target.lin_vel, w.target.ang_vel);
    let (s, slip, yaw) = motion(now.rot, now.lin_vel, now.ang_vel);
    let mut line = format!(
        "resume check +{} ticks: speed {s:.1} (target {ts:.1}) m/s, slip {slip:+.1} ({tslip:+.1}) deg, \
         yaw rate {yaw:+.0} ({tyaw:+.0}) deg/s, rpm {:.0} ({:.0}), max body spin {spin:.0} ({:.0}) rad/s",
        w.ticks, now.rpm, w.target.rpm, w.target_spin
    );
    if w.ticks == 1 {
        line += &format!(" | run-in rpm {:.0} -> {:.0}", w.rpm_first, w.rpm_last);
    }
    line
}

pub struct CarAvatarAdapter {
    game: Arc<Game>,
    spec: CarAvatarSpec,
    car_class: usize,
    locator: PawnLocator,
    pawn: Option<usize>,
    respawn_seq: Option<i32>,
    /// The last pawn seen; the pinned body set belongs to it.
    pin_pawn: Option<usize>,
    bodies: Option<BodyTracker>,
    hint: HintFn,
    axes: Option<AxisMap>,
    axes_configured: bool,
    fit: AxisFit,
    freezer: Box<dyn Freezer>,
    gear: i32,
    gear_polled: Option<Instant>,
    last_diag: Option<Instant>,
    warned_class: bool,
    warned_bodies: bool,
    resume_watch: Option<ResumeWatch>,
}

impl CarAvatarAdapter {
    /// Cheap check of what [`Self::new`] needs from reflection (no threads spawned).
    pub fn check_requirements(game: &Game, spec: &CarAvatarSpec) -> Result<(), String> {
        let missing: Vec<&str> = CORE_FUNCTIONS
            .iter()
            .copied()
            .chain(spec.required_functions.iter().map(String::as_str))
            .filter(|k| game.func(k).is_none())
            .collect();
        if !missing.is_empty() {
            return Err(format!("UFunctions not resolved: {missing:?}"));
        }
        game.class(&spec.pawn_class).ok_or_else(|| format!("class {} not found", spec.pawn_class))?;
        spec.ue_to_sim_axes.parse().map(|_| ())
    }

    /// Fails (with the reason) if the pawn class or a required UFunction is missing.
    pub fn new(
        game: Arc<Game>,
        spec: CarAvatarSpec,
        bodies: Option<BodyTracker>,
        hint: HintFn,
        freezer: Box<dyn Freezer>,
    ) -> Result<Self, String> {
        Self::check_requirements(&game, &spec)?;
        let car_class = game.class(&spec.pawn_class).ok_or_else(|| format!("class {} not found", spec.pawn_class))?;
        let axes = spec.ue_to_sim_axes.parse()?;
        let mut bodies = bodies;
        if let Some(tr) = bodies.as_mut() {
            tr.set_pinning(true);
            tr.set_history_primary_first(true);
        }
        Ok(Self {
            locator: PawnLocator::new(game.clone()),
            fit: AxisFit::new(spec.axes_fit_tolerance_m),
            axes_configured: axes.is_some(),
            axes,
            game,
            spec,
            car_class,
            pawn: None,
            respawn_seq: None,
            pin_pawn: None,
            bodies,
            hint,
            freezer,
            gear: 0,
            gear_polled: None,
            last_diag: None,
            warned_class: false,
            warned_bodies: false,
            resume_watch: None,
        })
    }

    /// Every full write may be the release: (re)arms the post-release log.
    fn arm_resume_watch(&mut self, pawn: usize, snap: &CarSnapshot, mode: WriteMode) {
        if mode == WriteMode::Pose {
            self.resume_watch = None;
            return;
        }
        let rpm = self.get_f32("car_get_rpms", pawn).map_or(0.0, f64::from);
        let rpm_first = match &self.resume_watch {
            Some(w) if w.ticks == 0 => w.rpm_first,
            _ => rpm,
        };
        let target_spin = self.bodies.as_ref().and_then(|tr| tr.history.sample(snap.t)).map_or(0.0, |s| max_spin(&s));
        self.resume_watch = Some(ResumeWatch { target: *snap, target_spin, rpm_first, rpm_last: rpm, ticks: 0 });
    }

    fn log_resume_watch(&mut self, snap: &CarSnapshot, bodies: Option<&[BodyState]>) {
        let Some(w) = self.resume_watch.as_mut() else {
            return;
        };
        w.ticks += 1;
        if matches!(w.ticks, 1 | 5 | 20) {
            // GetVelocityCMS lags a tick or more behind a write; the main body doesn't.
            let lin_vel = match (self.axes, bodies.and_then(|b| b.first())) {
                (Some(m), Some(b)) => m.sim_to_ue(b.lin_vel),
                _ => snap.lin_vel,
            };
            let now = CarSnapshot { lin_vel, ..*snap };
            tracing::info!("{}", resume_line(w, &now, bodies.map_or(0.0, max_spin)));
        }
        if w.ticks >= 20 {
            self.resume_watch = None;
        }
    }

    fn mem(&self) -> &dyn Memory {
        &self.game.ue.mem
    }

    fn func(&self, key: &str) -> Result<&FunctionInfo, BackendError> {
        self.game.func(key).ok_or_else(|| BackendError::Unsupported(format!("{key} not resolved")))
    }

    fn call<'a>(
        &'a self,
        key: &str,
        obj: usize,
        fill: impl FnOnce(&mut Params<'a>) -> Result<(), CallError>,
    ) -> Result<Params<'a>, BackendError> {
        let err = |e: CallError| BackendError::Call(format!("{key}: {e}"));
        let mut p = Params::new(self.func(key)?);
        fill(&mut p).map_err(err)?;
        p.call(obj).map_err(err)?;
        Ok(p)
    }

    fn get_vec(&self, key: &str, obj: usize) -> Option<DVec3> {
        self.call(key, obj, |_| Ok(())).ok()?.get_vec("ReturnValue").ok().filter(|v| v.is_finite())
    }

    fn get_f32(&self, key: &str, obj: usize) -> Option<f32> {
        self.game.func(key)?;
        self.call(key, obj, |_| Ok(())).ok()?.get_f32("ReturnValue").ok().filter(|v| v.is_finite())
    }

    fn respawn_seq_of(&self, pawn: usize) -> Option<i32> {
        self.game.func("car_get_respawn_seq")?;
        self.call("car_get_respawn_seq", pawn, |_| Ok(())).ok()?.get_i32("ReturnValue").ok()
    }

    fn poll_gear(&mut self, pawn: usize) -> i32 {
        let due = self.gear_polled.is_none_or(|t| t.elapsed() >= Duration::from_millis(self.spec.gear_poll_ms));
        if due && self.game.func("car_get_gear").is_some() {
            self.gear_polled = Some(Instant::now());
            let text = self
                .call("car_get_gear", pawn, |_| Ok(()))
                .ok()
                .and_then(|p| p.get_fstring("ReturnValue", self.mem()).ok());
            if let Some(g) = text.as_deref().and_then(parse_gear) {
                self.gear = g;
            }
        }
        self.gear
    }

    /// Body addresses in history order: main body first.
    fn body_addrs(&self) -> Option<Vec<usize>> {
        let car = self.bodies.as_ref()?.car()?;
        Some(std::iter::once(car.primary).chain(car.others()).collect())
    }

    fn read_bodies(&self) -> Option<Vec<BodyState>> {
        let tr = self.bodies.as_ref()?;
        tr.read_states(self.mem(), self.body_addrs()?.into_iter())
    }

    fn update_axes(&mut self, actor_m: DVec3, body: Option<&BodyState>) {
        if self.axes_configured || self.axes.is_some() {
            return;
        }
        let Some(b) = body else {
            return;
        };
        self.fit.add(actor_m, b.pos);
        if let Some((m, err)) = self.fit.resolved() {
            tracing::info!(
                "car_avatar: UE -> sim axes detected after {} samples: {} (max error {err:.3} m); \
                 pin it with [backend.car_avatar] ue_to_sim_axes = {}",
                self.fit.samples(),
                m.describe(),
                m.to_toml()
            );
            self.axes = Some(m);
        }
    }

    fn axes_note(&self) -> String {
        if self.axes_configured {
            return "configured".into();
        }
        match (self.axes, self.fit.best()) {
            (Some(_), _) => format!("auto, {} samples", self.fit.samples()),
            (None, Some((m, e))) => {
                format!("auto: unresolved after {} samples, best {} err {e:.2} m", self.fit.samples(), m.describe())
            }
            (None, None) => "auto: no main body yet".into(),
        }
    }

    fn maybe_diag(&mut self, actor_cm: DVec3, snap: &CarSnapshot, body: Option<BodyState>) {
        if self.spec.diag_interval_ms == 0
            || self.last_diag.is_some_and(|t| t.elapsed() < Duration::from_millis(self.spec.diag_interval_ms))
        {
            return;
        }
        self.last_diag = Some(Instant::now());
        let d = Diag {
            actor_cm,
            rot: snap.rot,
            vel_ms: snap.lin_vel,
            rpm: snap.rpm,
            gear: snap.gear,
            respawn_seq: self.respawn_seq,
            axes: self.axes,
            axes_note: self.axes_note(),
            body,
            shm: (self.hint)(),
            shm_tol: self.bodies.as_ref().map(|tr| (tr.select().tolerance_m, tr.select().shm_lag_s)),
        };
        tracing::info!("{}", diag_line(&d));
    }

    /// Release without a body history for the snapshot: every body gets the velocity of its
    /// point on the car moving rigidly at the snapshot's velocities (sim frame), instead of
    /// keeping whatever it has (zero after scrubbing, which drags the car to a stop).
    fn rigid_body_velocities(&self, snap: &CarSnapshot, addrs: &[usize]) -> Option<Result<(), usize>> {
        let axes = self.axes?;
        let tr = self.bodies.as_ref()?;
        let mem = &self.game.ue.mem;
        let layout = *tr.layout();
        let positions = tr.read_states(mem, addrs.iter().copied())?;
        let origin = positions.first()?.pos;
        let v = axes.ue_to_sim(snap.lin_vel);
        let w = axes.ue_to_sim_ang_vel(snap.ang_vel);
        Some(addrs.iter().zip(&positions).try_for_each(|(&a, s)| {
            write_body_velocities(mem, &layout, a, rigid_point_velocity(v, w, origin, s.pos), w)
        }))
    }

    fn write_bodies(&mut self, snap: &CarSnapshot, mode: WriteMode) -> Result<(), BackendError> {
        let t = snap.t;
        let scope = match self.spec.body_writes {
            BodyWrites::None => return Ok(()),
            BodyWrites::Velocities => BodyWriteScope::Velocities,
            BodyWrites::Full => BodyWriteScope::Full,
        };
        let (Some(addrs), Some(tr)) = (self.body_addrs(), self.bodies.as_ref()) else {
            return Ok(());
        };
        let mem = &self.game.ue.mem;
        if !tr.headers_ok(mem) {
            return Ok(());
        }
        let layout = *tr.layout();
        let recorded = tr.history.sample(t).filter(|s| s.len() == addrs.len());
        let at_rest = mode == WriteMode::Pose;
        let r = match (recorded, scope) {
            (Some(states), _) => apply_body_states(mem, &layout, &addrs, &states, scope, at_rest),
            // No recording for t (bodies found later): scrubbing still zeroes every body.
            (None, BodyWriteScope::Velocities) if at_rest => {
                addrs.iter().try_for_each(|&a| write_body_velocities(mem, &layout, a, DVec3::ZERO, DVec3::ZERO))
            }
            (None, _) if !at_rest => match self.rigid_body_velocities(snap, &addrs) {
                Some(r) => {
                    tracing::info!(
                        "car_avatar: no body history at t={t:.2}; {} bodies released with the car's rigid-body velocity",
                        addrs.len()
                    );
                    r
                }
                None => {
                    tracing::info!(
                        "car_avatar: no body history at t={t:.2} and bodies unreadable; only the UE transform/velocity is restored"
                    );
                    Ok(())
                }
            },
            (None, _) => {
                if !self.warned_bodies {
                    self.warned_bodies = true;
                    tracing::info!("car_avatar: no body history at t={t:.2}; bodies left alone while scrubbing");
                }
                Ok(())
            }
        };
        r.map_err(BackendError::Write)?;
        if let Some(tr) = self.bodies.as_mut() {
            tr.note_write(None);
        }
        Ok(())
    }
}

/// The transform / bool parameters of SetPhysicsTransform, by name with a layout fallback.
fn transform_param(info: &FunctionInfo) -> Option<&str> {
    info.param("InTransform")
        .or_else(|| info.params.iter().find(|p| p.size == FTRANSFORM_SIZE))
        .map(|p| p.name.as_str())
}

impl CarBackend for CarAvatarAdapter {
    fn name(&self) -> &'static str {
        "car_avatar"
    }

    fn refresh(&mut self) -> CarChange {
        let found = self.locator.locate();
        let pawn = found.filter(|&p| self.game.ue.is_a(p, self.car_class));
        if found.is_some() && pawn.is_none() && !self.warned_class {
            self.warned_class = true;
            tracing::warn!(
                "car_avatar: player pawn {:?} is not a {}; backend idle",
                found.and_then(|p| self.game.ue.class_name(p)),
                self.spec.pawn_class
            );
        }
        let mut change = match (self.pawn, pawn) {
            (_, None) => CarChange::Lost,
            (Some(a), Some(b)) if a == b => CarChange::Same,
            _ => CarChange::Changed,
        };
        if let Some(p) = pawn {
            let seq = self.respawn_seq_of(p);
            if change == CarChange::Same && seq.is_some() && self.respawn_seq.is_some() && seq != self.respawn_seq {
                tracing::info!("car_avatar: respawn ({:?} -> {:?}); history reset", self.respawn_seq, seq);
                change = CarChange::Changed;
            }
            self.respawn_seq = seq;
            let respawned = change == CarChange::Changed && self.pawn == Some(p);
            if self.pin_pawn.is_some_and(|q| q != p) || respawned {
                if let Some(tr) = self.bodies.as_mut() {
                    tr.unpin();
                }
            }
            self.pin_pawn = Some(p);
        }
        if change == CarChange::Changed {
            if let Some(p) = pawn {
                tracing::info!(
                    "car_avatar: player car {p:#x} class {:?} respawn_seq {:?}",
                    self.game.ue.class_hierarchy(self.game.ue.class_of(p).unwrap_or(0)),
                    self.respawn_seq
                );
            }
            if !self.axes_configured {
                self.axes = None;
                self.fit = AxisFit::new(self.spec.axes_fit_tolerance_m);
            }
        }
        let hint = self.hint.clone();
        if let Some(tr) = self.bodies.as_mut() {
            tr.refresh(&self.game.ue.mem, &hint);
        }
        self.pawn = pawn;
        change
    }

    fn read_state(&mut self) -> Option<CarSnapshot> {
        let pawn = self.pawn?;
        let loc_cm = self.get_vec("k2_get_actor_location", pawn)?;
        let r = self.get_vec("k2_get_actor_rotation", pawn)?;
        let rot = Rotator { pitch: r.x, yaw: r.y, roll: r.z };
        let vel_cms = self.get_vec("car_get_velocity_cms", pawn)?;
        let rpm = self.get_f32("car_get_rpms", pawn).map_or(0.0, f64::from);
        let gear = self.poll_gear(pawn);
        let bodies = self.read_bodies();
        let main = bodies.as_ref().and_then(|b| b.first().copied());
        self.update_axes(cm_to_m(loc_cm), main.as_ref());
        let ang = match (self.axes, main) {
            (Some(m), Some(b)) => m.sim_to_ue_ang_vel(b.ang_vel),
            _ => DVec3::ZERO,
        };
        let snap = snapshot_from_ue(loc_cm, rot, vel_cms, rpm, gear, ang);
        if !snap.is_valid() {
            return None;
        }
        self.log_resume_watch(&snap, bodies.as_deref());
        if let (Some(tr), Some(b)) = (self.bodies.as_mut(), bodies) {
            tr.history.stage(b);
        }
        self.maybe_diag(loc_cm, &snap, main);
        Some(snap)
    }

    fn write_state(&mut self, snap: &CarSnapshot, mode: WriteMode) -> Result<(), BackendError> {
        let pawn = self.pawn.ok_or(BackendError::NoCar)?;
        if !snap.is_valid() {
            return Err(BackendError::Unsupported("refusing to write an invalid snapshot".into()));
        }
        let info = self.func("car_set_physics_transform")?;
        let tparam = transform_param(info)
            .ok_or_else(|| BackendError::Unsupported("SetPhysicsTransform has no FTransform parameter".into()))?
            .to_owned();
        let reset = self.spec.reset_car;
        self.call("car_set_physics_transform", pawn, |p| {
            p.set_transform(&tparam, snap.rot, m_to_cm(snap.pos), DVec3::ONE)?;
            if p.has("bResetCar") {
                p.set_bool("bResetCar", reset)?;
            }
            Ok(())
        })?;
        let vel = match mode {
            WriteMode::Full => m_to_cm(snap.lin_vel),
            WriteMode::Pose => DVec3::ZERO,
        };
        let vinfo = self.func("car_set_velocity_cms")?;
        let vparam = vinfo
            .param("InVelocityCMS")
            .or_else(|| vinfo.params.first())
            .map(|p| p.name.clone())
            .ok_or_else(|| BackendError::Unsupported("SetVelocityCMS has no parameter".into()))?;
        self.call("car_set_velocity_cms", pawn, |p| p.set_vec(&vparam, vel))?;
        self.arm_resume_watch(pawn, snap, mode);
        self.write_bodies(snap, mode)
    }

    fn freeze(&mut self, frozen: bool) -> Result<(), BackendError> {
        if let Some(tr) = self.bodies.as_mut() {
            tr.set_held(frozen);
        }
        self.freezer.set(self.pawn, frozen)
    }

    fn can_freeze(&self) -> bool {
        self.freezer.freezes()
    }

    fn on_recorded(&mut self, t: f64) {
        if let Some(tr) = self.bodies.as_mut() {
            tr.history.commit(t);
        }
    }

    fn on_truncate(&mut self, t: Option<f64>) {
        if let Some(tr) = self.bodies.as_mut() {
            match t {
                Some(t) => tr.history.resume_at(t),
                None => tr.history.clear(),
            }
        }
    }

    fn health(&self) -> crate::backend::BackendHealth {
        let mut h = self.bodies.as_ref().map(BodyTracker::health).unwrap_or_default();
        h.car_locked = self.pawn.is_some() && (self.bodies.is_none() || h.car_locked);
        h
    }

    fn describe(&self) -> String {
        let bodies = match &self.bodies {
            None => "no sim bodies (vtable_scan not configured)".to_owned(),
            Some(tr) => match tr.car() {
                Some(c) => format!("{} sim bodies, main {:#x}", c.bodies.len(), c.primary),
                None => "sim bodies not located yet".to_owned(),
            },
        };
        format!(
            "car_avatar (pawn {}, {bodies}, body_writes {:?}, reset_car {}, axes {}, freeze: {})",
            self.pawn.map_or("?".into(), |p| format!("{p:#x}")),
            self.spec.body_writes,
            self.spec.reset_car,
            self.axes.map_or_else(|| self.axes_note(), |m| m.describe()),
            self.freezer.name()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gear_text() {
        assert_eq!(parse_gear("R"), Some(-1));
        assert_eq!(parse_gear(" N "), Some(0));
        assert_eq!(parse_gear("1"), Some(1));
        assert_eq!(parse_gear("6"), Some(6));
        assert_eq!(parse_gear(""), None);
        assert_eq!(parse_gear("0"), None);
        assert_eq!(parse_gear("P"), None);
    }

    #[test]
    fn snapshot_units() {
        let s = snapshot_from_ue(
            DVec3::new(12_000.0, -350.0, 80.0),
            Rotator { pitch: 0.0, yaw: 90.0, roll: 0.0 },
            DVec3::new(2500.0, 0.0, -100.0),
            4200.0,
            3,
            DVec3::new(0.0, 0.0, 0.5),
        );
        assert_eq!(s.pos, DVec3::new(120.0, -3.5, 0.8));
        assert_eq!(s.lin_vel, DVec3::new(25.0, 0.0, -1.0));
        assert!(s.rot.angle_between(DQuat::from_rotation_z(90f64.to_radians())) < 1e-9);
        assert_eq!((s.rpm, s.gear), (4200.0, 3));
        assert!(s.is_valid());
        // Round trip through the write units.
        assert_eq!(m_to_cm(s.pos), DVec3::new(12_000.0, -350.0, 80.0));
    }

    #[test]
    fn motion_reports_slip_and_yaw_in_the_car_frame() {
        let heading = DQuat::from_rotation_z(90f64.to_radians()); // facing UE +Y
        let (speed, slip, yaw) = motion(heading, DVec3::new(0.0, 20.0, 0.0), DVec3::new(0.0, 0.0, 0.5));
        assert!((speed - 20.0).abs() < 1e-9);
        assert!(slip.abs() < 1e-6, "straight ahead whatever the heading: {slip}");
        assert!((yaw - 0.5f64.to_degrees()).abs() < 1e-9);
        // Same velocity, car rotated 30 deg left of it: sliding towards its right.
        let (_, slip, _) = motion(DQuat::from_rotation_z(60f64.to_radians()), DVec3::new(0.0, 20.0, 0.0), DVec3::ZERO);
        assert!((slip - 30.0).abs() < 1e-6, "{slip}");
        let (_, slip, _) = motion(heading, DVec3::new(0.1, 0.1, 3.0), DVec3::ZERO);
        assert_eq!(slip, 0.0, "no slip angle when barely moving");
    }

    #[test]
    fn resume_line_compares_with_the_target() {
        let target = CarSnapshot {
            rot: DQuat::IDENTITY,
            lin_vel: DVec3::new(20.0, 0.0, 0.0),
            ang_vel: DVec3::new(0.0, 0.0, 0.1),
            rpm: 6000.0,
            ..Default::default()
        };
        let w = ResumeWatch { target, target_spin: 66.0, rpm_first: 47.0, rpm_last: 5800.0, ticks: 1 };
        let now = CarSnapshot { lin_vel: DVec3::new(19.0, 5.0, 0.0), rpm: 300.0, ..target };
        let line = resume_line(&w, &now, 3.0);
        assert!(line.contains("rpm 300 (6000)"), "{line}");
        assert!(line.contains("slip +14.7 (+0.0) deg"), "{line}");
        assert!(line.contains("max body spin 3 (66) rad/s"), "{line}");
        assert!(line.contains("run-in rpm 47 -> 5800"), "{line}");
        assert!(!resume_line(&ResumeWatch { ticks: 5, ..w }, &now, 3.0).contains("run-in"));
    }

    #[test]
    fn diag_line_reports_residuals() {
        let m = AxisMap::parse(&["+y".into(), "+z".into(), "+x".into()]).unwrap();
        let actor_cm = DVec3::new(10_000.0, 2_000.0, 300.0);
        let rot = DQuat::from_rotation_z(0.3);
        let body = BodyState {
            pos: m.ue_to_sim(cm_to_m(actor_cm)) + DVec3::new(0.0, 0.5, 0.0),
            rot: m.ue_to_sim_rot(rot),
            lin_vel: m.ue_to_sim(DVec3::new(20.0, 0.0, 0.0)),
            ang_vel: DVec3::ZERO,
        };
        let d = Diag {
            actor_cm,
            rot,
            vel_ms: DVec3::new(20.0, 0.0, 0.0),
            rpm: 5000.0,
            gear: 2,
            respawn_seq: Some(1),
            axes: Some(m),
            axes_note: "configured".into(),
            body: Some(body),
            shm: Some(body.pos),
            shm_tol: None,
        };
        let s = diag_line(&d);
        assert!(s.starts_with("car_avatar validate: actor UE (10000.0, 2000.0, 300.0) cm"), "{s}");
        assert!(s.contains("axes sim = (+ue.y, +ue.z, +ue.x) (configured) -> sim (20.000, 3.000, 100.000) m"), "{s}");
        assert!(s.contains("[actor-body 0.500 m, rot 0.00 deg, vel 0.00 m/s]"), "{s}");
        assert!(s.contains("[actor-shm 0.500 m] [body-shm 0.000 m]"), "{s}");
        // Speed-aware tolerance: 0.5 m at 20 m/s is within 0.2 + 20 * 0.06 = 1.4 m, 2 m is not.
        let tol = Some((0.2, 0.06));
        let s = diag_line(&Diag { shm_tol: tol, ..d.clone() });
        assert!(s.contains("[actor-shm 0.500 m, tol 1.40 m] [body-shm 0.000 m, tol 1.40 m]"), "{s}");
        let s = diag_line(&Diag { shm_tol: tol, shm: Some(body.pos + DVec3::new(2.0, 0.0, 0.0)), ..d.clone() });
        assert!(s.contains("[body-shm 2.000 m, tol 1.40 m MISMATCH]"), "{s}");
        let none = diag_line(&Diag { axes_note: "auto: no main body yet".into(), ..Default::default() });
        assert!(none.contains("axes unknown (auto: no main body yet) | main body n/a | shm n/a"), "{none}");
    }

    #[test]
    fn transform_param_by_name_or_layout() {
        use crate::reflection::PropInfo;
        let p = |name: &str, offset, size| PropInfo { name: name.into(), offset, size, flags: 0 };
        let named = FunctionInfo {
            addr: 1,
            name: "SetPhysicsTransform".into(),
            parms_size: 0x70,
            params: vec![p("bResetCar", 0x60, 1), p("InTransform", 0, 0x60)],
        };
        assert_eq!(transform_param(&named), Some("InTransform"));
        let renamed =
            FunctionInfo { params: vec![p("NewTransform", 0, 0x60), p("bResetCar", 0x60, 1)], ..named.clone() };
        assert_eq!(transform_param(&renamed), Some("NewTransform"));
        let none = FunctionInfo { params: vec![p("X", 0, 0x30)], ..named };
        assert_eq!(transform_param(&none), None);
    }
}
