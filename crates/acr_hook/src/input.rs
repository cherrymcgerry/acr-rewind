//! Action inputs from the keyboard (GetAsyncKeyState), XInput pads and DirectInput game
//! controllers (wheels, button boxes, pedals).
//!
//! A dedicated thread ([`hub`], Windows only) polls every device at ~200 Hz into a [`RawInput`]
//! and latches presses shorter than a frame. Everything below that is platform independent:
//! evaluating [`Binding`]s against a [`RawInput`], edge detection per action
//! ([`ActionTracker`]), detecting the input to bind in the settings panel ([`detect_capture`])
//! and the overlay hints ([`mode_hints`]). Input only counts while a window of this process
//! is in the foreground.

use rewind_core::bindings::{AxisDir, JoyBinding, JoyInput, PadInput, PovDir, StickAxis, JOY_AXES, JOY_POVS};
use rewind_core::{Binding, BindingsConfig, InputAction, ModeInput, ScrubInput};

/// XInput stick resting noise (XINPUT_GAMEPAD_LEFT_THUMB_DEADZONE / 32767).
pub const STICK_DEADZONE: f32 = 0.24;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PadState {
    pub buttons: u16,
    /// Triggers, `0..=1`.
    pub lt: f32,
    pub rt: f32,
    /// LX, LY, RX, RY in `-1..=1`.
    pub sticks: [f32; 4],
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct JoyState {
    /// Product GUID (upper case, no braces): stable per model, used by bindings.
    pub product: String,
    /// Instance GUID: changes with the USB port; identifies a device within one session.
    pub instance: String,
    pub name: String,
    /// The DirectInput view of an XInput pad (bind it with `pad:` instead).
    pub xinput: bool,
    /// X, Y, Z, RX, RY, RZ, S0, S1 in `-1..=1` (absent axes read 0).
    pub axes: [f32; 8],
    /// POV hats in hundredths of a degree clockwise from up; `None` = centred.
    pub povs: [Option<u32>; 4],
    /// Button bits, bit n = button n.
    pub buttons: u128,
    pub n_axes: u32,
    pub n_buttons: u32,
    pub n_povs: u32,
}

impl JoyState {
    pub fn button(&self, n: u8) -> bool {
        n < 128 && self.buttons & (1u128 << n) != 0
    }

    /// "VID_0EB7 PID_0020" for HID product GUIDs ("PIDVID" GUIDs), else empty.
    pub fn vid_pid(&self) -> Option<String> {
        if !self.product.ends_with("-0000-0000-0000-504944564944") {
            return None;
        }
        let d1 = u32::from_str_radix(&self.product[..8], 16).ok()?;
        Some(format!("VID_{:04X} PID_{:04X}", d1 & 0xFFFF, d1 >> 16))
    }
}

/// One poll of every input device.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RawInput {
    /// Virtual-key bitset (bit vk).
    pub keys: [u64; 4],
    pub pads: [Option<PadState>; 4],
    pub joys: Vec<JoyState>,
    /// A window of this process is in the foreground.
    pub focused: bool,
}

impl RawInput {
    pub fn key(&self, vk: u8) -> bool {
        self.keys[usize::from(vk >> 6)] & (1u64 << (vk & 63)) != 0
    }

    pub fn set_key(&mut self, vk: u8, down: bool) {
        let (w, b) = (usize::from(vk >> 6), 1u64 << (vk & 63));
        if down {
            self.keys[w] |= b;
        } else {
            self.keys[w] &= !b;
        }
    }

    pub fn any_pad(&self) -> bool {
        self.pads.iter().any(Option::is_some)
    }

    /// Adds the presses latched since the last read (short taps between two frames).
    pub fn merge_latch(&mut self, l: &Latch) {
        for (k, lk) in self.keys.iter_mut().zip(l.keys) {
            *k |= lk;
        }
        for (p, lb) in self.pads.iter_mut().zip(l.pad_buttons) {
            if let Some(p) = p {
                p.buttons |= lb;
            }
        }
        for j in &mut self.joys {
            if let Some((_, b)) = l.joy_buttons.iter().find(|(i, _)| *i == j.instance) {
                j.buttons |= b;
            }
        }
    }
}

/// Digital inputs seen down since the latch was last taken.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Latch {
    pub keys: [u64; 4],
    pub pad_buttons: [u16; 4],
    pub joy_buttons: Vec<(String, u128)>,
}

impl Latch {
    pub fn add(&mut self, r: &RawInput) {
        for (k, rk) in self.keys.iter_mut().zip(r.keys) {
            *k |= rk;
        }
        for (b, p) in self.pad_buttons.iter_mut().zip(&r.pads) {
            *b |= p.map_or(0, |p| p.buttons);
        }
        for j in &r.joys {
            match self.joy_buttons.iter_mut().find(|(i, _)| *i == j.instance) {
                Some((_, b)) => *b |= j.buttons,
                None => self.joy_buttons.push((j.instance.clone(), j.buttons)),
            }
        }
    }
}

// ---- Evaluation ------------------------------------------------------------------------

fn stick_value(v: f32, positive: bool) -> f32 {
    let v = if positive { v } else { -v };
    if v <= STICK_DEADZONE {
        0.0
    } else {
        ((v - STICK_DEADZONE) / (1.0 - STICK_DEADZONE)).min(1.0)
    }
}

fn pov_matches(pov: Option<u32>, dir: PovDir) -> bool {
    let Some(a) = pov.filter(|&a| a < 36000) else {
        return false;
    };
    let d = a.abs_diff(dir.centidegrees());
    d.min(36000 - d) <= 6750
}

fn axis_value(v: f32, dir: AxisDir) -> f32 {
    let v = v.clamp(-1.0, 1.0);
    match dir {
        AxisDir::Pos => v.max(0.0),
        AxisDir::Neg => (-v).max(0.0),
        AxisDir::FullPos => (v + 1.0) * 0.5,
        AxisDir::FullNeg => (1.0 - v) * 0.5,
    }
}

/// Devices a joystick binding applies to: same product (or any), preferring an exact name
/// match when several devices qualify. XInput pads only match an explicit product.
fn joy_devices<'a>(j: &'a JoyBinding, raw: &'a RawInput) -> impl Iterator<Item = &'a JoyState> + 'a {
    let product_ok = move |d: &&JoyState| match &j.product {
        Some(p) => d.product == *p,
        None => !d.xinput,
    };
    let name_hit =
        j.name.as_ref().is_some_and(|n| raw.joys.iter().filter(product_ok).any(|d| d.name.eq_ignore_ascii_case(n)));
    raw.joys
        .iter()
        .filter(product_ok)
        .filter(move |d| !name_hit || j.name.as_ref().is_some_and(|n| d.name.eq_ignore_ascii_case(n)))
}

