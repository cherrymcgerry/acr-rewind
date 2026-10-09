//! Wheel force-feedback protection around rewinds.
//!
//! While the rewind mode and the resume run-in hold the car (a pose written every frame, often
//! into scenery after a crash) the physics can produce violent steering forces. ACR drives
//! wheels through DirectInput effects (acr.exe imports DINPUT8.dll), so [`hook`] patches the
//! shared `IDirectInputEffect::SetParameters` slot of dinput8's effect vtable and scales the
//! game's own effect updates by the factor from [`FfbFade`]: 0 while the car is held, ramping
//! back to 1 over `ffb.fade_in_s` after the run-in. The gain of every effect is scaled, and
//! the magnitude of constant forces as well (in case a driver ignores the effect gain). At
//! factor 1 calls pass through untouched once the game's own values are restored, so normal
//! driving is never altered. Without a hook (no DirectInput force-feedback device, or the
//! game drives the wheel some other way) force feedback is left alone; that is logged once.

use rewind_core::FfbConfig;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

pub const DIEP_GAIN: u32 = 0x0000_0004;
pub const DIEP_TYPESPECIFICPARAMS: u32 = 0x0000_0100;
/// DI_FFNOMINALMAX: full effect gain.
pub const NOMINAL_MAX: u32 = 10_000;
/// The published factor is ignored (full force) when the tick stopped refreshing it this long.
const STALE_MS: u64 = 3000;

/// Output factor: 0 while the car is held (if muting), then a linear ramp back to 1.
#[derive(Clone, Debug, PartialEq)]
pub struct FfbFade {
    factor: f64,
}

impl Default for FfbFade {
    fn default() -> Self {
        Self { factor: 1.0 }
    }
}

impl FfbFade {
    pub fn factor(&self) -> f64 {
        self.factor
    }

    /// `held`: the rewind mode or the resume run-in holds the car this frame.
    pub fn update(&mut self, held: bool, dt: f64, cfg: &FfbConfig) -> f64 {
        if held && cfg.mute_during_rewind {
            self.factor = 0.0;
        } else if self.factor < 1.0 {
            let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
            self.factor = if cfg.fade_in_s > 0.0 { (self.factor + dt / cfg.fade_in_s).min(1.0) } else { 1.0 };
        }
        self.factor
    }
}

/// The game's own values for an effect whose parameters were scaled, restored once the
/// factor is back to 1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Saved {
    pub gain: Option<u32>,
    /// Constant-force magnitude.
    pub constant: Option<i32>,
}

/// What the game passes to `SetParameters`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EffectCall {
    pub flags: u32,
    /// `dwGain`, when `DIEP_GAIN` is set.
    pub gain: Option<u32>,
    /// `DICONSTANTFORCE::lMagnitude`, when type-specific parameters of a constant force are set.
    pub constant: Option<i32>,
}

/// Changes to the call: flags to add and values to write into a copy of the parameters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rewrite {
    pub add_flags: u32,
    pub gain: Option<u32>,
    pub constant: Option<i32>,
}

fn scale_gain(g: u32, f: f32) -> u32 {
    (g.min(NOMINAL_MAX) as f32 * f).round() as u32
}

fn scale_magnitude(m: i32, f: f32) -> i32 {
    (m as f32 * f).round() as i32
}

