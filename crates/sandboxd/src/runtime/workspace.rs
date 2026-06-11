//! Per-sandbox workspace helper shared by the runtimes. A workspace is the
//! sandbox's writable area: the COW disk mount inside the microVM for
//! Firecracker, or a host directory for the development runtime.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Workspaces {
    root: PathBuf,
}

impl Workspaces {
    pub fn new(root: impl Into<PathBuf>) -> Workspaces {
        Workspaces { root: root.into() }
    }

    pub fn dir_for(&self, handle: &str) -> PathBuf {
        self.root.join(handle)
    }

    pub fn create(&self, handle: &str) -> Result<PathBuf> {
        let dir = self.dir_for(handle);
        std::fs::create_dir_all(dir.join("workspace"))?;
        Ok(dir)
    }

    /// Resolve a guest path to a host path, jailed under the workspace so the
    /// file API can never escape it. Leading `/` is treated as workspace-root.
    pub fn resolve(&self, handle: &str, guest_path: &str) -> Result<PathBuf> {
        let base = self.dir_for(handle).join("workspace");
        let rel = guest_path.trim_start_matches('/');
        let candidate = base.join(rel);
        // Defend against `..` traversal.
        let normalized = normalize(&candidate);
        if !normalized.starts_with(&base) {
            bail!("path escapes workspace: {guest_path}");
        }
        Ok(normalized)
    }

    pub fn remove(&self, handle: &str) -> Result<()> {
        let dir = self.dir_for(handle);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }
}

/// Lexically normalize a path (resolve `.` and `..`) without touching the FS.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jails_traversal() {
        let tmp = std::env::temp_dir().join(format!("ws-test-{}", std::process::id()));
        let ws = Workspaces::new(&tmp);
        ws.create("sbx_x").unwrap();
        assert!(ws.resolve("sbx_x", "a/b.txt").is_ok());
        assert!(ws.resolve("sbx_x", "../../etc/passwd").is_err());
        ws.remove("sbx_x").ok();
    }
}
