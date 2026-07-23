//! Deciding which git commands must be serialized across worktrees.
//!
//! Agents in separate worktrees share one `.git`, so their git writes race on
//! the shared parts of it: `.git/config` (one file for every worktree) and
//! any ref not private to a worktree (tags, and a branch two worktrees both
//! touch) all fail with `could not lock config file: File exists` or `cannot
//! lock ref` under concurrency. This is the most-reported multi-agent failure,
//! and worktrees do not fix it because the contended state is shared behind
//! them. Grove already holds a per-repository git lock for its own worktree
//! plumbing; routing an agent's writes through the same lock serializes them
//! against each other and against grove.
//!
//! Reads and the writes that touch only a worktree's own index, working tree,
//! or a content-addressed object run without the lock, so status/log/diff/add
//! stay parallel. Everything else takes the lock: a needless lock is merely
//! slower, a missing one loses work, so the unknown case is serialized.

/// Git subcommands that touch only per-worktree or content-addressed state, so
/// concurrent invocations across worktrees cannot corrupt shared `.git` data.
/// Reads plus the index/worktree/object writes git already makes safe.
const UNSERIALIZED: &[&str] = &[
    // Pure reads.
    "status",
    "log",
    "diff",
    "show",
    "rev-parse",
    "rev-list",
    "ls-files",
    "ls-tree",
    "ls-remote",
    "cat-file",
    "blame",
    "describe",
    "merge-base",
    "for-each-ref",
    "show-ref",
    "name-rev",
    "shortlog",
    "whatchanged",
    "grep",
    "count-objects",
    "diff-tree",
    "diff-files",
    "diff-index",
    "check-ignore",
    "check-attr",
    "var",
    "version",
    // Writes confined to this worktree's index/working tree, or a
    // content-addressed object write git serializes itself.
    "add",
    "rm",
    "mv",
    "restore",
    "hash-object",
];

/// Options that consume the following argument, so the real subcommand is not
/// the token right after them. Attached forms (`--git-dir=…`) are handled by
/// the leading-dash skip and need not be listed.
const VALUE_OPTIONS: &[&str] = &[
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--super-prefix",
    "--config-env",
];

/// The subcommand `git` will run, skipping the global options that precede it.
fn subcommand(args: &[String]) -> Option<&str> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if VALUE_OPTIONS.contains(&arg.as_str()) {
            iter.next(); // its value is not the subcommand
            continue;
        }
        if arg.starts_with('-') {
            continue; // a flag, attached-value option, or `--`
        }
        return Some(arg);
    }
    None
}

/// Whether this git invocation must hold the repository's git lock. Unknown
/// subcommands serialize: correctness over parallelism, since the cost of an
/// unnecessary lock is latency and the cost of a missing one is lost work.
pub fn needs_serialization(args: &[String]) -> bool {
    match subcommand(args) {
        Some(command) => !UNSERIALIZED.contains(&command),
        // Bare `git` prints usage and touches nothing; no need to serialize.
        None => false,
    }
}

/// Write a `git` shim under `root` and return its directory, so a supervised
/// command with this directory first on PATH has its git writes serialized
/// without knowing to call `grove git`. Best-effort: returns `None` if grove's
/// own path or the real git cannot be resolved, so supervision never fails for
/// want of the shim. Unix only; on other platforms a caller uses `grove git`.
///
/// The shim routes every git call through `grove git`, except when
/// `GROVE_GIT_GATE` is already set — the marker grove sets while running git
/// under the lock — in which case it runs the real git directly, so a git that
/// grove itself spawned (or one a hook spawns underneath it) neither recurses
/// through the shim nor blocks on a lock its parent already holds.
#[cfg(unix)]
pub fn install_shim(root: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let grove = std::env::current_exe().ok()?;
    let dir = root.join("gitshim");
    std::fs::create_dir_all(&dir).ok()?;
    let real_git = resolve_real_git(&dir)?;
    let script = format!(
        "#!/bin/sh\n\
         if [ -n \"$GROVE_GIT_GATE\" ]; then exec {real:?} \"$@\"; fi\n\
         exec {grove:?} git -- \"$@\"\n",
        real = real_git,
        grove = grove,
    );
    // Atomic replace: a concurrent shim mid-exec must never read a torn file.
    let temp = dir.join(format!("git.{}.tmp", std::process::id()));
    std::fs::write(&temp, script).ok()?;
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755)).ok()?;
    std::fs::rename(&temp, dir.join("git")).ok()?;
    Some(dir)
}

/// The real `git` on PATH, skipping our own shim directory so the shim never
/// resolves to itself.
#[cfg(unix)]
fn resolve_real_git(shim_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir == shim_dir {
            continue;
        }
        let candidate = dir.join("git");
        if let Ok(meta) = std::fs::metadata(&candidate)
            && meta.is_file()
            && meta.permissions().mode() & 0o111 != 0
        {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn shared_state_writers_are_serialized() {
        // Exactly the operations the reproduction showed racing on shared
        // `.git`: config writes, tag updates, ref updates, commits.
        for command in [
            "commit",
            "config",
            "tag",
            "update-ref",
            "merge",
            "rebase",
            "push",
            "fetch",
            "pull",
            "gc",
            "repack",
            "pack-refs",
            "branch",
            "remote",
            "stash",
            "am",
        ] {
            assert!(needs_serialization(&args(&[command])), "{command}");
        }
    }

    #[test]
    fn reads_and_per_worktree_writes_run_free() {
        for command in [
            "status",
            "log",
            "diff",
            "show",
            "rev-parse",
            "ls-files",
            "cat-file",
            "add",
            "rm",
            "mv",
            "restore",
        ] {
            assert!(!needs_serialization(&args(&[command])), "{command}");
        }
    }

    #[test]
    fn global_options_are_skipped_to_find_the_subcommand() {
        // `git -C path -c k=v commit …` still resolves to commit.
        assert!(needs_serialization(&args(&[
            "-C",
            "/some/path",
            "-c",
            "user.name=x",
            "commit",
            "-m",
            "msg",
        ])));
        // `git --git-dir=… status` is still a read.
        assert!(!needs_serialization(&args(&[
            "--git-dir=/g",
            "--no-pager",
            "status",
        ])));
        // `-C path status` skips the path, finds status.
        assert!(!needs_serialization(&args(&["-C", "/repo", "status"])));
    }

    #[test]
    fn an_unknown_subcommand_is_serialized() {
        assert!(needs_serialization(&args(&["frobnicate"])));
        assert!(needs_serialization(&args(&["some-plugin", "--flag"])));
    }

    #[test]
    fn bare_git_is_not_serialized() {
        assert!(!needs_serialization(&args(&[])));
        assert!(!needs_serialization(&args(&["--help"])));
    }
}
