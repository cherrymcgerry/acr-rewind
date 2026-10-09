//! Input bindings: which physical input drives which [`InputAction`].
//!
//! Every binding has a compact text form used in `acr-rewind.toml`:
//!
//! | form | meaning |
//! |---|---|
//! | `key:R`, `key:F8`, `key:Left`, `key:0x5B` | keyboard virtual key |
//! | `pad:A`, `pad:Back`, `pad:DPadUp` | XInput button (any of the 4 pads) |
//! | `pad:LT`, `pad:RT@0.3` | XInput trigger, analog (optional digital threshold) |
//! | `pad:LX-`, `pad:RY+` | XInput stick half-axis, analog |
//! | `joy:<product guid>/<name>:button5` | DirectInput button (0-based index) |
//! | `joy:<product guid>/<name>:pov0up` | DirectInput POV hat direction (up/down/left/right) |
//! | `joy:<product guid>/<name>:axisZ+` | DirectInput axis, centre to max (stick) |
//! | `joy:<product guid>/<name>:axisZ-full@0.6` | DirectInput axis over its full range, max to min (pedal) |
//!
//! The device part may be `*` (any device), a product GUID, `*/<name>` or `<guid>/<name>`.
//! Devices are matched by product GUID (stable across reconnects and USB ports); the name
//! only disambiguates identical products and is shown in the UI.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// Default analog-to-digital threshold (fraction of travel) when none is given.
pub const DEFAULT_THRESHOLD: f32 = 0.5;

/// What a binding triggers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum InputAction {
    /// Enter the rewind mode; pressed again inside the mode it cancels.
    Rewind,
    /// Leave the mode from the cursor (history after it is discarded).
    Resume,
    /// Leave the mode from the newest snapshot, as if nothing happened.
    Cancel,
    ScrubBack,
    ScrubForward,
    /// Toggle the in-game settings panel.
    OpenSettings,
}

impl InputAction {
    pub const ALL: [InputAction; 6] = [
        InputAction::Rewind,
        InputAction::Resume,
        InputAction::Cancel,
        InputAction::ScrubBack,
        InputAction::ScrubForward,
        InputAction::OpenSettings,
    ];

    /// Key in the `[bindings]` table.
    pub fn key(self) -> &'static str {
        match self {
            Self::Rewind => "rewind",
            Self::Resume => "resume",
            Self::Cancel => "cancel",
            Self::ScrubBack => "scrub_back",
            Self::ScrubForward => "scrub_forward",
            Self::OpenSettings => "open_settings",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Rewind => "Rewind (press again to cancel)",
            Self::Resume => "Resume from cursor",
            Self::Cancel => "Cancel (back to live)",
            Self::ScrubBack => "Scrub back",
            Self::ScrubForward => "Scrub forward",
            Self::OpenSettings => "Open settings",
        }
    }

    pub fn from_key(k: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.key() == k)
    }
}

/// XInput stick half-axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StickAxis {
    LX,
    LY,
    RX,
    RY,
}

impl StickAxis {
    pub const ALL: [StickAxis; 4] = [StickAxis::LX, StickAxis::LY, StickAxis::RX, StickAxis::RY];

    pub fn index(self) -> usize {
        self as usize
    }

    fn name(self) -> &'static str {
        match self {
            Self::LX => "LX",
            Self::LY => "LY",
            Self::RX => "RX",
            Self::RY => "RY",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PadInput {
    /// `wButtons` mask (exactly one bit).
    Button(u16),
    /// `right == false` is LT.
    Trigger {
        right: bool,
        threshold: Option<u8>,
    },
    Stick {
        axis: StickAxis,
        positive: bool,
        threshold: Option<u8>,
    },
}

/// Direction of a POV hat.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PovDir {
    Up,
    Right,
    Down,
    Left,
}

impl PovDir {
    pub const ALL: [PovDir; 4] = [PovDir::Up, PovDir::Right, PovDir::Down, PovDir::Left];

    fn name(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Right => "right",
            Self::Down => "down",
            Self::Left => "left",
        }
    }

