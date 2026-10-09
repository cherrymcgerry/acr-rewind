//! Calling UFunctions through `UObject::ProcessEvent`.
//!
//! Parameter offsets always come from the UFunction's FProperty chain ([`FunctionInfo`]);
//! nothing is hardcoded. Calls are refused unless the hook marked the current thread as the
//! game thread (ProcessEvent is not thread-safe).

use crate::mem::Memory;
use crate::reflection::FunctionInfo;
use glam::{DQuat, DVec3};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

/// `sizeof(FTransform)` with UE5 double precision.
pub const FTRANSFORM_SIZE: usize = 0x60;

/// Address used to invoke ProcessEvent: the detour trampoline if hooked, else the original.
static PROCESS_EVENT: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static GAME_THREAD: Cell<bool> = const { Cell::new(false) };
}

pub fn set_process_event_target(addr: usize) {
    PROCESS_EVENT.store(addr, Ordering::SeqCst);
}

pub fn process_event_target() -> usize {
    PROCESS_EVENT.load(Ordering::SeqCst)
}

/// Marks the current thread as (not) the game thread. Set by the tick detour.
pub fn set_game_thread(on: bool) {
    GAME_THREAD.with(|g| g.set(on));
}

pub fn on_game_thread() -> bool {
    GAME_THREAD.with(Cell::get)
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum CallError {
    #[error("ProcessEvent not resolved")]
    NoProcessEvent,
    #[error("UFunction calls are only allowed on the game thread")]
    NotGameThread,
    #[error("parameter '{0}' not found on {1}")]
    MissingParam(String, String),
    #[error("parameter '{0}' has unexpected size {1}")]
    BadSize(String, usize),
    #[error("null object")]
    NullObject,
}

/// A parameter buffer laid out per a [`FunctionInfo`].
pub struct Params<'a> {
    pub info: &'a FunctionInfo,
    buf: Vec<u8>,
}

impl<'a> Params<'a> {
    pub fn new(info: &'a FunctionInfo) -> Self {
        let end = info.params.iter().map(|p| p.offset + p.size).max().unwrap_or(0).max(info.parms_size);
        Self { info, buf: vec![0u8; (end + 15) & !15] }
    }

    fn slot(&self, name: &str) -> Result<(usize, usize), CallError> {
        self.info
            .param(name)
            .map(|p| (p.offset, p.size))
            .ok_or_else(|| CallError::MissingParam(name.into(), self.info.name.clone()))
    }

    pub fn has(&self, name: &str) -> bool {
        self.info.param(name).is_some()
    }

    pub fn set_vec(&mut self, name: &str, v: DVec3) -> Result<(), CallError> {
        let (o, size) = self.slot(name)?;
        match size {
            24 => {
                for (i, c) in [v.x, v.y, v.z].iter().enumerate() {
                    self.buf[o + i * 8..o + i * 8 + 8].copy_from_slice(&c.to_le_bytes());
                }
            }
            12 => {
                for (i, c) in [v.x, v.y, v.z].iter().enumerate() {
                    self.buf[o + i * 4..o + i * 4 + 4].copy_from_slice(&(*c as f32).to_le_bytes());
                }
            }
            s => return Err(CallError::BadSize(name.into(), s)),
        }
        Ok(())
    }

    pub fn get_vec(&self, name: &str) -> Result<DVec3, CallError> {
        let (o, size) = self.slot(name)?;
        let f64_at = |i: usize| f64::from_le_bytes(self.buf[o + i * 8..o + i * 8 + 8].try_into().unwrap());
        let f32_at = |i: usize| f32::from_le_bytes(self.buf[o + i * 4..o + i * 4 + 4].try_into().unwrap()) as f64;
        match size {
            24 => Ok(DVec3::new(f64_at(0), f64_at(1), f64_at(2))),
            12 => Ok(DVec3::new(f32_at(0), f32_at(1), f32_at(2))),
            s => Err(CallError::BadSize(name.into(), s)),
        }
    }

    pub fn set_bool(&mut self, name: &str, b: bool) -> Result<(), CallError> {
        let (o, _) = self.slot(name)?;
        self.buf[o] = u8::from(b);
        Ok(())
    }

