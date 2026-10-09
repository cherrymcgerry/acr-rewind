use crate::bindings::{Binding, InputAction};
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
    #[error("cannot update config file: {0}")]
    Edit(String),
}

/// Top-level settings, matching `config/acr-rewind.toml`. Missing fields take defaults.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub enabled: bool,
    pub buffer: BufferConfig,
    pub scrub: ScrubConfig,
    pub mode: ModeConfig,
    pub bindings: BindingsConfig,
    pub ffb: FfbConfig,
}

/// Wheel force feedback around a rewind. Normal driving is never touched.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FfbConfig {
    /// Silence the game's force feedback while the rewind mode and the resume run-in hold
    /// the car (teleports and held poses can produce violent forces).
    pub mute_during_rewind: bool,
    /// Seconds over which the force feedback ramps back to full after the run-in.
    pub fade_in_s: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BufferConfig {
    /// Seconds of history to keep.
    pub seconds: f64,
    /// Snapshot capture rate (Hz).
    pub tick_hz: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScrubConfig {
    /// Digital scrub (keys, buttons): speed (history seconds per real second) when first held.
    pub initial_speed: f64,
    /// Digital scrub: upper bound for the speed.
    pub max_speed: f64,
    /// Digital scrub: speed increase per second held.
    pub acceleration: f64,
    /// Analog scrub (triggers, axes): travel below this fraction is ignored.
    pub analog_deadzone: f64,
    /// Analog scrub: response exponent applied after the deadzone (1 = linear).
    pub analog_curve: f64,
    /// Analog scrub: speed at full travel (before the hold boost).
    pub analog_max_speed: f64,
    /// Analog scrub: speed multiplier gained per second held.
    pub analog_hold_boost: f64,
    /// Analog scrub: cap of the hold multiplier.
    pub analog_boost_max: f64,
    /// Seconds the car is held frozen at the resume point before control returns.
    pub resume_delay: f64,
    /// Minimum recorded history (s) before a rewind may start.
    pub min_history: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModeConfig {
    /// Keep holding the rewind binding after entering the mode to scrub back (digital speed).
    pub hold_rewind_scrubs: bool,
    /// Leave the mode automatically (cancel) after this many seconds; 0 = never.
    pub max_time_s: f64,
    /// Resume only once every scrub input is released (otherwise a held trigger reaches the
    /// game as throttle / brake the moment the car is released).
    pub resume_requires_release: bool,
}

/// Bindings per [`InputAction`]. Invalid entries are dropped and reported in `warnings`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BindingsConfig {
    pub rewind: Vec<Binding>,
    pub resume: Vec<Binding>,
    pub cancel: Vec<Binding>,
    pub scrub_back: Vec<Binding>,
    pub scrub_forward: Vec<Binding>,
    pub open_settings: Vec<Binding>,
    #[serde(skip)]
    pub warnings: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            buffer: BufferConfig::default(),
            scrub: ScrubConfig::default(),
            mode: ModeConfig::default(),
            bindings: BindingsConfig::default(),
            ffb: FfbConfig::default(),
        }
    }
}

impl Default for FfbConfig {
    fn default() -> Self {
        Self { mute_during_rewind: true, fade_in_s: 1.0 }
    }
}

impl Default for BufferConfig {
    fn default() -> Self {
        Self { seconds: 30.0, tick_hz: 60 }
    }
}

impl Default for ScrubConfig {
    fn default() -> Self {
        Self {
            initial_speed: 1.0,
            max_speed: 4.0,
            acceleration: 2.0,
            analog_deadzone: 0.08,
            analog_curve: 1.6,
            analog_max_speed: 4.0,
            analog_hold_boost: 0.5,
            analog_boost_max: 2.0,
            resume_delay: 0.25,
            min_history: 0.5,
        }
    }
}

impl Default for ModeConfig {
    fn default() -> Self {
        Self { hold_rewind_scrubs: true, max_time_s: 0.0, resume_requires_release: false }
    }
}

fn b(list: &[&str]) -> Vec<Binding> {
    list.iter().map(|s| s.parse().expect("default binding parses")).collect()
}

