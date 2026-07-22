//! Grove's coordination surface as an MCP server.
//!
//! Seven of the eight major agent CLIs speak MCP as clients, and it is the only
//! integration channel they all share: hooks, sandboxes, and env markers each
//! differ per vendor or do not exist. Serving claims, tasks, and worktrees as
//! MCP tools lets any harness coordinate through grove without shell knowledge,
//! which is what makes a mixed-vendor fleet against one repository workable.
//!
//! Deliberately tools-only and poll-based: MCP resource subscriptions have no
//! production precedent among dev tools, and the largest client has declined
//! to support them. Transport is the stdio framing every client implements —
//! newline-delimited JSON-RPC 2.0, one message per line.

use anyhow::{Context, Result};
use grove::{claim, config, project, status, task, verify, worktree};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// Versions this server knows; the newest is offered when the client asks for
/// something unknown, per the MCP negotiation rule.
const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

pub(crate) fn serve(workspace: &Path) -> Result<i32> {
    let workspace = workspace.to_path_buf();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.context("reading MCP stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle(&line, &workspace) {
            // One message per line; a pretty-printed response would be framed
            // as several.
            writeln!(stdout, "{response}").context("writing MCP stdout")?;
            stdout.flush().context("flushing MCP stdout")?;
        }
    }
    Ok(0)
}

/// Handle one JSON-RPC message. `None` means no response is owed (a
/// notification, or a malformed line without an id to answer to).
fn handle(line: &str, workspace: &Path) -> Option<String> {
    let message: Value = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(error) => {
            return Some(
                json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {"code": -32700, "message": format!("parse error: {error}")},
                })
                .to_string(),
            );
        }
    };
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    // Requests carry an id and are owed a response; notifications do not.
    let id = match id {
        Some(id) if !id.is_null() => id,
        _ => return None,
    };
    let params = message.get("params").cloned().unwrap_or(json!({}));
    let body = match method {
        "initialize" => Ok(initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => Ok(call(&params, workspace)),
        other => Err(format!("method not found: {other}")),
    };
    let response = match body {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(message) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": message},
        }),
    };
    Some(response.to_string())
}

fn initialize(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    let version = if PROTOCOL_VERSIONS.contains(&requested) {
        requested
    } else {
        PROTOCOL_VERSIONS[0]
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "grove",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

/// Execute a tool. Domain refusals (a claim conflict, a finish refusal) are
/// successful answers carried in the JSON, not tool errors; `isError` is
/// reserved for the tool itself failing.
fn call(params: &Value, workspace: &Path) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
    match dispatch(name, &arguments, workspace) {
        Ok(value) => json!({
            "content": [{"type": "text", "text": value.to_string()}],
            "isError": false,
        }),
        Err(error) => json!({
            "content": [{"type": "text", "text": format!("{error:#}")}],
            "isError": true,
        }),
    }
}

fn required<'a>(arguments: &'a Value, key: &str) -> Result<&'a str> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("missing required argument {key:?}"))
}

fn optional(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
}

fn strings(arguments: &Value, key: &str) -> Result<Vec<String>> {
    let list = arguments
        .get(key)
        .and_then(Value::as_array)
        .with_context(|| format!("missing required array {key:?}"))?;
    let scope: Vec<String> = list
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    if scope.is_empty() || scope.len() != list.len() {
        anyhow::bail!("{key:?} must be a non-empty array of strings");
    }
    Ok(scope)
}

