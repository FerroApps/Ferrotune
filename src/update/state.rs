//! What the updater remembers between runs (`$XDG_STATE_HOME/ferrotune/
//! update.json`): the dismissed version, the API ETag and the release it
//! belongs to (a `304` means "the same as this"). No secrets, no IDs.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::release::Release;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Persisted {
    /// The banner's [x] was pressed for this version.
    pub dismissed: Option<String>,
    pub etag: Option<String>,
    pub release: Option<Release>,
}

pub const FILE: &str = "update.json";

impl Persisted {
    /// Missing or unreadable → defaults (never fails).
    pub fn load(dir: &Path) -> Self {
        std::fs::read(dir.join(FILE))
            .ok()
            .filter(|b| b.len() < 256 * 1024)
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Atomic write (temp + rename).
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{FILE}.tmp"));
        std::fs::write(
            &tmp,
            serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?,
        )?;
        std::fs::rename(tmp, dir.join(FILE))
    }

    /// Should the banner offer `version`? Not once it was dismissed; a
    /// newer version shows again.
    pub fn should_offer(&self, version: &str) -> bool {
        self.dismissed.as_deref() != Some(version)
    }
}

/// Dismiss `version` in the file in `dir` (load, modify, save).
pub fn dismiss(dir: &Path, version: &str) -> std::io::Result<()> {
    let mut p = Persisted::load(dir);
    p.dismissed = Some(version.to_owned());
    p.save(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dismissed_version_persists_until_a_newer_one() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(Persisted::load(tmp.path()), Persisted::default());
        dismiss(tmp.path(), "0.9.0").unwrap();
        let p = Persisted::load(tmp.path());
        assert!(!p.should_offer("0.9.0"));
        assert!(p.should_offer("0.9.1"));
        // Other fields survive a dismissal.
        let mut p = p;
        p.etag = Some("W/\"abc\"".into());
        p.save(tmp.path()).unwrap();
        dismiss(tmp.path(), "0.9.1").unwrap();
        let p = Persisted::load(tmp.path());
        assert_eq!(p.etag.as_deref(), Some("W/\"abc\""));
        assert_eq!(p.dismissed.as_deref(), Some("0.9.1"));
    }

    #[test]
    fn corrupt_file_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(FILE), "{nope").unwrap();
        assert_eq!(Persisted::load(tmp.path()), Persisted::default());
    }
}