    pub fn get_bool(&self, name: &str) -> Result<bool, CallError> {
        let (o, _) = self.slot(name)?;
        Ok(self.buf[o] != 0)
    }

    pub fn set_i32(&mut self, name: &str, v: i32) -> Result<(), CallError> {
        let (o, size) = self.slot(name)?;
        if size != 4 {
            return Err(CallError::BadSize(name.into(), size));
        }
        self.buf[o..o + 4].copy_from_slice(&v.to_le_bytes());
        Ok(())
    }

    pub fn get_i32(&self, name: &str) -> Result<i32, CallError> {
        let (o, size) = self.slot(name)?;
        if size != 4 {
            return Err(CallError::BadSize(name.into(), size));
        }
        Ok(i32::from_le_bytes(self.buf[o..o + 4].try_into().unwrap()))
    }

    pub fn set_f32(&mut self, name: &str, v: f32) -> Result<(), CallError> {
        let (o, size) = self.slot(name)?;
        match size {
            4 => self.buf[o..o + 4].copy_from_slice(&v.to_le_bytes()),
            8 => self.buf[o..o + 8].copy_from_slice(&(v as f64).to_le_bytes()),
            s => return Err(CallError::BadSize(name.into(), s)),
        }
        Ok(())
    }

    pub fn get_f32(&self, name: &str) -> Result<f32, CallError> {
        let (o, size) = self.slot(name)?;
        match size {
            4 => Ok(f32::from_le_bytes(self.buf[o..o + 4].try_into().unwrap())),
            8 => Ok(f64::from_le_bytes(self.buf[o..o + 8].try_into().unwrap()) as f32),
            s => Err(CallError::BadSize(name.into(), s)),
        }
    }

    /// UE5 (LWC) `FTransform`, 0x60 bytes of f64: rotation quat x,y,z,w @0x00, translation
    /// @0x20, scale @0x40 (the 4th lane of each is padding and stays zero). Lengths in cm.
    pub fn set_transform(
        &mut self,
        name: &str,
        rot: DQuat,
        translation_cm: DVec3,
        scale: DVec3,
    ) -> Result<(), CallError> {
        let (o, size) = self.slot(name)?;
        if size != FTRANSFORM_SIZE {
            return Err(CallError::BadSize(name.into(), size));
        }
        let q = rot.normalize();
        let lanes = [
            (0x00, [q.x, q.y, q.z, q.w]),
            (0x20, [translation_cm.x, translation_cm.y, translation_cm.z, 0.0]),
            (0x40, [scale.x, scale.y, scale.z, 0.0]),
        ];
        for (base, vals) in lanes {
            for (i, v) in vals.iter().enumerate() {
                let a = o + base + i * 8;
                self.buf[a..a + 8].copy_from_slice(&v.to_le_bytes());
            }
        }
        Ok(())
    }

    /// Inverse of [`Params::set_transform`]: (rotation, translation cm, scale).
    pub fn get_transform(&self, name: &str) -> Result<(DQuat, DVec3, DVec3), CallError> {
        let (o, size) = self.slot(name)?;
        if size != FTRANSFORM_SIZE {
            return Err(CallError::BadSize(name.into(), size));
        }
        let f = |i: usize| f64::from_le_bytes(self.buf[o + i * 8..o + i * 8 + 8].try_into().unwrap());
        Ok((DQuat::from_xyzw(f(0), f(1), f(2), f(3)), DVec3::new(f(4), f(5), f(6)), DVec3::new(f(8), f(9), f(10))))
    }

