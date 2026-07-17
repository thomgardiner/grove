use super::{effective_lane_environment, environment};
use crate::api::Grove;
use crate::config::Config;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

struct Cwd(PathBuf);

impl Drop for Cwd {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.0).expect("restore test current directory");
    }
}

struct Env {
    key: &'static str,
    value: Option<OsString>,
}

impl Env {
    fn remove(key: &'static str) -> Self {
        let value = std::env::var_os(key);
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::remove_var(key) };
        Self { key, value }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            // SAFETY: nextest runs each test in its own process.
            unsafe { std::env::set_var(self.key, value) };
        }
    }
}

fn workspace(root: &Path, name: &str, keep_debuginfo: bool) -> PathBuf {
    let workspace = root.join(name);
    std::fs::create_dir_all(&workspace).expect("create policy workspace");
    std::fs::write(
        workspace.join(".grove.toml"),
        format!("keep_debuginfo = {keep_debuginfo}\n"),
    )
    .expect("write workspace policy");
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\n",
    )
    .expect("write workspace manifest");
    workspace
}

fn digest(root: &Path, workspace: &Path) -> (bool, String) {
    let config = Config::resolve(workspace);
    let lane = Grove::bind(root.to_path_buf(), workspace.to_path_buf(), config.clone())
        .lane()
        .expect("acquire policy lane");
    assert_eq!(config.debuginfo(), lane.keep_debuginfo);
    let mut values = BTreeMap::new();
    effective_lane_environment(&mut values, lane.keep_debuginfo);
    (
        lane.keep_debuginfo,
        environment(workspace, &values).expect("digest lane environment"),
    )
}

#[test]
fn environment_digest_follows_lane_policy_in_either_cwd() {
    let _env = Env::remove("GROVE_KEEP_DEBUGINFO");
    let _cwd = Cwd(std::env::current_dir().expect("read test current directory"));
    let base = tempdir().expect("create policy fixture");
    let root = base.path().join("cache");
    let lean = workspace(base.path(), "lean", false);
    let debug = workspace(base.path(), "debug", true);

    std::env::set_current_dir(&debug).expect("enter debug workspace");
    let lean_first = digest(&root, &lean);
    let debug_second = digest(&root, &debug);
    std::env::set_current_dir(&lean).expect("enter lean workspace");
    let debug_first = digest(&root, &debug);
    let lean_second = digest(&root, &lean);

    assert_eq!(lean_first, lean_second);
    assert_eq!(debug_first, debug_second);
    assert!(!lean_first.0);
    assert!(debug_first.0);
    assert_ne!(lean_first.1, debug_first.1);
}
