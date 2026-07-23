use anyhow::Result;
use std::path::Path;

use super::load;
use crate::snapshot;

/// Persistent writes since task begin that lie outside the task's declared scope.
/// Pre-v0.3 tasks lack a baseline, so they remain readable but cannot produce a false
/// positive from an already-dirty worktree.
pub(crate) fn outside_scope(root: &Path, repo: &str, id: &str) -> Result<Vec<String>> {
    let task = load(root, repo, id)?;
    let Some(reference) = task.scope_snapshot.as_ref() else {
        return Ok(Vec::new());
    };
    let workspace = Path::new(&task.workspace);
    let before = snapshot::validate(root, repo, reference)?;
    let after = snapshot::capture(workspace)?;
    let scope = if task.resolved_scope.is_empty() {
        &task.scope
    } else {
        &task.resolved_scope
    };
    // Only exempt a workspace-root Cargo.lock that did not exist at task begin
    // (Cargo generated it as a build byproduct). A tracked lockfile that was
    // already present at begin is agent-authored dependency surface and stays
    // in the out-of-scope set when not declared.
    let lock_absent_at_begin = !before
        .entries
        .iter()
        .any(|entry| entry.path == "Cargo.lock");
    let build_byproduct = crate::project::is_cargo_workspace(workspace) && lock_absent_at_begin;
    Ok(snapshot::changed_paths(workspace, &before, &after)?
        .into_iter()
        .filter(|path| !scope.iter().any(|scope| contains(scope, path)))
        .filter(|path| !(build_byproduct && path == "Cargo.lock"))
        .collect())
}

fn contains(scope: &str, path: &str) -> bool {
    scope == "." || path == scope || path.starts_with(&format!("{scope}/"))
}

#[cfg(test)]
mod tests {
    use super::contains;

    #[test]
    fn scope_matches_exact_and_prefix() {
        assert!(contains("src", "src"));
        assert!(contains("src", "src/lib.rs"));
        assert!(!contains("src", "README.md"));
    }
}
