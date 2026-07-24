//! Stable machine-readable compatibility surface.

use serde::Serialize;

#[derive(Serialize)]
pub struct Capabilities {
    schema_version: u32,
    grove_version: &'static str,
    status: StatusSchemas,
    task: TaskCapabilities,
    coordination: CoordinationCapabilities,
    inspection: InspectionCapabilities,
}

#[derive(Serialize)]
struct CoordinationCapabilities {
    /// `grove mcp serve` exposes claims, tasks, status, and worktrees as MCP
    /// tools over stdio, so any MCP-client harness coordinates without shell
    /// access. Tools-only by design; no resources or subscriptions.
    mcp_tools: bool,
    /// `--agent` has no implicit default: identity comes from the flag or the
    /// GROVE_AGENT environment variable, so unrelated sessions conflict
    /// instead of silently renewing each other's claims.
    agent_identity_required: bool,
    /// `grove git` serializes the git writes that race concurrent worktrees on
    /// shared `.git` state; `task exec` routes a supervised command's git
    /// through it automatically (Unix).
    git_write_serialization: bool,
}

#[derive(Serialize)]
struct TaskCapabilities {
    /// Capabilities `task exec --capability` accepts. `edit` supervises without
    /// reserving a build lane or admission slot.
    exec_capabilities: &'static [&'static str],
    /// `task begin` pins the verification policy digest and `task finish`
    /// refuses on drift unless the caller passes `--accept-policy`.
    verification_policy_pinned: bool,
    /// `grove candidate capture` emits a content-addressed candidate object
    /// binding commit, tree, complete source digest, and pinned policy.
    candidate_capture: bool,
}

#[derive(Serialize)]
struct StatusSchemas {
    repository_schema: u32,
    task_status_schema: u32,
    task_record_schema: u32,
}

#[derive(Serialize)]
struct InspectionCapabilities {
    binding_schema: u32,
    execution_schema: u32,
    finish_source_cas: bool,
    process_tree: &'static str,
    filesystem: &'static str,
    output: &'static str,
}

pub fn report() -> Capabilities {
    Capabilities {
        schema_version: 1,
        grove_version: env!("CARGO_PKG_VERSION"),
        status: StatusSchemas {
            repository_schema: crate::status::SCHEMA_VERSION,
            task_status_schema: crate::status::TASK_SCHEMA_VERSION,
            task_record_schema: grove_core::task::SCHEMA_VERSION,
        },
        task: TaskCapabilities {
            exec_capabilities: &["build", "edit"],
            verification_policy_pinned: true,
            candidate_capture: true,
        },
        coordination: CoordinationCapabilities {
            mcp_tools: true,
            agent_identity_required: true,
            git_write_serialization: true,
        },
        inspection: InspectionCapabilities {
            binding_schema: crate::inspection_snapshot::SCHEMA_VERSION,
            execution_schema: crate::inspection::SCHEMA_VERSION,
            finish_source_cas: true,
            process_tree: process_tree(),
            filesystem: "read_only_permissions_and_digest",
            output: "captured_logs_json_report",
        },
    }
}

#[cfg(windows)]
fn process_tree() -> &'static str {
    "windows_job_object"
}

#[cfg(unix)]
fn process_tree() -> &'static str {
    "unix_process_group_best_effort"
}

#[cfg(not(any(unix, windows)))]
fn process_tree() -> &'static str {
    "direct_child_only"
}

#[cfg(test)]
mod tests {
    #[test]
    fn reports_actual_schema_constants() {
        let value = serde_json::to_value(super::report()).unwrap();
        assert_eq!(value["grove_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(value["status"]["task_status_schema"], 4);
        assert_eq!(
            value["status"]["task_record_schema"],
            grove_core::task::SCHEMA_VERSION
        );
    }
}