impl Default for BindingsConfig {
    fn default() -> Self {
        Self {
            rewind: b(&["key:R", "pad:Back"]),
            resume: b(&["key:Enter", "pad:A"]),
            cancel: b(&["key:Backspace"]),
            scrub_back: b(&["key:Left", "pad:LT"]),
            scrub_forward: b(&["key:Right", "pad:RT"]),
            open_settings: b(&["key:F8"]),
            warnings: Vec::new(),
        }
    }
}

impl BindingsConfig {
    pub fn get(&self, a: InputAction) -> &Vec<Binding> {
        match a {
            InputAction::Rewind => &self.rewind,
            InputAction::Resume => &self.resume,
            InputAction::Cancel => &self.cancel,
            InputAction::ScrubBack => &self.scrub_back,
            InputAction::ScrubForward => &self.scrub_forward,
            InputAction::OpenSettings => &self.open_settings,
        }
    }

    pub fn get_mut(&mut self, a: InputAction) -> &mut Vec<Binding> {
        match a {
            InputAction::Rewind => &mut self.rewind,
            InputAction::Resume => &mut self.resume,
            InputAction::Cancel => &mut self.cancel,
            InputAction::ScrubBack => &mut self.scrub_back,
            InputAction::ScrubForward => &mut self.scrub_forward,
            InputAction::OpenSettings => &mut self.open_settings,
        }
    }

    /// Adds a binding unless the action already has it. Returns whether it was added.
    pub fn add(&mut self, a: InputAction, b: Binding) -> bool {
        let list = self.get_mut(a);
        if list.contains(&b) {
            return false;
        }
        list.push(b);
        true
    }

    /// Other actions that already use `b`.
    pub fn conflicts(&self, a: InputAction, b: &Binding) -> Vec<InputAction> {
        InputAction::ALL.into_iter().filter(|&o| o != a && self.get(o).contains(b)).collect()
    }

    pub fn all(&self) -> impl Iterator<Item = (InputAction, &Binding)> {
        InputAction::ALL.into_iter().flat_map(move |a| self.get(a).iter().map(move |b| (a, b)))
    }
}

/// `[bindings]` as written: per-action string lists, plus the pre-0.2 single-button keys.
#[derive(Deserialize, Default)]
#[serde(default)]
struct RawBindings {
    rewind: Option<Vec<String>>,
    resume: Option<Vec<String>>,
    cancel: Option<Vec<String>>,
    scrub_back: Option<Vec<String>>,
    scrub_forward: Option<Vec<String>>,
    open_settings: Option<Vec<String>>,
    keyboard: Option<String>,
    gamepad: Option<String>,
    wheel_button: Option<i64>,
}

impl<'de> Deserialize<'de> for BindingsConfig {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = RawBindings::deserialize(d)?;
        Ok(BindingsConfig::from_raw(raw))
    }
}

impl BindingsConfig {
    fn from_raw(raw: RawBindings) -> Self {
        let mut out = BindingsConfig::default();
        let mut warnings = Vec::new();
        let lists = [
            (InputAction::Rewind, &raw.rewind),
            (InputAction::Resume, &raw.resume),
            (InputAction::Cancel, &raw.cancel),
            (InputAction::ScrubBack, &raw.scrub_back),
            (InputAction::ScrubForward, &raw.scrub_forward),
            (InputAction::OpenSettings, &raw.open_settings),
        ];
        for (a, list) in lists {
            let Some(list) = list else { continue };
            let parsed = out.get_mut(a);
            parsed.clear();
            for s in list {
                match s.parse::<Binding>() {
                    Ok(b) if !parsed.contains(&b) => parsed.push(b),
                    Ok(_) => {}
                    Err(e) => warnings.push(format!("bindings.{}: ignoring '{s}': {e}", a.key())),
                }
            }
        }
        let legacy = raw.keyboard.is_some() || raw.gamepad.is_some() || raw.wheel_button.is_some();
        if legacy && raw.rewind.is_none() {
            let mut rewind = Vec::new();
            let mut convert = |kind: &str, v: &str| {
                if v.trim().is_empty() {
                    return;
                }
                match format!("{kind}:{v}").parse::<Binding>() {
                    Ok(b) => rewind.push(b),
                    Err(e) => warnings.push(format!("bindings.{kind}: ignoring '{v}': {e}")),
                }
            };
            if let Some(k) = &raw.keyboard {
                convert("key", k);
            }
            if let Some(g) = &raw.gamepad {
                convert("pad", g);
            }
            match raw.wheel_button {
                None | Some(-1) => {}
                Some(n @ 0..=127) => convert("joy", &format!("*:button{n}")),
                Some(n) => warnings.push(format!("bindings.wheel_button {n} out of range 0..=127")),
            }
            out.rewind = rewind;
            warnings.push(
                "bindings: converted the old keyboard / gamepad / wheel_button keys to `rewind = [...]`; \
                 save from the settings panel to rewrite the file"
                    .into(),
            );
        } else if legacy {
            warnings
                .push("bindings: old keyboard / gamepad / wheel_button keys ignored (rewind = [...] is set)".into());
        }
        out.warnings = warnings;
        out
    }
}