/// Plans one game `SetParameters` call. Returns the rewrite (`None` = pass through unchanged)
/// and what to remember for the effect afterwards. `query_gain` reads the gain the game set
/// earlier (only asked when muting an effect whose gain is unknown; if it fails the gain is
/// left alone rather than guessed, so it can never be restored above the game's value).
pub fn plan(
    factor: f32,
    call: &EffectCall,
    saved: Option<Saved>,
    query_gain: impl FnOnce() -> Option<u32>,
) -> (Option<Rewrite>, Option<Saved>) {
    if factor.is_nan() || factor >= 1.0 {
        let Some(s) = saved else {
            return (None, None);
        };
        let mut rw = Rewrite::default();
        if let (Some(g), 0) = (s.gain, call.flags & DIEP_GAIN) {
            rw.gain = Some(g);
            rw.add_flags |= DIEP_GAIN;
        }
        if let (Some(m), 0) = (s.constant, call.flags & DIEP_TYPESPECIFICPARAMS) {
            rw.constant = Some(m);
            rw.add_flags |= DIEP_TYPESPECIFICPARAMS;
        }
        return ((rw != Rewrite::default()).then_some(rw), None);
    }
    let f = factor.max(0.0);
    let mut s = saved.unwrap_or_default();
    let mut rw = Rewrite::default();
    if let Some(g) = call.gain.or(s.gain).or_else(query_gain).filter(|&g| g <= NOMINAL_MAX) {
        s.gain = Some(g);
        rw.gain = Some(scale_gain(g, f));
        rw.add_flags |= DIEP_GAIN;
    }
    if let Some(m) = call.constant {
        s.constant = Some(m);
        rw.constant = Some(scale_magnitude(m, f));
    }
    ((rw != Rewrite::default()).then_some(rw), (s != Saved::default()).then_some(s))
}

// ---- Shared state (tick -> hook) ----------------------------------------------------------

static FACTOR_BITS: AtomicU32 = AtomicU32::new(0x3F80_0000); // 1.0f32
static UPDATED_MS: AtomicU64 = AtomicU64::new(0);
/// The DirectInput effect hook is live.
pub static HOOKED: AtomicBool = AtomicBool::new(false);
/// Game `SetParameters` calls seen by the hook.
pub static GAME_CALLS: AtomicU64 = AtomicU64::new(0);
/// Game calls whose force was reduced.
pub static SCALED_CALLS: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

pub fn publish(factor: f64) {
    FACTOR_BITS.store((factor.clamp(0.0, 1.0) as f32).to_bits(), Ordering::Relaxed);
    UPDATED_MS.store(now_ms(), Ordering::Relaxed);
}

/// Factor the hook applies now.
pub fn effective_factor() -> f32 {
    let f = f32::from_bits(FACTOR_BITS.load(Ordering::Relaxed));
    if f >= 1.0 || now_ms().saturating_sub(UPDATED_MS.load(Ordering::Relaxed)) > STALE_MS {
        1.0
    } else {
        f
    }
}

/// One line for the settings panel.
pub fn status_line() -> String {
    if !HOOKED.load(Ordering::Relaxed) {
        return "Inactive: no DirectInput force-feedback wheel hooked (force feedback is left alone)".into();
    }
    match GAME_CALLS.load(Ordering::Relaxed) {
        0 => "Hooked; the game has not sent DirectInput effect updates yet".into(),
        n => format!("Active: {n} game effect updates seen, {} reduced", SCALED_CALLS.load(Ordering::Relaxed)),
    }
}

/// Per-tick driver of the fade, with its log lines.
#[derive(Debug, Default)]
pub struct FfbGuard {
    fade: FfbFade,
    scaled_at_mute: u64,
    warned: bool,
}

impl FfbGuard {
    pub fn tick(&mut self, held: bool, dt: f64, cfg: &FfbConfig) -> f64 {
        let before = self.fade.factor();
        let f = self.fade.update(held, dt, cfg);
        publish(f);
        if f < 1.0 && before >= 1.0 {
            self.scaled_at_mute = SCALED_CALLS.load(Ordering::Relaxed);
            if !self.warned {
                if !HOOKED.load(Ordering::Relaxed) {
                    self.warned = true;
                    tracing::warn!(
                        "ffb: no DirectInput force-feedback hook (no FFB wheel found through DirectInput); \
                         force feedback is NOT reduced during rewinds"
                    );
                } else if GAME_CALLS.load(Ordering::Relaxed) == 0 {
                    self.warned = true;
                    tracing::warn!(
                        "ffb: the game has not sent any DirectInput effect updates; it may drive this wheel another \
                         way, so force feedback may NOT be reduced during rewinds"
                    );
                }
            }
        } else if f >= 1.0 && before < 1.0 {
            tracing::info!(
                "ffb: back to full force ({} game effect updates reduced during the rewind)",
                SCALED_CALLS.load(Ordering::Relaxed).saturating_sub(self.scaled_at_mute)
            );
        }
        f
    }
}