    /// Centre angle in hundredths of a degree (DirectInput convention, 0 = up, clockwise).
    pub fn centidegrees(self) -> u32 {
        self as u32 * 9000
    }
}

/// How a DirectInput axis maps to a `[0, 1]` value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AxisDir {
    /// Centre to maximum (sticks, wheel rotation).
    Pos,
    /// Centre to minimum.
    Neg,
    /// Minimum to maximum (a pedal resting at the minimum).
    FullPos,
    /// Maximum to minimum (a pedal resting at the maximum).
    FullNeg,
}

impl AxisDir {
    fn suffix(self) -> &'static str {
        match self {
            Self::Pos => "+",
            Self::Neg => "-",
            Self::FullPos => "+full",
            Self::FullNeg => "-full",
        }
    }
}

/// DirectInput axis names in `DIJOYSTATE2` order.
pub const JOY_AXES: [&str; 8] = ["X", "Y", "Z", "RX", "RY", "RZ", "S0", "S1"];
pub const JOY_BUTTONS: usize = 128;
pub const JOY_POVS: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JoyInput {
    /// 0-based button index (Windows' controller panel shows it 1-based).
    Button(u8),
    Pov {
        index: u8,
        dir: PovDir,
    },
    Axis {
        axis: u8,
        dir: AxisDir,
        threshold: Option<u8>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct JoyBinding {
    /// Product GUID, upper case without braces. `None` matches any device.
    pub product: Option<String>,
    /// Product name; disambiguates identical products, shown in the UI.
    pub name: Option<String>,
    pub input: JoyInput,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Binding {
    /// Windows virtual-key code.
    Key(u8),
    Pad(PadInput),
    Joy(JoyBinding),
}

impl Binding {
    /// Analog bindings report a continuous value (triggers, sticks, axes).
    pub fn is_analog(&self) -> bool {
        matches!(
            self,
            Binding::Pad(PadInput::Trigger { .. } | PadInput::Stick { .. })
                | Binding::Joy(JoyBinding { input: JoyInput::Axis { .. }, .. })
        )
    }

    /// Value at or above which an analog binding counts as pressed.
    pub fn threshold(&self) -> f32 {
        let t = match self {
            Binding::Pad(PadInput::Trigger { threshold, .. } | PadInput::Stick { threshold, .. }) => *threshold,
            Binding::Joy(JoyBinding { input: JoyInput::Axis { threshold, .. }, .. }) => *threshold,
            _ => None,
        };
        t.map_or(DEFAULT_THRESHOLD, |p| f32::from(p) / 100.0)
    }

    /// Short human-readable label for the overlay and the settings panel.
    pub fn label(&self) -> String {
        match self {
            Binding::Key(vk) => key_name(*vk),
            Binding::Pad(p) => pad_label(p),
            Binding::Joy(j) => {
                let dev = j.name.clone().unwrap_or_else(|| "any controller".into());
                let what = match j.input {
                    JoyInput::Button(n) => format!("Button {}", u32::from(n) + 1),
                    JoyInput::Pov { index, dir } => format!("POV{} {}", index, dir.name()),
                    JoyInput::Axis { axis, dir, .. } => {
                        format!("Axis {}{}", JOY_AXES[usize::from(axis)], dir.suffix())
                    }
                };
                format!("{dev}: {what}")
            }
        }
    }

    /// Short label without the device name (overlay hints).
    pub fn short_label(&self) -> String {
        match self {
            Binding::Joy(j) => match j.input {
                JoyInput::Button(n) => format!("Btn {}", u32::from(n) + 1),
                JoyInput::Pov { dir, .. } => format!("POV {}", dir.name()),
                JoyInput::Axis { axis, dir, .. } => format!("Axis {}{}", JOY_AXES[usize::from(axis)], dir.suffix()),
            },
            other => other.label(),
        }
    }
}

// ---- Keys ------------------------------------------------------------------------------

const NAMED_KEYS: &[(&str, u8)] = &[
    ("Backspace", 0x08),
    ("Tab", 0x09),
    ("Enter", 0x0D),
    ("Shift", 0x10),
    ("Ctrl", 0x11),
    ("Alt", 0x12),
    ("Pause", 0x13),
    ("CapsLock", 0x14),
    ("Esc", 0x1B),
    ("Space", 0x20),
    ("PageUp", 0x21),
    ("PageDown", 0x22),
    ("End", 0x23),
    ("Home", 0x24),
    ("Left", 0x25),
    ("Up", 0x26),
    ("Right", 0x27),
    ("Down", 0x28),
    ("PrintScreen", 0x2C),
    ("Insert", 0x2D),
    ("Delete", 0x2E),
    ("LWin", 0x5B),
    ("RWin", 0x5C),
    ("Apps", 0x5D),
    ("NumpadMultiply", 0x6A),
    ("NumpadAdd", 0x6B),
    ("NumpadSubtract", 0x6D),
    ("NumpadDecimal", 0x6E),
    ("NumpadDivide", 0x6F),
    ("NumLock", 0x90),
    ("ScrollLock", 0x91),
    ("LShift", 0xA0),
    ("RShift", 0xA1),
    ("LCtrl", 0xA2),
    ("RCtrl", 0xA3),
    ("LAlt", 0xA4),
    ("RAlt", 0xA5),
    ("Mouse4", 0x05),
    ("Mouse5", 0x06),
    ("Semicolon", 0xBA),
    ("Equals", 0xBB),
    ("Comma", 0xBC),
    ("Minus", 0xBD),
    ("Period", 0xBE),
    ("Slash", 0xBF),
    ("Backtick", 0xC0),
    ("LBracket", 0xDB),
    ("Backslash", 0xDC),
    ("RBracket", 0xDD),
    ("Quote", 0xDE),
];

const KEY_ALIASES: &[(&str, u8)] = &[
    ("BACK", 0x08),
    ("RETURN", 0x0D),
    ("CONTROL", 0x11),
    ("ESCAPE", 0x1B),
    ("XBUTTON1", 0x05),
    ("XBUTTON2", 0x06),
    ("PLUS", 0xBB),
];

/// Virtual-key code for a key name ("R", "F5", "Backspace", "Numpad0", "0x5B", ...).
pub fn vk_from_name(name: &str) -> Option<u8> {
    let n = name.trim();
    if n.is_empty() {
        return None;
    }
    let up = n.to_ascii_uppercase();
    if up.len() == 1 {
        let c = up.as_bytes()[0];
        if c.is_ascii_uppercase() || c.is_ascii_digit() {
            return Some(c);
        }
    }
    if let Some(hex) = up.strip_prefix("0X").or_else(|| up.strip_prefix("VK0X")) {
        return u8::from_str_radix(hex, 16).ok().filter(|&v| v != 0);
    }
    if let Some(num) = up.strip_prefix('F').and_then(|d| d.parse::<u8>().ok()) {
        if (1..=24).contains(&num) {
            return Some(0x70 + num - 1);
        }
    }
    if let Some(num) = up.strip_prefix("NUMPAD").and_then(|d| d.parse::<u8>().ok()) {
        if num <= 9 {
            return Some(0x60 + num);
        }
    }
    NAMED_KEYS.iter().chain(KEY_ALIASES).find(|(k, _)| k.eq_ignore_ascii_case(&up)).map(|&(_, v)| v)
}

/// Canonical name of a virtual key (falls back to `0xNN`).
pub fn key_name(vk: u8) -> String {
    match vk {
        b'A'..=b'Z' | b'0'..=b'9' => (vk as char).to_string(),
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        0x60..=0x69 => format!("Numpad{}", vk - 0x60),
        _ => NAMED_KEYS.iter().find(|&&(_, v)| v == vk).map_or_else(|| format!("0x{vk:02X}"), |(k, _)| (*k).to_owned()),
    }
}

// ---- XInput ----------------------------------------------------------------------------

/// (canonical name, mask, PlayStation name).
const PAD_BUTTONS: &[(&str, u16, &str)] = &[
    ("DPadUp", 0x0001, "D-pad up"),
    ("DPadDown", 0x0002, "D-pad down"),
    ("DPadLeft", 0x0004, "D-pad left"),
    ("DPadRight", 0x0008, "D-pad right"),
    ("Start", 0x0010, "Options"),
    ("Back", 0x0020, "Create"),
    ("LS", 0x0040, "L3"),
    ("RS", 0x0080, "R3"),
    ("LB", 0x0100, "L1"),
    ("RB", 0x0200, "R1"),
    ("A", 0x1000, "Cross"),
    ("B", 0x2000, "Circle"),
    ("X", 0x4000, "Square"),
    ("Y", 0x8000, "Triangle"),
];

const PAD_ALIASES: &[(&str, &str)] = &[
    ("VIEW", "Back"),
    ("SHARE", "Back"),
    ("CREATE", "Back"),
    ("MENU", "Start"),
    ("OPTIONS", "Start"),
    ("CROSS", "A"),
    ("CIRCLE", "B"),
    ("SQUARE", "X"),
    ("TRIANGLE", "Y"),
    ("L1", "LB"),
    ("R1", "RB"),
    ("L2", "LT"),
    ("R2", "RT"),
    ("L3", "LS"),
    ("R3", "RS"),
    ("LEFTSHOULDER", "LB"),
    ("RIGHTSHOULDER", "RB"),
    ("LEFTTHUMB", "LS"),
    ("RIGHTTHUMB", "RS"),
];

/// XInput `wButtons` mask for a button name (Xbox or PlayStation naming).
pub fn pad_mask(name: &str) -> Option<u16> {
    let n = canonical_pad_name(name);
    PAD_BUTTONS.iter().find(|(k, _, _)| k.eq_ignore_ascii_case(n)).map(|&(_, m, _)| m)
}

fn canonical_pad_name(name: &str) -> &str {
    let n = name.trim();
    PAD_ALIASES.iter().find(|(a, _)| a.eq_ignore_ascii_case(n)).map_or(n, |(_, c)| c)
}

pub fn pad_button_name(mask: u16) -> Option<&'static str> {
    PAD_BUTTONS.iter().find(|&&(_, m, _)| m == mask).map(|(n, _, _)| *n)
}

/// All XInput buttons, as (canonical name, mask).
pub fn pad_buttons() -> impl Iterator<Item = (&'static str, u16)> {
    PAD_BUTTONS.iter().map(|&(n, m, _)| (n, m))
}

/// "A (Cross)", "LT (L2)", "LX-": Xbox name with the PlayStation name, since a DualSense
/// through Steam Input looks exactly like an Xbox pad.
fn pad_label(p: &PadInput) -> String {
    match *p {
        PadInput::Button(m) => PAD_BUTTONS
            .iter()
            .find(|&&(_, bm, _)| bm == m)
            .map_or_else(|| format!("pad 0x{m:04X}"), |(x, _, ps)| format!("{x} ({ps})")),
        PadInput::Trigger { right, .. } => if right { "RT (R2)" } else { "LT (L2)" }.into(),
        PadInput::Stick { axis, positive, .. } => format!("{}{}", axis.name(), if positive { '+' } else { '-' }),
    }
}

// ---- Text form -------------------------------------------------------------------------

fn parse_threshold(s: &str) -> Result<(&str, Option<u8>), String> {
    let Some((head, t)) = s.split_once('@') else {
        return Ok((s, None));
    };
    let v: f32 = t.trim().parse().map_err(|_| format!("bad threshold '{t}'"))?;
    if !(v > 0.0 && v <= 1.0) {
        return Err(format!("threshold {v} must be in (0, 1]"));
    }
    Ok((head, Some(((v * 100.0).round() as u8).max(1))))
}

fn fmt_threshold(t: Option<u8>) -> String {
    t.map_or_else(String::new, |p| format!("@{}", f32::from(p) / 100.0))
}

fn parse_pad(s: &str) -> Result<PadInput, String> {
    let (head, threshold) = parse_threshold(s.trim())?;
    let name = canonical_pad_name(head);
    if name.eq_ignore_ascii_case("LT") || name.eq_ignore_ascii_case("RT") {
        return Ok(PadInput::Trigger { right: name.eq_ignore_ascii_case("RT"), threshold });
    }
    if let Some(sign) = name.chars().last().filter(|c| matches!(c, '+' | '-')) {
        let axis = &name[..name.len() - 1];
        if let Some(a) = StickAxis::ALL.into_iter().find(|a| a.name().eq_ignore_ascii_case(axis)) {
            return Ok(PadInput::Stick { axis: a, positive: sign == '+', threshold });
        }
    }
    if threshold.is_some() {
        return Err(format!("pad button '{head}' takes no threshold"));
    }
    pad_mask(name).map(PadInput::Button).ok_or_else(|| format!("unknown pad input '{s}'"))
}

/// Upper-case GUID without braces, or `None` if malformed.
pub fn normalize_guid(s: &str) -> Option<String> {
    let g = s.trim().trim_start_matches('{').trim_end_matches('}').to_ascii_uppercase();
    let parts: Vec<&str> = g.split('-').collect();
    let lens = [8, 4, 4, 4, 12];
    (parts.len() == 5 && parts.iter().zip(lens).all(|(p, l)| p.len() == l && p.bytes().all(|b| b.is_ascii_hexdigit())))
        .then_some(g)
}

fn parse_joy_input(s: &str) -> Result<JoyInput, String> {
    let lower = s.trim().to_ascii_lowercase();
    if let Some(n) = lower.strip_prefix("button") {
        let n: usize = n.parse().map_err(|_| format!("bad button '{s}'"))?;
        return (n < JOY_BUTTONS)
            .then_some(JoyInput::Button(n as u8))
            .ok_or_else(|| format!("button {n} out of range 0..{JOY_BUTTONS}"));
    }
    if let Some(rest) = lower.strip_prefix("pov") {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let index: usize = digits.parse().map_err(|_| format!("bad POV '{s}'"))?;
        let dir = PovDir::ALL
            .into_iter()
            .find(|d| d.name() == &rest[digits.len()..])
            .ok_or_else(|| format!("bad POV direction in '{s}' (up/down/left/right)"))?;
        return (index < JOY_POVS)
            .then_some(JoyInput::Pov { index: index as u8, dir })
            .ok_or_else(|| format!("POV {index} out of range 0..{JOY_POVS}"));
    }
    if let Some(rest) = lower.strip_prefix("axis") {
        let (rest, threshold) = parse_threshold(rest)?;
        let split = rest.find(['+', '-']).ok_or_else(|| format!("axis '{s}' needs a direction (+ or -)"))?;
        let (name, dir) = rest.split_at(split);
        let axis = JOY_AXES
            .iter()
            .position(|a| a.eq_ignore_ascii_case(name))
            .ok_or_else(|| format!("unknown axis '{name}' (X Y Z RX RY RZ S0 S1)"))?;
        let dir = match dir {
            "+" => AxisDir::Pos,
            "-" => AxisDir::Neg,
            "+full" => AxisDir::FullPos,
            "-full" => AxisDir::FullNeg,
            d => return Err(format!("bad axis direction '{d}' (+, -, +full, -full)")),
        };
        return Ok(JoyInput::Axis { axis: axis as u8, dir, threshold });
    }
    Err(format!("unknown joystick input '{s}' (buttonN, povNdir, axisA±)"))
}

fn parse_joy(s: &str) -> Result<JoyBinding, String> {
    let (dev, input) = s.rsplit_once(':').ok_or_else(|| format!("'{s}' needs <device>:<input>"))?;
    let (guid, name) = match dev.split_once('/') {
        Some((g, n)) => (g.trim(), Some(n.trim().to_owned()).filter(|n| !n.is_empty())),
        None => (dev.trim(), None),
    };
    let product = match guid {
        "" | "*" => None,
        g => Some(normalize_guid(g).ok_or_else(|| format!("bad product GUID '{g}'"))?),
    };
    Ok(JoyBinding { product, name, input: parse_joy_input(input)? })
}

impl FromStr for Binding {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (kind, rest) = s.split_once(':').ok_or_else(|| format!("'{s}' needs a kind prefix (key:, pad:, joy:)"))?;
        match kind.trim().to_ascii_lowercase().as_str() {
            "key" => vk_from_name(rest).map(Binding::Key).ok_or_else(|| format!("unknown key '{rest}'")),
            "pad" => parse_pad(rest).map(Binding::Pad),
            "joy" => parse_joy(rest).map(Binding::Joy),
            k => Err(format!("unknown binding kind '{k}' (key, pad, joy)")),
        }
    }
}

impl fmt::Display for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Binding::Key(vk) => write!(f, "key:{}", key_name(*vk)),
            Binding::Pad(PadInput::Button(m)) => {
                write!(f, "pad:{}", pad_button_name(*m).map_or_else(|| format!("0x{m:04X}"), str::to_owned))
            }
            Binding::Pad(PadInput::Trigger { right, threshold }) => {
                write!(f, "pad:{}{}", if *right { "RT" } else { "LT" }, fmt_threshold(*threshold))
            }
            Binding::Pad(PadInput::Stick { axis, positive, threshold }) => {
                write!(f, "pad:{}{}{}", axis.name(), if *positive { '+' } else { '-' }, fmt_threshold(*threshold))
            }
            Binding::Joy(j) => {
                f.write_str("joy:")?;
                f.write_str(j.product.as_deref().unwrap_or("*"))?;
                if let Some(n) = &j.name {
                    write!(f, "/{n}")?;
                }
                match j.input {
                    JoyInput::Button(n) => write!(f, ":button{n}"),
                    JoyInput::Pov { index, dir } => write!(f, ":pov{index}{}", dir.name()),
                    JoyInput::Axis { axis, dir, threshold } => {
                        write!(f, ":axis{}{}{}", JOY_AXES[usize::from(axis)], dir.suffix(), fmt_threshold(threshold))
                    }
                }
            }
        }
    }
}

