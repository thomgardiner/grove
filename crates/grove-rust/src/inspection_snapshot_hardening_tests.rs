use super::*;
use crate::cache;

#[test]
fn lease_requires_the_authoritative_namespace_and_expected_binding() {
    let fixture = Fixture::new();
    let request = fixture.request("bound-lease");
    let capsule = acquire(&request).unwrap();

    let copied = TempDir::new().unwrap();
    let copied_root = fs::canonicalize(copied.path()).unwrap();
    let source = fs::canonicalize(&fixture.source).unwrap();
    let namespace = copied_root
        .join("inspections")
        .join(cache::repo_slug(&source.to_string_lossy()))
        .join("bound-lease");
    fs::create_dir_all(&namespace).unwrap();
    fs::copy(&capsule.lease, namespace.join("lease.json")).unwrap();
    let copied_request = Request {
        root: &copied_root,
        workspace: &fixture.source,
        task_id: request.task_id,
        capsule_id: request.capsule_id,
        expires_at: request.expires_at,
    };
    assert!(
        load(&copied_request, &capsule.binding.source_sha256).is_err(),
        "a copied lease must not authorize another state root"
    );
    assert!(
        load(&request, &"0".repeat(64)).is_err(),
        "the caller's expected source digest must be authoritative"
    );
    let mut tampered = capsule.binding.clone();
    tampered.expires_at += 1;
    fs::write(&capsule.lease, serde_json::to_vec(&tampered).unwrap()).unwrap();
    assert!(
        load(&request, &capsule.binding.source_sha256).is_err(),
        "a tampered lease must not match the authoritative expiry"
    );
    fs::write(
        &capsule.lease,
        serde_json::to_vec(&capsule.binding).unwrap(),
    )
    .unwrap();
    let wrong_task = Request {
        task_id: "another-task",
        ..request
    };
    assert!(load(&wrong_task, &capsule.binding.source_sha256).is_err());
}

#[cfg(unix)]
#[test]
fn namespace_redirect_is_rejected_without_writing_through_it() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let outside = fixture._temp.path().join("outside-state");
    fs::create_dir(&outside).unwrap();
    fs::create_dir(&fixture.root).unwrap();
    symlink(&outside, fixture.root.join("inspections")).unwrap();

    assert!(acquire(&fixture.request("redirected")).is_err());
    assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn existing_descendant_behind_intermediate_redirect_is_rejected() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let base = fs::canonicalize(fixture._temp.path())
        .unwrap()
        .join("state-base");
    let outside = fixture._temp.path().join("outside-existing");
    fs::create_dir(&base).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::create_dir(outside.join("state")).unwrap();
    symlink(&outside, base.join("link")).unwrap();
    let redirected = base.join("link/state");
    let request = Request {
        root: &redirected,
        workspace: &fixture.source,
        task_id: "redirect-ancestor",
        capsule_id: "redirect-ancestor",
        expires_at: now() + 3_600,
    };

    assert!(acquire(&request).is_err());
    assert!(!outside.join("state/inspections").exists());
}

#[cfg(unix)]
#[test]
fn redirected_capsule_directory_cannot_replay_an_exact_lease() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let request = fixture.request("redirected-capsule");
    let capsule = acquire(&request).unwrap();
    let capsule_dir = capsule.path.parent().unwrap().to_path_buf();
    let moved = fixture._temp.path().join("moved-capsule");
    fs::rename(&capsule_dir, &moved).unwrap();
    symlink(&moved, &capsule_dir).unwrap();

    assert!(load(&request, &capsule.binding.source_sha256).is_err());
}

#[test]
fn non_default_index_flags_are_rejected() {
    for (name, prepare) in [
        (
            "assume",
            &["update-index", "--assume-unchanged", "plain.txt"][..],
        ),
        (
            "skip",
            &["update-index", "--skip-worktree", "plain.txt"][..],
        ),
    ] {
        let fixture = Fixture::new();
        git(&fixture.source, prepare);
        let error = acquisition_error(&fixture.request(name));
        assert!(error.contains("index flags"), "{error}");
    }

    let intent = Fixture::new();
    write(&intent.source, "intent.txt", b"not staged yet");
    git(&intent.source, &["add", "--intent-to-add", "intent.txt"]);
    let error = acquisition_error(&intent.request("intent"));
    assert!(error.contains("index flags"), "{error}");

    let split = Fixture::new();
    git(&split.source, &["update-index", "--split-index"]);
    let error = acquisition_error(&split.request("split"));
    assert!(error.contains("split-index"), "{error}");
}

