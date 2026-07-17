mod tests {
    use crate::task::task_activity::{pulse, start};
    use crate::task::{Begin, BeginOutcome, Task, abandon, begin};
    use crate::{cache, project, worktree};
    use serde_json::Value;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use tempfile::tempdir;

    fn git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn repo(path: &Path) {
        fs::create_dir_all(path.join("src")).unwrap();
        fs::write(
            path.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(path.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
        git(path, &["init", "-q"]);
        git(path, &["config", "user.email", "grove@example.invalid"]);
        git(path, &["config", "user.name", "Grove Test"]);
        git(path, &["add", "."]);
        git(path, &["commit", "-qm", "fixture"]);
    }

    fn lease_path(root: &Path, workspace: &Path) -> PathBuf {
        root.join("leases").join(format!(
            "{}.json",
            cache::lane_id(&workspace.to_string_lossy(), &project::toolchain(workspace))
        ))
    }

    fn write_lease(root: &Path, workspace: &Path) -> PathBuf {
        let path = lease_path(root, workspace);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let lease = worktree::Lease {
            workspace: workspace.to_string_lossy().into_owned(),
            branch: "main".into(),
            agent: "agent".into(),
            toolchain: project::toolchain(workspace),
            repo: project::repo_identity(workspace),
            created_at: 1,
            generation: "fixture".into(),
            last_activity: 1,
            base_oid: "base".into(),
            materialization: None,
        };
        cache::write_atomic(&path, &serde_json::to_vec_pretty(&lease).unwrap()).unwrap();
        path
    }

    fn begin_task(root: &Path, workspace: &Path) -> Task {
        let BeginOutcome::Begun { task } = begin(Begin {
            root,
            workspace,
            agent: "agent".into(),
            description: "fixture".into(),
            scope: vec!["src".into()],
            claim_group: None,
        })
        .unwrap() else {
            panic!("fixture task conflicted")
        };
        *task
    }

    fn reset_activity(path: &Path) {
        let mut lease: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        lease["last_activity"] = 1.into();
        cache::write_atomic(path, &serde_json::to_vec_pretty(&lease).unwrap()).unwrap();
    }

    fn assert_renewed(path: &Path) {
        let lease: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert!(lease["last_activity"].as_u64().unwrap() > 1);
    }

    #[test]
    fn begin_renews_matching_lease() {
        let base = tempdir().unwrap();
        let workspace = base.path().join("repo");
        let root = base.path().join("cache");
        repo(&workspace);
        let workspace = fs::canonicalize(workspace).unwrap();
        let lease = write_lease(&root, &workspace);

        begin_task(&root, &workspace);

        assert_renewed(&lease);
    }

    #[test]
    fn command_pulse_renews_after_publishing_task_activity() {
        let base = tempdir().unwrap();
        let workspace = base.path().join("repo");
        let root = base.path().join("cache");
        repo(&workspace);
        let workspace = fs::canonicalize(workspace).unwrap();
        let lease = write_lease(&root, &workspace);
        let task = begin_task(&root, &workspace);
        let key = (&*root, &*task.repo, &*task.id);
        let index = start(key, &["true".into()]).unwrap();
        reset_activity(&lease);

        pulse(key, index).unwrap();

        assert_renewed(&lease);
    }

    #[test]
    fn abandon_renews_after_the_terminal_record_is_durable() {
        let base = tempdir().unwrap();
        let workspace = base.path().join("repo");
        let root = base.path().join("cache");
        repo(&workspace);
        let workspace = fs::canonicalize(workspace).unwrap();
        let lease = write_lease(&root, &workspace);
        let task = begin_task(&root, &workspace);
        reset_activity(&lease);

        abandon(&root, &task.repo, &task.id, "done".into()).unwrap();

        assert_renewed(&lease);
    }

    #[test]
    fn finish_renews_after_the_verified_terminal_record_is_durable() {
        let base = tempdir().unwrap();
        let workspace = base.path().join("repo");
        let root = base.path().join("cache");
        repo(&workspace);
        let workspace = fs::canonicalize(workspace).unwrap();
        let lease = write_lease(&root, &workspace);
        let task = begin_task(&root, &workspace);
        reset_activity(&lease);

        crate::verify::finish(
            &root,
            &task.repo,
            &crate::config::Config::resolve(&workspace),
            &task.id,
            Some("unit-test fixture has no verification profile"),
        )
        .unwrap();

        assert_renewed(&lease);
    }
}