impl Serialize for Binding {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Binding {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(s: &str) -> String {
        s.parse::<Binding>().unwrap_or_else(|e| panic!("{s}: {e}")).to_string()
    }

    #[test]
    fn key_names() {
        assert_eq!(vk_from_name("R"), Some(b'R'));
        assert_eq!(vk_from_name("r"), Some(b'R'));
        assert_eq!(vk_from_name("7"), Some(b'7'));
        assert_eq!(vk_from_name("F5"), Some(0x74));
        assert_eq!(vk_from_name("f24"), Some(0x87));
        assert_eq!(vk_from_name("F25"), None);
        assert_eq!(vk_from_name("Backspace"), Some(0x08));
        assert_eq!(vk_from_name("back"), Some(0x08));
        assert_eq!(vk_from_name("Numpad3"), Some(0x63));
        assert_eq!(vk_from_name("enter"), Some(0x0D));
        assert_eq!(vk_from_name("0x5b"), Some(0x5B));
        assert_eq!(vk_from_name("0x00"), None);
        assert_eq!(vk_from_name(""), None);
        assert_eq!(vk_from_name("Hyper"), None);
        for vk in 1..=255u8 {
            assert_eq!(vk_from_name(&key_name(vk)), Some(vk), "vk {vk:#x} -> {}", key_name(vk));
        }
    }

