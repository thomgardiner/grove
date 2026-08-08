//! Install and print the embedded agent skill (`skill/SKILL.md`).

use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;

/// Packaged skill markdown (source of truth at build time).
pub const SKILL_MD: &str = include_str!("../skill/SKILL.md");

/// Directories that should receive `grove/SKILL.md` when present or creatable.
pub fn install_targets() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        out.push(home.join(".grok/skills/grove"));
        out.push(home.join(".agents/skills/grove"));
        out.push(home.join(".codex/skills/grove"));
        out.push(home.join(".claude/skills/grove"));
        out.push(home.join(".config/claude/skills/grove"));
    }
    if let Ok(extra) = std::env::var("GROVE_SKILL_DIR") {
        out.push(PathBuf::from(extra));
    }
    out
}

/// Write `SKILL.md` into each install target. Returns paths written.
pub fn install() -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for dir in install_targets() {
        if let Err(e) = fs::create_dir_all(&dir) {
            eprintln!("grove: skill skip {}: {e}", dir.display());
            continue;
        }
        let path = dir.join("SKILL.md");
        fs::write(&path, SKILL_MD).with_context(|| format!("write {}", path.display()))?;
        written.push(path);
    }
    if written.is_empty() {
        anyhow::bail!("no skill install targets writable (set HOME or GROVE_SKILL_DIR)");
    }
    Ok(written)
}

/// Print skill body to stdout (for piping or review).
pub fn print_skill() {
    print!("{SKILL_MD}");
}

/// Path hint for humans (embedded; not a filesystem path after install).
pub fn describe() -> String {
    format!(
        "embedded skill/SKILL.md ({} bytes); install with: grove skill install",
        SKILL_MD.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_body_is_present() {
        assert!(SKILL_MD.contains("name: grove"));
        assert!(SKILL_MD.contains("grove check"));
        assert!(SKILL_MD.contains("CacheHost"));
    }

    #[test]
    fn skill_body_roundtrip_write() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("SKILL.md");
        fs::write(&path, SKILL_MD).unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("grove check"));
    }
}
