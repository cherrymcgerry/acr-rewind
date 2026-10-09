//! Wheel force-feedback protection around rewinds.
//!
//! While the rewind mode and the resume run-in hold the car (a pose written every frame, often
//! into scenery after a crash) the physics can produce violent steering forces. ACR drives
//! non-Fanatec wheels through DirectInput effects (acr.exe imports DINPUT8.dll), so [`hook`]
//! patches dinput8's shared vtables (`IDirectInputEffect::SetParameters` and `CreateEffect` of
//! both `IDirectInputDevice8W` and `IDirectInputDevice8A`) and scales the forces the game
//! requests by the factor from [`FfbFade`]: 0 while the car is held, ramping back to 1 over
//! `ffb.fade_in_s` after the run-in.
//!
//! Only constant, ramp and periodic forces and envelope levels are scaled, always in a copy of
//! the game's parameters; springs, dampers and other conditions are left alone, and so is the
//! game's own gain setting (never written). At factor 1 calls pass through unchanged, so
//! normal driving is never altered. Without a hook (no DirectInput force-feedback device, or
//! the game drives the wheel some other way) force feedback is left alone; that is logged.

use rewind_core::FfbConfig;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

pub const DIEP_ENVELOPE: u32 = 0x0000_0080;
pub const DIEP_TYPESPECIFICPARAMS: u32 = 0x0000_0100;
/// DI_FFNOMINALMAX: full force.
pub const NOMINAL_MAX: u32 = 10_000;
/// The published factor is ignored (full force) when the tick stopped refreshing it this long.
const STALE_MS: u64 = 5000;
/// Longest frame counted towards the fade, so a hitch after the resume cannot skip it.
const MAX_FADE_STEP_S: f64 = 0.1;

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
            let dt = if dt.is_finite() { dt.clamp(0.0, MAX_FADE_STEP_S) } else { 0.0 };
            self.factor = if cfg.fade_in_s > 0.0 { (self.factor + dt / cfg.fade_in_s).min(1.0) } else { 1.0 };
        }
        self.factor
    }
}

fn scale_i(v: i32, f: f32) -> i32 {
    if f >= 1.0 {
        v
    } else {
        (f64::from(v) * f64::from(f.max(0.0))).round() as i32
    }
}

fn scale_u(v: u32, f: f32) -> u32 {
    if f >= 1.0 {
        v
    } else {
        (f64::from(v) * f64::from(f.max(0.0))).round() as u32
    }
}

/// Envelope attack and fade levels, scaled.
pub fn scale_envelope(attack: u32, fade: u32, f: f32) -> (u32, u32) {
    (scale_u(attack, f), scale_u(fade, f))
}

/// The type-specific parameters of a force effect. Recognised by size alone (on x64 a
/// `DICONDITION` is 24 bytes per axis and a `DICUSTOMFORCE` 24 bytes, so neither collides).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Force {
    /// `DICONSTANTFORCE`
    Constant(i32),
    /// `DIRAMPFORCE`
    Ramp { start: i32, end: i32 },
    /// `DIPERIODIC`
    Periodic { magnitude: u32, offset: i32, phase: u32, period: u32 },
}

impl Force {
    /// From the parameter block as 32-bit words (`cbTypeSpecificParams / 4` of them).
    pub fn decode(w: &[u32]) -> Option<Self> {
        match *w {
            [m] => Some(Self::Constant(m as i32)),
            [s, e] => Some(Self::Ramp { start: s as i32, end: e as i32 }),
            [magnitude, o, phase, period] => Some(Self::Periodic { magnitude, offset: o as i32, phase, period }),
            _ => None,
        }
    }

    /// The parameter block and its length in words.
    pub fn encode(&self) -> ([u32; 4], usize) {
        match *self {
            Self::Constant(m) => ([m as u32, 0, 0, 0], 1),
            Self::Ramp { start, end } => ([start as u32, end as u32, 0, 0], 2),
            Self::Periodic { magnitude, offset, phase, period } => ([magnitude, offset as u32, phase, period], 4),
        }
    }

    /// Magnitudes (and the periodic offset) times `f`; unchanged at `f >= 1`.
    pub fn scaled(self, f: f32) -> Self {
        match self {
            Self::Constant(m) => Self::Constant(scale_i(m, f)),
            Self::Ramp { start, end } => Self::Ramp { start: scale_i(start, f), end: scale_i(end, f) },
            Self::Periodic { magnitude, offset, phase, period } => {
                Self::Periodic { magnitude: scale_u(magnitude, f), offset: scale_i(offset, f), phase, period }
            }
        }
    }

