//! Live settings shared by the tick (game thread), the input thread and the overlay's
//! settings panel (render thread).
//!
//! The panel edits a copy and publishes it with [`update`]; the tick picks up the new
//! [`version`] and hands the config to the driver. [`save`] writes `[scrub]`, `[mode]`,
//! `[bindings]` and `[ffb]` back into `acr-rewind.toml`, keeping the rest of the file and its
//! comments.

use rewind_core::{Binding, Config};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

struct Shared {
    cfg: Config,
    path: Option<PathBuf>,
    /// Config as last loaded from / saved to the file.
    saved: Config,
}

static SHARED: Mutex<Option<Shared>> = Mutex::new(None);
static VERSION: AtomicU64 = AtomicU64::new(0);
/// The settings panel is open: actions must not fire.
pub static PANEL_OPEN: AtomicBool = AtomicBool::new(false);

pub fn panel_open() -> bool {
    PANEL_OPEN.load(Ordering::Relaxed)
}

/// Installs the loaded config and the file it came from.
pub fn init(cfg: Config, path: Option<PathBuf>) {
    publish_keys(&cfg);
    if let Ok(mut g) = SHARED.lock() {
        *g = Some(Shared { saved: cfg.clone(), cfg, path });
    }
    VERSION.fetch_add(1, Ordering::SeqCst);
}

/// Bumped on every change.
pub fn version() -> u64 {
    VERSION.load(Ordering::SeqCst)
}

pub fn current() -> Option<Config> {
    SHARED.lock().ok()?.as_ref().map(|s| s.cfg.clone())
}

/// Unsaved changes exist.
pub fn dirty() -> bool {
    SHARED.lock().ok().and_then(|g| g.as_ref().map(|s| s.cfg != s.saved)).unwrap_or(false)
}

/// Replaces the live config after validating it.
pub fn update(cfg: Config) -> Result<(), String> {
    cfg.validate().map_err(|e| e.to_string())?;
    let mut g = SHARED.lock().map_err(|_| "settings lock poisoned".to_owned())?;
    let s = g.as_mut().ok_or("settings not initialised")?;
    if s.cfg == cfg {
        return Ok(());
    }
    publish_keys(&cfg);
    s.cfg = cfg;
    drop(g);
    VERSION.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

/// Writes the live config into the file. Returns a status line.
pub fn save() -> Result<String, String> {
    let (cfg, path) = {
        let g = SHARED.lock().map_err(|_| "settings lock poisoned".to_owned())?;
        let s = g.as_ref().ok_or("settings not initialised")?;
        (s.cfg.clone(), s.path.clone().ok_or("no config file path")?)
    };
    write_config(&cfg, &path)?;
    if let Ok(mut g) = SHARED.lock() {
        if let Some(s) = g.as_mut() {
            s.saved = cfg;
        }
    }
    Ok(format!("saved to {}", path.display()))
}

/// Re-reads `[scrub]`, `[mode]`, `[bindings]` and `[ffb]` from the file.
pub fn revert() -> Result<String, String> {
    let path = SHARED.lock().ok().and_then(|g| g.as_ref().and_then(|s| s.path.clone())).ok_or("no config file path")?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let file = Config::from_toml_str(&text).map_err(|e| e.to_string())?;
    let mut cfg = current().ok_or("settings not initialised")?;
    cfg.scrub = file.scrub;
    cfg.mode = file.mode;
    cfg.bindings = file.bindings;
    cfg.ffb = file.ffb;
    update(cfg.clone())?;
    if let Ok(mut g) = SHARED.lock() {
        if let Some(s) = g.as_mut() {
            s.saved = cfg;
        }
    }
    Ok(format!("reloaded {}", path.display()))
}

/// Merges `cfg` into the file at `path` (atomic replace).
pub fn write_config(cfg: &Config, path: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let out = cfg.update_toml(&text).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, out).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Keyboard keys used by any binding (the input thread polls only those).
pub fn bound_keys(cfg: &Config) -> Vec<u8> {
    let mut keys: Vec<u8> = cfg
        .bindings
        .all()
        .filter_map(|(_, b)| match b {
            Binding::Key(vk) => Some(*vk),
            _ => None,
        })
        .collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

#[cfg(windows)]
fn publish_keys(cfg: &Config) {
    crate::input::hub::set_wanted_keys(bound_keys(cfg));
}

#[cfg(not(windows))]
fn publish_keys(_cfg: &Config) {}

#[cfg(test)]
mod tests {
    use super::*;
    use rewind_core::InputAction;

    #[test]
    fn bound_keys_are_unique_and_sorted() {
        let mut cfg = Config::default();
        cfg.bindings.add(InputAction::Cancel, "key:R".parse().unwrap());
        assert_eq!(bound_keys(&cfg), vec![0x08, 0x0D, 0x25, 0x27, b'R', 0x77]);
    }

    #[test]
    fn write_config_merges_into_the_file() {
        let dir = std::env::temp_dir().join(format!("acr_rewind_settings_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("acr-rewind.toml");
        std::fs::write(&path, "# keep me\n[hook]\nread_only = false\n[bindings]\nkeyboard = \"T\"\n").unwrap();
        let mut cfg = Config::from_toml_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        cfg.bindings.add(InputAction::Resume, "joy:*/Wheel:button2".parse().unwrap());
        cfg.scrub.analog_max_speed = 5.5;
        write_config(&cfg, &path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# keep me"));
        assert!(text.contains("read_only = false"));
        let back = Config::from_toml_str(&text).unwrap();
        assert_eq!(back.scrub.analog_max_speed, 5.5);
        assert_eq!(back.bindings.rewind, vec!["key:T".parse().unwrap()]);
        assert!(back.bindings.resume.contains(&"joy:*/Wheel:button2".parse().unwrap()));
        assert!(!dir.join("acr-rewind.toml.tmp").exists());
        // A missing file is created from scratch.
        let fresh = dir.join("fresh.toml");
        let _ = std::fs::remove_file(&fresh);
        write_config(&Config::default(), &fresh).unwrap();
        assert_eq!(
            Config::from_toml_str(&std::fs::read_to_string(&fresh).unwrap()).unwrap().bindings,
            Config::default().bindings
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shared_state_versions_and_validation() {
        let dir = std::env::temp_dir().join(format!("acr_rewind_shared_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("acr-rewind.toml");
        std::fs::write(&path, "enabled = true\n").unwrap();
        init(Config::default(), Some(path.clone()));
        let v0 = version();
        assert!(!dirty());
        let mut c = current().unwrap();
        c.scrub.max_speed = 0.1; // < initial_speed: rejected
        assert!(update(c.clone()).is_err());
        assert_eq!(version(), v0);
        c.scrub.max_speed = 9.0;
        update(c.clone()).unwrap();
        assert_eq!(version(), v0 + 1);
        update(c.clone()).unwrap();
        assert_eq!(version(), v0 + 1, "no-op update keeps the version");
        assert!(dirty());
        assert!(save().unwrap().starts_with("saved to"));
        assert!(!dirty());
        c.scrub.max_speed = 7.0;
        update(c).unwrap();
        assert!(revert().is_ok());
        assert_eq!(current().unwrap().scrub.max_speed, 9.0);
        assert!(!dirty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