#[cfg(windows)]
pub mod hook {
    //! The `IDirectInputEffect::SetParameters` vtable patch.
    //!
    //! dinput8 implements every effect with one COM class, so its vtable is shared by the
    //! game's effects and by an uninitialised effect created on our own non-exclusive device
    //! (a null `DIEFFECT`: nothing is downloaded or played). That vtable slot is swapped
    //! atomically; the original is always called, with a modified copy of the parameters.

    use super::{plan, EffectCall, Saved, DIEP_GAIN, DIEP_TYPESPECIFICPARAMS, GAME_CALLS, HOOKED, SCALED_CALLS};
    use std::ffi::c_void;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use windows::core::{Interface, HRESULT};
    use windows::Win32::Devices::HumanInterfaceDevice::{
        GUID_ConstantForce, IDirectInputDevice8W, IDirectInputEffect, IDirectInputEffect_Vtbl, DICONSTANTFORCE,
        DIEFFECT,
    };
    use windows::Win32::System::Memory::{VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS};

    type ParamsFn = unsafe extern "system" fn(*mut c_void, *mut DIEFFECT, u32) -> HRESULT;

    /// `sizeof(DIEFFECT_DX5)`: without `dwStartDelay`.
    const DIEFFECT_DX5_SIZE: u32 = 72;

    static ORIG_SET: AtomicUsize = AtomicUsize::new(0);
    static ORIG_GET: AtomicUsize = AtomicUsize::new(0);
    /// Effects (by interface pointer) whose parameters were scaled.
    static SAVED: Mutex<Vec<(usize, Saved)>> = Mutex::new(Vec::new());
    static ANY_SAVED: AtomicBool = AtomicBool::new(false);

    /// Installs the hook using a force-feedback device opened by the input thread. Once.
    pub fn install(dev: &IDirectInputDevice8W, device_name: &str) {
        if HOOKED.load(Ordering::SeqCst) {
            return;
        }
        // SAFETY: `dev` is a live DirectInput device of this process.
        match unsafe { patch(dev) } {
            Ok(()) => tracing::info!(
                "ffb: DirectInput effect hook installed (found via {device_name}); force feedback is muted during \
                 rewinds and fades back in after the resume"
            ),
            Err(e) => tracing::info!("ffb: cannot hook DirectInput effects via {device_name}: {e}"),
        }
    }

