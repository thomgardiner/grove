use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessesToUpdate, System};

use super::{CommandRecord, CommandState, Lifecycle, Task, load, now_secs, records, write};
use crate::api::Grove;
use crate::{cache, claim, worktree};

type Key<'a> = (&'a Path, &'a str, &'a str);

fn tag(task: &Task) -> String {
    format!("task-{}", task.id)
}

fn process_start(pid: u32) -> Option<u64> {
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).map(|process| process.start_time())
}

pub(crate) fn process_live(command: &CommandRecord) -> bool {
    matches!(
        (command.pid, command.process_start),
        (Some(pid), Some(start)) if process_start(pid) == Some(start)
    )
}

pub(crate) fn lane_busy(root: &Path, task: &Task) -> bool {
    cache::tagged_busy(root, &task.workspace, &task.toolchain, &tag(task))
}

pub(crate) fn reconcile(task: &mut Task, now: u64, lane_held: bool) -> bool {
    let Some(command) = task.commands.last_mut() else {
        return false;
    };
    if !matches!(
        command.state,
        CommandState::Starting | CommandState::Running
    ) {
        return false;
    }
    if lane_held || process_live(command) || command.state == CommandState::Starting {
        return true;
    }
    command.state = CommandState::Interrupted;
    command.ended_at = Some(now);
    command.exit_code = Some(1);
    task.last_activity = now;
    false
}

pub(crate) fn reconciled(root: &Path, repo: &str) -> Result<Vec<Task>> {
    let _lock = claim::registry_lock(root, repo)?;
    let mut tasks = records(root, repo)?;
    let now = now_secs();
    for task in &mut tasks {
        if task.lifecycle != Lifecycle::Running {
            continue;
        }
        let before = task.commands.last().map(|command| command.state);
        reconcile(task, now, lane_busy(root, task));
        if before != task.commands.last().map(|command| command.state) {
            write(root, task)?;
        }
    }
    Ok(tasks)
}

/// Renew only after the caller has released the task registry lock. An unmanaged
/// human worktree is intentionally a no-op.
pub(crate) fn renew(root: &Path, task: &Task) {
    if let Err(error) = worktree::touch(root, Path::new(&task.workspace)) {
        eprintln!(
            "grove: task {} activity is durable, but its worktree lease was not renewed: {error:#}",
            task.id
        );
    }
}

fn start((root, repo, id): Key<'_>, argv: &[String]) -> Result<usize> {
    let (index, task) = {
        let _lock = claim::registry_lock(root, repo)?;
        let mut task = load(root, repo, id)?;
        if task.lifecycle != Lifecycle::Running {
            bail!("task {id} is terminal");
        }
        if reconcile(&mut task, now_secs(), false) {
            bail!("task {id} already has a live command");
        }
        let index = task.commands.len();
        task.commands.push(CommandRecord {
            argv: argv.to_vec(),
            pid: None,
            process_start: None,
            started_at: now_secs(),
            ended_at: None,
            exit_code: None,
            state: CommandState::Starting,
        });
        task.last_activity = now_secs();
        write(root, &task)?;
        (index, task)
    };
    renew(root, &task);
    Ok(index)
}

fn running((root, repo, id): Key<'_>, index: usize, pid: u32, start: u64) -> Result<()> {
    let task = {
        let _lock = claim::registry_lock(root, repo)?;
        let mut task = load(root, repo, id)?;
        let record = task
            .commands
            .get_mut(index)
            .context("task command record disappeared")?;
        record.pid = Some(pid);
        record.process_start = Some(start);
        record.state = CommandState::Running;
        task.last_activity = now_secs();
        write(root, &task)?;
        task
    };
    renew(root, &task);
    Ok(())
}

fn complete(
    (root, repo, id): Key<'_>,
    index: usize,
    code: Option<i32>,
    state: CommandState,
) -> Result<()> {
    let task = {
        let _lock = claim::registry_lock(root, repo)?;
        let mut task = load(root, repo, id)?;
        let record = task
            .commands
            .get_mut(index)
            .context("task command record disappeared")?;
        record.state = state;
        record.exit_code = code.or(Some(1));
        record.ended_at = Some(now_secs());
        task.last_activity = now_secs();
        write(root, &task)?;
        task
    };
    renew(root, &task);
    Ok(())
}

fn pulse((root, repo, id): Key<'_>, index: usize) -> Result<()> {
    let task = {
        let _lock = claim::registry_lock(root, repo)?;
        let mut task = load(root, repo, id)?;
        let live = task.commands.get(index).is_some_and(|command| {
            matches!(
                command.state,
                CommandState::Starting | CommandState::Running
            )
        });
        if !live {
            return Ok(());
        }
        task.last_activity = now_secs();
        write(root, &task)?;
        task
    };
    renew(root, &task);
    Ok(())
}

pub fn exec(root: &Path, repo: &str, id: &str, argv: &[String]) -> Result<i32> {
    cache::maintain(root, || {
        let key = (root, repo, id);
        let snapshot = load(root, repo, id)?;
        worktree::full(root, Path::new(&snapshot.workspace))?;
        let snapshot = load(root, repo, id)?;
        let grove =
            Grove::with_root_for_command(root.to_path_buf(), Path::new(&snapshot.workspace), argv);
        let lane = grove.seeded_tagged_lane(&tag(&snapshot))?;
        let index = start(key, argv)?;
        let (program, args) = argv.split_first().context("task exec requires a command")?;
        let mut command = Command::new(program);
        command.args(args).current_dir(&snapshot.workspace);
        cache::apply_env(&mut command, &lane);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                complete(key, index, None, CommandState::Interrupted)?;
                return Err(error).with_context(|| format!("spawning {program}"));
            }
        };
        // Probe outside the registry lock so process inspection never stalls other tasks.
        let mut probed = process_start(child.id());
        for _ in 0..20 {
            if probed.is_some() || child.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
            probed = process_start(child.id());
        }
        if let Some(start) = probed {
            running(key, index, child.id(), start)?;
        }
        let mut pulse_due = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child
                .try_wait()
                .with_context(|| format!("waiting for {program}"))?
            {
                break status;
            }
            std::thread::sleep(Duration::from_secs(1));
            if Instant::now() >= pulse_due {
                pulse(key, index)?;
                pulse_due += Duration::from_secs(5);
            }
        };
        let state = if status.code().is_some() {
            CommandState::Exited
        } else {
            CommandState::Interrupted
        };
        complete(key, index, status.code(), state)?;
        Ok(status.code().unwrap_or(1))
    })
}

#[cfg(test)]
mod tests {
    use super::super::{Begin, BeginOutcome, Task, abandon, begin};
    use super::{pulse, start};
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
