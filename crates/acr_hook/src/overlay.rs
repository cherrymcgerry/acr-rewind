//! Overlay state shared between the tick and the renderer, plus the hudhook (imgui) renderer
//! (status line, rewind-mode HUD and the settings panel) behind the `overlay` cargo feature.

use rewind_core::{ModeView, Timeline};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusKind {
    Starting,
    Active,
    ReadOnly,
    Inactive,
    Blocked,
    Disabled,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OverlayState {
    pub kind: StatusKind,
    pub status: String,
    /// In the rewind mode (frozen, scrubbing).
    pub rewinding: bool,
    /// Releasing after resume / cancel.
    pub resuming: bool,
    /// The release in progress is a cancel (back to the newest snapshot).
    pub cancelling: bool,
    pub timeline: Option<Timeline>,
    pub mode: Option<ModeView>,
}

impl Default for OverlayState {
    fn default() -> Self {
        Self {
            kind: StatusKind::Starting,
            status: "starting".into(),
            rewinding: false,
            resuming: false,
            cancelling: false,
            timeline: None,
            mode: None,
        }
    }
}

static STATE: Mutex<Option<OverlayState>> = Mutex::new(None);

/// Bumped once per presented frame by the renderer; used to run one tick per frame.
pub static PRESENT_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn publish(s: OverlayState) {
    if let Ok(mut g) = STATE.lock() {
        *g = Some(s);
    }
}

/// Updates only the status line (init / fatal paths).
pub fn set_status(kind: StatusKind, status: impl Into<String>) {
    if let Ok(mut g) = STATE.lock() {
        let s = g.get_or_insert_with(OverlayState::default);
        s.kind = kind;
        s.status = status.into();
        if kind != StatusKind::Active {
            s.rewinding = false;
            s.resuming = false;
            s.cancelling = false;
            s.mode = None;
        }
    }
}

pub fn snapshot() -> OverlayState {
    STATE.lock().ok().and_then(|g| g.clone()).unwrap_or_default()
}

pub fn present_count() -> u64 {
    PRESENT_COUNTER.load(Ordering::Relaxed)
}

/// Text of the bottom-left status line.
pub fn status_line(s: &OverlayState) -> String {
    let prefix = match s.kind {
        StatusKind::Starting => "ACR Rewind: starting",
        StatusKind::Active => "ACR Rewind: active",
        StatusKind::ReadOnly => "ACR Rewind: READ-ONLY validation",
        StatusKind::Inactive => "ACR Rewind: idle",
        StatusKind::Blocked => "ACR Rewind: OFF",
        StatusKind::Disabled => "ACR Rewind: DISABLED",
    };
    if s.status.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix} - {}", s.status)
    }
}

/// Banner text for the rewind mode / release.
pub fn banner(s: &OverlayState) -> Option<String> {
    if s.resuming {
        return Some(if s.cancelling { "CANCEL" } else { "RESUMING" }.into());
    }
    if !s.rewinding {
        return None;
    }
    let speed = s.mode.map_or(0.0, |m| m.speed);
    Some(if speed < -0.01 {
        format!("<<  REWIND  x{:.1}", -speed)
    } else if speed > 0.01 {
        format!("FORWARD  x{speed:.1}  >>")
    } else {
        "REWIND  ||".into()
    })
}

/// Offset of the cursor from the newest snapshot, e.g. "-7.4 s".
pub fn offset_text(tl: &Timeline) -> String {
    let off = tl.offset();
    if off < 0.05 {
        "0.0 s".into()
    } else {
        format!("-{off:.1} s")
    }
}