    /// Reads an `FString` (`TArray<TCHAR>`: data ptr, i32 num incl. NUL, i32 max) returned by a
    /// call. The engine allocated the characters; they are not freed here (callers keep such
    /// calls rare).
    pub fn get_fstring(&self, name: &str, mem: &dyn Memory) -> Result<String, CallError> {
        let (o, size) = self.slot(name)?;
        if size != 16 {
            return Err(CallError::BadSize(name.into(), size));
        }
        let data = u64::from_le_bytes(self.buf[o..o + 8].try_into().unwrap()) as usize;
        let num = i32::from_le_bytes(self.buf[o + 8..o + 12].try_into().unwrap());
        if data == 0 || num <= 0 {
            return Ok(String::new());
        }
        let n = (num as usize).min(1024);
        let bytes = mem.read_vec(data, n * 2).ok_or(CallError::NullObject)?;
        let units: Vec<u16> =
            bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).take_while(|&u| u != 0).collect();
        Ok(String::from_utf16_lossy(&units))
    }

    pub fn set_ptr(&mut self, name: &str, p: usize) -> Result<(), CallError> {
        let (o, size) = self.slot(name)?;
        if size != 8 {
            return Err(CallError::BadSize(name.into(), size));
        }
        self.buf[o..o + 8].copy_from_slice(&(p as u64).to_le_bytes());
        Ok(())
    }

    pub fn get_ptr(&self, name: &str) -> Result<usize, CallError> {
        let (o, size) = self.slot(name)?;
        if size != 8 {
            return Err(CallError::BadSize(name.into(), size));
        }
        Ok(u64::from_le_bytes(self.buf[o..o + 8].try_into().unwrap()) as usize)
    }

    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Invokes `ProcessEvent(obj, func, params)` on the game thread.
    pub fn call(&mut self, obj: usize) -> Result<(), CallError> {
        if obj == 0 {
            return Err(CallError::NullObject);
        }
        if !on_game_thread() {
            return Err(CallError::NotGameThread);
        }
        let pe = process_event_target();
        if pe == 0 {
            return Err(CallError::NoProcessEvent);
        }
        type ProcessEventFn = unsafe extern "C" fn(usize, usize, *mut u8);
        // SAFETY: `pe` is UObject::ProcessEvent (or its trampoline) resolved and sanity-checked
        // at init; we are on the game thread; the buffer is at least ParmsSize bytes and laid
        // out from the function's own property offsets.
        unsafe {
            let f: ProcessEventFn = std::mem::transmute::<usize, ProcessEventFn>(pe);
            f(obj, self.info.addr, self.buf.as_mut_ptr());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reflection::PropInfo;

    fn info() -> FunctionInfo {
        let p = |name: &str, offset, size| PropInfo { name: name.into(), offset, size, flags: 0 };
        FunctionInfo {
            addr: 0x1000,
            name: "K2_SetActorLocationAndRotation".into(),
            parms_size: 0x40,
            params: vec![
                p("NewLocation", 0, 24),
                p("OldVec", 24, 12),
                p("bSweep", 36, 1),
                p("Index", 40, 4),
                p("Obj", 48, 8),
            ],
        }
    }

    #[test]
    fn param_layout_roundtrip() {
        let fi = info();
        let mut p = Params::new(&fi);
        assert!(p.bytes().len() >= 0x40);
        p.set_vec("NewLocation", DVec3::new(1.0, -2.0, 3.5)).unwrap();
        p.set_vec("OldVec", DVec3::new(4.0, 5.0, 6.0)).unwrap();
        p.set_bool("bSweep", true).unwrap();
        p.set_i32("Index", -7).unwrap();
        p.set_ptr("Obj", 0xABCD).unwrap();
        assert_eq!(p.get_vec("NewLocation").unwrap(), DVec3::new(1.0, -2.0, 3.5));
        assert_eq!(p.get_vec("OldVec").unwrap(), DVec3::new(4.0, 5.0, 6.0));
        assert!(p.get_bool("bSweep").unwrap());
        assert_eq!(p.get_i32("Index").unwrap(), -7);
        assert_eq!(p.get_ptr("Obj").unwrap(), 0xABCD);
        assert!(matches!(p.set_i32("Obj", 1), Err(CallError::BadSize(..))));
        assert!(matches!(p.set_vec("Nope", DVec3::ZERO), Err(CallError::MissingParam(..))));
    }

    fn p(name: &str, offset: usize, size: usize) -> PropInfo {
        PropInfo { name: name.into(), offset, size, flags: 0 }
    }

    /// dmphysics_parameters.hpp (Dumper-7, build 25170642).
    fn set_physics_transform() -> FunctionInfo {
        FunctionInfo {
            addr: 0x2000,
            name: "SetPhysicsTransform".into(),
            parms_size: 0x70,
            params: vec![p("InTransform", 0x00, 0x60), p("bResetCar", 0x60, 1)],
        }
    }

    #[test]
    fn set_physics_transform_packing() {
        let fi = set_physics_transform();
        let mut pr = Params::new(&fi);
        assert_eq!(pr.bytes().len(), 0x70);
        let q = DQuat::from_rotation_z(0.75);
        pr.set_transform("InTransform", q, DVec3::new(12_345.5, -678.25, 90.0), DVec3::ONE).unwrap();
        pr.set_bool("bResetCar", false).unwrap();
        let b = pr.bytes();
        let f = |o: usize| f64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        assert_eq!([f(0x00), f(0x08), f(0x10), f(0x18)], [q.x, q.y, q.z, q.w]);
        assert_eq!([f(0x20), f(0x28), f(0x30), f(0x38)], [12_345.5, -678.25, 90.0, 0.0]);
        assert_eq!([f(0x40), f(0x48), f(0x50), f(0x58)], [1.0, 1.0, 1.0, 0.0]);
        assert_eq!(b[0x60], 0);
        pr.set_bool("bResetCar", true).unwrap();
        assert_eq!(pr.bytes()[0x60], 1);
        assert!(pr.bytes()[0x61..].iter().all(|&x| x == 0), "padding after the bool untouched");
        let (rq, t, s) = pr.get_transform("InTransform").unwrap();
        assert!(rq.angle_between(q) < 1e-12);
        assert_eq!(t, DVec3::new(12_345.5, -678.25, 90.0));
        assert_eq!(s, DVec3::ONE);
        // A non-LWC (f32) transform is refused rather than mis-packed.
        let small = FunctionInfo { params: vec![p("InTransform", 0, 0x30)], ..set_physics_transform() };
        assert!(matches!(
            Params::new(&small).set_transform("InTransform", q, DVec3::ZERO, DVec3::ONE),
            Err(CallError::BadSize(..))
        ));
    }

    #[test]
    fn velocity_cms_and_scalar_returns() {
        let set = FunctionInfo {
            addr: 0x3000,
            name: "SetVelocityCMS".into(),
            parms_size: 0x18,
            params: vec![p("InVelocityCMS", 0, 0x18)],
        };
        let mut pr = Params::new(&set);
        pr.set_vec("InVelocityCMS", DVec3::new(2500.0, -10.0, 0.5)).unwrap();
        let b = pr.bytes();
        assert_eq!(f64::from_le_bytes(b[0..8].try_into().unwrap()), 2500.0);
        assert_eq!(f64::from_le_bytes(b[8..16].try_into().unwrap()), -10.0);
        assert_eq!(f64::from_le_bytes(b[16..24].try_into().unwrap()), 0.5);
        let rpm =
            FunctionInfo { addr: 0x3100, name: "GetRPMS".into(), parms_size: 4, params: vec![p("ReturnValue", 0, 4)] };
        let mut pr = Params::new(&rpm);
        pr.set_f32("ReturnValue", 6123.5).unwrap();
        assert_eq!(pr.get_f32("ReturnValue").unwrap(), 6123.5);
    }

    #[test]
    fn fstring_return() {
        use crate::mem::SliceMemory;
        let mem = SliceMemory::new(0x5000_0000, vec![0; 0x100]);
        let text: Vec<u8> = "R\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert!(mem.write(0x5000_0010, &text));
        let gear = FunctionInfo {
            addr: 0x3200,
            name: "GetGear".into(),
            parms_size: 0x10,
            params: vec![p("ReturnValue", 0, 0x10)],
        };
        let mut pr = Params::new(&gear);
        assert_eq!(pr.get_fstring("ReturnValue", &mem).unwrap(), "", "empty FString");
        pr.buf[0..8].copy_from_slice(&0x5000_0010u64.to_le_bytes());
        pr.buf[8..12].copy_from_slice(&2i32.to_le_bytes());
        assert_eq!(pr.get_fstring("ReturnValue", &mem).unwrap(), "R");
    }

    #[test]
    fn call_refused_off_game_thread() {
        let fi = info();
        let mut p = Params::new(&fi);
        set_game_thread(false);
        assert_eq!(p.call(0x1234), Err(CallError::NotGameThread));
        assert_eq!(p.call(0), Err(CallError::NullObject));
        set_game_thread(true);
        if process_event_target() == 0 {
            assert_eq!(p.call(0x1234), Err(CallError::NoProcessEvent));
        }
        set_game_thread(false);
    }
}