#[cfg(unix)]
#[test]
fn sparse_filter_and_omitted_symlink_are_rejected_before_filter_execution() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let fixture = Fixture::new();
    write(
        &fixture.source,
        ".gitattributes",
        b"filtered.txt filter=marker\n",
    );
    write(&fixture.source, "filtered.txt", b"filtered\n");
    symlink("plain.txt", fixture.source.join("sparse-link")).unwrap();
    git(&fixture.source, &["add", "."]);
    git(&fixture.source, &["commit", "-m", "sparse inputs"]);

    let marker = fixture._temp.path().join("filter-ran");
    let filter = fixture._temp.path().join("filter.sh");
    fs::write(
        &filter,
        format!("#!/bin/sh\nprintf ran > '{}'\ncat\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&filter, fs::Permissions::from_mode(0o755)).unwrap();
    git(
        &fixture.source,
        &["config", "filter.marker.smudge", filter.to_str().unwrap()],
    );
    git(&fixture.source, &["sparse-checkout", "init", "--no-cone"]);
    git(
        &fixture.source,
        &["sparse-checkout", "set", "--no-cone", "plain.txt"],
    );
    let _ = fs::remove_file(&marker);
    assert!(!fixture.source.join("filtered.txt").exists());
    assert!(!fixture.source.join("sparse-link").exists());

    let error = acquisition_error(&fixture.request("sparse"));
    assert!(error.contains("index flags"), "{error}");
    assert!(
        !marker.exists(),
        "inspection acquisition executed a Git filter"
    );
}

#[cfg(unix)]
#[test]
fn symlink_targets_must_be_complete_snapshot_entries() {
    use std::os::unix::fs::symlink;

    let git_state = Fixture::new();
    symlink(".git/config", git_state.source.join("git-config-link")).unwrap();
    let error = acquisition_error(&git_state.request("git-state-link"));
    assert!(error.contains("absent from the snapshot"), "{error}");

    let ignored = Fixture::new();
    write(&ignored.source, ".gitignore", b"ignored.txt\n");
    write(&ignored.source, "ignored.txt", b"omitted bytes");
    symlink("ignored.txt", ignored.source.join("ignored-link")).unwrap();
    let error = acquisition_error(&ignored.request("ignored-link"));
    assert!(error.contains("absent from the snapshot"), "{error}");

    let directory = Fixture::new();
    write(&directory.source, "tree/visible.txt", b"visible bytes");
    symlink("tree", directory.source.join("directory-link")).unwrap();
    let error = acquisition_error(&directory.request("directory-link"));
    assert!(error.contains("absent from the snapshot"), "{error}");

    let dangling = Fixture::new();
    symlink("missing.txt", dangling.source.join("dangling-link")).unwrap();
    assert!(acquire(&dangling.request("dangling-link")).is_err());
}

#[test]
fn linked_worktree_captures_its_unique_branch_head() {
    let fixture = Fixture::new();
    let linked = fixture._temp.path().join("linked-review");
    git(
        &fixture.source,
        &[
            "worktree",
            "add",
            "-b",
            "linked-review",
            linked.to_str().unwrap(),
        ],
    );
    write(&linked, "linked-only.txt", b"linked branch\n");
    git(&linked, &["add", "linked-only.txt"]);
    git(&linked, &["commit", "-m", "linked branch head"]);
    write(&linked, "plain.txt", b"linked dirty state\n");
    let head = capture(&linked, &["rev-parse", "HEAD"]);
    let request = Request {
        root: &fixture.root,
        workspace: &linked,
        task_id: "linked-task",
        capsule_id: "linked-capsule",
        expires_at: now() + 3_600,
    };

    let capsule = acquire(&request).unwrap();
    assert_eq!(capture(&capsule.path, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        fs::read(capsule.path.join("linked-only.txt")).unwrap(),
        b"linked branch\n"
    );
    assert_eq!(
        fs::read(capsule.path.join("plain.txt")).unwrap(),
        b"linked dirty state\n"
    );
}

#[cfg(unix)]
#[test]
fn clone_ignores_global_include_and_template_hooks() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let template = fixture._temp.path().join("malicious-template");
    let hooks = template.join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    let marker = fixture._temp.path().join("hook-ran");
    fs::write(
        hooks.join("post-checkout"),
        format!("#!/bin/sh\nprintf ran > '{}'\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(
        hooks.join("post-checkout"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let included = fixture._temp.path().join("included-config");
    fs::write(
        &included,
        format!("[init]\n\ttemplateDir = {}\n", template.display()),
    )
    .unwrap();
    let global = fixture._temp.path().join("global-config");
    fs::write(
        &global,
        format!("[include]\n\tpath = {}\n", included.display()),
    )
    .unwrap();

    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "inspection_snapshot::tests::hardening::malicious_config_child",
        ])
        .env("GIT_CONFIG_GLOBAL", &global)
        .env("GROVE_INSPECTION_HOOK_MARKER", &marker)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(!marker.exists());
}

#[cfg(unix)]
#[test]
#[ignore = "spawned with a malicious inherited Git configuration"]
fn malicious_config_child() {
    let marker = std::env::var_os("GROVE_INSPECTION_HOOK_MARKER").unwrap();
    let fixture = Fixture::new();
    let capsule = acquire(&fixture.request("isolated-config")).unwrap();
    assert!(!Path::new(&marker).exists());
    let git = canonical_git(&capsule.path);
    assert!(!git.join("hooks").exists());
    let config = fs::read_to_string(git.join("config")).unwrap();
    assert!(!config.to_ascii_lowercase().contains("include"));
}

fn acquisition_error(request: &Request<'_>) -> String {
    acquire(request)
        .err()
        .expect("inspection acquisition unexpectedly succeeded")
        .to_string()
}