fn dispatch(name: &str, arguments: &Value, workspace: &Path) -> Result<Value> {
    // Resolved per call: config governs TTLs and roots, and a server can
    // outlive an edit to it.
    let config = config::Config::resolve(workspace);
    let root = config.root();
    let repo = project::repo_identity(workspace);
    match name {
        "grove_status" => {
            let report = status::bound(&root, workspace, &config)?;
            Ok(serde_json::to_value(report)?)
        }
        "grove_claim" => {
            let request = claim::ClaimRequest {
                root: &root,
                repo: &repo,
                workspace: Some(workspace),
                agent: required(arguments, "agent")?.to_string(),
                task: optional(arguments, "task").unwrap_or_default(),
                scope: strings(arguments, "scope")?,
                branch: optional(arguments, "branch"),
                force: false,
            };
            Ok(serde_json::to_value(claim::claim(&request)?)?)
        }
        "grove_release_claims" => {
            let scope = strings(arguments, "scope").unwrap_or_default();
            let outcome = claim::release(
                &root,
                &repo,
                Some(workspace),
                required(arguments, "agent")?,
                &scope,
            )?;
            Ok(serde_json::to_value(outcome)?)
        }
        "grove_task_begin" => {
            let outcome = task::begin(task::Begin {
                root: &root,
                workspace,
                agent: required(arguments, "agent")?.to_string(),
                description: required(arguments, "task")?.to_string(),
                scope: strings(arguments, "scope")?,
                claim_group: optional(arguments, "claim_group"),
            })?;
            Ok(serde_json::to_value(outcome)?)
        }
        "grove_task_status" => {
            let report = status::task_report(
                &root,
                workspace,
                optional(arguments, "task_id").as_deref(),
                arguments
                    .get("active")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            )?;
            Ok(serde_json::to_value(report)?)
        }
        "grove_task_finish" => {
            let outcome = verify::finish_bound(
                &root,
                &repo,
                &config,
                required(arguments, "task_id")?,
                optional(arguments, "expected_source_sha256").as_deref(),
                optional(arguments, "allow_unverified").as_deref(),
                optional(arguments, "accept_policy").as_deref(),
            )?;
            Ok(serde_json::to_value(outcome)?)
        }
        "grove_worktree_acquire" => {
            let request = worktree::AcquireRequest {
                root: &root,
                cwd: workspace,
                agent: required(arguments, "agent")?.to_string(),
                branch: optional(arguments, "branch"),
                base: optional(arguments, "base").unwrap_or_else(|| "HEAD".to_string()),
            };
            let path = worktree::bind(&request, &config)?;
            Ok(json!({ "path": path }))
        }
        "grove_worktree_release" => {
            let path = PathBuf::from(required(arguments, "path")?);
            let outcome = worktree::release(&root, &path)?;
            Ok(serde_json::to_value(outcome)?)
        }
        "grove_why_rebuilt" => {
            // A cheap read-only wrapper is not available yet; the CLI command
            // acquires lanes. Direct the caller there rather than half-doing it.
            anyhow::bail!("use the `grove why-rebuilt` CLI; it needs a build lane")
        }
        other => anyhow::bail!("unknown tool {other:?}"),
    }
}