/// Current value of a binding in `0..=1` (digital inputs are 0 or 1).
pub fn binding_value(b: &Binding, raw: &RawInput) -> f32 {
    let pads = || raw.pads.iter().flatten();
    let v = match b {
        Binding::Key(vk) => f32::from(u8::from(raw.key(*vk))),
        Binding::Pad(PadInput::Button(m)) => f32::from(u8::from(pads().any(|p| p.buttons & m != 0))),
        Binding::Pad(PadInput::Trigger { right, .. }) => {
            pads().map(|p| if *right { p.rt } else { p.lt }).fold(0.0, f32::max)
        }
        Binding::Pad(PadInput::Stick { axis, positive, .. }) => {
            pads().map(|p| stick_value(p.sticks[axis.index()], *positive)).fold(0.0, f32::max)
        }
        Binding::Joy(j) => joy_devices(j, raw)
            .map(|d| match j.input {
                JoyInput::Button(n) => f32::from(u8::from(d.button(n))),
                JoyInput::Pov { index, dir } => {
                    f32::from(u8::from(pov_matches(d.povs[usize::from(index) % JOY_POVS], dir)))
                }
                JoyInput::Axis { axis, dir, .. } => axis_value(d.axes[usize::from(axis) % 8], dir),
            })
            .fold(0.0, f32::max),
    };
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

pub fn binding_pressed(b: &Binding, raw: &RawInput) -> bool {
    binding_value(b, raw) >= b.threshold()
}

/// State of one action this frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ActionLevel {
    /// Any binding pressed (analog ones past their threshold).
    pub held: bool,
    /// Strongest analog binding value (raw, before the scrub deadzone).
    pub analog: f32,
    /// Any digital binding pressed.
    pub digital: bool,
}

pub fn action_level(list: &[Binding], raw: &RawInput) -> ActionLevel {
    if !raw.focused {
        return ActionLevel::default();
    }
    let mut l = ActionLevel::default();
    for b in list {
        let v = binding_value(b, raw);
        if v >= b.threshold() {
            l.held = true;
        }
        if b.is_analog() {
            l.analog = l.analog.max(v);
        } else if v >= b.threshold() {
            l.digital = true;
        }
    }
    l
}

/// Turns held levels into per-frame [`ModeInput`] with rising edges. While disabled (settings
/// panel open) nothing is reported, and anything still held when it is enabled again stays
/// ignored until released, so closing the panel never fires an action.
#[derive(Clone, Debug, Default)]
pub struct ActionTracker {
    prev: [bool; 6],
    suppressed: [bool; 6],
}

/// Analog travel below which a suppressed action counts as released (resting noise).
const RELEASED_ANALOG: f32 = 0.05;

fn idx(a: InputAction) -> usize {
    InputAction::ALL.iter().position(|&x| x == a).unwrap_or(0)
}

/// Inputs of one frame: the mode input plus whether open_settings was pressed.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ActionFrame {
    pub mode: ModeInput,
    pub open_settings: bool,
}

impl ActionTracker {
    pub fn update(&mut self, b: &BindingsConfig, raw: &RawInput, enabled: bool) -> ActionFrame {
        let mut levels = [ActionLevel::default(); 6];
        for a in InputAction::ALL {
            let i = idx(a);
            let mut l = action_level(b.get(a), raw);
            let active = l.held || l.analog > RELEASED_ANALOG;
            if !enabled {
                self.suppressed[i] |= active;
            }
            if self.suppressed[i] {
                if !active {
                    self.suppressed[i] = false;
                }
                l = ActionLevel::default();
            }
            levels[i] = l;
        }
        let mut pressed = [false; 6];
        for i in 0..6 {
            pressed[i] = levels[i].held && !self.prev[i];
            self.prev[i] = levels[i].held;
        }
        let lv = |a: InputAction| levels[idx(a)];
        let scrub = |a: InputAction| ScrubInput { analog: f64::from(lv(a).analog), digital: lv(a).digital };
        ActionFrame {
            mode: ModeInput {
                rewind_pressed: pressed[idx(InputAction::Rewind)],
                rewind_held: lv(InputAction::Rewind).held,
                resume_pressed: pressed[idx(InputAction::Resume)],
                cancel_pressed: pressed[idx(InputAction::Cancel)],
                back: scrub(InputAction::ScrubBack),
                forward: scrub(InputAction::ScrubForward),
            },
            open_settings: pressed[idx(InputAction::OpenSettings)],
        }
    }
}

// ---- Capture (settings panel) ----------------------------------------------------------

/// Keys never captured: left / right / middle mouse (they operate the panel itself) and Esc
/// (cancels the capture).
pub const CAPTURE_SKIP_KEYS: [u8; 4] = [0x01, 0x02, 0x04, 0x1B];

/// Drops released inputs from the capture base, so a key / button that was held when the
/// capture started (e.g. the one that clicked "Add binding") can be captured once pressed again.
pub fn capture_rebase(base: &mut RawInput, now: &RawInput) {
    for (b, n) in base.keys.iter_mut().zip(now.keys) {
        *b &= n;
    }
    for (b, n) in base.pads.iter_mut().zip(&now.pads) {
        match (b.as_mut(), n) {
            (Some(b), Some(n)) => {
                b.buttons &= n.buttons;
                b.lt = b.lt.min(n.lt);
                b.rt = b.rt.min(n.rt);
                for (bs, ns) in b.sticks.iter_mut().zip(n.sticks) {
                    if ns.abs() < bs.abs() {
                        *bs = ns;
                    }
                }
            }
            (Some(_), None) => *b = None,
            _ => {}
        }
    }
    for b in &mut base.joys {
        if let Some(n) = now.joys.iter().find(|n| n.instance == b.instance) {
            b.buttons &= n.buttons;
            for (bp, np) in b.povs.iter_mut().zip(n.povs) {
                if np.is_none_or(|a| a >= 36000) {
                    *bp = None;
                }
            }
        }
    }
    for n in &now.joys {
        if !base.joys.iter().any(|b| b.instance == n.instance) {
            base.joys.push(n.clone());
        }
    }
}

