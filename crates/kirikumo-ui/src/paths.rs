//! Where this app keeps what little it keeps.
//!
//! `~/.kirikumo/` holds the window's settings and its logs, and nothing else.
//! Nothing about a cluster is ever written to disk — no response cache, no
//! snapshot, no credential (`AGENTS.md` rule 10) — so unlike its sibling
//! apps there is no cache directory here at all. `KIRIKUMO_HOME` moves the
//! directory, which is how a test or a second profile keeps out of the real
//! one.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The root and the files under it.
#[derive(Debug, Clone)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    /// `KIRIKUMO_HOME`, or `~/.kirikumo`.
    pub fn from_env() -> Result<Self> {
        if let Some(root) = std::env::var_os("KIRIKUMO_HOME") {
            return Ok(Self::with_root(root));
        }
        let home = dirs::home_dir().context("no home directory")?;
        Ok(Self::with_root(home.join(".kirikumo")))
    }

    /// Rooted somewhere specific.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory itself.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The window's settings: `app.json`.
    pub fn app_settings(&self) -> PathBuf {
        self.root.join("app.json")
    }

    /// Daily-rotated logs.
    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// Create the directories, which is safe to repeat.
    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(self.logs())
            .with_context(|| format!("creating {}", self.logs().display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_hangs_off_one_directory() {
        let paths = Paths::with_root("/tmp/kirikumo-test");
        assert_eq!(
            paths.app_settings(),
            Path::new("/tmp/kirikumo-test/app.json")
        );
        assert_eq!(paths.logs(), Path::new("/tmp/kirikumo-test/logs"));
    }

    #[test]
    fn ensure_is_safe_to_repeat() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().join("home"));
        paths.ensure().unwrap();
        paths.ensure().unwrap();
        assert!(paths.logs().is_dir());
    }
}