    #[test]
    fn pad_names_and_labels() {
        assert_eq!(pad_mask("Back"), Some(0x20));
        assert_eq!(pad_mask("create"), Some(0x20));
        assert_eq!(pad_mask("Cross"), Some(0x1000));
        assert_eq!(pad_mask("rb"), Some(0x200));
        assert_eq!(pad_mask("Z"), None);
        assert_eq!("pad:Cross".parse::<Binding>().unwrap().label(), "A (Cross)");
        assert_eq!("pad:back".parse::<Binding>().unwrap().label(), "Back (Create)");
        assert_eq!("pad:L2".parse::<Binding>().unwrap().label(), "LT (L2)");
        assert_eq!("pad:RT".parse::<Binding>().unwrap().label(), "RT (R2)");
    }

    #[test]
    fn canonical_round_trips() {
        for s in [
            "key:R",
            "key:F8",
            "key:Left",
            "key:Backspace",
            "key:0xFF",
            "pad:A",
            "pad:Back",
            "pad:DPadLeft",
            "pad:LT",
            "pad:RT@0.3",
            "pad:LX-",
            "pad:RY+@0.75",
            "joy:*:button0",
            "joy:*:button127",
            "joy:0EB71BAD-0000-0000-0000-504944564944/FANATEC Wheel:button5",
            "joy:*/Button Box: Left:pov0up",
            "joy:0EB71BAD-0000-0000-0000-504944564944:pov3left",
            "joy:*:axisZ+",
            "joy:*:axisRX-",
            "joy:*:axisS1+full",
            "joy:*:axisY-full@0.6",
        ] {
            assert_eq!(rt(s), s);
        }
    }