fn tool_definitions() -> Vec<Value> {
    let agent = json!({"type": "string", "description": "Stable identity for this agent or session; unrelated sessions must use different values or they will renew each other's claims."});
    let scope = json!({"type": "array", "items": {"type": "string"}, "description": "Paths or crate:<name> entries."});
    vec![
        json!({
            "name": "grove_status",
            "description": "Live claims, tasks, and worktrees for this repository — check before writing.",
            "inputSchema": {"type": "object", "properties": {}},
        }),
        json!({
            "name": "grove_claim",
            "description": "Claim paths or crates so concurrent agents avoid overlap. First wins; a conflict is reported, not an error. Re-claiming the same scope with the same agent renews it.",
            "inputSchema": {"type": "object", "required": ["agent", "scope"], "properties": {
                "agent": agent, "scope": scope,
                "task": {"type": "string", "description": "What this claim is for."},
                "branch": {"type": "string"},
            }},
        }),
        json!({
            "name": "grove_release_claims",
            "description": "Release this agent's standalone claims (all of them, or the named scope entries).",
            "inputSchema": {"type": "object", "required": ["agent"], "properties": {
                "agent": agent, "scope": scope,
            }},
        }),
        json!({
            "name": "grove_task_begin",
            "description": "Atomically claim scope and create a durable task record for work longer than a few minutes.",
            "inputSchema": {"type": "object", "required": ["agent", "task", "scope"], "properties": {
                "agent": agent, "scope": scope,
                "task": {"type": "string", "description": "What the task does."},
                "claim_group": {"type": "string", "description": "Tasks sharing a group may overlap scope (N-version attempts)."},
            }},
        }),
        json!({
            "name": "grove_task_status",
            "description": "Task ownership, heartbeat, command, verification, and conflict state.",
            "inputSchema": {"type": "object", "properties": {
                "task_id": {"type": "string"},
                "active": {"type": "boolean", "description": "Only nonterminal tasks."},
            }},
        }),
        json!({
            "name": "grove_task_finish",
            "description": "Finish a task. Requires fresh verification receipts for the repository's required profiles, or an explicit allow_unverified reason, which is recorded.",
            "inputSchema": {"type": "object", "required": ["task_id"], "properties": {
                "task_id": {"type": "string"},
                "allow_unverified": {"type": "string", "description": "Recorded reason for finishing without verification."},
                "expected_source_sha256": {"type": "string"},
                "accept_policy": {"type": "string", "description": "Accept a verification policy that changed since begin, by its current digest."},
            }},
        }),
        json!({
            "name": "grove_worktree_acquire",
            "description": "Assign a fresh, prewarmed worktree on its own branch; returns its path.",
            "inputSchema": {"type": "object", "required": ["agent"], "properties": {
                "agent": agent,
                "branch": {"type": "string"},
                "base": {"type": "string", "description": "Commit-ish to branch from (default HEAD)."},
            }},
        }),
        json!({
            "name": "grove_worktree_release",
            "description": "Release a worktree after its work landed; committed and dirty work is salvaged to its branch.",
            "inputSchema": {"type": "object", "required": ["path"], "properties": {
                "path": {"type": "string"},
            }},
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rpc(line: &Value, workspace: &Path) -> Value {
        let response = handle(&line.to_string(), workspace).expect("response owed");
        serde_json::from_str(&response).expect("valid JSON response")
    }

    #[test]
    fn initialize_negotiates_a_known_version_and_advertises_tools() {
        let dir = tempfile::tempdir().unwrap();
        let response = rpc(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {"protocolVersion": "2025-06-18"}}),
            dir.path(),
        );
        assert_eq!(response["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(response["result"]["serverInfo"]["name"], "grove");
        assert!(response["result"]["capabilities"]["tools"].is_object());

        // An unknown requested version gets our newest, per the MCP rule.
        let response = rpc(
            &json!({"jsonrpc": "2.0", "id": 2, "method": "initialize",
                    "params": {"protocolVersion": "9999-01-01"}}),
            dir.path(),
        );
        assert_eq!(response["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn notifications_get_no_response_and_malformed_lines_get_a_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            handle(
                &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
                dir.path(),
            )
            .is_none()
        );
        let error = handle("this is not json", dir.path()).unwrap();
        let error: Value = serde_json::from_str(&error).unwrap();
        assert_eq!(error["error"]["code"], -32700);
    }

    #[test]
    fn tools_list_names_every_tool_with_a_schema() {
        let dir = tempfile::tempdir().unwrap();
        let response = rpc(
            &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}),
            dir.path(),
        );
        let tools = response["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 8, "{tools:?}");
        for tool in tools {
            assert!(tool["name"].as_str().unwrap().starts_with("grove_"));
            assert!(tool["inputSchema"]["type"] == "object", "{tool}");
            assert!(!tool["description"].as_str().unwrap().is_empty());
        }
    }

    #[test]
    fn unknown_methods_and_unknown_tools_error_without_crashing() {
        let dir = tempfile::tempdir().unwrap();
        let response = rpc(
            &json!({"jsonrpc": "2.0", "id": 4, "method": "resources/list"}),
            dir.path(),
        );
        assert_eq!(response["error"]["code"], -32601);

        let response = rpc(
            &json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call",
                    "params": {"name": "grove_frobnicate", "arguments": {}}}),
            dir.path(),
        );
        assert_eq!(response["result"]["isError"], true);
        assert!(
            response["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("unknown tool")
        );
    }

    #[test]
    fn a_missing_required_argument_is_a_tool_error_not_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let response = rpc(
            &json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call",
                    "params": {"name": "grove_claim", "arguments": {"scope": ["src"]}}}),
            dir.path(),
        );
        assert_eq!(response["result"]["isError"], true);
        assert!(
            response["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("agent"),
        );
    }
}