impl Config {
    /// Parses and validates a TOML document.
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        let cfg: Self = toml::from_str(s)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).expect("Config is always serializable")
    }

    /// Rewrites `[scrub]`, `[mode]`, `[bindings]` and `[ffb]` of an existing document with this
    /// config, keeping every other key and the comments of the file.
    pub fn update_toml(&self, existing: &str) -> Result<String, ConfigError> {
        use toml_edit::{value, Array, DocumentMut, Item, Table};
        let mut doc: DocumentMut =
            existing.parse().map_err(|e: toml_edit::TomlError| ConfigError::Edit(e.to_string()))?;
        fn table<'a>(doc: &'a mut DocumentMut, name: &str) -> Result<&'a mut Table, ConfigError> {
            if !doc.contains_key(name) {
                doc[name] = Item::Table(Table::new());
            }
            doc[name].as_table_mut().ok_or_else(|| ConfigError::Edit(format!("`{name}` is not a table")))
        }
        let s = &self.scrub;
        let scrub = table(&mut doc, "scrub")?;
        for (k, v) in [
            ("initial_speed", s.initial_speed),
            ("max_speed", s.max_speed),
            ("acceleration", s.acceleration),
            ("analog_deadzone", s.analog_deadzone),
            ("analog_curve", s.analog_curve),
            ("analog_max_speed", s.analog_max_speed),
            ("analog_hold_boost", s.analog_hold_boost),
            ("analog_boost_max", s.analog_boost_max),
            ("resume_delay", s.resume_delay),
            ("min_history", s.min_history),
        ] {
            scrub[k] = value(round3(v));
        }
        let m = &self.mode;
        let mode = table(&mut doc, "mode")?;
        mode["hold_rewind_scrubs"] = value(m.hold_rewind_scrubs);
        mode["max_time_s"] = value(round3(m.max_time_s));
        mode["resume_requires_release"] = value(m.resume_requires_release);
        let bindings = table(&mut doc, "bindings")?;
        for legacy in ["keyboard", "gamepad", "wheel_button"] {
            bindings.remove(legacy);
        }
        for a in InputAction::ALL {
            let arr: Array = self.bindings.get(a).iter().map(ToString::to_string).collect();
            bindings[a.key()] = value(arr);
        }
        let ffb = table(&mut doc, "ffb")?;
        ffb["mute_during_rewind"] = value(self.ffb.mute_during_rewind);
        ffb["fade_in_s"] = value(round3(self.ffb.fade_in_s));
        let out = doc.to_string();
        Ok(if existing.contains("\r\n") { out.replace("\r\n", "\n").replace('\n', "\r\n") } else { out })
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |m: &str| Err(ConfigError::Invalid(m.to_owned()));
        let b = &self.buffer;
        let s = &self.scrub;
        if !(b.seconds.is_finite() && b.seconds > 0.0 && b.seconds <= 600.0) {
            return invalid("buffer.seconds must be in (0, 600]");
        }
        if !(1..=1000).contains(&b.tick_hz) {
            return invalid("buffer.tick_hz must be in 1..=1000");
        }
        if !(s.initial_speed.is_finite() && s.initial_speed > 0.0) {
            return invalid("scrub.initial_speed must be > 0");
        }
        if !(s.max_speed.is_finite() && s.max_speed >= s.initial_speed) {
            return invalid("scrub.max_speed must be >= scrub.initial_speed");
        }
        if !(s.acceleration.is_finite() && s.acceleration >= 0.0) {
            return invalid("scrub.acceleration must be >= 0");
        }
        if !(s.analog_deadzone.is_finite() && (0.0..=0.95).contains(&s.analog_deadzone)) {
            return invalid("scrub.analog_deadzone must be in [0, 0.95]");
        }
        if !(s.analog_curve.is_finite() && (0.1..=10.0).contains(&s.analog_curve)) {
            return invalid("scrub.analog_curve must be in [0.1, 10]");
        }
        if !(s.analog_max_speed.is_finite() && s.analog_max_speed > 0.0) {
            return invalid("scrub.analog_max_speed must be > 0");
        }
        if !(s.analog_hold_boost.is_finite() && s.analog_hold_boost >= 0.0) {
            return invalid("scrub.analog_hold_boost must be >= 0");
        }
        if !(s.analog_boost_max.is_finite() && s.analog_boost_max >= 1.0) {
            return invalid("scrub.analog_boost_max must be >= 1");
        }
        if !(s.resume_delay.is_finite() && s.resume_delay >= 0.0) {
            return invalid("scrub.resume_delay must be >= 0");
        }
        if !(s.min_history.is_finite() && s.min_history >= 0.0) {
            return invalid("scrub.min_history must be >= 0");
        }
        if !(self.mode.max_time_s.is_finite() && self.mode.max_time_s >= 0.0) {
            return invalid("mode.max_time_s must be >= 0");
        }
        if !(self.ffb.fade_in_s.is_finite() && (0.0..=10.0).contains(&self.ffb.fade_in_s)) {
            return invalid("ffb.fade_in_s must be in [0, 10]");
        }
        Ok(())
    }

    /// Ring buffer capacity implied by `buffer.seconds * buffer.tick_hz`.
    pub fn capacity(&self) -> usize {
        (self.buffer.seconds * f64::from(self.buffer.tick_hz)).ceil() as usize + 1
    }
}

