//! Project detection shared by the CLI and the worktree pool: the toolchain a
//! workspace pins, and a stable identity for the repo a canonical is keyed by.
//!
//! These live in the library, not the binary, so a build and the worktree pool
//! derive the *same* lane and canonical keys for the same worktree — if they
//! drifted, the pool would prewarm a lane a build never reads.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The workspace root containing `dir`: the directory of the enclosing workspace
/// `Cargo.toml` (`cargo locate-project --workspace`), with symlinks resolved so a build
/// and prewarm key the same lane (macOS `/var` vs `/private/var`). Falls back to `dir`
/// itself when cargo cannot locate a project.
pub fn workspace(dir: &Path) -> PathBuf {
    let located = (|| {
        let out = Command::new("cargo")
            .args(["locate-project", "--workspace", "--message-format", "plain"])
            .current_dir(dir)
            .output()
            .ok()?;
        let manifest = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !manifest.is_empty())
            .then(|| Path::new(&manifest).parent().map(Path::to_path_buf))?
    })();
    crate::cache::canonical_path(&located.unwrap_or_else(|| dir.to_path_buf()))
}

/// The active rustup toolchain for a workspace. The environment override wins;
/// otherwise rustup resolves directory overrides and both toolchain file formats.
pub fn toolchain(ws: &Path) -> String {
    if let Some(toolchain) = std::env::var_os("RUSTUP_TOOLCHAIN")
        .and_then(|value| value.into_string().ok())
        .filter(|value| !value.is_empty())
    {
        return toolchain;
    }
    if let Some(toolchain) = directory_override(ws) {
        return toolchain;
    }
    if let Some(toolchain) = toolchain_file(ws) {
        return toolchain;
    }
    active_toolchain(ws).unwrap_or_else(|| "stable".to_string())
}

fn active_toolchain(ws: &Path) -> Option<String> {
    if let Ok(output) = Command::new("rustup")
        .args(["show", "active-toolchain"])
        .current_dir(ws)
        .output()
        && output.status.success()
        && let Some(toolchain) = String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
    {
        return Some(toolchain.to_string());
    }
    None
}

fn directory_override(ws: &Path) -> Option<String> {
    let output = Command::new("rustup")
        .args(["override", "list"])
        .current_dir(ws)
        .output()
        .ok()?;
    output.status.success().then_some(())?;
    override_from(ws, &String::from_utf8_lossy(&output.stdout))
}

fn override_from(ws: &Path, output: &str) -> Option<String> {
    let workspace = crate::cache::canonical_path(ws);
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.rsplitn(2, char::is_whitespace);
            let toolchain = fields.next()?.trim();
            let path = fields.next()?.trim();
            let path = crate::cache::canonical_path(Path::new(path));
            workspace
                .starts_with(&path)
                .then(|| (path.components().count(), toolchain.to_string()))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, toolchain)| toolchain)
}

/// Apply a rustup proxy selector from an arbitrary command before choosing its lane.
pub fn command_toolchain(ws: &Path, command: &[String]) -> String {
    commands_toolchain(ws, std::iter::once(command))
}

/// Toolchain identity for a command set. Mixed explicit selectors get a distinct cold
/// key rather than borrowing artifacts from any one compiler's canonical.
pub fn commands_toolchain<'a>(
    ws: &Path,
    commands: impl IntoIterator<Item = &'a [String]>,
) -> String {
    let mut selectors = BTreeSet::new();
    let mut uses_default = false;
    for command in commands {
        if let Some(value) = selector(command) {
            selectors.insert(value);
        } else if cargo(command) {
            uses_default = true;
        }
    }
    if uses_default {
        selectors.insert(toolchain(ws));
    }
    if selectors.is_empty() {
        return toolchain(ws);
    }
    if selectors.len() == 1 {
        return selectors.into_iter().next().unwrap();
    }
    format!(
        "mixed:{}",
        selectors.into_iter().collect::<Vec<_>>().join(",")
    )
}

fn selector(command: &[String]) -> Option<String> {
    cargo(command).then_some(())?;
    command
        .get(1)
        .and_then(|argument| argument.strip_prefix('+'))
        .filter(|selector| !selector.is_empty())
        .map(str::to_string)
}

