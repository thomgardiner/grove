//! Install Grove as a user-level skill in agent harness skill directories.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

const MARKER: &str = "<!-- grove:skill:v1 -->";
const SKILL_NAME: &str = "grove";

const SKILL: &str = r#"---
name: grove
description: >
  Verified local execution for parallel Rust agents: CoW build lanes, machine-wide
  governor, path/crate claims, and verification receipts. Never launches models.
  Invoke with /grove (Claude) or by asking to use Grove for builds/claims/worktrees.
  Use when multiple agents share a Rust repo, when cargo thrash or cold targets hurt,
  or when verification must be receipt-bound rather than model-claimed.
---
<!-- grove:skill:v1 -->

# Grove

You are working in a multi-agent Rust environment. **Shell the `grove` CLI** for
builds, claims, worktrees, and verification. Do not set `CARGO_TARGET_DIR` or
`CARGO_BUILD_BUILD_DIR` yourself; Grove owns lanes and the governor.

## Invoke

| Harness | How |
| --- | --- |
| Claude Code | `/grove` or “use Grove for this build” |
| Codex | skill under `~/.codex/skills/grove` |
| Shell | `grove check` · `grove test` · `grove claim` · `grove task …` |
| First install | `grove setup` then `grove init` in each repo |

## Everyday loop

```sh
grove status --json          # live claims, tasks, worktrees, lanes
grove check                  # affected packages from the git diff
grove test
grove claim --agent <id> --task "<what>" <paths|crate:name …>
grove task begin --agent <id> --task "<what>" --scope <path …>
grove task exec --task-id <id> -- <command>
grove verify <profile> --task-id <id>
grove task finish --task-id <id>
```

Worktrees:

```sh
grove worktree acquire --agent <id>
# … work …
grove worktree heartbeat <path>    # if not under task exec
grove worktree release <path>
```

Opaque commands and gates:

```sh
grove exec --tag <gate> -- <command>
grove cache warm                   # once per machine after a green full build
```

## Rules

- Exit 0 success, 1 domain refusal (claim conflict, failed verify/tests), else error.
- Most commands print JSON.
- Claims refuse overlap; first wins. Prefer durable tasks for multi-minute work.
- `verified` requires fresh profile receipts (or recorded `--allow-unverified`).
- Never run plain concurrent cargo across worktrees sharing one target.
- Grove does **not** launch models. For fleets, use Summoner or another orchestrator
  that speaks this CLI; Grove stays the host/plugin.

## Repo contract

`grove init` writes `AGENTS.md` + `.grove.toml` starter. Prefer that over copying
this skill into every repository.
"#;

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub written: Vec<String>,
    pub skipped: Vec<String>,
    pub next_steps: Vec<String>,
}

pub fn install_user_skills(refresh: bool) -> Result<Report> {
    let home =
        crate::config::home_dir().context("HOME/USERPROFILE unset; cannot install skills")?;
    let codex_home = std::env::var_os("CODEX_HOME").map(PathBuf::from);
    install_into(&home, codex_home.as_deref(), refresh)
}

pub fn installed_paths() -> Vec<PathBuf> {
    let Some(home) = crate::config::home_dir() else {
        return Vec::new();
    };
    let codex_home = std::env::var_os("CODEX_HOME").map(PathBuf::from);
    skill_roots(&home, codex_home.as_deref())
        .into_iter()
        .map(|root| root.join(SKILL_NAME).join("SKILL.md"))
        .filter(|path| {
            std::fs::read_to_string(path)
                .map(|text| text.contains(MARKER))
                .unwrap_or(false)
        })
        .collect()
}

pub(crate) fn install_into(
    home: &Path,
    codex_home: Option<&Path>,
    refresh: bool,
) -> Result<Report> {
    let mut report = Report::default();
    for root in skill_roots(home, codex_home) {
        install_one(&root, refresh, &mut report)?;
    }
    report.next_steps = next_steps();
    Ok(report)
}

fn skill_roots(home: &Path, codex_home: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = vec![home.join(".claude").join("skills")];
    let codex = codex_home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".codex"));
    roots.push(codex.join("skills"));
    roots.push(home.join(".agents").join("skills"));
    roots.push(home.join(".grok").join("skills"));
    roots
}

fn install_one(root: &Path, refresh: bool, report: &mut Report) -> Result<()> {
    let dir = root.join(SKILL_NAME);
    let path = dir.join("SKILL.md");
    let label = path.display().to_string();
    match std::fs::read_to_string(&path) {
        Ok(existing) if existing == SKILL => {
            report.skipped.push(label);
            return Ok(());
        }
        Ok(existing) if existing.contains(MARKER) && !refresh => {
            report
                .skipped
                .push(format!("{label} (managed; pass --refresh to update)"));
            return Ok(());
        }
        Ok(existing) if !existing.contains(MARKER) && !refresh => {
            report.skipped.push(format!(
                "{label} (exists, not grove-managed; pass --refresh to overwrite)"
            ));
            return Ok(());
        }
        Ok(_) | Err(_) => {}
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(&path, SKILL).with_context(|| format!("writing {label}"))?;
    report.written.push(label);
    Ok(())
}

fn next_steps() -> Vec<String> {
    vec![
        "Claude Code: /grove (reload session if missing)".into(),
        "Codex: skill under ~/.codex/skills/grove".into(),
        "In each Rust repo: grove init && grove cache warm".into(),
        "Agents: grove check / grove test instead of plain cargo".into(),
        "Optional fleets: install summoner; host plugin uses this CLI".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_four_harness_roots() {
        let tmp = tempfile::TempDir::new().unwrap();
        let report = install_into(tmp.path(), None, false).unwrap();
        assert_eq!(report.written.len(), 4, "{report:?}");
        let again = install_into(tmp.path(), None, false).unwrap();
        assert!(again.written.is_empty());
    }
}