    /// Largest force this asks for (DirectInput units, 10000 = full).
    pub fn peak(&self) -> u32 {
        match *self {
            Self::Constant(m) => m.unsigned_abs(),
            Self::Ramp { start, end } => start.unsigned_abs().max(end.unsigned_abs()),
            Self::Periodic { magnitude, offset, .. } => magnitude.saturating_add(offset.unsigned_abs()),
        }
    }
}

/// The game's last force for an effect and the factor the device currently has it at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tracked {
    pub force: Force,
    pub applied: f32,
}

/// What to do with one game `SetParameters` call.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plan {
    /// Type-specific parameters to send instead (adding `DIEP_TYPESPECIFICPARAMS` if the game
    /// did not set them). `None` = the game's own parameters.
    pub send: Option<Force>,
    /// What to remember for the effect afterwards.
    pub keep: Option<Tracked>,
}

/// Plans one game `SetParameters` call. `call` is the force it sets (if it sets one), `prev`
/// what is known about the effect. A muted effect the game updates without a new force (e.g.
/// direction only) gets its last force re-sent scaled, and once the fade is over the game's
/// own last force is put back the same way; this only ever happens inside the game's own call
/// on that effect, so no freed effect is touched.
pub fn plan(factor: f32, call: Option<Force>, prev: Option<Tracked>) -> Plan {
    let factor = if factor.is_nan() { 1.0 } else { factor.clamp(0.0, 1.0) };
    if let Some(force) = call {
        let send = (factor < 1.0).then(|| force.scaled(factor));
        return Plan { send, keep: Some(Tracked { force, applied: factor }) };
    }
    match prev {
        Some(t) if t.applied != factor => {
            Plan { send: Some(t.force.scaled(factor)), keep: Some(Tracked { force: t.force, applied: factor }) }
        }
        other => Plan { send: None, keep: other },
    }
}

// ---- Shared state (tick -> hook) ----------------------------------------------------------

static FACTOR_BITS: AtomicU32 = AtomicU32::new(0x3F80_0000); // 1.0f32
static UPDATED_MS: AtomicU64 = AtomicU64::new(0);
/// The DirectInput effect hook is live.
pub static HOOKED: AtomicBool = AtomicBool::new(false);
/// Game effect calls (`SetParameters` / `CreateEffect`) seen by the hook.
pub static GAME_CALLS: AtomicU64 = AtomicU64::new(0);
/// Game calls whose forces were scaled down.
pub static SCALED_CALLS: AtomicU64 = AtomicU64::new(0);
/// Largest force the game asked for while scaled, since the last mute onset.
pub static PEAK_REQUESTED: AtomicU32 = AtomicU32::new(0);

fn now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

pub fn publish(factor: f64) {
    FACTOR_BITS.store((factor.clamp(0.0, 1.0) as f32).to_bits(), Ordering::Relaxed);
    UPDATED_MS.store(now_ms(), Ordering::Relaxed);
}

/// The factor published at `updated_ms`, as seen at `now_ms`: full force once stale.
pub fn factor_at(published: f32, updated_ms: u64, now_ms: u64) -> f32 {
    if published.is_nan() || published >= 1.0 || now_ms.saturating_sub(updated_ms) > STALE_MS {
        1.0
    } else {
        published.max(0.0)
    }
}

/// Factor the hook applies now.
pub fn effective_factor() -> f32 {
    factor_at(f32::from_bits(FACTOR_BITS.load(Ordering::Relaxed)), UPDATED_MS.load(Ordering::Relaxed), now_ms())
}

/// One line for the settings panel.
pub fn status_line() -> String {
    if !HOOKED.load(Ordering::Relaxed) {
        return "Inactive: no DirectInput force-feedback wheel hooked (force feedback is left alone)".into();
    }
    match GAME_CALLS.load(Ordering::Relaxed) {
        0 => "Hooked; the game has not sent DirectInput effect updates yet".into(),
        n => format!("Active: {n} game effect updates seen, {} scaled", SCALED_CALLS.load(Ordering::Relaxed)),
    }
}

/// Per-tick driver of the fade, with its log lines.
#[derive(Debug, Default)]
pub struct FfbGuard {
    fade: FfbFade,
    calls_at_mute: u64,
    scaled_at_mute: u64,
    warned: bool,
}