/// Keeps slider values readable in the file (0.30000001 -> 0.3).
fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bs(list: &[Binding]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn shipped_config_parses() {
        let s = include_str!("../../../config/acr-rewind.toml");
        let cfg = Config::from_toml_str(s).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.buffer.seconds, 30.0);
        assert_eq!(cfg.buffer.tick_hz, 60);
        assert_eq!(cfg.capacity(), 1801);
        assert!(cfg.bindings.warnings.is_empty(), "{:?}", cfg.bindings.warnings);
        let mut defaults = BindingsConfig::default();
        defaults.warnings.clear();
        assert_eq!(cfg.bindings, defaults, "shipped bindings are the documented defaults");
        assert_eq!(cfg.scrub, ScrubConfig::default());
        assert_eq!(cfg.mode, ModeConfig::default());
        assert_eq!(cfg.ffb, FfbConfig { mute_during_rewind: true, fade_in_s: 1.0 });
    }

    #[test]
    fn ffb_section() {
        let cfg = Config::from_toml_str("[ffb]\nmute_during_rewind = false\n").unwrap();
        assert!(!cfg.ffb.mute_during_rewind);
        assert_eq!(cfg.ffb.fade_in_s, 1.0, "missing key keeps the default");
        assert!(Config::from_toml_str("").unwrap().ffb.mute_during_rewind, "missing section mutes");
        let mut cfg = Config::default();
        cfg.ffb.mute_during_rewind = false;
        cfg.ffb.fade_in_s = 2.5;
        let out = cfg.update_toml("# mine\n[ffb]\n# keep\nmute_during_rewind = true\n").unwrap();
        assert!(out.contains("# keep"), "{out}");
        assert_eq!(Config::from_toml_str(&out).unwrap().ffb, cfg.ffb);
    }

    #[test]
    fn default_bindings() {
        let b = BindingsConfig::default();
        assert_eq!(bs(&b.rewind), ["key:R", "pad:Back"]);
        assert_eq!(bs(&b.resume), ["key:Enter", "pad:A"]);
        assert_eq!(bs(&b.cancel), ["key:Backspace"]);
        assert_eq!(bs(&b.scrub_back), ["key:Left", "pad:LT"]);
        assert_eq!(bs(&b.scrub_forward), ["key:Right", "pad:RT"]);
        assert_eq!(bs(&b.open_settings), ["key:F8"]);
        assert!(b.all().all(|(_, b)| !matches!(b, Binding::Joy(_))), "no wheel bindings by default");
    }

    #[test]
    fn empty_uses_defaults() {
        assert_eq!(Config::from_toml_str("").unwrap(), Config::default());
    }

    #[test]
    fn partial_override() {
        let cfg = Config::from_toml_str("enabled = false\n[scrub]\nmax_speed = 8.0\n").unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.scrub.max_speed, 8.0);
        assert_eq!(cfg.scrub.initial_speed, 1.0);
        assert_eq!(cfg.buffer.tick_hz, 60);
    }

    #[test]
    fn per_action_lists_replace_only_that_action() {
        let cfg =
            Config::from_toml_str("[bindings]\nrewind = [\"joy:*/Wheel:button4\", \"key:F5\"]\nresume = []\n").unwrap();
        assert_eq!(bs(&cfg.bindings.rewind), ["joy:*/Wheel:button4", "key:F5"]);
        assert!(cfg.bindings.resume.is_empty(), "explicit empty list unbinds");
        assert_eq!(bs(&cfg.bindings.cancel), ["key:Backspace"], "missing action keeps default");
        assert!(cfg.bindings.warnings.is_empty());
    }

    #[test]
    fn invalid_entries_are_dropped_with_warnings() {
        let cfg = Config::from_toml_str(
            "[bindings]\nrewind = [\"key:Hyper\", \"pad:Back\", \"pad:Back\", \"joy:*:button999\"]\n",
        )
        .unwrap();
        assert_eq!(bs(&cfg.bindings.rewind), ["pad:Back"], "duplicates collapse");
        assert_eq!(cfg.bindings.warnings.len(), 2, "{:?}", cfg.bindings.warnings);
        assert!(cfg.bindings.warnings[0].contains("bindings.rewind: ignoring 'key:Hyper'"));
        assert!(Config::from_toml_str("[bindings]\nrewind = \"key:R\"").is_err(), "must be a list");
    }

    #[test]
    fn legacy_bindings_convert() {
        let cfg =
            Config::from_toml_str("[bindings]\nkeyboard = \"R\"\ngamepad = \"Back\"\nwheel_button = 5\n").unwrap();
        assert_eq!(bs(&cfg.bindings.rewind), ["key:R", "pad:Back", "joy:*:button5"]);
        assert_eq!(bs(&cfg.bindings.resume), ["key:Enter", "pad:A"]);
        assert_eq!(cfg.bindings.warnings.len(), 1);
        let cfg = Config::from_toml_str("[bindings]\nkeyboard = \"\"\ngamepad = \"Z\"\nwheel_button = -1\n").unwrap();
        assert!(cfg.bindings.rewind.is_empty());
        assert_eq!(cfg.bindings.warnings.len(), 2, "{:?}", cfg.bindings.warnings);
        let cfg = Config::from_toml_str("[bindings]\nkeyboard = \"T\"\nrewind = [\"key:Y\"]\n").unwrap();
        assert_eq!(bs(&cfg.bindings.rewind), ["key:Y"], "new list wins");
        assert!(cfg.bindings.warnings[0].contains("ignored"));
    }

    #[test]
    fn roundtrip() {
        let mut cfg = Config::default();
        cfg.bindings
            .add(InputAction::Rewind, "joy:0EB71BAD-0000-0000-0000-504944564944/CSL DD:button7".parse().unwrap());
        cfg.bindings.scrub_back.push("joy:*:axisZ-full@0.3".parse().unwrap());
        cfg.mode.max_time_s = 120.0;
        assert_eq!(Config::from_toml_str(&cfg.to_toml_string()).unwrap(), cfg);
    }

    #[test]
    fn add_and_conflicts() {
        let mut b = BindingsConfig::default();
        let r: Binding = "key:R".parse().unwrap();
        assert!(!b.add(InputAction::Rewind, r.clone()), "already bound");
        assert!(b.add(InputAction::Cancel, r.clone()));
        assert_eq!(b.conflicts(InputAction::Cancel, &r), [InputAction::Rewind]);
        assert_eq!(b.all().count(), 11);
    }

    #[test]
    fn update_toml_keeps_comments_and_other_keys() {
        let original = include_str!("../../../config/acr-rewind.toml");
        let mut cfg = Config::from_toml_str(original).unwrap();
        cfg.scrub.analog_max_speed = 6.0;
        cfg.scrub.analog_curve = 1.2345678;
        cfg.mode.resume_requires_release = true;
        cfg.bindings.rewind.push("joy:*/My Wheel:button3".parse().unwrap());
        cfg.bindings.cancel.clear();
        let out = cfg.update_toml(original).unwrap();
        let back = Config::from_toml_str(&out).unwrap();
        assert_eq!(back.scrub.analog_max_speed, 6.0);
        assert_eq!(back.scrub.analog_curve, 1.235, "rounded to 3 decimals");
        assert!(back.mode.resume_requires_release);
        assert_eq!(bs(&back.bindings.rewind), ["key:R", "pad:Back", "joy:*/My Wheel:button3"]);
        assert!(back.bindings.cancel.is_empty());
        assert!(out.contains("# OFFLINE / SINGLE-PLAYER ONLY"), "header comment kept");
        assert!(out.contains("# Snapshot capture rate"), "buffer comments kept");
        assert!(out.contains("[hook]") && out.contains("post_resume_check"), "other tables kept");
        // Untouched sections keep their exact text.
        let hook_before = &original[original.find("[hook]").unwrap()..];
        assert!(out.contains(hook_before.trim_end()), "[hook] section byte-identical:\n{out}");
        assert_eq!(original.contains("\r\n"), out.contains("\r\n"), "line endings kept");
        let crlf = original.replace("\r\n", "\n").replace('\n', "\r\n");
        let out = cfg.update_toml(&crlf).unwrap();
        assert_eq!(out.matches('\n').count(), out.matches("\r\n").count(), "all CRLF");
    }

    #[test]
    fn update_toml_migrates_legacy_and_creates_tables() {
        let legacy =
            "# mine\nenabled = true\n[bindings]\n# old style\nkeyboard = \"T\"\ngamepad = \"Y\"\nwheel_button = 2\n";
        let cfg = Config::from_toml_str(legacy).unwrap();
        let out = cfg.update_toml(legacy).unwrap();
        assert!(!out.contains("keyboard") && !out.contains("wheel_button"), "{out}");
        assert!(out.starts_with("# mine"));
        let back = Config::from_toml_str(&out).unwrap();
        assert_eq!(bs(&back.bindings.rewind), ["key:T", "pad:Y", "joy:*:button2"]);
        assert!(back.bindings.warnings.is_empty(), "{:?}", back.bindings.warnings);
        assert_eq!(back.scrub, ScrubConfig::default());
        assert!(Config::default().update_toml("scrub = 3").is_err());
        assert!(Config::default().update_toml("[[[").is_err());
    }

    #[test]
    fn rejects_invalid() {
        for s in [
            "[buffer]\nseconds = 0.0",
            "[buffer]\ntick_hz = 0",
            "[scrub]\ninitial_speed = 5.0\nmax_speed = 2.0",
            "[scrub]\nresume_delay = -1.0",
            "[scrub]\nanalog_deadzone = 1.0",
            "[scrub]\nanalog_curve = 0.0",
            "[scrub]\nanalog_max_speed = 0.0",
            "[scrub]\nanalog_boost_max = 0.5",
            "[mode]\nmax_time_s = -1.0",
            "[ffb]\nfade_in_s = -0.5",
            "[ffb]\nfade_in_s = 60.0",
        ] {
            assert!(matches!(Config::from_toml_str(s), Err(ConfigError::Invalid(_))), "{s}");
        }
        assert!(matches!(Config::from_toml_str("enabled = 3"), Err(ConfigError::Parse(_))));
    }
}
