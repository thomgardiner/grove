//! `grove init`: drop the coordination contract into a repository so any agent
//! harness (Codex, Claude, OpenCode, anything driving a shell) learns the same rules
//! from the repo itself instead of from one vendor's private configuration.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;

const MARKER: &str = "<!-- grove:agents:v1 -->";

const AGENTS_HEADER: &str = r#"<!-- grove:agents:v1 -->
## Grove: build and coordination contract

Every agent in this repository coordinates through `grove`'s registry; in a
Cargo workspace it also builds through grove. These rules keep many parallel
agents from corrupting builds or each other's work. Most commands print JSON.
Exit codes: 0 is success, 1 is a domain refusal (claim conflict, failed
verification, failing tests), anything else is an error.
"#;

const AGENTS_RUST: &str = r#"
Build and test (never run plain cargo in a shared checkout):

- `grove check` / `grove test` route to the packages affected by your git diff.
- `grove exec --tag <gate> -- <command>` runs anything else in an isolated lane.
- Never set `CARGO_TARGET_DIR`, `CARGO_BUILD_BUILD_DIR`, or `MAKEFLAGS`; grove owns
  lane isolation and the machine-wide build governor.
"#;

const AGENTS_COORDINATION: &str = r#"
Coordinate before writing:

- `grove status --json` shows every live claim and task; check it first.
- `grove claim --agent <stable-id> --task "<what>" <paths|crate:name ...>` claims
  your scope. First wins; a conflict exits 1. Claims expire after the claim TTL
  (default 30 minutes); re-running the same claim renews it. There is no default
  identity: pick a stable id unique to your session (or export GROVE_AGENT once),
  because two sessions sharing a name renew each other's claims instead of
  conflicting.
- Work longer than a few minutes belongs in a durable task instead:
  `grove task begin --agent <id> --task "<what>" --scope <path ...>`, then
  `grove task exec --task-id <id> -- <command>`. Command heartbeats keep it alive.
- Choose the capability. `--capability build` (the default) reserves the task's
  build lane and an admission slot for the command's whole lifetime: right when
  the command IS a build, wrong for anything long-lived. Supervise an agent
  session with `--capability edit`, which keeps the heartbeats, deadline, and
  signal forwarding but takes no lane, so builds the session runs acquire
  admission only while they build. Under `build`, `max_builders` caps live
  sessions rather than concurrent compilers, and a grove build started inside
  the supervised command refuses immediately rather than waiting on a lane its
  own supervisor holds.
- `grove verify <profile> --task-id <id>` records verification receipts.
- `grove task finish --task-id <id>` needs fresh receipts for the repository's
  required profiles, or an explicit `--allow-unverified "<reason>"`, which is
  recorded. Writes outside the task's declared scope block finish unconditionally.
  The verification policy is pinned at `task begin`, so weakening `.grove.toml`
  mid-task makes finish refuse with `policy_changed` rather than accept it.
- Release standalone claims with `grove release claims --agent <id>`.

Worktrees:

- `grove worktree acquire --agent <stable-id>` gives an isolated checkout on its
  own branch; `grove worktree release <path>` only after the work is landed.
- For large repositories, request a proved sparse checkout with
  `grove worktree acquire --agent <id> --materialize crate:<name>`. Add scope with
  `grove worktree expand PATH <scope...>` or convert permanently with
  `grove worktree full PATH`; expansion never shrinks an active checkout.
- Sparse checkout is a size optimization, not a sandbox or claim. Affected builds
  expand package closure; opaque commands, verification, task exec, cache warm, and
  release freeze convert full before launching.
- Agents outside supervised `grove task exec` commands run
  `grove worktree heartbeat PATH` periodically while they own the checkout.
- Idle worktrees are reaped after the TTL; committed and dirty work is salvaged to
  the worktree's branch first; nonterminal tasks and live lanes also protect work.

Observe the fleet with `grove status --json --watch` and the event signal at
`<cache-root>/events/<repo>.jsonl` (claims, tasks, verifications, reaps).
JSONL is a low-latency best-effort signal: rotation or write failure can create gaps.
Consumers reconcile durable task, claim, lease, and receipt state before acting.

Keep `docs/ai/` to exactly `RECURRING_BUGS.md`, `DEBUG_RECIPES.md`, and
`LESSONS_LEARNED.md`, recording continuity notes in Symptom/Cause/Fix form.
"#;

