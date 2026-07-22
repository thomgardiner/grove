//! One MCP session over real stdio against a real repository: the mixed-vendor
//! scenario the server exists for. Two differently-identified agents claim the
//! same scope through the protocol, and the CLI observes the same durable state
//! the server wrote — coordination is shared, not per-channel.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};

const GROVE: &str = env!("CARGO_BIN_EXE_grove");

fn git(dir: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap()
            .success()
    );
}

struct Server {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
}

impl Server {
    fn start(repo: &Path, cache: &Path) -> Self {
        let mut child = Command::new(GROVE)
            .args(["mcp", "serve"])
            .current_dir(repo)
            .env("GROVE_CACHE_ROOT", cache)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        Server { child, reader }
    }

    fn request(&mut self, message: Value) -> Value {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|_| panic!("bad response: {line}"))
    }

    fn call(&mut self, id: u64, tool: &str, arguments: Value) -> Value {
        let response = self.request(json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        }));
        let result = response["result"].clone();
        assert!(
            result.get("content").is_some(),
            "tool call had no content: {response}"
        );
        result
    }

    /// The tool payload is JSON carried as text content.
    fn payload(result: &Value) -> Value {
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn two_agents_coordinate_over_mcp_and_the_cli_sees_the_same_state() {
    let base = tempfile::tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname='m'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    std::fs::write(repo.join("src/lib.rs"), "").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "mcp@example.invalid"]);
    git(&repo, &["config", "user.name", "MCP Test"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);

    let mut server = Server::start(&repo, &cache);

    let init = server.request(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18",
                   "clientInfo": {"name": "claude-code", "version": "test"}},
    }));
    assert_eq!(init["result"]["serverInfo"]["name"], "grove");

    // Agent from harness A claims src.
    let claimed = server.call(
        2,
        "grove_claim",
        json!({"agent": "claude-session", "task": "refactor", "scope": ["src"]}),
    );
    assert_eq!(claimed["isError"], false, "{claimed}");
    assert_eq!(Server::payload(&claimed)["outcome"], "granted");

    // Agent from harness B claims the same scope: a conflict, reported as a
    // domain answer rather than a tool failure, naming the holder.
    let contested = server.call(
        3,
        "grove_claim",
        json!({"agent": "codex-session", "task": "same files", "scope": ["src"]}),
    );
    assert_eq!(contested["isError"], false, "{contested}");
    let contested = Server::payload(&contested);
    assert_eq!(contested["outcome"], "conflict", "{contested}");
    assert_eq!(contested["conflicts"][0]["agent"], "claude-session");

    // The same durable state is visible to the plain CLI: no per-channel truth.
    let status = Command::new(GROVE)
        .args(["status", "--json"])
        .current_dir(&repo)
        .env("GROVE_CACHE_ROOT", &cache)
        .output()
        .unwrap();
    assert!(status.status.success());
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    let agents: Vec<&str> = status["claims"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|claim| claim["agent"].as_str())
        .collect();
    assert_eq!(agents, ["claude-session"], "{status}");

    // Release over MCP; the CLI sees it gone.
    let released = server.call(
        4,
        "grove_release_claims",
        json!({"agent": "claude-session"}),
    );
    assert_eq!(released["isError"], false, "{released}");
    let status = Command::new(GROVE)
        .args(["status", "--json"])
        .current_dir(&repo)
        .env("GROVE_CACHE_ROOT", &cache)
        .output()
        .unwrap();
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert!(
        status["claims"].as_array().unwrap().is_empty(),
        "released claim must be gone: {status}"
    );
}