#[cfg(all(windows, feature = "overlay"))]
mod render {
    use super::*;
    use crate::input::{self, hub, ActionTracker, RawInput};
    use crate::settings;
    use hudhook::imgui::{self, BackendFlags, Condition, ConfigFlags, Key, TreeNodeFlags, WindowFlags};
    use hudhook::windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use hudhook::windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, MapVirtualKeyW, MAPVK_VK_TO_VSC};
    use hudhook::windows::Win32::UI::WindowsAndMessaging::{
        PostMessageW, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
    };
    use hudhook::{BeforeWndProc, ImguiRenderLoop, MessageFilter, RenderContext};
    use rewind_core::{Binding, Config, InputAction};
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    const AMBER: [f32; 4] = [1.0, 0.72, 0.1, 1.0];
    const GREEN: [f32; 4] = [0.45, 0.9, 0.45, 0.85];
    const RED: [f32; 4] = [1.0, 0.4, 0.35, 0.95];
    const GREY: [f32; 4] = [0.8, 0.8, 0.8, 0.75];
    const WHITE: [f32; 4] = [1.0, 1.0, 1.0, 0.9];

    const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

    /// Keys the game saw go down (and not up) while the panel was closed, and its window.
    static GAME_KEYS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
    static GAME_HWND: AtomicUsize = AtomicUsize::new(0);

    fn set_game_key(vk: usize, down: bool) {
        if vk == 0 || vk > 255 {
            return;
        }
        let bit = 1u64 << (vk % 64);
        if down {
            GAME_KEYS[vk / 64].fetch_or(bit, Ordering::Relaxed);
        } else {
            GAME_KEYS[vk / 64].fetch_and(!bit, Ordering::Relaxed);
        }
    }

    /// The panel filters keyboard messages, so keys released while it was open never reach
    /// the game as WM_KEYUP: post the missing key-ups when it closes.
    fn release_stuck_keys() {
        let hwnd = GAME_HWND.load(Ordering::Relaxed);
        if hwnd == 0 {
            return;
        }
        for vk in 1..=255usize {
            if GAME_KEYS[vk / 64].load(Ordering::Relaxed) & (1 << (vk % 64)) == 0 {
                continue;
            }
            // SAFETY: plain Win32 calls with valid arguments.
            unsafe {
                if GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 {
                    continue;
                }
                let scan = MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) as isize;
                let lparam = 0xC000_0001_isize | (scan << 16);
                let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_KEYUP, WPARAM(vk), LPARAM(lparam));
            }
            set_game_key(vk, false);
        }
    }

    struct Capture {
        action: InputAction,
        base: RawInput,
        started: Instant,
    }

    #[derive(Default)]
    pub struct RewindOverlay {
        tracker: ActionTracker,
        raw: RawInput,
        cfg: Option<Config>,
        cfg_version: u64,
        open: bool,
        draft: Option<Config>,
        capture: Option<Capture>,
        /// Inputs of the captured binding must be released before the panel reacts again.
        wait_release: Option<RawInput>,
        status: Option<(String, bool)>,
        nav_sent: u16,
    }

    fn color(kind: StatusKind) -> [f32; 4] {
        match kind {
            StatusKind::Active => GREEN,
            StatusKind::ReadOnly | StatusKind::Starting => AMBER,
            StatusKind::Inactive => GREY,
            StatusKind::Blocked | StatusKind::Disabled => RED,
        }
    }

    /// XInput buttons driving imgui's gamepad navigation.
    const NAV_KEYS: [(u16, Key); 10] = [
        (0x0001, Key::GamepadDpadUp),
        (0x0002, Key::GamepadDpadDown),
        (0x0004, Key::GamepadDpadLeft),
        (0x0008, Key::GamepadDpadRight),
        (0x1000, Key::GamepadFaceDown),
        (0x2000, Key::GamepadFaceRight),
        (0x4000, Key::GamepadFaceLeft),
        (0x8000, Key::GamepadFaceUp),
        (0x0100, Key::GamepadL1),
        (0x0200, Key::GamepadR1),
    ];

    impl RewindOverlay {
        fn set_open(&mut self, open: bool) {
            if open == self.open {
                return;
            }
            self.open = open;
            self.capture = None;
            self.wait_release = None;
            settings::PANEL_OPEN.store(open, Ordering::SeqCst);
            hub::poll_all_keys(open);
            if open {
                self.draft = self.cfg.clone();
                self.status = None;
            } else {
                release_stuck_keys();
            }
        }

        fn feed_nav(&mut self, ctx: &mut imgui::Context) {
            let io = ctx.io_mut();
            let nav = self.open && self.capture.is_none() && self.wait_release.is_none();
            io.config_flags.set(ConfigFlags::NAV_ENABLE_KEYBOARD | ConfigFlags::NAV_ENABLE_GAMEPAD, self.open);
            io.backend_flags.set(BackendFlags::HAS_GAMEPAD, self.open);
            io.mouse_draw_cursor = self.open;
            let buttons = if nav { self.raw.pads.iter().flatten().fold(0u16, |m, p| m | p.buttons) } else { 0 };
            for (bit, key) in NAV_KEYS {
                let (now, was) = (buttons & bit != 0, self.nav_sent & bit != 0);
                if now != was {
                    io.add_key_event(key, now);
                }
            }
            self.nav_sent = buttons;
        }

        fn status_ok(&mut self, msg: impl Into<String>) {
            self.status = Some((msg.into(), false));
        }

        fn status_err(&mut self, msg: impl Into<String>) {
            self.status = Some((msg.into(), true));
        }

        fn hud(&self, ui: &imgui::Ui, s: &OverlayState) {
            let [w, h] = ui.io().display_size;
            let flags = WindowFlags::NO_DECORATION
                | WindowFlags::NO_INPUTS
                | WindowFlags::NO_SAVED_SETTINGS
                | WindowFlags::NO_FOCUS_ON_APPEARING
                | WindowFlags::NO_NAV
                | WindowFlags::ALWAYS_AUTO_RESIZE;

            ui.window("##acr_rewind_status")
                .flags(flags)
                .bg_alpha(0.35)
                .position([10.0, h - 34.0], Condition::Always)
                .build(|| {
                    ui.text_colored(color(s.kind), status_line(s));
                });

            let Some(banner) = banner(s) else { return };
            ui.window("##acr_rewind_banner")
                .flags(flags)
                .bg_alpha(0.0)
                .position([w * 0.5, h * 0.12], Condition::Always)
                .position_pivot([0.5, 0.5])
                .build(|| {
                    ui.set_window_font_scale(2.6);
                    ui.text_colored(AMBER, &banner);
                });

            let Some(tl) = s.timeline else { return };
            let dl = ui.get_foreground_draw_list();
            let bar_w = (w * 0.5).max(200.0);
            let (x0, y0) = ((w - bar_w) * 0.5, h * 0.12 + 36.0);
            let (x1, y1) = (x0 + bar_w, y0 + 10.0);
            dl.add_rect([x0, y0], [x1, y1], [0.0, 0.0, 0.0, 0.55]).filled(true).rounding(4.0).build();
            let cx = x0 + bar_w * tl.fraction() as f32;
            dl.add_rect([x0, y0], [cx, y1], [1.0, 0.72, 0.1, 0.8]).filled(true).rounding(4.0).build();
            dl.add_rect([cx - 2.0, y0 - 5.0], [cx + 2.0, y1 + 5.0], [1.0, 1.0, 1.0, 0.95]).filled(true).build();
            dl.add_text([x0, y1 + 6.0], WHITE, offset_text(&tl));
            dl.add_text([x1 - 70.0, y1 + 6.0], [1.0, 1.0, 1.0, 0.6], format!("{:.0} s buffer", tl.newest - tl.oldest));
            if !s.rewinding {
                return;
            }
            let mut lines = Vec::new();
            if s.mode.is_some_and(|m| m.resume_pending) {
                lines.push("release the triggers to resume".to_owned());
            }
            if let Some(cfg) = &self.cfg {
                lines.extend(input::mode_hints(&cfg.bindings, &self.raw));
            }
            let mut y = y1 + 26.0;
            for l in lines {
                let tw = ui.calc_text_size(&l)[0];
                dl.add_text([(w - tw) * 0.5, y], WHITE, &l);
                y += 18.0;
            }
        }

        fn panel(&mut self, ui: &imgui::Ui) {
            let Some(mut draft) = self.draft.take() else {
                self.set_open(false);
                return;
            };
            let [w, h] = ui.io().display_size;
            let mut keep_open = true;
            let mut action: Option<PanelAction> = None;
            ui.window("ACR Rewind settings")
                .opened(&mut keep_open)
                .size([640.0, 560.0], Condition::FirstUseEver)
                .position([w * 0.5, h * 0.5], Condition::FirstUseEver)
                .position_pivot([0.5, 0.5])
                .flags(WindowFlags::NO_SAVED_SETTINGS | WindowFlags::NO_COLLAPSE)
                .build(|| {
                    action = self.panel_body(ui, &mut draft);
                });
            self.draft = Some(draft);
            match action {
                Some(PanelAction::Save) => self.save(),
                Some(PanelAction::Revert) => match settings::revert() {
                    Ok(m) => {
                        self.cfg = settings::current();
                        self.draft = self.cfg.clone();
                        self.status_ok(m);
                    }
                    Err(e) => self.status_err(e),
                },
                Some(PanelAction::Close) => keep_open = false,
                None => {}
            }
            if !keep_open {
                self.set_open(false);
                return;
            }
            if let Some(d) = self.draft.clone() {
                if self.cfg.as_ref() != Some(&d) {
                    match settings::update(d) {
                        Ok(()) => self.cfg = settings::current(),
                        Err(e) => self.status_err(format!("not applied: {e}")),
                    }
                }
            }
        }

        fn save(&mut self) {
            match settings::save() {
                Ok(m) => self.status_ok(m),
                Err(e) => self.status_err(format!("save failed: {e}")),
            }
        }

        fn panel_body(&mut self, ui: &imgui::Ui, d: &mut Config) -> Option<PanelAction> {
            let mut action = None;
            ui.text_disabled(
                "F8 (or your open_settings binding) toggles this panel. Rewind is paused while it is open.",
            );
            if let Some((msg, err)) = &self.status {
                ui.text_colored(if *err { RED } else { GREEN }, msg);
            }
            if ui.button("Save") {
                action = Some(PanelAction::Save);
            }
            ui.same_line();
            if ui.button("Revert to file") {
                action = Some(PanelAction::Revert);
            }
            ui.same_line();
            if ui.button("Defaults") {
                let def = Config::default();
                d.scrub = def.scrub;
                d.mode = def.mode;
                d.bindings = def.bindings;
                d.ffb = def.ffb;
            }
            ui.same_line();
            if ui.button("Close") {
                action = Some(PanelAction::Close);
            }
            if settings::dirty() {
                ui.same_line();
                ui.text_colored(AMBER, "unsaved changes");
            }
            ui.separator();

            if ui.collapsing_header("Bindings", TreeNodeFlags::DEFAULT_OPEN) {
                self.bindings_section(ui, d);
            }
            if ui.collapsing_header("Scrub speed", TreeNodeFlags::DEFAULT_OPEN) {
                ui.text_disabled("Triggers / pedals: speed grows with depth (history seconds per second)");
                ui.slider("max analog speed", 0.5, 12.0, &mut d.scrub.analog_max_speed);
                ui.slider("deadzone", 0.0, 0.5, &mut d.scrub.analog_deadzone);
                ui.slider("curve (1 = linear)", 0.5, 3.0, &mut d.scrub.analog_curve);
                ui.slider("hold boost per second", 0.0, 3.0, &mut d.scrub.analog_hold_boost);
                ui.slider("max hold boost", 1.0, 4.0, &mut d.scrub.analog_boost_max);
                ui.text_disabled("Keys / buttons: speed accelerates while held");
                ui.slider("initial speed", 0.1, 5.0, &mut d.scrub.initial_speed);
                ui.slider("max speed", 0.5, 12.0, &mut d.scrub.max_speed);
                ui.slider("acceleration", 0.0, 10.0, &mut d.scrub.acceleration);
            }
            if ui.collapsing_header("Rewind mode", TreeNodeFlags::empty()) {
                ui.slider("resume delay (s)", 0.0, 1.0, &mut d.scrub.resume_delay);
                ui.checkbox("holding rewind scrubs back", &mut d.mode.hold_rewind_scrubs);
                ui.checkbox("resume waits for released triggers", &mut d.mode.resume_requires_release);
                ui.slider("auto-cancel after (s, 0 = never)", 0.0, 300.0, &mut d.mode.max_time_s);
            }
            if ui.collapsing_header("Wheel force feedback", TreeNodeFlags::empty()) {
                ui.checkbox("mute during rewind and resume", &mut d.ffb.mute_during_rewind);
                ui.slider("fade back in over (s)", 0.0, 3.0, &mut d.ffb.fade_in_s);
                ui.text_disabled(crate::ffb::status_line());
            }
            if ui.collapsing_header("Devices", TreeNodeFlags::empty()) {
                self.devices_section(ui);
            }
            action
        }

        fn bindings_section(&mut self, ui: &imgui::Ui, d: &mut Config) {
            for a in InputAction::ALL {
                let _id = ui.push_id(a.key());
                ui.text(a.label());
                let mut remove = None;
                for (i, b) in d.bindings.get(a).iter().enumerate() {
                    ui.bullet();
                    ui.same_line();
                    ui.text(b.label());
                    let others = d.bindings.conflicts(a, b);
                    if !others.is_empty() {
                        ui.same_line();
                        let names: Vec<&str> = others.iter().map(|o| o.key()).collect();
                        ui.text_colored(AMBER, format!("(also {})", names.join(", ")));
                    }
                    ui.same_line();
                    if ui.small_button(format!("remove##{i}")) {
                        remove = Some(i);
                    }
                }
                if let Some(i) = remove {
                    d.bindings.get_mut(a).remove(i);
                }
                if d.bindings.get(a).is_empty() {
                    ui.text_disabled("  (unbound)");
                }
                match &self.capture {
                    Some(c) if c.action == a => {
                        let left = CAPTURE_TIMEOUT.saturating_sub(c.started.elapsed()).as_secs();
                        ui.text_colored(
                            AMBER,
                            format!(
                                "  press a key, pad button, trigger, wheel button or pedal... (Esc cancels, {left} s)"
                            ),
                        );
                    }
                    Some(_) => {}
                    None => {
                        if ui.small_button("add binding") {
                            self.capture = Some(Capture { action: a, base: self.raw.clone(), started: Instant::now() });
                            self.status = None;
                        }
                    }
                }
                ui.spacing();
            }
        }

        fn devices_section(&self, ui: &imgui::Ui) {
            let mut any = false;
            for (i, p) in self.raw.pads.iter().enumerate() {
                if p.is_some() {
                    any = true;
                    ui.text(format!("XInput pad {}", i + 1));
                }
            }
            for j in &self.raw.joys {
                any = true;
                let id = j.vid_pid().unwrap_or_else(|| j.product.clone());
                let twin = if j.xinput { ", XInput (bind it as pad:)" } else { "" };
                ui.text(format!(
                    "{} [{id}] - {} buttons, {} axes, {} POV{twin}",
                    j.name, j.n_buttons, j.n_axes, j.n_povs
                ));
            }
            if !any {
                ui.text_disabled("no controllers detected");
            }
            if !self.raw.focused {
                ui.text_disabled("(the game window is not focused: inputs read as idle)");
            }
        }

        /// Runs the capture in progress; returns true while it consumes input.
        fn poll_capture(&mut self) {
            if let Some(base) = &self.wait_release {
                let mut b = base.clone();
                input::capture_rebase(&mut b, &self.raw);
                if b.keys == [0; 4] && input::detect_capture(&RawInput::default(), &b).is_none() {
                    self.wait_release = None;
                } else {
                    self.wait_release = Some(b);
                }
            }
            let Some(c) = &mut self.capture else { return };
            if self.raw.key(0x1B) || c.started.elapsed() > CAPTURE_TIMEOUT {
                let a = c.action;
                self.capture = None;
                self.wait_release = Some(self.raw.clone());
                self.status_ok(format!("{}: capture cancelled", a.key()));
                return;
            }
            if let Some(b) = input::detect_capture(&c.base, &self.raw) {
                let a = c.action;
                self.capture = None;
                self.wait_release = Some(self.raw.clone());
                self.add_binding(a, b);
            } else {
                input::capture_rebase(&mut c.base, &self.raw);
            }
        }

        fn add_binding(&mut self, a: InputAction, b: Binding) {
            let Some(d) = self.draft.as_mut() else { return };
            let label = b.label();
            let others = d.bindings.conflicts(a, &b);
            if !d.bindings.add(a, b) {
                self.status_ok(format!("{label} is already bound to {}", a.key()));
            } else if others.is_empty() {
                self.status_ok(format!("{label} added to {}", a.key()));
            } else {
                let names: Vec<&str> = others.iter().map(|o| o.key()).collect();
                self.status_err(format!("{label} added to {} (also bound to {})", a.key(), names.join(", ")));
            }
        }
    }

    enum PanelAction {
        Save,
        Revert,
        Close,
    }

    impl ImguiRenderLoop for RewindOverlay {
        fn before_render<'a>(&'a mut self, ctx: &mut imgui::Context, _render_context: &'a mut dyn RenderContext) {
            self.raw = hub::take(hub::CONSUMER_UI);
            let v = settings::version();
            if self.cfg.is_none() || v != self.cfg_version {
                self.cfg_version = v;
                self.cfg = settings::current();
                if self.open && self.capture.is_none() {
                    self.draft = self.cfg.clone();
                }
            }
            if self.open {
                self.poll_capture();
            }
            if let Some(cfg) = &self.cfg {
                let enabled = self.capture.is_none() && self.wait_release.is_none();
                if self.tracker.update(&cfg.bindings, &self.raw, enabled).open_settings {
                    let open = !self.open;
                    self.set_open(open);
                }
            }
            self.feed_nav(ctx);
        }

        fn render(&mut self, ui: &mut imgui::Ui) {
            PRESENT_COUNTER.fetch_add(1, Ordering::Relaxed);
            let s = snapshot();
            self.hud(ui, &s);
            if self.open {
                self.panel(ui);
            }
        }

        fn before_wnd_proc(&self, hwnd: HWND, umsg: u32, wparam: WPARAM, _lparam: LPARAM) -> BeforeWndProc {
            GAME_HWND.store(hwnd.0 as usize, Ordering::Relaxed);
            if !settings::panel_open() {
                match umsg {
                    WM_KEYDOWN | WM_SYSKEYDOWN => set_game_key(wparam.0, true),
                    WM_KEYUP | WM_SYSKEYUP => set_game_key(wparam.0, false),
                    _ => {}
                }
            }
            BeforeWndProc::Continue
        }

        fn message_filter(&self, _io: &imgui::Io) -> MessageFilter {
            if settings::panel_open() {
                MessageFilter::InputAll
            } else {
                MessageFilter::empty()
            }
        }
    }

    /// Installs the DX12 / DX11 hooks. Must not be called from DllMain.
    pub fn start(hinstance: usize, backend: &str) -> Result<(), String> {
        use hudhook::hooks::dx11::ImguiDx11Hooks;
        use hudhook::hooks::dx12::ImguiDx12Hooks;
        use hudhook::windows::Win32::Foundation::HINSTANCE;
        let builder = hudhook::Hudhook::builder();
        let builder = match backend {
            "dx11" => builder.with::<ImguiDx11Hooks>(RewindOverlay::default()),
            _ => builder.with::<ImguiDx12Hooks>(RewindOverlay::default()),
        };
        builder
            .with_hmodule(HINSTANCE(hinstance as *mut _))
            .build()
            .apply()
            .map_err(|e| format!("hudhook apply failed: {e:?}"))
    }
}

