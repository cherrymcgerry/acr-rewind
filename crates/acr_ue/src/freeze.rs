//! Levers that stop the simulation while scrubbing (see `[freeze]` in signatures.toml).

use crate::backend::BackendError;
use crate::game::Game;
use crate::mem::Memory;
use std::sync::Arc;

pub trait Freezer: Send {
    fn name(&self) -> &'static str;
    /// `pawn` is the current player car (some levers act on it).
    fn set(&mut self, pawn: Option<usize>, frozen: bool) -> Result<(), BackendError>;
    /// False for [`NoFreeze`].
    fn freezes(&self) -> bool {
        true
    }
}

/// No lever available: the car is only held in place by writing the pose every frame.
pub struct NoFreeze;

impl Freezer for NoFreeze {
    fn name(&self) -> &'static str {
        "none"
    }
    fn set(&mut self, _pawn: Option<usize>, _frozen: bool) -> Result<(), BackendError> {
        Ok(())
    }
    fn freezes(&self) -> bool {
        false
    }
}

/// Writes `AActor::CustomTimeDilation = 0` on the car (thread-agnostic raw write) and
/// restores the previous value on unfreeze.
pub struct CustomTimeDilation {
    game: Arc<Game>,
    saved: Option<(usize, usize, f32)>,
}

impl CustomTimeDilation {
    pub fn new(game: Arc<Game>) -> Option<Self> {
        let prop = &game.sigs.reflection.time_dilation_property;
        let actor = game.class("Actor")?;
        game.ue.find_property(actor, prop)?;
        Some(Self { game, saved: None })
    }
}

impl Freezer for CustomTimeDilation {
    fn name(&self) -> &'static str {
        "custom_time_dilation"
    }

    fn set(&mut self, pawn: Option<usize>, frozen: bool) -> Result<(), BackendError> {
        let ue = &self.game.ue;
        if frozen {
            let pawn = pawn.ok_or(BackendError::NoCar)?;
            let class = ue.class_of(pawn).ok_or(BackendError::NoCar)?;
            let prop = ue
                .find_property(class, &self.game.sigs.reflection.time_dilation_property)
                .ok_or_else(|| BackendError::Unsupported("CustomTimeDilation property missing".into()))?;
            let addr = pawn + prop.offset;
            if self.saved.is_none() {
                let old = ue.mem.read_f32(addr).ok_or(BackendError::Write(addr))?;
                let old = if old.is_finite() && old > 0.0 { old } else { 1.0 };
                self.saved = Some((pawn, addr, old));
            }
            if !ue.mem.write_f32(addr, 0.0) {
                return Err(BackendError::Write(addr));
            }
        } else if let Some((saved_pawn, addr, old)) = self.saved.take() {
            if pawn == Some(saved_pawn) && !ue.mem.write_f32(addr, old) {
                return Err(BackendError::Write(addr));
            }
        }
        Ok(())
    }
}

/// dmphysics `APhysicsAvatar::SetPaused(bInPaused)` on the live PhysicsAvatar (the actor that
/// owns the custom physics world); game thread only. UNVERIFIED in-game.
pub struct PhysicsAvatarPause {
    game: Arc<Game>,
    class: usize,
    paused: Option<usize>,
}

impl PhysicsAvatarPause {
    pub fn new(game: Arc<Game>) -> Option<Self> {
        let class = &game.sigs.reflection.functions.get("physics_avatar_set_paused")?.class;
        let class = game.class(class)?;
        game.func("physics_avatar_set_paused")?;
        Some(Self { game, class, paused: None })
    }

    fn call(&self, obj: usize, paused: bool) -> Result<(), BackendError> {
        let info = self
            .game
            .func("physics_avatar_set_paused")
            .ok_or_else(|| BackendError::Unsupported("SetPaused not resolved".into()))?;
        let mut p = crate::ue_call::Params::new(info);
        let err = |e: crate::ue_call::CallError| BackendError::Call(e.to_string());
        p.set_bool("bInPaused", paused).map_err(err)?;
        p.call(obj).map_err(err)
    }
}

impl Freezer for PhysicsAvatarPause {
    fn name(&self) -> &'static str {
        "physics_avatar_pause"
    }

    fn set(&mut self, _pawn: Option<usize>, frozen: bool) -> Result<(), BackendError> {
        if frozen {
            if self.paused.is_none() {
                let obj = self
                    .game
                    .ue
                    .find_live_instance_of(self.class)
                    .ok_or_else(|| BackendError::Unsupported("no live PhysicsAvatar".into()))?;
                self.call(obj, true)?;
                self.paused = Some(obj);
            }
        } else if let Some(obj) = self.paused.take() {
            if self.game.ue.class_of(obj) == Some(self.class) {
                self.call(obj, false)?;
            }
        }
        Ok(())
    }
}

/// `UGameplayStatics::SetGlobalTimeDilation(GWorld, v)`; game thread only.
pub struct GlobalTimeDilation {
    game: Arc<Game>,
    value: f32,
    active: bool,
}

impl GlobalTimeDilation {
    pub fn new(game: Arc<Game>) -> Option<Self> {
        game.func("set_global_time_dilation")?;
        let class = &game.sigs.reflection.functions.get("set_global_time_dilation")?.class;
        game.cdo(class)?;
        let value = game.sigs.freeze.global_time_dilation_value;
        Some(Self { game, value, active: false })
    }

    fn apply(&self, v: f32) -> Result<(), BackendError> {
        let world = self.game.world().ok_or(BackendError::NoCar)?;
        self.game
            .call_static("set_global_time_dilation", |p| {
                p.set_ptr("WorldContextObject", world)?;
                p.set_f32("TimeDilation", v)
            })
            .map(|_| ())
            .map_err(BackendError::Call)
    }
}

impl Freezer for GlobalTimeDilation {
    fn name(&self) -> &'static str {
        "global_time_dilation"
    }

    fn set(&mut self, _pawn: Option<usize>, frozen: bool) -> Result<(), BackendError> {
        if frozen {
            self.apply(self.value)?;
            self.active = true;
        } else if self.active {
            self.apply(1.0)?;
            self.active = false;
        }
        Ok(())
    }
}