impl FfbGuard {
    pub fn tick(&mut self, held: bool, dt: f64, cfg: &FfbConfig) -> f64 {
        let before = self.fade.factor();
        let f = self.fade.update(held, dt, cfg);
        publish(f);
        let hooked = HOOKED.load(Ordering::Relaxed);
        if f < 1.0 && before >= 1.0 {
            self.calls_at_mute = GAME_CALLS.load(Ordering::Relaxed);
            self.scaled_at_mute = SCALED_CALLS.load(Ordering::Relaxed);
            PEAK_REQUESTED.store(0, Ordering::Relaxed);
            if !hooked && !self.warned {
                self.warned = true;
                tracing::warn!(
                    "ffb: no DirectInput force-feedback hook (no FFB wheel found through DirectInput); \
                     force feedback is NOT reduced during rewinds"
                );
            }
        } else if f >= 1.0 && before < 1.0 && hooked {
            let calls = GAME_CALLS.load(Ordering::Relaxed).saturating_sub(self.calls_at_mute);
            let scaled = SCALED_CALLS.load(Ordering::Relaxed).saturating_sub(self.scaled_at_mute);
            let peak = PEAK_REQUESTED.load(Ordering::Relaxed);
            if scaled == 0 {
                tracing::warn!(
                    "ffb: rewind over: 0 force updates scaled ({calls} DirectInput effect calls seen); the game may \
                     drive this wheel another way, so its force feedback was probably NOT reduced"
                );
            } else {
                tracing::info!(
                    "ffb: rewind over, full force again: {scaled} force updates scaled of {calls} effect calls \
                     (peak requested magnitude {peak} / {NOMINAL_MAX})"
                );
            }
        }
        f
    }
}

#[cfg(windows)]
pub mod hook {
    //! The DirectInput vtable patches.
    //!
    //! dinput8 implements every effect with one COM class and every device with one class per
    //! character set, so their vtables (in dinput8's read-only data) are shared by the game's
    //! objects and by ones we create: an uninitialised effect on our own non-exclusive device
    //! (a null `DIEFFECT`: nothing is downloaded or played) and an ANSI device on the same
    //! wheel. Each slot is swapped atomically; the originals are always called, with a
    //! modified copy of the parameters when scaling.

    use super::{plan, Force, Tracked, DIEP_ENVELOPE, DIEP_TYPESPECIFICPARAMS, GAME_CALLS, HOOKED};
    use super::{PEAK_REQUESTED, SCALED_CALLS};
    use std::cell::Cell;
    use std::ffi::c_void;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use windows::core::{Interface, GUID, HRESULT};
    use windows::Win32::Devices::HumanInterfaceDevice::{
        DirectInput8Create, GUID_ConstantForce, IDirectInput8A, IDirectInputDevice8A, IDirectInputDevice8A_Vtbl,
        IDirectInputDevice8W, IDirectInputDevice8W_Vtbl, IDirectInputEffect, IDirectInputEffect_Vtbl, DIEFFECT,
        DIENVELOPE, DIRECTINPUT_VERSION,
    };
    use windows::Win32::Foundation::HINSTANCE;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Memory::{VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS};