    #[test]
    fn lenient_forms_normalise() {
        assert_eq!(rt(" key : r "), "key:R");
        assert_eq!(rt("KEY:return"), "key:Enter");
        assert_eq!(rt("pad:cross"), "pad:A");
        assert_eq!(rt("pad:R2"), "pad:RT");
        assert_eq!(rt("pad:options"), "pad:Start");
        assert_eq!(
            rt("joy:{0eb71bad-0000-0000-0000-504944564944}:BUTTON3"),
            "joy:0EB71BAD-0000-0000-0000-504944564944:button3"
        );
        assert_eq!(rt("joy::button1"), "joy:*:button1");
        assert_eq!(rt("joy:*/  :pov1DOWN"), "joy:*:pov1down");
        assert_eq!(rt("joy:*:axisz+@1"), "joy:*:axisZ+@1");
    }

    #[test]
    fn rejects_bad_bindings() {
        for s in [
            "R",
            "mouse:1",
            "key:Hyper",
            "pad:Z",
            "pad:A@0.5",
            "pad:LT@0",
            "pad:LT@1.5",
            "pad:LZ+",
            "joy:*:button128",
            "joy:*:pov4up",
            "joy:*:pov0north",
            "joy:*:axisW+",
            "joy:*:axisX",
            "joy:*:axisX*",
            "joy:not-a-guid:button1",
            "joy:button1",
            "joy:*:trigger",
        ] {
            assert!(s.parse::<Binding>().is_err(), "{s} should be rejected");
        }
    }