/// The first input that changed between `base` (when "Add binding" was clicked) and `now`.
/// Keyboard first, then XInput, then DirectInput (an XInput pad's DirectInput twin is skipped).
pub fn detect_capture(base: &RawInput, now: &RawInput) -> Option<Binding> {
    for vk in 1..=255u8 {
        if CAPTURE_SKIP_KEYS.contains(&vk) {
            continue;
        }
        if now.key(vk) && !base.key(vk) {
            return Some(Binding::Key(vk));
        }
    }
    for (i, p) in now.pads.iter().enumerate() {
        let Some(p) = p else { continue };
        let b = base.pads[i].unwrap_or_default();
        let new = p.buttons & !b.buttons;
        if new != 0 {
            return Some(Binding::Pad(PadInput::Button(1 << new.trailing_zeros())));
        }
        for (right, v, bv) in [(false, p.lt, b.lt), (true, p.rt, b.rt)] {
            if v > 0.6 && bv < 0.3 {
                return Some(Binding::Pad(PadInput::Trigger { right, threshold: None }));
            }
        }
        for axis in StickAxis::ALL {
            let (v, bv) = (p.sticks[axis.index()], b.sticks[axis.index()]);
            if v.abs() > 0.7 && bv.abs() < 0.4 {
                return Some(Binding::Pad(PadInput::Stick { axis, positive: v > 0.0, threshold: None }));
            }
        }
    }
    for j in now.joys.iter().filter(|j| !j.xinput) {
        let Some(b) = base.joys.iter().find(|b| b.instance == j.instance) else {
            continue;
        };
        let dev = |input| {
            Some(Binding::Joy(JoyBinding {
                product: Some(j.product.clone()),
                name: Some(j.name.replace(':', " ").trim().to_owned()).filter(|n| !n.is_empty()),
                input,
            }))
        };
        let new = j.buttons & !b.buttons;
        if new != 0 {
            return dev(JoyInput::Button(new.trailing_zeros() as u8));
        }
        for (k, (pov, bpov)) in j.povs.iter().zip(b.povs).enumerate() {
            if let Some(a) = pov.filter(|&a| a < 36000) {
                if bpov.is_none_or(|ba| ba >= 36000) {
                    let dir = PovDir::ALL[(((a + 4500) / 9000) % 4) as usize];
                    return dev(JoyInput::Pov { index: k as u8, dir });
                }
            }
        }
        for (k, (&v, &bv)) in j.axes.iter().zip(&b.axes).enumerate().take(JOY_AXES.len()) {
            if (v - bv).abs() < 0.6 {
                continue;
            }
            let dir = if bv.abs() > 0.8 {
                if bv < 0.0 {
                    AxisDir::FullPos
                } else {
                    AxisDir::FullNeg
                }
            } else if v > bv {
                AxisDir::Pos
            } else {
                AxisDir::Neg
            };
            return dev(JoyInput::Axis { axis: k as u8, dir, threshold: None });
        }
    }
    None
}

// ---- Overlay hints ---------------------------------------------------------------------

fn first<'a>(list: &'a [Binding], f: &dyn Fn(&Binding) -> bool) -> Option<&'a Binding> {
    list.iter().find(|b| f(b))
}

fn hint_line(b: &BindingsConfig, f: &dyn Fn(&Binding) -> bool) -> Option<String> {
    let label = |a: InputAction| first(b.get(a), f).map(Binding::short_label);
    let mut parts = Vec::new();
    match (label(InputAction::ScrubBack), label(InputAction::ScrubForward)) {
        (Some(x), Some(y)) => parts.push(format!("{x} / {y} scrub")),
        (Some(x), None) => parts.push(format!("{x} back")),
        (None, Some(y)) => parts.push(format!("{y} forward")),
        (None, None) => {}
    }
    if let Some(r) = label(InputAction::Resume) {
        parts.push(format!("{r} resume"));
    }
    let cancel: Vec<String> =
        [label(InputAction::Cancel), label(InputAction::Rewind)].into_iter().flatten().fold(Vec::new(), |mut v, l| {
            if !v.contains(&l) {
                v.push(l);
            }
            v
        });
    if !cancel.is_empty() {
        parts.push(format!("{} cancel", cancel.join(" / ")));
    }
    (!parts.is_empty()).then(|| parts.join(" \u{b7} "))
}

/// Control hints shown in the rewind mode: one line per input kind that is bound (pads only
/// when one is connected, wheels per connected device that has bindings).
pub fn mode_hints(b: &BindingsConfig, raw: &RawInput) -> Vec<String> {
    let mut out = Vec::new();
    if raw.any_pad() {
        if let Some(l) = hint_line(b, &|x| matches!(x, Binding::Pad(_))) {
            out.push(l);
        }
    }
    let mut seen: Vec<&str> = Vec::new();
    for d in raw.joys.iter().filter(|d| !d.xinput) {
        if seen.contains(&d.instance.as_str()) {
            continue;
        }
        seen.push(&d.instance);
        let single = RawInput { joys: vec![d.clone()], ..Default::default() };
        let on_device = |x: &Binding| match x {
            Binding::Joy(j) => joy_devices(j, &single).next().is_some(),
            _ => false,
        };
        if let Some(l) = hint_line(b, &on_device) {
            out.push(format!("{}: {l}", d.name));
        }
    }
    if let Some(l) = hint_line(b, &|x| matches!(x, Binding::Key(_))) {
        out.push(format!("Keys: {l}"));
    }
    out
}

#[cfg(windows)]
pub mod hub {
    //! The polling thread. COM objects (DirectInput) live and die on it.

    use super::{Latch, PadState, RawInput};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    pub const CONSUMER_TICK: usize = 0;
    pub const CONSUMER_UI: usize = 1;
    const POLL_PERIOD: Duration = Duration::from_millis(5);
    const XINPUT_PROBE: Duration = Duration::from_secs(1);
    const DINPUT_ENUM: Duration = Duration::from_secs(3);

    struct Shared {
        current: Mutex<(RawInput, [Latch; 2])>,
        all_keys: AtomicBool,
        wanted_keys: Mutex<Vec<u8>>,
    }

    static HUB: OnceLock<Shared> = OnceLock::new();

    /// Starts the polling thread once.
    pub fn start() {
        if HUB.get().is_some() {
            return;
        }
        let _ = HUB.set(Shared {
            current: Mutex::new((RawInput::default(), [Latch::default(), Latch::default()])),
            all_keys: AtomicBool::new(false),
            wanted_keys: Mutex::new(Vec::new()),
        });
        let _ = std::thread::Builder::new().name("acr-rewind-input".into()).spawn(|| {
            let _ = std::panic::catch_unwind(run);
        });
    }