    type ParamsFn = unsafe extern "system" fn(*mut c_void, *mut DIEFFECT, u32) -> HRESULT;
    type CreateFn =
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut DIEFFECT, *mut *mut c_void, *mut c_void) -> HRESULT;

    /// `sizeof(DIEFFECT_DX5)`: without `dwStartDelay`.
    const DIEFFECT_DX5_SIZE: u32 = 72;
    /// Effects remembered at most (the game creates a handful).
    const MAX_TRACKED: usize = 64;

    static INSTALLED: AtomicBool = AtomicBool::new(false);
    static ORIG_SET: AtomicUsize = AtomicUsize::new(0);
    static ORIG_CREATE_W: AtomicUsize = AtomicUsize::new(0);
    static ORIG_CREATE_A: AtomicUsize = AtomicUsize::new(0);
    /// Effects by interface pointer. Entries are only used inside a game call on that same
    /// pointer, and dropped when `CreateEffect` hands out the address again.
    static TRACKED: Mutex<Vec<(usize, Tracked)>> = Mutex::new(Vec::new());

    thread_local! {
        /// Inside `CreateEffect`, whose parameters were already scaled (dinput may set them
        /// through the effect's own `SetParameters`).
        static IN_CREATE: Cell<bool> = const { Cell::new(false) };
    }

    /// Installs the hooks using a force-feedback device opened by the input thread. Once; a
    /// failure is logged and force feedback is then left alone.
    pub fn install(dev: &IDirectInputDevice8W, instance: &GUID, device_name: &str) {
        if INSTALLED.swap(true, Ordering::SeqCst) {
            return;
        }
        // SAFETY: `dev` is a live DirectInput device of this process.
        match unsafe { patch_effect(dev) } {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!(
                    "ffb: cannot hook DirectInput effects via {device_name}: {e}; force feedback is NOT reduced \
                     during rewinds"
                );
                return;
            }
        }
        HOOKED.store(true, Ordering::SeqCst);
        // SAFETY: as above.
        let w = unsafe { patch_create_w(dev) };
        // SAFETY: creates and releases an ANSI device of the same wheel.
        let a = unsafe { patch_create_a(instance) };
        let note = |r: &Result<(), String>| match r {
            Ok(()) => "hooked".to_owned(),
            Err(e) => format!("not hooked ({e})"),
        };
        tracing::info!(
            "ffb: DirectInput effect hook installed (found via {device_name}); SetParameters hooked, CreateEffect W {}, \
             A {}; force feedback is muted during rewinds and fades back in after the resume",
            note(&w),
            note(&a)
        );
    }

    /// Swaps a vtable slot to `ours`, keeping the original in `orig` (stored first).
    unsafe fn patch_slot(slot: *mut usize, ours: usize, orig: &AtomicUsize) -> Result<(), String> {
        // SAFETY: `slot` is a pointer-aligned vtable entry of a live COM class, so the swap
        // is a single atomic store.
        unsafe {
            let current = std::ptr::read_volatile(slot);
            if current == ours {
                return Ok(());
            }
            if current == 0 {
                return Err("empty vtable slot".into());
            }
            orig.store(current, Ordering::SeqCst);
            let mut old = PAGE_PROTECTION_FLAGS::default();
            VirtualProtect(slot as *const c_void, 8, PAGE_EXECUTE_READWRITE, &mut old)
                .map_err(|e| format!("VirtualProtect: {e}"))?;
            (*(slot as *const AtomicUsize)).store(ours, Ordering::SeqCst);
            let mut tmp = PAGE_PROTECTION_FLAGS::default();
            let _ = VirtualProtect(slot as *const c_void, 8, old, &mut tmp);
            Ok(())
        }
    }

    unsafe fn vtable<T>(obj: *mut c_void) -> Result<*mut T, String> {
        // SAFETY: the first pointer of a live COM object is its vtable.
        let vt = unsafe { *(obj as *const *mut T) };
        if vt.is_null() {
            Err("no vtable".into())
        } else {
            Ok(vt)
        }
    }

    unsafe fn patch_effect(dev: &IDirectInputDevice8W) -> Result<(), String> {
        // SAFETY: COM calls on a live device; vtable entries of a live effect.
        unsafe {
            let mut eff: Option<IDirectInputEffect> = None;
            dev.CreateEffect(&GUID_ConstantForce, std::ptr::null_mut(), &mut eff, None::<&windows::core::IUnknown>)
                .map_err(|e| format!("CreateEffect: {e}"))?;
            let eff = eff.ok_or("CreateEffect returned no effect")?;
            let vt = vtable::<IDirectInputEffect_Vtbl>(eff.as_raw())?;
            let slot = std::ptr::addr_of_mut!((*vt).SetParameters) as *mut usize;
            patch_slot(slot, set_parameters as ParamsFn as usize, &ORIG_SET)
        }
    }

    unsafe fn patch_create_w(dev: &IDirectInputDevice8W) -> Result<(), String> {
        // SAFETY: vtable entry of a live device.
        unsafe {
            let vt = vtable::<IDirectInputDevice8W_Vtbl>(dev.as_raw())?;
            let slot = std::ptr::addr_of_mut!((*vt).CreateEffect) as *mut usize;
            patch_slot(slot, create_effect_w as CreateFn as usize, &ORIG_CREATE_W)
        }
    }

    unsafe fn patch_create_a(instance: &GUID) -> Result<(), String> {
        // SAFETY: standard DirectInput8 creation; the vtable outlives the released device
        // (it is static data of dinput8, which stays loaded for the game).
        unsafe {
            let hm = GetModuleHandleW(None).map_err(|e| e.to_string())?;
            let mut out: *mut c_void = std::ptr::null_mut();
            DirectInput8Create(
                HINSTANCE(hm.0),
                DIRECTINPUT_VERSION,
                &IDirectInput8A::IID,
                &mut out,
                None::<&windows::core::IUnknown>,
            )
            .map_err(|e| format!("DirectInput8Create(A): {e}"))?;
            if out.is_null() {
                return Err("DirectInput8Create(A) returned null".into());
            }
            let di = IDirectInput8A::from_raw(out);
            let mut dev: Option<IDirectInputDevice8A> = None;
            di.CreateDevice(instance, &mut dev, None::<&windows::core::IUnknown>)
                .map_err(|e| format!("CreateDevice(A): {e}"))?;
            let dev = dev.ok_or("CreateDevice(A) returned null")?;
            let vt = vtable::<IDirectInputDevice8A_Vtbl>(dev.as_raw())?;
            let slot = std::ptr::addr_of_mut!((*vt).CreateEffect) as *mut usize;
            patch_slot(slot, create_effect_a as CreateFn as usize, &ORIG_CREATE_A)
        }
    }

    /// A copy of the game's `DIEFFECT` with room for replaced parameters. The copy's pointers
    /// are aimed at `words` / `env` by [`Scratch::bind`] right before the call.
    struct Scratch {
        eff: DIEFFECT,
        words: [u32; 4],
        n_words: usize,
        env: Option<DIENVELOPE>,
    }

    impl Scratch {
        /// Copies `dwSize` bytes of the caller's struct, if it has a known size.
        unsafe fn copy(peff: *const DIEFFECT) -> Option<Self> {
            // SAFETY: `dwSize` is checked before copying that many bytes.
            unsafe {
                let size = (*peff).dwSize;
                if size != std::mem::size_of::<DIEFFECT>() as u32 && size != DIEFFECT_DX5_SIZE {
                    return None;
                }
                let mut eff: DIEFFECT = std::mem::zeroed();
                std::ptr::copy_nonoverlapping(peff as *const u8, &mut eff as *mut DIEFFECT as *mut u8, size as usize);
                Some(Self { eff, words: [0; 4], n_words: 0, env: None })
            }
        }

        fn set_force(&mut self, f: Force) {
            (self.words, self.n_words) = f.encode();
        }

        /// Scales the copy's envelope levels, if it has a valid envelope.
        unsafe fn scale_envelope(&mut self, factor: f32) -> bool {
            let p = self.eff.lpEnvelope;
            // SAFETY: the caller's envelope, read once its size is checked.
            unsafe {
                if p.is_null() || (*p).dwSize != std::mem::size_of::<DIENVELOPE>() as u32 {
                    return false;
                }
                let mut env = std::ptr::read_unaligned(p);
                (env.dwAttackLevel, env.dwFadeLevel) =
                    super::scale_envelope(env.dwAttackLevel, env.dwFadeLevel, factor);
                self.env = Some(env);
            }
            true
        }

        fn bind(&mut self) -> *mut DIEFFECT {
            if self.n_words > 0 {
                self.eff.cbTypeSpecificParams = (self.n_words * 4) as u32;
                self.eff.lpvTypeSpecificParams = self.words.as_mut_ptr().cast();
            }
            if let Some(env) = self.env.as_mut() {
                self.eff.lpEnvelope = env;
            }
            &mut self.eff
        }
    }

    /// The force in the type-specific parameters of `eff`, if it is one.
    unsafe fn read_force(eff: &DIEFFECT) -> Option<Force> {
        let cb = eff.cbTypeSpecificParams as usize;
        if eff.lpvTypeSpecificParams.is_null() || !matches!(cb, 4 | 8 | 16) {
            return None;
        }
        let mut w = [0u32; 4];
        // SAFETY: the caller's block of `cb` bytes.
        unsafe {
            for (i, v) in w.iter_mut().take(cb / 4).enumerate() {
                *v = std::ptr::read_unaligned((eff.lpvTypeSpecificParams as *const u32).add(i));
            }
        }
        Force::decode(&w[..cb / 4])
    }

    fn note_scaled(force: Option<Force>) {
        SCALED_CALLS.fetch_add(1, Ordering::Relaxed);
        if let Some(f) = force {
            PEAK_REQUESTED.fetch_max(f.peak(), Ordering::Relaxed);
        }
    }

    fn take_tracked(key: usize) -> Option<Tracked> {
        let mut t = TRACKED.lock().ok()?;
        let i = t.iter().position(|(k, _)| *k == key)?;
        Some(t.remove(i).1)
    }

    fn put_tracked(key: usize, v: Tracked) {
        if let Ok(mut t) = TRACKED.lock() {
            t.retain(|(k, _)| *k != key);
            if t.len() >= MAX_TRACKED {
                t.remove(0);
            }
            t.push((key, v));
        }
    }

    unsafe extern "system" fn set_parameters(this: *mut c_void, peff: *mut DIEFFECT, flags: u32) -> HRESULT {
        // SAFETY: only reachable through the patched slot, after ORIG_SET was stored.
        let orig = unsafe { std::mem::transmute::<usize, ParamsFn>(ORIG_SET.load(Ordering::SeqCst)) };
        if peff.is_null() || this.is_null() || IN_CREATE.with(Cell::get) {
            // SAFETY: the caller's own arguments, unchanged.
            return unsafe { orig(this, peff, flags) };
        }
        GAME_CALLS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `peff` is the caller's DIEFFECT, valid for the duration of the call.
        let r = catch_unwind(AssertUnwindSafe(|| unsafe { set_parameters_scaled(orig, this, peff, flags) }));
        match r {
            Ok(Some(hr)) => hr,
            // SAFETY: as above.
            _ => unsafe { orig(this, peff, flags) },
        }
    }

    /// `None`: nothing to change, the caller passes the game's call through.
    unsafe fn set_parameters_scaled(
        orig: ParamsFn,
        this: *mut c_void,
        peff: *mut DIEFFECT,
        flags: u32,
    ) -> Option<HRESULT> {
        let factor = super::effective_factor();
        let key = this as usize;
        // SAFETY: the caller's DIEFFECT and the blocks it points to, valid during the call.
        unsafe {
            let mut s = Scratch::copy(peff)?;
            let sets_params = flags & DIEP_TYPESPECIFICPARAMS != 0;
            let call = if sets_params { read_force(&s.eff) } else { None };
            let prev = take_tracked(key);
            if sets_params && call.is_none() {
                // A condition or custom force (or unreadable block): never touched.
                return None;
            }
            let p = plan(factor, call, prev);
            if let Some(k) = p.keep {
                put_tracked(key, k);
            }
            let envelope = factor < 1.0 && flags & DIEP_ENVELOPE != 0 && s.scale_envelope(factor);
            if p.send.is_none() && !envelope {
                return None;
            }
            if factor < 1.0 {
                note_scaled(call.or(prev.map(|t| t.force)));
            }
            let mut new_flags = flags;
            if let Some(f) = p.send {
                s.set_force(f);
                new_flags |= DIEP_TYPESPECIFICPARAMS;
            }
            let hr = orig(this, s.bind(), new_flags);
            if hr.is_err() && new_flags != flags {
                // The device refused the added parameters: keep the game's call working and
                // try again on its next one.
                if let Some(t) = prev {
                    put_tracked(key, t);
                }
                return None;
            }
            Some(hr)
        }
    }

    unsafe extern "system" fn create_effect_w(
        this: *mut c_void,
        guid: *const GUID,
        peff: *mut DIEFFECT,
        out: *mut *mut c_void,
        outer: *mut c_void,
    ) -> HRESULT {
        // SAFETY: only reachable through the patched slot, after ORIG_CREATE_W was stored.
        unsafe { create_effect(ORIG_CREATE_W.load(Ordering::SeqCst), this, guid, peff, out, outer) }
    }

    unsafe extern "system" fn create_effect_a(
        this: *mut c_void,
        guid: *const GUID,
        peff: *mut DIEFFECT,
        out: *mut *mut c_void,
        outer: *mut c_void,
    ) -> HRESULT {
        // SAFETY: only reachable through the patched slot, after ORIG_CREATE_A was stored.
        unsafe { create_effect(ORIG_CREATE_A.load(Ordering::SeqCst), this, guid, peff, out, outer) }
    }

    unsafe fn create_effect(
        orig: usize,
        this: *mut c_void,
        guid: *const GUID,
        peff: *mut DIEFFECT,
        out: *mut *mut c_void,
        outer: *mut c_void,
    ) -> HRESULT {
        // SAFETY: the original of the slot this was called through.
        let orig = unsafe { std::mem::transmute::<usize, CreateFn>(orig) };
        if peff.is_null() {
            // SAFETY: the caller's own arguments, unchanged.
            return unsafe { orig(this, guid, peff, out, outer) };
        }
        GAME_CALLS.fetch_add(1, Ordering::Relaxed);
        let factor = super::effective_factor();
        // SAFETY: the caller's DIEFFECT, valid during the call.
        let prepared = catch_unwind(AssertUnwindSafe(|| unsafe {
            let mut s = Scratch::copy(peff)?;
            let force = read_force(&s.eff);
            let mut changed = false;
            if factor < 1.0 {
                if let Some(f) = force {
                    s.set_force(f.scaled(factor));
                    changed = true;
                }
                changed |= s.scale_envelope(factor);
            }
            Some((s, force, changed))
        }))
        .ok()
        .flatten();
        let requested = prepared.as_ref().and_then(|(_, f, _)| *f);
        IN_CREATE.with(|c| c.set(true));
        let hr = match prepared {
            Some((mut s, force, true)) => {
                note_scaled(force);
                // SAFETY: a copy of the caller's parameters (its other pointers stay valid
                // during the call) with forces replaced by locals that outlive the call.
                unsafe { orig(this, guid, s.bind(), out, outer) }
            }
            // SAFETY: the caller's own arguments, unchanged.
            _ => unsafe { orig(this, guid, peff, out, outer) },
        };
        IN_CREATE.with(|c| c.set(false));
        // SAFETY: `out` is the caller's out-pointer, filled on success.
        let created = if hr.is_ok() && !out.is_null() { unsafe { *out as usize } } else { 0 };
        if created != 0 {
            match requested {
                Some(force) => put_tracked(created, Tracked { force, applied: factor }),
                None => {
                    let _ = take_tracked(created);
                }
            }
        }
        hr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mute: bool, fade: f64) -> FfbConfig {
        FfbConfig { mute_during_rewind: mute, fade_in_s: fade }
    }

    const FRAME: f64 = 1.0 / 60.0;

    #[test]
    fn fade_mutes_while_held_then_ramps_back() {
        let c = cfg(true, 1.0);
        let mut f = FfbFade::default();
        assert_eq!(f.update(false, FRAME, &c), 1.0, "normal driving untouched");
        assert_eq!(f.update(true, FRAME, &c), 0.0, "muted at once when the car is held");
        assert_eq!(f.update(true, 5.0, &c), 0.0, "stays muted through the mode and run-in");
        assert!((f.update(false, 0.05, &c) - 0.05).abs() < 1e-9);
        assert!((f.update(false, 0.05, &c) - 0.1).abs() < 1e-9);
        for _ in 0..70 {
            f.update(false, FRAME, &c);
        }
        assert_eq!(f.factor(), 1.0, "capped at full");
    }

    #[test]
    fn rewind_mid_fade_mutes_again() {
        let c = cfg(true, 1.0);
        let mut f = FfbFade::default();
        f.update(true, FRAME, &c);
        for _ in 0..30 {
            f.update(false, FRAME, &c);
        }
        assert!(f.factor() > 0.4 && f.factor() < 0.6);
        assert_eq!(f.update(true, FRAME, &c), 0.0);
    }

    #[test]
    fn long_frames_cannot_skip_the_fade() {
        let c = cfg(true, 1.0);
        let mut f = FfbFade::default();
        f.update(true, FRAME, &c);
        assert!((f.update(false, 10.0, &c) - MAX_FADE_STEP_S).abs() < 1e-9, "a hitch counts as one short step");
        assert_eq!(f.update(false, f64::NAN, &c), MAX_FADE_STEP_S, "bad dt ignored");
        assert_eq!(f.update(false, -1.0, &c), MAX_FADE_STEP_S);
    }

    #[test]
    fn fade_options() {
        let mut f = FfbFade::default();
        assert_eq!(f.update(true, FRAME, &cfg(false, 1.0)), 1.0, "muting disabled");
        f.update(true, FRAME, &cfg(true, 1.0));
        assert_eq!(f.update(false, 0.001, &cfg(true, 0.0)), 1.0, "fade 0 = instant");
        f.update(true, FRAME, &cfg(true, 1.0));
        assert!((f.update(false, 0.05, &cfg(false, 1.0)) - 0.05).abs() < 1e-9, "disabling mid-fade keeps fading");
    }

    #[test]
    fn forces_are_recognised_by_size() {
        assert_eq!(Force::decode(&[(-7000i32) as u32]), Some(Force::Constant(-7000)));
        assert_eq!(Force::decode(&[100, (-200i32) as u32]), Some(Force::Ramp { start: 100, end: -200 }));
        assert_eq!(
            Force::decode(&[5000, (-1000i32) as u32, 9000, 50_000]),
            Some(Force::Periodic { magnitude: 5000, offset: -1000, phase: 9000, period: 50_000 })
        );
        assert_eq!(Force::decode(&[]), None);
        assert_eq!(Force::decode(&[0; 3]), None);
        assert_eq!(Force::decode(&[0; 6]), None, "a DICONDITION (spring / damper) is never a force");
        for f in [Force::Constant(-3), Force::Ramp { start: 1, end: -2 }, Force::decode(&[1, 2, 3, 4]).unwrap()] {
            let (w, n) = f.encode();
            assert_eq!(Force::decode(&w[..n]), Some(f), "round trip");
        }
    }

    #[test]
    fn scaling_forces() {
        assert_eq!(Force::Constant(-10_000).scaled(0.25), Force::Constant(-2500));
        assert_eq!(Force::Constant(i32::MAX).scaled(0.0), Force::Constant(0));
        assert_eq!(Force::Ramp { start: 8000, end: -4000 }.scaled(0.5), Force::Ramp { start: 4000, end: -2000 });
        let p = Force::Periodic { magnitude: 6000, offset: -2000, phase: 9000, period: 20_000 };
        assert_eq!(p.scaled(0.5), Force::Periodic { magnitude: 3000, offset: -1000, phase: 9000, period: 20_000 });
        assert_eq!(p.scaled(1.0), p, "full force is the game's value bit for bit");
        assert_eq!(scale_envelope(10_000, 4000, 0.1), (1000, 400));
        assert_eq!(scale_envelope(10_000, 4000, 1.0), (10_000, 4000));
    }

    #[test]
    fn peak_magnitude() {
        assert_eq!(Force::Constant(-7000).peak(), 7000);
        assert_eq!(Force::Ramp { start: 100, end: -9000 }.peak(), 9000);
        assert_eq!(Force::Periodic { magnitude: 3000, offset: -2000, phase: 0, period: 1 }.peak(), 5000);
        assert_eq!(Force::Constant(i32::MIN).peak(), 2_147_483_648);
    }

    #[test]
    fn full_force_passes_through() {
        let c = Force::Constant(5000);
        let p = plan(1.0, Some(c), None);
        assert_eq!(p, Plan { send: None, keep: Some(Tracked { force: c, applied: 1.0 }) });
        assert_eq!(plan(f32::NAN, Some(c), None).send, None);
        assert_eq!(plan(1.0, None, p.keep).send, None, "direction-only update at full force");
        assert_eq!(plan(1.0, None, None), Plan { send: None, keep: None });
    }

    #[test]
    fn muting_scales_the_games_force() {
        let p = plan(0.0, Some(Force::Constant(-7000)), None);
        assert_eq!(p.send, Some(Force::Constant(0)));
        assert_eq!(p.keep, Some(Tracked { force: Force::Constant(-7000), applied: 0.0 }), "the game's value is kept");
        assert_eq!(plan(0.5, Some(Force::Constant(-7000)), p.keep).send, Some(Force::Constant(-3500)));
    }

    #[test]
    fn mute_onset_resends_the_last_force_scaled() {
        let running = Some(Tracked { force: Force::Constant(9000), applied: 1.0 });
        let p = plan(0.0, None, running);
        assert_eq!(p.send, Some(Force::Constant(0)), "a direction-only update also zeroes the force");
        assert_eq!(p.keep, Some(Tracked { force: Force::Constant(9000), applied: 0.0 }));
        assert_eq!(plan(0.0, None, p.keep).send, None, "only once per factor");
        assert_eq!(plan(0.0, None, None).send, None, "an unknown effect is left alone");
    }

    #[test]
    fn game_force_is_restored_after_the_fade() {
        let muted = Some(Tracked { force: Force::Constant(3000), applied: 0.2 });
        let p = plan(1.0, None, muted);
        assert_eq!(p.send, Some(Force::Constant(3000)), "the game's own value, not a guess");
        assert_eq!(p.keep, Some(Tracked { force: Force::Constant(3000), applied: 1.0 }));
        assert_eq!(plan(1.0, None, p.keep).send, None, "then pass-through again");
        assert_eq!(plan(1.0, Some(Force::Constant(1)), muted).send, None, "the game's call sets it itself");
    }

    #[test]
    fn stale_factor_reads_as_full() {
        assert_eq!(factor_at(0.0, 1000, 1000), 0.0);
        assert_eq!(factor_at(0.4, 1000, 1000 + STALE_MS), 0.4);
        assert_eq!(factor_at(0.0, 1000, 1001 + STALE_MS), 1.0, "tick stopped: never stuck muted");
        assert_eq!(factor_at(f32::NAN, 0, 0), 1.0);
        assert_eq!(factor_at(-1.0, 0, 0), 0.0);
    }

    #[test]
    fn published_factor_roundtrip() {
        publish(0.4);
        assert!((effective_factor() - 0.4).abs() < 1e-6);
        publish(1.0);
        assert_eq!(effective_factor(), 1.0);
    }
}
