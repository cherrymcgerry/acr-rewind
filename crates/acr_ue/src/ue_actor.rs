//! Fallback backend: the UE actor via ProcessEvent. Only effective if the UE body drives the
//! simulation; on ACR the dmphysics solver overwrites these writes, which the post-resume
//! validation in the hook detects.
//!
//! Snapshot convention: UE world space with lengths converted cm -> m; rotation is the actor
//! quaternion; angular velocity in rad/s.

use crate::backend::{BackendError, CarBackend, CarChange, WriteMode};
use crate::freeze::Freezer;
use crate::game::{Game, PawnLocator};
use crate::math::{quat_to_rotator, rotator_to_quat, Rotator};
use crate::reflection::FunctionInfo;
use crate::ue_call::Params;
use glam::DVec3;
use rewind_core::CarSnapshot;
use std::sync::Arc;

const CM_PER_M: f64 = 100.0;

pub struct UeActorAdapter {
    game: Arc<Game>,
    locator: PawnLocator,
    freezer: Box<dyn Freezer>,
    pawn: Option<usize>,
    root: Option<usize>,
    warned_physics: bool,
}

impl UeActorAdapter {
    pub fn new(game: Arc<Game>, freezer: Box<dyn Freezer>) -> Result<Self, String> {
        let missing = game.missing_required_functions();
        if !missing.is_empty() {
            return Err(format!("required UFunctions not resolved: {missing:?}"));
        }
        Ok(Self {
            locator: PawnLocator::new(game.clone()),
            game,
            freezer,
            pawn: None,
            root: None,
            warned_physics: false,
        })
    }

    fn func(&self, key: &str) -> Result<&FunctionInfo, BackendError> {
        self.game.func(key).ok_or_else(|| BackendError::Unsupported(format!("{key} not resolved")))
    }

    /// Calls a `() -> FVector` getter. A `BoneName` parameter stays zeroed (`NAME_None`).
    fn call_vec_getter(&self, obj: usize, key: &str) -> Result<DVec3, BackendError> {
        let info = self.func(key)?;
        let mut p = Params::new(info);
        p.call(obj).map_err(|e| BackendError::Call(e.to_string()))?;
        p.get_vec("ReturnValue").map_err(|e| BackendError::Call(e.to_string()))
    }

    fn set_velocity(&self, key: &str, v: DVec3) -> Result<(), BackendError> {
        let Some(root) = self.root else {
            return Ok(());
        };
        let Some(info) = self.game.func(key) else {
            return Ok(());
        };
        let mut p = Params::new(info);
        let err = |e: crate::ue_call::CallError| BackendError::Call(e.to_string());
        let vec_param = info
            .params
            .iter()
            .find(|x| x.name.starts_with("NewAng") || x.name == "NewVel")
            .map(|x| x.name.clone())
            .ok_or_else(|| BackendError::Unsupported(format!("{key}: no velocity param")))?;
        p.set_vec(&vec_param, v).map_err(err)?;
        if p.has("bAddToCurrent") {
            p.set_bool("bAddToCurrent", false).map_err(err)?;
        }
        p.call(root).map_err(err)
    }

    fn is_simulating(&self) -> bool {
        let (Some(root), Some(info)) = (self.root, self.game.func("is_simulating_physics")) else {
            return false;
        };
        let mut p = Params::new(info);
        p.call(root).is_ok() && p.get_bool("ReturnValue").unwrap_or(false)
    }
}

impl CarBackend for UeActorAdapter {
    fn name(&self) -> &'static str {
        "ue_actor"
    }

    fn refresh(&mut self) -> CarChange {
        let pawn = self.locator.locate();
        let change = match (self.pawn, pawn) {
            (_, None) => CarChange::Lost,
            (Some(a), Some(b)) if a == b => CarChange::Same,
            _ => CarChange::Changed,
        };
        if change == CarChange::Changed {
            let p = pawn.unwrap_or_default();
            self.root = self.game.ue.read_object_property(p, &self.game.sigs.reflection.root_component_property);
            tracing::info!(
                "player car {:#x} class {:?}, root component {:?}",
                p,
                self.game.ue.class_name(p),
                self.root.and_then(|r| self.game.ue.class_name(r))
            );
        }
        self.pawn = pawn;
        change
    }

    fn read_state(&mut self) -> Option<CarSnapshot> {
        let pawn = self.pawn?;
        let loc = self.call_vec_getter(pawn, "k2_get_actor_location").ok()?;
        let rot = {
            let info = self.game.func("k2_get_actor_rotation")?;
            let mut p = Params::new(info);
            p.call(pawn).ok()?;
            let v = p.get_vec("ReturnValue").ok()?;
            rotator_to_quat(Rotator { pitch: v.x, yaw: v.y, roll: v.z })
        };
        let lin_vel = self.call_vec_getter(pawn, "get_velocity").unwrap_or(DVec3::ZERO);
        let ang_vel = match self.root {
            Some(root) if self.game.func("get_physics_angular_velocity").is_some() => {
                self.call_vec_getter(root, "get_physics_angular_velocity").unwrap_or(DVec3::ZERO)
            }
            _ => DVec3::ZERO,
        };
        let snap = CarSnapshot { pos: loc / CM_PER_M, rot, lin_vel: lin_vel / CM_PER_M, ang_vel, ..Default::default() };
        snap.is_valid().then_some(snap)
    }

    fn write_state(&mut self, snap: &CarSnapshot, mode: WriteMode) -> Result<(), BackendError> {
        let pawn = self.pawn.ok_or(BackendError::NoCar)?;
        if !snap.is_valid() {
            return Err(BackendError::Unsupported("refusing to write an invalid snapshot".into()));
        }
        let info = self.func("k2_set_actor_location_and_rotation")?;
        let err = |e: crate::ue_call::CallError| BackendError::Call(e.to_string());
        let r = quat_to_rotator(snap.rot);
        let mut p = Params::new(info);
        p.set_vec("NewLocation", snap.pos * CM_PER_M).map_err(err)?;
        p.set_vec("NewRotation", DVec3::new(r.pitch, r.yaw, r.roll)).map_err(err)?;
        p.set_bool("bSweep", false).map_err(err)?;
        p.set_bool("bTeleport", true).map_err(err)?;
        p.call(pawn).map_err(err)?;

        let simulating = self.is_simulating();
        if !simulating && mode == WriteMode::Full && !self.warned_physics {
            self.warned_physics = true;
            tracing::warn!(
                "root component is not simulating physics: velocities can't be restored through UE \
                 (expected for the Kunos solver; configure [raw_offsets.sim_car])"
            );
        }
        if simulating {
            let (lin, ang) = match mode {
                WriteMode::Full => (snap.lin_vel * CM_PER_M, snap.ang_vel),
                WriteMode::Pose => (DVec3::ZERO, DVec3::ZERO),
            };
            self.set_velocity("set_physics_linear_velocity", lin)?;
            self.set_velocity("set_physics_angular_velocity", ang)?;
        }
        Ok(())
    }

    fn freeze(&mut self, frozen: bool) -> Result<(), BackendError> {
        self.freezer.set(self.pawn, frozen)
    }

    fn can_freeze(&self) -> bool {
        self.freezer.freezes()
    }

    fn describe(&self) -> String {
        format!(
            "ue_actor (ProcessEvent teleport, freeze: {}, pawn {})",
            self.freezer.name(),
            self.pawn.map_or("?".into(), |p| format!("{p:#x}"))
        )
    }
}