    /// Latest state plus everything pressed since `consumer` last called `take`.
    pub fn take(consumer: usize) -> RawInput {
        let Some(h) = HUB.get() else {
            return RawInput::default();
        };
        let Ok(mut g) = h.current.lock() else {
            return RawInput::default();
        };
        let (cur, latches) = &mut *g;
        let mut r = cur.clone();
        r.merge_latch(&latches[consumer]);
        latches[consumer] = Latch::default();
        r
    }

    /// Latest state without consuming latches.
    pub fn peek() -> RawInput {
        HUB.get().and_then(|h| h.current.lock().ok().map(|g| g.0.clone())).unwrap_or_default()
    }

    /// Keys polled normally (the bound ones); everything while the settings panel is open.
    pub fn set_wanted_keys(keys: Vec<u8>) {
        if let Some(h) = HUB.get() {
            if let Ok(mut w) = h.wanted_keys.lock() {
                *w = keys;
            }
        }
    }

    pub fn poll_all_keys(on: bool) {
        if let Some(h) = HUB.get() {
            h.all_keys.store(on, Ordering::Relaxed);
        }
    }

    fn game_has_focus() -> bool {
        use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
        // SAFETY: plain Win32 queries with valid out-pointers.
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0.is_null() {
                return false;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            pid == std::process::id()
        }
    }

    fn key_down(vk: u8) -> bool {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        // SAFETY: GetAsyncKeyState has no preconditions.
        (unsafe { GetAsyncKeyState(i32::from(vk)) } as u16 & 0x8000) != 0
    }

    fn read_pad(i: u32) -> Option<PadState> {
        use windows::Win32::UI::Input::XboxController::{XInputGetState, XINPUT_STATE};
        let mut st = XINPUT_STATE::default();
        // SAFETY: `st` is a valid out-pointer.
        if unsafe { XInputGetState(i, &mut st) } != 0 {
            return None;
        }
        let g = st.Gamepad;
        let s = |v: i16| (f32::from(v) / 32767.0).clamp(-1.0, 1.0);
        Some(PadState {
            buttons: g.wButtons.0,
            lt: f32::from(g.bLeftTrigger) / 255.0,
            rt: f32::from(g.bRightTrigger) / 255.0,
            sticks: [s(g.sThumbLX), s(g.sThumbLY), s(g.sThumbRX), s(g.sThumbRY)],
        })
    }

    fn run() {
        let mut dinput = super::dinput::DInput::new();
        if let Err(e) = &dinput {
            tracing::warn!("input: DirectInput unavailable ({e}); wheels / button boxes will not work");
        }
        let mut pad_present = [false; 4];
        let mut last_pad_probe: Option<Instant> = None;
        let mut last_enum: Option<Instant> = None;
        let mut logged_devices = String::new();
        loop {
            let t0 = Instant::now();
            let Some(h) = HUB.get() else { return };
            let mut raw = RawInput { focused: game_has_focus(), ..Default::default() };
            if h.all_keys.load(Ordering::Relaxed) {
                for vk in 1..=255u8 {
                    raw.set_key(vk, key_down(vk));
                }
            } else if let Ok(w) = h.wanted_keys.lock() {
                for &vk in w.iter() {
                    raw.set_key(vk, key_down(vk));
                }
            }
            let probe = last_pad_probe.is_none_or(|t| t.elapsed() >= XINPUT_PROBE);
            if probe {
                last_pad_probe = Some(t0);
            }
            for (i, (pad, present)) in raw.pads.iter_mut().zip(&mut pad_present).enumerate() {
                if *present || probe {
                    *pad = read_pad(i as u32);
                    *present = pad.is_some();
                }
            }
            if let Ok(di) = &mut dinput {
                if last_enum.is_none_or(|t| t.elapsed() >= DINPUT_ENUM) {
                    last_enum = Some(t0);
                    di.enumerate();
                    let list = di.describe();
                    if list != logged_devices {
                        tracing::info!("input devices: {}", if list.is_empty() { "none (DirectInput)" } else { &list });
                        logged_devices = list;
                    }
                }
                raw.joys = di.poll();
            }
            if let Ok(mut g) = h.current.lock() {
                for l in &mut g.1 {
                    l.add(&raw);
                }
                g.0 = raw;
            }
            if let Some(rest) = POLL_PERIOD.checked_sub(t0.elapsed()) {
                std::thread::sleep(rest);
            }
        }
    }
}

#[cfg(windows)]
mod dinput {
    //! DirectInput8 game controllers, read non-exclusively in the background (no window, no
    //! force feedback, the game keeps its own access).

    use super::JoyState;
    use std::ffi::c_void;
    use windows::core::{Interface, GUID};
    use windows::Win32::Devices::HumanInterfaceDevice::*;
    use windows::Win32::Foundation::{BOOL, HINSTANCE};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;