/// Starts the overlay renderer (no-op error when built without the `overlay` feature).
#[cfg(all(windows, feature = "overlay"))]
pub fn start(hinstance: usize, backend: &str) -> Result<(), String> {
    render::start(hinstance, backend)
}

#[cfg(not(all(windows, feature = "overlay")))]
pub fn start(_hinstance: usize, _backend: &str) -> Result<(), String> {
    Err("built without the `overlay` feature".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_lines() {
        let mut s = OverlayState::default();
        assert_eq!(status_line(&s), "ACR Rewind: starting - starting");
        s.kind = StatusKind::Blocked;
        s.status = "online server 'EU'".into();
        assert_eq!(status_line(&s), "ACR Rewind: OFF - online server 'EU'");
        s.status.clear();
        s.kind = StatusKind::Active;
        assert_eq!(status_line(&s), "ACR Rewind: active");
    }

    #[test]
    fn banners_and_offsets() {
        let mut s = OverlayState { kind: StatusKind::Active, ..Default::default() };
        assert_eq!(banner(&s), None);
        s.rewinding = true;
        assert_eq!(banner(&s).unwrap(), "REWIND  ||");
        s.mode = Some(ModeView { speed: -2.25, ..Default::default() });
        assert_eq!(banner(&s).unwrap(), "<<  REWIND  x2.2");
        s.mode = Some(ModeView { speed: 1.0, ..Default::default() });
        assert_eq!(banner(&s).unwrap(), "FORWARD  x1.0  >>");
        s.rewinding = false;
        s.resuming = true;
        assert_eq!(banner(&s).unwrap(), "RESUMING");
        s.cancelling = true;
        assert_eq!(banner(&s).unwrap(), "CANCEL");

        let tl = Timeline { oldest: 0.0, newest: 30.0, cursor: 22.6 };
        assert_eq!(offset_text(&tl), "-7.4 s");
        assert_eq!(offset_text(&Timeline { cursor: 30.0, ..tl }), "0.0 s");
    }

    #[test]
    fn publish_and_set_status() {
        publish(OverlayState {
            kind: StatusKind::Active,
            rewinding: true,
            mode: Some(ModeView::default()),
            ..Default::default()
        });
        assert!(snapshot().rewinding);
        set_status(StatusKind::Disabled, "x");
        let s = snapshot();
        assert_eq!(s.kind, StatusKind::Disabled);
        assert!(!s.rewinding && s.mode.is_none());
    }
}
