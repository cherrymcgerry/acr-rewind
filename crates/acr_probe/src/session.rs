//! Persistent state in the work dir (default `re-work/`, gitignored):
//!
//! ```text
//! re-work/scans/<session>/session.json    scan parameters, variants, history
//! re-work/scans/<session>/candidates.bin  compact candidate list (see scan::CandidateSet)
//! re-work/pointers.json                   pointer-scan results (for pointer-verify)
//! re-work/correlate.json                  last struct-correlate result
//! re-work/dumper7-import.json             last import-dumper7 report
//! ```

use crate::oracle::Target;
use crate::scan::{NextMode, NumType};
use crate::variants::Variant;
use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Step {
    pub op: String,
    pub mode: Option<NextMode>,
    pub before: usize,
    pub after: usize,
    pub target: Vec<f64>,
    pub unix_time: u64,
    pub millis: u128,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    pub pid: u32,
    pub module_base: u64,
    pub target: Target,
    pub value_type: NumType,
    pub tol: f64,
    pub stride: usize,
    pub align: usize,
    pub wheel: usize,
    pub custom_value: Option<f64>,
    pub variants: Vec<Variant>,
    pub variant_desc: Vec<String>,
    pub count: usize,
    pub history: Vec<Step>,
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub struct WorkDir {
    pub root: PathBuf,
}

impl WorkDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn ensure(&self, sub: &str) -> Result<PathBuf> {
        let p = self.root.join(sub);
        std::fs::create_dir_all(&p).with_context(|| format!("creating {}", p.display()))?;
        Ok(p)
    }

    pub fn scan_dir(&self, session: &str) -> PathBuf {
        self.root.join("scans").join(session)
    }

    pub fn session_json(&self, session: &str) -> PathBuf {
        self.scan_dir(session).join("session.json")
    }

    pub fn candidates_bin(&self, session: &str) -> PathBuf {
        self.scan_dir(session).join("candidates.bin")
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    pub fn save_json<T: Serialize>(&self, path: &Path, v: &T) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(v)?).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn load_json<T: DeserializeOwned>(&self, path: &Path) -> Result<T> {
        let s = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display()))
    }

    /// Scan sessions present on disk.
    pub fn sessions(&self) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(self.root.join("scans"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().join("session.json").exists())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_roundtrip() {
        let root = std::env::temp_dir().join(format!("acr_probe_wd_{}", std::process::id()));
        let wd = WorkDir::new(&root);
        let s = Session {
            version: 1,
            pid: 7,
            module_base: 0x1_4000_0000,
            target: Target::Pos,
            value_type: NumType::F64,
            tol: 0.05,
            stride: 1,
            align: 4,
            wheel: 0,
            custom_value: None,
            variants: vec![Variant::identity(3, 100.0)],
            variant_desc: vec!["x".into()],
            count: 3,
            history: vec![],
        };
        wd.save_json(&wd.session_json("pos"), &s).unwrap();
        let back: Session = wd.load_json(&wd.session_json("pos")).unwrap();
        assert_eq!(back.module_base, s.module_base);
        assert_eq!(back.target, Target::Pos);
        assert_eq!(wd.sessions(), vec!["pos".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