fn cargo(command: &[String]) -> bool {
    command
        .first()
        .and_then(|program| Path::new(program).file_stem())
        .is_some_and(|program| program == "cargo")
}

fn toolchain_file(ws: &Path) -> Option<String> {
    for name in ["rust-toolchain.toml", "rust-toolchain"] {
        let Ok(text) = std::fs::read_to_string(ws.join(name)) else {
            continue;
        };
        if let Ok(value) = text.parse::<toml::Value>()
            && let Some(table) = value.get("toolchain")
        {
            if let Some(path) = table.get("path").and_then(toml::Value::as_str) {
                return Some(format!(
                    "path:{}",
                    crate::cache::canonical_path(&ws.join(path)).display()
                ));
            }
            if let Some(channel) = table.get("channel").and_then(toml::Value::as_str) {
                return Some(channel.to_string());
            }
        }
        if name == "rust-toolchain" {
            return text
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty() && !line.starts_with('#'))
                .map(str::to_string);
        }
    }
    None
}

/// A stable identity for the repo `ws` belongs to: its canonical shared git directory,
/// which is the same for every worktree of the repo. This is what the canonical is
/// keyed by, so all of a repo's worktrees seed from one warm canonical. Using the
/// canonical common dir (not its parent) keeps the key correct under `--separate-git-dir`
/// and symlinked git dirs, where the parent is not a worktree at all.
pub fn repo_identity(ws: &Path) -> String {
    if let Ok(out) = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(ws)
        .output()
        && out.status.success()
    {
        let common = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !common.is_empty() {
            return crate::cache::canonical_path(&ws.join(common))
                .to_string_lossy()
                .into_owned();
        }
    }
    ws.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::{command_toolchain, commands_toolchain, override_from, toolchain_file};
    use std::collections::BTreeSet;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn command_selector_overrides_the_workspace_toolchain() {
        let workspace = tempdir().unwrap();
        let command = vec![
            "cargo".to_string(),
            "+nightly".to_string(),
            "test".to_string(),
        ];
        assert_eq!(command_toolchain(workspace.path(), &command), "nightly");
    }

    #[test]
    fn reads_legacy_and_path_toolchain_files() {
        let workspace = tempdir().unwrap();
        fs::write(workspace.path().join("rust-toolchain"), "beta\n").unwrap();
        assert_eq!(toolchain_file(workspace.path()).as_deref(), Some("beta"));

        fs::remove_file(workspace.path().join("rust-toolchain")).unwrap();
        fs::create_dir(workspace.path().join("compiler")).unwrap();
        fs::write(
            workspace.path().join("rust-toolchain.toml"),
            "[toolchain]\npath = 'compiler'\n",
        )
        .unwrap();
        assert!(
            toolchain_file(workspace.path())
                .unwrap()
                .starts_with("path:")
        );
    }

    #[test]
    fn mixed_command_selectors_get_a_distinct_identity() {
        let workspace = tempdir().unwrap();
        let stable = ["cargo".into(), "+stable".into(), "check".into()];
        let nightly = ["cargo".into(), "+nightly".into(), "test".into()];
        assert_eq!(
            commands_toolchain(workspace.path(), [&stable[..], &nightly[..]]),
            "mixed:nightly,stable"
        );
    }

    #[test]
    fn explicit_and_default_commands_get_a_mixed_identity() {
        let workspace = tempdir().unwrap();
        let default = super::toolchain(workspace.path());
        let explicit = ["cargo".into(), "+nightly".into(), "check".into()];
        let implicit = ["cargo".into(), "test".into()];
        let expected = BTreeSet::from([default, "nightly".into()])
            .into_iter()
            .collect::<Vec<_>>()
            .join(",");

        assert_eq!(
            commands_toolchain(workspace.path(), [&explicit[..], &implicit[..]]),
            format!("mixed:{expected}")
        );
    }

    #[test]
    fn nearest_directory_override_wins() {
        let root = tempdir().unwrap();
        let workspace = root.path().join("nested/workspace");
        fs::create_dir_all(&workspace).unwrap();
        let output = format!(
            "{} beta\n{} nightly\n",
            root.path().display(),
            workspace.parent().unwrap().display()
        );
        assert_eq!(
            override_from(&workspace, &output).as_deref(),
            Some("nightly")
        );
    }
}