    unsafe fn patch(dev: &IDirectInputDevice8W) -> Result<(), String> {
        // SAFETY: COM calls on a live device; the vtable pointer of a live COM object is valid
        // and its slot is pointer-aligned, so the swap is a single atomic store.
        unsafe {
            let mut eff: Option<IDirectInputEffect> = None;
            dev.CreateEffect(&GUID_ConstantForce, std::ptr::null_mut(), &mut eff, None::<&windows::core::IUnknown>)
                .map_err(|e| format!("CreateEffect: {e}"))?;
            let eff = eff.ok_or("CreateEffect returned no effect")?;
            let vt = *(eff.as_raw() as *const *mut IDirectInputEffect_Vtbl);
            if vt.is_null() {
                return Err("effect has no vtable".into());
            }
            let slot = std::ptr::addr_of_mut!((*vt).SetParameters) as *mut usize;
            let ours = set_parameters as ParamsFn as usize;
            let current = std::ptr::read_volatile(slot);
            if current != ours {
                ORIG_GET.store((*vt).GetParameters as usize, Ordering::SeqCst);
                ORIG_SET.store(current, Ordering::SeqCst);
                let mut old = PAGE_PROTECTION_FLAGS::default();
                VirtualProtect(slot as *const c_void, 8, PAGE_EXECUTE_READWRITE, &mut old)
                    .map_err(|e| format!("VirtualProtect: {e}"))?;
                (*(slot as *const AtomicUsize)).store(ours, Ordering::SeqCst);
                let mut tmp = PAGE_PROTECTION_FLAGS::default();
                let _ = VirtualProtect(slot as *const c_void, 8, old, &mut tmp);
            }
            HOOKED.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    struct Prepared {
        eff: DIEFFECT,
        flags: u32,
        constant: Option<DICONSTANTFORCE>,
    }

    unsafe extern "system" fn set_parameters(this: *mut c_void, peff: *mut DIEFFECT, flags: u32) -> HRESULT {
        // SAFETY: only reachable through the patched slot, after ORIG_SET was stored.
        let orig = unsafe { std::mem::transmute::<usize, ParamsFn>(ORIG_SET.load(Ordering::SeqCst)) };
        GAME_CALLS.fetch_add(1, Ordering::Relaxed);
        let factor = super::effective_factor();
        if (factor >= 1.0 && !ANY_SAVED.load(Ordering::Relaxed)) || peff.is_null() || this.is_null() {
            // SAFETY: the caller's own arguments, unchanged.
            return unsafe { orig(this, peff, flags) };
        }
        // SAFETY: `peff` is the caller's DIEFFECT, valid for the duration of the call.
        let prepared = catch_unwind(AssertUnwindSafe(|| unsafe { prepare(this, peff, flags, factor) })).ok().flatten();
        let Some(mut p) = prepared else {
            // SAFETY: as above.
            return unsafe { orig(this, peff, flags) };
        };
        if let Some(c) = p.constant.as_mut() {
            p.eff.cbTypeSpecificParams = std::mem::size_of::<DICONSTANTFORCE>() as u32;
            p.eff.lpvTypeSpecificParams = c as *mut DICONSTANTFORCE as *mut c_void;
        }
        // SAFETY: a copy of the caller's parameters (its other pointers stay valid during the
        // call) with the gain / constant force replaced by locals that outlive the call.
        unsafe { orig(this, &mut p.eff, p.flags) }
    }

    unsafe fn prepare(this: *mut c_void, peff: *mut DIEFFECT, flags: u32, factor: f32) -> Option<Prepared> {
        // SAFETY: `dwSize` is checked before copying that many bytes of the caller's struct;
        // type-specific parameters are read only when they have the constant-force size.
        unsafe {
            let size = (*peff).dwSize;
            if size != std::mem::size_of::<DIEFFECT>() as u32 && size != DIEFFECT_DX5_SIZE {
                return None;
            }
            let mut eff: DIEFFECT = std::mem::zeroed();
            std::ptr::copy_nonoverlapping(peff as *const u8, &mut eff as *mut DIEFFECT as *mut u8, size as usize);
            let constant = (flags & DIEP_TYPESPECIFICPARAMS != 0
                && eff.cbTypeSpecificParams as usize == std::mem::size_of::<DICONSTANTFORCE>()
                && !eff.lpvTypeSpecificParams.is_null())
            .then(|| (*(eff.lpvTypeSpecificParams as *const DICONSTANTFORCE)).lMagnitude);
            let call = EffectCall { flags, gain: (flags & DIEP_GAIN != 0).then_some(eff.dwGain), constant };
            let key = this as usize;
            let mut saved = SAVED.lock().ok()?;
            let prev = saved.iter().position(|(k, _)| *k == key).map(|i| saved.swap_remove(i).1);
            let (rw, keep) = plan(factor, &call, prev, || query_gain(this, size));
            if let Some(s) = keep {
                saved.push((key, s));
            }
            ANY_SAVED.store(!saved.is_empty(), Ordering::Relaxed);
            drop(saved);
            let rw = rw?;
            if factor < 1.0 {
                SCALED_CALLS.fetch_add(1, Ordering::Relaxed);
            }
            if let Some(g) = rw.gain {
                eff.dwGain = g;
            }
            Some(Prepared {
                eff,
                flags: flags | rw.add_flags,
                constant: rw.constant.map(|m| DICONSTANTFORCE { lMagnitude: m }),
            })
        }
    }

    /// The gain the game last set on `this`.
    fn query_gain(this: *mut c_void, size: u32) -> Option<u32> {
        // SAFETY: the original GetParameters of the same vtable, on the effect being updated,
        // with a zeroed DIEFFECT of the caller's size (DIEP_GAIN fills dwGain only).
        unsafe {
            let get = std::mem::transmute::<usize, ParamsFn>(ORIG_GET.load(Ordering::SeqCst));
            let mut q: DIEFFECT = std::mem::zeroed();
            q.dwSize = size;
            get(this, &mut q, DIEP_GAIN).is_ok().then_some(q.dwGain)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mute: bool, fade: f64) -> FfbConfig {
        FfbConfig { mute_during_rewind: mute, fade_in_s: fade }
    }

    #[test]
    fn fade_mutes_while_held_then_ramps_back() {
        let c = cfg(true, 1.0);
        let mut f = FfbFade::default();
        assert_eq!(f.update(false, 0.016, &c), 1.0, "normal driving untouched");
        assert_eq!(f.update(true, 0.016, &c), 0.0, "muted at once when the car is held");
        assert_eq!(f.update(true, 5.0, &c), 0.0, "stays muted through the mode and run-in");
        assert!((f.update(false, 0.25, &c) - 0.25).abs() < 1e-9);
        assert!((f.update(false, 0.25, &c) - 0.5).abs() < 1e-9);
        assert_eq!(f.update(true, 0.016, &c), 0.0, "a new rewind mid-fade mutes again");
        for _ in 0..70 {
            f.update(false, 1.0 / 60.0, &c);
        }
        assert_eq!(f.factor(), 1.0, "capped at full");
    }

    #[test]
    fn fade_options() {
        let mut f = FfbFade::default();
        assert_eq!(f.update(true, 0.016, &cfg(false, 1.0)), 1.0, "muting disabled");
        f.update(true, 0.016, &cfg(true, 1.0));
        assert_eq!(f.update(false, 0.001, &cfg(true, 0.0)), 1.0, "fade 0 = instant");
        f.update(true, 0.016, &cfg(true, 2.0));
        assert_eq!(f.update(false, f64::NAN, &cfg(true, 2.0)), 0.0, "bad dt ignored");
        assert_eq!(f.update(false, -1.0, &cfg(true, 2.0)), 0.0);
        assert_eq!(f.update(false, 10.0, &cfg(true, 2.0)), 1.0, "a long frame finishes the fade");
        f.update(true, 0.016, &cfg(true, 1.0));
        assert!((f.update(false, 0.5, &cfg(false, 1.0)) - 0.5).abs() < 1e-9, "disabling mid-fade keeps fading");
    }

    fn call(flags: u32, gain: Option<u32>, constant: Option<i32>) -> EffectCall {
        EffectCall { flags, gain, constant }
    }

    const BOTH: u32 = DIEP_GAIN | DIEP_TYPESPECIFICPARAMS;

    #[test]
    fn full_force_passes_through() {
        let c = call(BOTH, Some(8000), Some(5000));
        assert_eq!(plan(1.0, &c, None, || panic!("no query at full force")), (None, None));
        assert_eq!(plan(f32::NAN, &c, None, || None), (None, None));
    }

    #[test]
    fn muting_scales_gain_and_constant_force() {
        let (rw, keep) = plan(0.0, &call(BOTH, Some(8000), Some(-7000)), None, || panic!("gain known"));
        assert_eq!(rw, Some(Rewrite { add_flags: DIEP_GAIN, gain: Some(0), constant: Some(0) }));
        assert_eq!(keep, Some(Saved { gain: Some(8000), constant: Some(-7000) }));
        let (rw, _) = plan(0.5, &call(DIEP_TYPESPECIFICPARAMS, None, Some(-7000)), keep, || panic!("saved"));
        assert_eq!(rw, Some(Rewrite { add_flags: DIEP_GAIN, gain: Some(4000), constant: Some(-3500) }));
    }

    #[test]
    fn unknown_gain_is_queried_once_and_never_guessed() {
        let (rw, keep) = plan(0.0, &call(DIEP_TYPESPECIFICPARAMS, None, Some(100)), None, || Some(6000));
        assert_eq!(rw.unwrap().gain, Some(0));
        assert_eq!(keep.unwrap().gain, Some(6000));
        // GetParameters failed: the gain is left alone, only the constant force is scaled.
        let (rw, keep) = plan(0.0, &call(DIEP_TYPESPECIFICPARAMS, None, Some(100)), None, || None);
        assert_eq!(rw, Some(Rewrite { add_flags: 0, gain: None, constant: Some(0) }));
        assert_eq!(keep, Some(Saved { gain: None, constant: Some(100) }));
        // A periodic / condition effect update without a gain we can learn: untouched.
        assert_eq!(plan(0.0, &call(DIEP_TYPESPECIFICPARAMS, None, None), None, || None), (None, None));
        // Out-of-range gains are not trusted.
        assert_eq!(plan(0.0, &call(DIEP_GAIN, Some(20_000), None), None, || None), (None, None));
    }

    #[test]
    fn game_values_are_restored_once_at_full_force() {
        let saved = Some(Saved { gain: Some(9000), constant: Some(3000) });
        // The game's next call carries neither: both are put back.
        let (rw, keep) = plan(1.0, &call(0, None, None), saved, || None);
        assert_eq!(
            rw,
            Some(Rewrite { add_flags: BOTH, gain: Some(9000), constant: Some(3000) }),
            "restored with the game's values"
        );
        assert_eq!(keep, None, "forgotten after restoring");
        // The game's call sets both itself: nothing to add.
        assert_eq!(plan(1.0, &call(BOTH, Some(7000), Some(1)), saved, || None), (None, None));
        // Only the gain is missing.
        let (rw, _) = plan(1.0, &call(DIEP_TYPESPECIFICPARAMS, None, Some(1)), saved, || None);
        assert_eq!(rw, Some(Rewrite { add_flags: DIEP_GAIN, gain: Some(9000), constant: None }));
    }

    #[test]
    fn newest_game_values_win() {
        let (_, keep) = plan(0.0, &call(BOTH, Some(8000), Some(10)), None, || None);
        let (_, keep) = plan(0.0, &call(DIEP_GAIN, Some(5000), None), keep, || None);
        assert_eq!(keep, Some(Saved { gain: Some(5000), constant: Some(10) }));
        let (rw, _) = plan(1.0, &call(0, None, None), keep, || None);
        assert_eq!(rw.unwrap().gain, Some(5000));
    }

    #[test]
    fn scaling_rounds_and_clamps() {
        assert_eq!(scale_gain(10_000, 0.333), 3330);
        assert_eq!(scale_gain(12_000, 1.0), 10_000);
        assert_eq!(scale_magnitude(-10_000, 0.25), -2500);
        assert_eq!(scale_magnitude(i32::MAX, 0.0), 0);
    }

    #[test]
    fn stale_or_full_factor_reads_as_full() {
        publish(0.0);
        assert_eq!(effective_factor(), 0.0);
        publish(0.4);
        assert!((effective_factor() - 0.4).abs() < 1e-6);
        publish(1.0);
        assert_eq!(effective_factor(), 1.0);
    }
}