const GROVE_TOML: &str = r#"# Grove configuration. Defaults are sensible; uncomment to tune.
# `grove config` prints the resolved values and where this file lives.

# min_free_gb      = 20   # disk watermark grove keeps free
# max_canonical_gb = 40   # cap on warm canonical caches
# governor_mode    = "best_effort" # set strict for Unix fail-closed admission
# cpu_slots        = 8    # cooperating jobserver jobs (default: core count)
# max_builders     = 1    # admitted builders in strict mode
# reap_ttl_secs    = 7200 # idle time before an agent worktree is reaped
# claim_ttl_secs   = 1800 # idle time before a standalone claim expires
#
# [worktree]
# materialize = ["schemas/generated"] # extra repo-relative cones for scoped worktrees

# [verification]
# required = ["fast"]
#
# [verification.profiles.fast]
# continue_on_failure = false
# commands = [{ argv = ["cargo", "nextest", "run"], allow_zero_tests = false }]
"#;

#[derive(Serialize)]
pub struct Report {
    pub written: Vec<String>,
    pub skipped: Vec<String>,
}

/// Write the `.grove.toml` starter and the `AGENTS.md` contract section, without ever
/// clobbering what a repository already has: an existing `.grove.toml` is left alone,
/// and an existing `AGENTS.md` only gains the section when the marker is absent.
/// The contract for this repository: the coordination surface everywhere,
/// with the Cargo build rules included only where a Cargo workspace exists.
fn agents_section(workspace: &Path) -> String {
    let rust = if crate::project::is_cargo_workspace(workspace) {
        AGENTS_RUST
    } else {
        ""
    };
    format!("{AGENTS_HEADER}{rust}{AGENTS_COORDINATION}")
}

pub fn init(workspace: &Path) -> Result<Report> {
    let mut written = Vec::new();
    let mut skipped = Vec::new();

    let toml = workspace.join(".grove.toml");
    if toml.exists() {
        skipped.push(".grove.toml".to_string());
    } else {
        std::fs::write(&toml, GROVE_TOML).context("writing .grove.toml")?;
        written.push(".grove.toml".to_string());
    }

    let section = agents_section(workspace);
    let agents = workspace.join("AGENTS.md");
    match std::fs::read_to_string(&agents) {
        Ok(existing) if existing.contains(MARKER) => skipped.push("AGENTS.md".to_string()),
        Ok(existing) => {
            let joined = format!("{}\n{}", existing.trim_end(), section);
            std::fs::write(&agents, joined).context("appending to AGENTS.md")?;
            written.push("AGENTS.md (appended)".to_string());
        }
        Err(_) => {
            std::fs::write(&agents, format!("# Agent guide\n\n{section}"))
                .context("writing AGENTS.md")?;
            written.push("AGENTS.md".to_string());
        }
    }

    // Claude Code reads CLAUDE.md, not AGENTS.md, so without this bridge the
    // contract is invisible to the one major harness that skips the standard
    // filename. `@AGENTS.md` is Claude Code's own import syntax; the bridge
    // adds a single line rather than duplicating the contract into a second
    // file that would drift.
    let claude = workspace.join("CLAUDE.md");
    match std::fs::read_to_string(&claude) {
        Ok(existing) if existing.contains("@AGENTS.md") || existing.contains(MARKER) => {
            skipped.push("CLAUDE.md".to_string());
        }
        Ok(existing) => {
            let joined = format!("{}\n\n{}", existing.trim_end(), CLAUDE_BRIDGE);
            std::fs::write(&claude, joined).context("appending to CLAUDE.md")?;
            written.push("CLAUDE.md (appended)".to_string());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::write(&claude, CLAUDE_BRIDGE).context("writing CLAUDE.md")?;
            written.push("CLAUDE.md".to_string());
        }
        // A read error that is not "absent" (a permission problem, a non-UTF-8
        // file) must not be mistaken for a missing file and overwritten;
        // no-clobber means no-clobber.
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", claude.display()));
        }
    }
    Ok(Report { written, skipped })
}

/// One import line, not a copy: Claude Code inlines the referenced file, so
/// the contract stays single-sourced in AGENTS.md.
const CLAUDE_BRIDGE: &str = "<!-- grove:claude-bridge:v1 -->\n@AGENTS.md\n";
