//! Hook-specific settings: the `[hook]` table of `acr-rewind.toml` (ignored by `rewind_core`).

use serde::Deserialize;

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default)]
pub struct HookConfig {
    /// Phase-3 validation: record and log snapshots vs shared memory; never write or freeze.
    pub read_only: bool,
    /// How often read-only mode logs a snapshot-vs-shm comparison (ms).
    pub validation_log_interval_ms: u64,
    /// `tracing` filter, e.g. "info", "debug", "acr_hook=trace".
    pub log_level: String,
    /// "dx12" | "dx11" | "off"
    pub overlay: String,
    /// Give up waiting for the engine (GObjects / names) after this many seconds.
    pub init_timeout_s: u64,
    /// After a resume, compare shared memory against the restored state and disable the mod
    /// if the write evidently did not reach the simulation.
    pub post_resume_check: bool,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            read_only: true,
            validation_log_interval_ms: 1000,
            log_level: "info".into(),
            overlay: "dx12".into(),
            init_timeout_s: 300,
            post_resume_check: true,
        }
    }
}

#[derive(Deserialize, Default)]
struct Wrapper {
    #[serde(default)]
    hook: HookConfig,
}

impl HookConfig {
    pub fn from_toml_str(s: &str) -> Result<Self, String> {
        let w: Wrapper = toml::from_str(s).map_err(|e| e.to_string())?;
        let c = w.hook;
        if !matches!(c.overlay.as_str(), "dx12" | "dx11" | "off") {
            return Err(format!("hook.overlay must be dx12, dx11 or off (got '{}')", c.overlay));
        }
        if c.validation_log_interval_ms == 0 {
            return Err("hook.validation_log_interval_ms must be > 0".into());
        }
        Ok(c)
    }
}

/// Steam `buildid` of the installed game, read from the `appmanifest_*.acf` whose
/// `installdir` contains `exe_path`. Read-only; never touches the game files.
pub fn steam_build_id(exe_path: &std::path::Path) -> Option<String> {
    let mut install_dir = None;
    let mut steamapps = None;
    for a in exe_path.ancestors() {
        if a.file_name().is_some_and(|n| n.eq_ignore_ascii_case("common")) {
            steamapps = a.parent().map(std::path::Path::to_path_buf);
            break;
        }
        install_dir = a.file_name().map(|n| n.to_string_lossy().into_owned());
    }
    let (steamapps, install_dir) = (steamapps?, install_dir?);
    for entry in std::fs::read_dir(&steamapps).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("appmanifest_") && name.ends_with(".acf")) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        if acf_value(&text, "installdir").is_some_and(|d| d.eq_ignore_ascii_case(&install_dir)) {
            return acf_value(&text, "buildid");
        }
    }
    None
}

/// First `"key"  "value"` pair in a Valve KeyValues text.
pub fn acf_value(text: &str, key: &str) -> Option<String> {
    let quoted = format!("\"{key}\"");
    text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix(&quoted)?;
        let v = rest.trim();
        let v = v.strip_prefix('"')?.strip_suffix('"')?;
        Some(v.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_config_hook_section() {
        let s = include_str!("../../../config/acr-rewind.toml");
        let c = HookConfig::from_toml_str(s).unwrap();
        assert!(!c.read_only, "shipped config writes (verified in-game); the online guard still applies");
        assert!(HookConfig::default().read_only, "a missing [hook] section stays fail-safe");
        assert_eq!(c.overlay, "dx12");
        // The core config still parses the same file.
        rewind_core::Config::from_toml_str(s).unwrap();
    }

    #[test]
    fn defaults_and_validation() {
        assert_eq!(HookConfig::from_toml_str("").unwrap(), HookConfig::default());
        let c = HookConfig::from_toml_str("[hook]\nread_only = false\noverlay = \"off\"").unwrap();
        assert!(!c.read_only);
        assert!(HookConfig::from_toml_str("[hook]\noverlay = \"vulkan\"").is_err());
        assert!(HookConfig::from_toml_str("[hook]\nread_only = 3").is_err());
    }

    #[test]
    fn acf_parsing() {
        let acf = "\"AppState\"\n{\n\t\"appid\"\t\t\"3917090\"\n\t\"installdir\"\t\t\"Assetto Corsa Rally\"\n\t\"buildid\"\t\t\"25170642\"\n}";
        assert_eq!(acf_value(acf, "buildid").as_deref(), Some("25170642"));
        assert_eq!(acf_value(acf, "installdir").as_deref(), Some("Assetto Corsa Rally"));
        assert_eq!(acf_value(acf, "missing"), None);
    }

    #[test]
    fn build_id_from_fake_library() {
        let root = std::env::temp_dir().join(format!("acr_rewind_acf_{}", std::process::id()));
        let exe_dir = root.join("steamapps/common/My Game/acr/Binaries/Win64");
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::write(
            root.join("steamapps/appmanifest_1.acf"),
            "\"AppState\"\n{\n\t\"installdir\"\t\t\"Other\"\n\t\"buildid\"\t\t\"1\"\n}",
        )
        .unwrap();
        std::fs::write(
            root.join("steamapps/appmanifest_2.acf"),
            "\"AppState\"\n{\n\t\"installdir\"\t\t\"My Game\"\n\t\"buildid\"\t\t\"777\"\n}",
        )
        .unwrap();
        assert_eq!(steam_build_id(&exe_dir.join("acr.exe")).as_deref(), Some("777"));
        assert_eq!(steam_build_id(std::path::Path::new("C:/nowhere/acr.exe")), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