    /// Our device-state layout: the position part of DIJOYSTATE2.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct JoyData {
        axes: [i32; 8],
        povs: [u32; 4],
        buttons: [u8; 128],
    }

    const AXIS_RANGE: i32 = 32767;

    struct Format {
        _objs: Box<[DIOBJECTDATAFORMAT]>,
        df: DIDATAFORMAT,
    }

    static AXIS_GUIDS: [GUID; 8] =
        [GUID_XAxis, GUID_YAxis, GUID_ZAxis, GUID_RxAxis, GUID_RyAxis, GUID_RzAxis, GUID_Slider, GUID_Slider];

    fn format() -> Format {
        let any = DIDFT_ANYINSTANCE | 0x8000_0000; // DIDFT_OPTIONAL
        let mut objs = Vec::with_capacity(8 + 4 + 128);
        for (i, g) in AXIS_GUIDS.iter().enumerate() {
            objs.push(DIOBJECTDATAFORMAT {
                pguid: g,
                dwOfs: (i * 4) as u32,
                dwType: DIDFT_AXIS | any,
                dwFlags: DIDOI_ASPECTPOSITION,
            });
        }
        for i in 0..4 {
            objs.push(DIOBJECTDATAFORMAT {
                pguid: &GUID_POV,
                dwOfs: (32 + i * 4) as u32,
                dwType: DIDFT_POV | any,
                dwFlags: 0,
            });
        }
        for i in 0..128 {
            objs.push(DIOBJECTDATAFORMAT {
                pguid: std::ptr::null(),
                dwOfs: (48 + i) as u32,
                dwType: DIDFT_BUTTON | any,
                dwFlags: 0,
            });
        }
        let mut objs = objs.into_boxed_slice();
        let df = DIDATAFORMAT {
            dwSize: std::mem::size_of::<DIDATAFORMAT>() as u32,
            dwObjSize: std::mem::size_of::<DIOBJECTDATAFORMAT>() as u32,
            dwFlags: DIDF_ABSAXIS,
            dwDataSize: std::mem::size_of::<JoyData>() as u32,
            dwNumObjs: objs.len() as u32,
            rgodf: objs.as_mut_ptr(),
        };
        Format { _objs: objs, df }
    }

    pub fn guid_string(g: &GUID) -> String {
        let d = g.data4;
        format!(
            "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
            g.data1, g.data2, g.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
        )
    }

    fn wide(s: &[u16]) -> String {
        let n = s.iter().position(|&c| c == 0).unwrap_or(s.len());
        String::from_utf16_lossy(&s[..n]).trim().to_owned()
    }

    struct Device {
        dev: IDirectInputDevice8W,
        state: JoyState,
        ok: bool,
    }

    pub struct DInput {
        di: IDirectInput8W,
        format: Format,
        devices: Vec<Device>,
    }

    unsafe extern "system" fn collect(inst: *mut DIDEVICEINSTANCEW, ctx: *mut c_void) -> BOOL {
        // SAFETY: DirectInput passes a valid instance; ctx is the Vec passed to EnumDevices.
        unsafe {
            let list = &mut *(ctx as *mut Vec<DIDEVICEINSTANCEW>);
            list.push(*inst);
        }
        BOOL(1) // DIENUM_CONTINUE
    }

    impl DInput {
        pub fn new() -> Result<Self, String> {
            // SAFETY: standard DirectInput8 creation with the module handle of the exe.
            unsafe {
                let hm = GetModuleHandleW(None).map_err(|e| e.to_string())?;
                let mut out: *mut c_void = std::ptr::null_mut();
                DirectInput8Create(
                    HINSTANCE(hm.0),
                    DIRECTINPUT_VERSION,
                    &IDirectInput8W::IID,
                    &mut out,
                    None::<&windows::core::IUnknown>,
                )
                .map_err(|e| e.to_string())?;
                if out.is_null() {
                    return Err("DirectInput8Create returned null".into());
                }
                Ok(Self { di: IDirectInput8W::from_raw(out), format: format(), devices: Vec::new() })
            }
        }

        /// Re-enumerates attached game controllers, keeping devices already open.
        pub fn enumerate(&mut self) {
            let mut found: Vec<DIDEVICEINSTANCEW> = Vec::new();
            // SAFETY: callback and context outlive the synchronous enumeration.
            let r = unsafe {
                self.di.EnumDevices(
                    DI8DEVCLASS_GAMECTRL,
                    Some(collect),
                    &mut found as *mut _ as *mut c_void,
                    DIEDFL_ATTACHEDONLY,
                )
            };
            if r.is_err() {
                return;
            }
            let ids: Vec<String> = found.iter().map(|i| guid_string(&i.guidInstance)).collect();
            self.devices.retain(|d| d.ok && ids.contains(&d.state.instance));
            for inst in &found {
                let id = guid_string(&inst.guidInstance);
                if self.devices.iter().any(|d| d.state.instance == id) {
                    continue;
                }
                match self.open(inst) {
                    Ok(d) => self.devices.push(d),
                    Err(e) => tracing::debug!("input: cannot open {}: {e}", wide(&inst.tszProductName)),
                }
            }
        }

        fn open(&mut self, inst: &DIDEVICEINSTANCEW) -> Result<Device, String> {
            // SAFETY: COM calls on a freshly created device with correctly sized structs.
            unsafe {
                let mut dev: Option<IDirectInputDevice8W> = None;
                self.di
                    .CreateDevice(&inst.guidInstance, &mut dev, None::<&windows::core::IUnknown>)
                    .map_err(|e| e.to_string())?;
                let dev = dev.ok_or("CreateDevice returned null")?;
                dev.SetDataFormat(&mut self.format.df).map_err(|e| format!("SetDataFormat: {e}"))?;
                let mut range = DIPROPRANGE {
                    diph: DIPROPHEADER {
                        dwSize: std::mem::size_of::<DIPROPRANGE>() as u32,
                        dwHeaderSize: std::mem::size_of::<DIPROPHEADER>() as u32,
                        dwObj: 0,
                        dwHow: DIPH_DEVICE,
                    },
                    lMin: -AXIS_RANGE,
                    lMax: AXIS_RANGE,
                };
                let _ = dev.SetProperty(&DIPROP_RANGE, &mut range.diph);
                let mut path = DIPROPGUIDANDPATH {
                    diph: DIPROPHEADER {
                        dwSize: std::mem::size_of::<DIPROPGUIDANDPATH>() as u32,
                        dwHeaderSize: std::mem::size_of::<DIPROPHEADER>() as u32,
                        dwObj: 0,
                        dwHow: DIPH_DEVICE,
                    },
                    ..Default::default()
                };
                let xinput = dev.GetProperty(&DIPROP_GUIDANDPATH, &mut path.diph).is_ok()
                    && wide(&path.wszPath).to_ascii_uppercase().contains("IG_");
                let mut caps = DIDEVCAPS { dwSize: std::mem::size_of::<DIDEVCAPS>() as u32, ..Default::default() };
                let _ = dev.GetCapabilities(&mut caps);
                let _ = dev.Acquire();
                let name = wide(&inst.tszProductName);
                let name = if name.is_empty() { wide(&inst.tszInstanceName) } else { name };
                Ok(Device {
                    dev,
                    ok: true,
                    state: JoyState {
                        product: guid_string(&inst.guidProduct),
                        instance: guid_string(&inst.guidInstance),
                        name,
                        xinput,
                        n_axes: caps.dwAxes,
                        n_buttons: caps.dwButtons,
                        n_povs: caps.dwPOVs,
                        ..Default::default()
                    },
                })
            }
        }

        pub fn poll(&mut self) -> Vec<JoyState> {
            let mut out = Vec::with_capacity(self.devices.len());
            for d in &mut self.devices {
                let mut data = JoyData { axes: [0; 8], povs: [u32::MAX; 4], buttons: [0; 128] };
                // SAFETY: `data` matches the data format set on the device.
                let read = unsafe {
                    let _ = d.dev.Poll();
                    let size = std::mem::size_of::<JoyData>() as u32;
                    let mut r = d.dev.GetDeviceState(size, &mut data as *mut _ as *mut c_void);
                    if r.is_err() && d.dev.Acquire().is_ok() {
                        let _ = d.dev.Poll();
                        r = d.dev.GetDeviceState(size, &mut data as *mut _ as *mut c_void);
                    }
                    r
                };
                if read.is_err() {
                    d.ok = false;
                    continue;
                }
                let s = &mut d.state;
                for (a, v) in s.axes.iter_mut().zip(data.axes) {
                    *a = (v as f32 / AXIS_RANGE as f32).clamp(-1.0, 1.0);
                }
                // Absent hats may read 0 (= up), so only the reported ones count.
                for (k, (p, v)) in s.povs.iter_mut().zip(data.povs).enumerate() {
                    *p = ((k as u32) < s.n_povs && v & 0xFFFF != 0xFFFF && v < 36000).then_some(v);
                }
                s.buttons = data
                    .buttons
                    .iter()
                    .enumerate()
                    .filter(|(_, &b)| b & 0x80 != 0)
                    .fold(0u128, |m, (i, _)| m | (1u128 << i));
                out.push(s.clone());
            }
            out
        }

        pub fn describe(&self) -> String {
            self.devices
                .iter()
                .map(|d| {
                    let s = &d.state;
                    format!(
                        "{} [{}{}] {} axes, {} buttons, {} POV{}",
                        s.name,
                        s.product,
                        s.vid_pid().map_or_else(String::new, |v| format!(" {v}")),
                        s.n_axes,
                        s.n_buttons,
                        s.n_povs,
                        if s.xinput { " (XInput pad: bind it as pad:)" } else { "" }
                    )
                })
                .collect::<Vec<_>>()
                .join("; ")
        }
    }

    impl Drop for Device {
        fn drop(&mut self) {
            // SAFETY: releasing our own acquisition.
            unsafe {
                let _ = self.dev.Unacquire();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> Binding {
        s.parse().unwrap()
    }

    fn focused() -> RawInput {
        RawInput { focused: true, ..Default::default() }
    }

    fn wheel(product: &str, instance: &str, name: &str) -> JoyState {
        JoyState {
            product: product.into(),
            instance: instance.into(),
            name: name.into(),
            n_axes: 3,
            n_buttons: 20,
            n_povs: 1,
            ..Default::default()
        }
    }

    const FANATEC: &str = "0EB71BAD-0000-0000-0000-504944564944";
    const MOZA: &str = "00061FC9-0000-0000-0000-504944564944";

    #[test]
    fn key_bitset() {
        let mut r = RawInput::default();
        for vk in [1u8, 63, 64, 127, 128, 255] {
            assert!(!r.key(vk));
            r.set_key(vk, true);
            assert!(r.key(vk));
        }
        r.set_key(64, false);
        assert!(!r.key(64) && r.key(63));
    }

    #[test]
    fn keys_and_pad_buttons() {
        let mut r = focused();
        r.set_key(b'R', true);
        assert_eq!(binding_value(&b("key:R"), &r), 1.0);
        assert_eq!(binding_value(&b("key:T"), &r), 0.0);
        assert_eq!(binding_value(&b("pad:Back"), &r), 0.0, "no pad connected");
        r.pads[2] = Some(PadState { buttons: 0x0020 | 0x1000, ..Default::default() });
        assert!(binding_pressed(&b("pad:Back"), &r), "any pad slot");
        assert!(binding_pressed(&b("pad:Cross"), &r));
        assert!(!binding_pressed(&b("pad:B"), &r));
    }

    #[test]
    fn triggers_and_sticks() {
        let mut r = focused();
        r.pads[0] = Some(PadState { lt: 0.4, rt: 0.9, sticks: [-0.2, 0.62, 1.0, -1.0], ..Default::default() });
        r.pads[1] = Some(PadState { lt: 0.7, ..Default::default() });
        assert_eq!(binding_value(&b("pad:LT"), &r), 0.7, "strongest pad wins");
        assert_eq!(binding_value(&b("pad:RT"), &r), 0.9);
        assert!(binding_pressed(&b("pad:LT"), &r), "0.7 >= default 0.5");
        assert!(!binding_pressed(&b("pad:LT@0.8"), &r));
        assert_eq!(binding_value(&b("pad:LX-"), &r), 0.0, "inside the stick deadzone");
        assert!((binding_value(&b("pad:LY+"), &r) - 0.5).abs() < 1e-6);
        assert_eq!(binding_value(&b("pad:LY-"), &r), 0.0);
        assert_eq!(binding_value(&b("pad:RX+"), &r), 1.0);
        assert_eq!(binding_value(&b("pad:RY-"), &r), 1.0);
    }

    #[test]
    fn joystick_buttons_povs_axes() {
        let mut w = wheel(FANATEC, "I1", "FANATEC CSL DD");
        w.buttons = 1 << 5 | 1 << 127;
        w.povs[0] = Some(4500); // up-right diagonal
        w.axes = [0.5, -1.0, 1.0, -0.25, 0.0, 0.0, 0.0, 0.0];
        let mut r = focused();
        r.joys.push(w);
        assert!(binding_pressed(&b("joy:*:button5"), &r));
        assert!(binding_pressed(&b("joy:*:button127"), &r));
        assert!(!binding_pressed(&b("joy:*:button4"), &r));
        assert!(binding_pressed(&b("joy:*:pov0up"), &r), "diagonal counts for up");
        assert!(binding_pressed(&b("joy:*:pov0right"), &r), "and for right");
        assert!(!binding_pressed(&b("joy:*:pov0down"), &r));
        assert!(!binding_pressed(&b("joy:*:pov1up"), &r), "other hat centred");
        assert_eq!(binding_value(&b("joy:*:axisX+"), &r), 0.5);
        assert_eq!(binding_value(&b("joy:*:axisX-"), &r), 0.0);
        assert_eq!(binding_value(&b("joy:*:axisY-full"), &r), 1.0, "pedal pressed to the minimum");
        assert_eq!(binding_value(&b("joy:*:axisY+full"), &r), 0.0);
        assert_eq!(binding_value(&b("joy:*:axisZ+full"), &r), 1.0);
        assert_eq!(binding_value(&b("joy:*:axisRX-"), &r), 0.25);
        assert_eq!(binding_value(&b("joy:*:axisRY+full"), &r), 0.5, "centred axis = half travel");
    }

    #[test]
    fn joystick_device_matching() {
        let mut fan = wheel(FANATEC, "I1", "FANATEC CSL DD");
        fan.buttons = 1 << 1;
        let mut moza = wheel(MOZA, "I2", "MOZA R9");
        moza.buttons = 1 << 2;
        let mut pad = wheel("028E045E-0000-0000-0000-504944564944", "I3", "Controller (Xbox 360)");
        pad.xinput = true;
        pad.buttons = 1 << 3;
        let mut r = focused();
        r.joys = vec![fan, moza, pad];
        let fb = |i: u8| format!("joy:{FANATEC}/whatever:button{i}");
        assert!(binding_pressed(&b(&fb(1)), &r), "product match, name only advisory");
        assert!(!binding_pressed(&b(&fb(2)), &r), "Moza's button is not the Fanatec's");
        assert!(binding_pressed(&b("joy:*:button2"), &r), "any device");
        assert!(!binding_pressed(&b("joy:*:button3"), &r), "XInput twin ignored by wildcards");
        assert!(binding_pressed(&b("joy:028E045E-0000-0000-0000-504944564944:button3"), &r), "unless named");
        // Two identical products: the name picks one.
        let mut a = wheel(FANATEC, "IA", "Pedals");
        a.buttons = 1;
        let b2 = wheel(FANATEC, "IB", "Rim");
        let mut r = focused();
        r.joys = vec![a, b2];
        assert!(!binding_pressed(&b(&format!("joy:{FANATEC}/Rim:button0")), &r));
        assert!(binding_pressed(&b(&format!("joy:{FANATEC}/pedals:button0")), &r));
        assert!(binding_pressed(&b(&format!("joy:{FANATEC}/Gone:button0")), &r), "unknown name: any of the product");
    }

    #[test]
    fn unfocused_reports_nothing() {
        let mut r = focused();
        r.set_key(b'R', true);
        assert!(action_level(&[b("key:R")], &r).held);
        r.focused = false;
        assert_eq!(action_level(&[b("key:R")], &r), ActionLevel::default());
    }

    #[test]
    fn action_levels_split_analog_and_digital() {
        let mut r = focused();
        r.set_key(0x25, true);
        r.pads[0] = Some(PadState { lt: 0.3, ..Default::default() });
        let l = action_level(&[b("key:Left"), b("pad:LT")], &r);
        assert!(l.held && l.digital);
        assert!((l.analog - 0.3).abs() < 1e-6);
        r.set_key(0x25, false);
        let l = action_level(&[b("key:Left"), b("pad:LT")], &r);
        assert!(!l.held && !l.digital, "0.3 < threshold");
        assert!((l.analog - 0.3).abs() < 1e-6, "still scrubs analog");
    }

    #[test]
    fn tracker_edges_and_scrub_values() {
        let cfg = BindingsConfig::default();
        let mut t = ActionTracker::default();
        let mut r = focused();
        r.pads[0] = Some(PadState::default());
        assert_eq!(t.update(&cfg, &r, true), ActionFrame::default());
        r.pads[0].as_mut().unwrap().buttons = 0x0020; // Back
        let f = t.update(&cfg, &r, true);
        assert!(f.mode.rewind_pressed && f.mode.rewind_held);
        let f = t.update(&cfg, &r, true);
        assert!(!f.mode.rewind_pressed && f.mode.rewind_held, "edge only once");
        let p = r.pads[0].as_mut().unwrap();
        p.buttons = 0x1000; // A
        p.lt = 0.8;
        p.rt = 0.1;
        let f = t.update(&cfg, &r, true);
        assert!(f.mode.resume_pressed && !f.mode.rewind_held);
        assert!((f.mode.back.analog - 0.8).abs() < 1e-6 && !f.mode.back.digital);
        assert!((f.mode.forward.analog - 0.1).abs() < 1e-6);
        r.set_key(0x27, true); // Right arrow
        let f = t.update(&cfg, &r, true);
        assert!(f.mode.forward.digital);
        r.set_key(0x77, true); // F8
        assert!(t.update(&cfg, &r, true).open_settings);
        r.set_key(0x08, true); // Backspace
        assert!(t.update(&cfg, &r, true).mode.cancel_pressed);
    }

    #[test]
    fn tracker_suppresses_while_disabled_and_until_released() {
        let cfg = BindingsConfig::default();
        let mut t = ActionTracker::default();
        let mut r = focused();
        r.set_key(0x0D, true); // Enter held while the panel is open
        r.set_key(0x25, true); // Left held
        assert_eq!(t.update(&cfg, &r, false), ActionFrame::default());
        // Panel closed with the keys still down: no edge, no scrub.
        let f = t.update(&cfg, &r, true);
        assert!(!f.mode.resume_pressed && !f.mode.back.digital, "{f:?}");
        // Released, then pressed again: works.
        r.set_key(0x0D, false);
        r.set_key(0x25, false);
        t.update(&cfg, &r, true);
        r.set_key(0x0D, true);
        r.set_key(0x25, true);
        let f = t.update(&cfg, &r, true);
        assert!(f.mode.resume_pressed && f.mode.back.digital);
    }

    #[test]
    fn latch_catches_taps_between_frames() {
        let mut l = Latch::default();
        let mut down = focused();
        down.set_key(b'R', true);
        down.pads[1] = Some(PadState { buttons: 0x20, ..Default::default() });
        let mut w = wheel(FANATEC, "I1", "W");
        w.buttons = 1 << 9;
        down.joys.push(w.clone());
        l.add(&down);
        // The frame reads after everything was released again.
        let mut up = focused();
        up.pads[1] = Some(PadState::default());
        w.buttons = 0;
        up.joys.push(w);
        up.merge_latch(&l);
        assert!(up.key(b'R'));
        assert_eq!(up.pads[1].unwrap().buttons, 0x20);
        assert!(up.joys[0].button(9));
        let cfg = BindingsConfig::default();
        let mut t = ActionTracker::default();
        assert!(t.update(&cfg, &up, true).mode.rewind_pressed);
    }

    #[test]
    fn capture_keys_pads_and_wheels() {
        let base = focused();
        let mut now = base.clone();
        now.set_key(0x01, true); // left click on the panel: ignored
        now.set_key(0x1B, true); // Esc: ignored (cancels the capture)
        assert_eq!(detect_capture(&base, &now), None);
        now.set_key(0x77, true);
        assert_eq!(detect_capture(&base, &now), Some(b("key:F8")));
        // Already held when the capture started: not captured.
        assert_eq!(detect_capture(&now, &now), None);

        let mut base = focused();
        base.pads[0] = Some(PadState { lt: 0.1, sticks: [0.1, 0.0, 0.0, 0.0], ..Default::default() });
        let mut now = base.clone();
        now.pads[0].as_mut().unwrap().buttons = 0x0001 | 0x2000;
        assert_eq!(detect_capture(&base, &now), Some(b("pad:DPadUp")), "lowest new bit");
        let mut now = base.clone();
        now.pads[0].as_mut().unwrap().rt = 0.9;
        assert_eq!(detect_capture(&base, &now), Some(b("pad:RT")));
        let mut now = base.clone();
        now.pads[0].as_mut().unwrap().sticks[0] = -0.95;
        assert_eq!(detect_capture(&base, &now), Some(b("pad:LX-")));

        let mut base = focused();
        let mut w = wheel(FANATEC, "I1", "CSL: DD");
        w.axes = [0.0, 1.0, -1.0, 0.1, 0.0, 0.0, 0.0, 0.0]; // wheel centred, two pedals at rest
        base.joys.push(w);
        let mut xpad = wheel("028E045E-0000-0000-0000-504944564944", "IX", "Xbox");
        xpad.xinput = true;
        base.joys.push(xpad);
        let dev = format!("joy:{FANATEC}/CSL  DD");
        let cap = |f: &dyn Fn(&mut JoyState)| {
            let mut now = base.clone();
            f(&mut now.joys[0]);
            detect_capture(&base, &now).map(|b| b.to_string())
        };
        assert_eq!(cap(&|j| j.buttons = 1 << 12), Some(format!("{dev}:button12")), "':' stripped from the name");
        assert_eq!(cap(&|j| j.povs[0] = Some(27000)), Some(format!("{dev}:pov0left")));
        assert_eq!(cap(&|j| j.povs[0] = Some(31500)), Some(format!("{dev}:pov0up")), "diagonal rounds");
        assert_eq!(cap(&|j| j.axes[1] = -0.9), Some(format!("{dev}:axisY-full")), "pedal resting at max");
        assert_eq!(cap(&|j| j.axes[2] = 0.2), Some(format!("{dev}:axisZ+full")), "pedal resting at min");
        assert_eq!(cap(&|j| j.axes[3] = -0.7), Some(format!("{dev}:axisRX-")), "centred axis");
        assert_eq!(cap(&|j| j.axes[0] = 0.3), None, "small wheel turn ignored");
        let mut now = base.clone();
        now.joys[1].buttons = 1;
        assert_eq!(detect_capture(&base, &now), None, "XInput twin never captured");
        // A device plugged in during the capture has no baseline: ignored.
        let mut now = base.clone();
        let mut n = wheel(MOZA, "NEW", "Box");
        n.buttons = 1;
        now.joys.push(n);
        assert_eq!(detect_capture(&base, &now), None);
        // Captured bindings work against the same state.
        let mut now = base.clone();
        now.joys[0].axes[1] = -1.0;
        let got = detect_capture(&base, &now).unwrap();
        assert_eq!(binding_value(&got, &now), 1.0);
        assert_eq!(binding_value(&got, &base), 0.0);
    }

    #[test]
    fn capture_rebase_allows_re_pressing_held_inputs() {
        // Enter / pad A activated "Add binding" and are still held.
        let mut base = focused();
        base.set_key(0x0D, true);
        base.pads[0] = Some(PadState { buttons: 0x1000, rt: 0.8, ..Default::default() });
        let mut w = wheel(FANATEC, "I1", "Wheel");
        w.buttons = 1 << 3;
        w.povs[0] = Some(0);
        base.joys.push(w);
        let held = base.clone();
        assert_eq!(detect_capture(&base, &held), None);
        capture_rebase(&mut base, &held);
        assert_eq!(detect_capture(&base, &held), None, "still held: still ignored");

        let mut released = focused();
        released.pads[0] = Some(PadState::default());
        let mut w = wheel(FANATEC, "I1", "Wheel");
        w.povs[0] = Some(u32::MAX);
        released.joys.push(w);
        capture_rebase(&mut base, &released);
        assert_eq!(detect_capture(&base, &held), Some(b("key:Enter")), "pressed again");
        let mut again = released.clone();
        again.pads[0].as_mut().unwrap().buttons = 0x1000;
        assert_eq!(detect_capture(&base, &again), Some(b("pad:A")));
        let mut again = released.clone();
        again.pads[0].as_mut().unwrap().rt = 0.9;
        assert_eq!(detect_capture(&base, &again), Some(b("pad:RT")));
        let mut again = released.clone();
        again.joys[0].buttons = 1 << 3;
        assert_eq!(detect_capture(&base, &again).unwrap().to_string(), format!("joy:{FANATEC}/Wheel:button3"));
        let mut again = released.clone();
        again.joys[0].povs[0] = Some(0);
        assert_eq!(detect_capture(&base, &again).unwrap().to_string(), format!("joy:{FANATEC}/Wheel:pov0up"));

        // A device that appears mid-capture gets a baseline on the next frame.
        let mut plugged = released.clone();
        plugged.joys.push(wheel(MOZA, "NEW", "Box"));
        capture_rebase(&mut base, &plugged);
        let mut press = plugged.clone();
        press.joys[1].buttons = 1;
        assert!(detect_capture(&base, &press).unwrap().to_string().ends_with("Box:button0"));
    }

    #[test]
    fn hints_follow_bindings_and_devices() {
        let mut cfg = BindingsConfig::default();
        let mut r = focused();
        assert_eq!(mode_hints(&cfg, &r), ["Keys: Left / Right scrub \u{b7} Enter resume \u{b7} Backspace / R cancel"]);
        r.pads[0] = Some(PadState::default());
        assert_eq!(
            mode_hints(&cfg, &r)[0],
            "LT (L2) / RT (R2) scrub \u{b7} A (Cross) resume \u{b7} Back (Create) cancel"
        );
        r.joys.push(wheel(FANATEC, "I1", "CSL DD"));
        assert_eq!(mode_hints(&cfg, &r).len(), 2, "wheel without bindings: no line");
        cfg.rewind.push(b(&format!("joy:{FANATEC}/CSL DD:button0")));
        cfg.resume.push(b(&format!("joy:{FANATEC}/CSL DD:button1")));
        cfg.scrub_back.push(b("joy:*:pov0left"));
        cfg.scrub_forward.push(b("joy:*:pov0right"));
        let h = mode_hints(&cfg, &r);
        assert_eq!(h[1], "CSL DD: POV left / POV right scrub \u{b7} Btn 2 resume \u{b7} Btn 1 cancel");
        cfg.cancel.clear();
        cfg.rewind.retain(|x| !matches!(x, Binding::Key(_)));
        assert!(mode_hints(&cfg, &r).last().unwrap().ends_with("Enter resume"), "{:?}", mode_hints(&cfg, &r));
    }

    #[test]
    fn vid_pid_from_product_guid() {
        let w = wheel(FANATEC, "I", "W");
        assert_eq!(w.vid_pid().as_deref(), Some("VID_1BAD PID_0EB7"));
        let w = wheel("12345678-1234-1234-1234-123456789ABC", "I", "W");
        assert_eq!(w.vid_pid(), None);
    }
}