    #[test]
    fn analog_and_thresholds() {
        let b = |s: &str| s.parse::<Binding>().unwrap();
        assert!(b("pad:LT").is_analog());
        assert!(b("pad:LX+").is_analog());
        assert!(b("joy:*:axisZ-full").is_analog());
        assert!(!b("pad:A").is_analog());
        assert!(!b("joy:*:button2").is_analog());
        assert!(!b("key:R").is_analog());
        assert_eq!(b("pad:LT").threshold(), DEFAULT_THRESHOLD);
        assert_eq!(b("pad:LT@0.25").threshold(), 0.25);
        assert_eq!(b("joy:*:axisZ+@0.8").threshold(), 0.8);
    }

    #[test]
    fn joy_labels() {
        let b: Binding = "joy:0EB71BAD-0000-0000-0000-504944564944/CSL DD:button5".parse().unwrap();
        assert_eq!(b.label(), "CSL DD: Button 6");
        assert_eq!(b.short_label(), "Btn 6");
        let b: Binding = "joy:*:axisZ-full".parse().unwrap();
        assert_eq!(b.label(), "any controller: Axis Z-full");
        let b: Binding = "joy:*/Box:pov0right".parse().unwrap();
        assert_eq!(b.label(), "Box: POV0 right");
    }

    #[test]
    fn serde_as_strings() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct W {
            b: Vec<Binding>,
        }
        let w: W = toml::from_str(r#"b = ["key:R", "pad:LT@0.2", "joy:*:pov0up"]"#).unwrap();
        assert_eq!(w.b.len(), 3);
        let s = toml::to_string(&w).unwrap();
        assert_eq!(toml::from_str::<W>(&s).unwrap(), w);
        assert!(toml::from_str::<W>(r#"b = ["key:Hyper"]"#).is_err());
    }

    #[test]
    fn action_keys() {
        for a in InputAction::ALL {
            assert_eq!(InputAction::from_key(a.key()), Some(a));
        }
        assert_eq!(InputAction::from_key("nope"), None);
    }
}
