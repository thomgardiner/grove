use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "grove",
    version,
    about = "A build cache and worktree manager for Rust repos worked on by many agents at once."
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) cmd: Cmd,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
    /// Type-check the affected packages (routed from the git diff, or an explicit -p).
    Check {
        #[arg(short, long)]
        package: Option<String>,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        files: Option<String>,
    },
    /// Test the affected packages (routed), or one package's target with -p.
    Test {
        #[arg(short, long)]
        package: Option<String>,
        #[arg(long)]
        lib: bool,
        #[arg(long)]
        test: Option<String>,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        files: Option<String>,
    },
    /// Manage the shared cache.
    Cache {
        #[command(subcommand)]
        action: CacheCmd,
    },
    /// Prewarm every worktree lane, then watch for and prewarm new worktrees.
    Watch,
    /// Manage the pool of agent worktrees.
    Worktree {
        #[command(subcommand)]
        action: WorktreeCmd,
    },
    /// Claim paths or `crate:<name>` so a swarm avoids overlap.
    Claim {
        #[arg(long, default_value = "agent")]
        agent: String,
        #[arg(long, default_value = "")]
        task: String,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        force: bool,
        #[arg(required = true)]
        scope: Vec<String>,
    },
    /// Release claims or create a frozen release bundle.
    Release {
        #[command(subcommand)]
        action: ReleaseCmd,
    },
    /// Show what every agent is currently working on.
    Status {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        watch: bool,
    },
    /// Manage a durable agent task and its claimed scope.
    Task {
        #[command(subcommand)]
        action: TaskCmd,
    },
    /// Run a repository-declared verification profile and append command receipts.
    Verify {
        #[command(subcommand)]
        action: Option<VerifyCmd>,
        /// Name under [verification.profiles] in .grove.toml (legacy run form).
        profile: Option<String>,
        #[arg(long)]
        task_id: Option<String>,
    },
    /// Show the dependency-ordered work plan without launching agents.
    Plan {
        #[arg(long, default_value = "HEAD")]
        base: String,
        #[arg(long)]
        json: bool,
        /// Emit the workspace package topology (packages, dependency edges,
        /// claim scopes) for decomposition, instead of the diff plan.
        #[arg(long)]
        topology: bool,
        /// Read proposed scope sets as JSON [{"id": ..., "scope": [...]}] on
        /// stdin; emit conflicts, couplings, and suggested execution waves.
        #[arg(long)]
        partition: bool,
    },
    /// Export an output from a tagged lane without exposing cache internals.
    Artifact {
        #[command(subcommand)]
        action: ArtifactCmd,
    },
    /// Run a command inside a seeded lane.
    Exec {
        #[arg(long, default_value = "")]
        tag: String,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Show the resolved configuration and where the config file lives.
    Config,
    /// Report repository-local build acceleration opportunities without changing policy.
    Doctor,
    /// Write AGENTS.md and a .grove.toml starter.
    Init,
    /// Report versioned machine capabilities.
    Capabilities,
    /// Manage immutable, leased inspection capsules.
    Inspect {
        #[command(subcommand)]
        action: InspectCmd,
    },
}

#[derive(Subcommand)]
pub(crate) enum InspectCmd {
    /// Capture the current task workspace into a standalone private repository.
    Acquire {
        #[arg(long)]
        task_id: String,
        #[arg(long, default_value_t = 3_600)]
        ttl_secs: u64,
    },
    /// Run a command against the captured state and emit one JSON report.
    Exec {
        capsule_id: String,
        #[arg(long)]
        timeout_secs: Option<u64>,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Remove a terminal capsule after validating its authority record.
    Release { capsule_id: String },
    /// Reclaim expired, inactive capsules.
    Reap {
        #[arg(long)]
        dry_run: bool,
    },
    /// Internal blocked worker used to close the Windows Job Object spawn race.
    #[command(name = "__worker", hide = true)]
    Worker {
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum TaskCmd {
    /// Atomically claim scope and create a durable task record.
    Begin {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        task: String,
        #[arg(long, required = true, num_args = 1..)]
        scope: Vec<String>,
        /// Tasks sharing a claim group may overlap each other's scope without
        /// conflicting (N-version attempts at one piece of work; only one
        /// result lands). Outsiders still conflict with every member.
        #[arg(long)]
        claim_group: Option<String>,
    },
    /// Run a command in the task's isolated tagged lane.
    Exec {
        #[arg(long)]
        task_id: String,
        /// Kill the command's process group after this many seconds (exit 124,
        /// as timeout(1); the task record's command state distinguishes a
        /// supervisor kill from a child that exited 124 itself). The deadline
        /// survives the caller: grove enforces it even if the process that
        /// launched `task exec` is gone.
        #[arg(long)]
        timeout_secs: Option<u64>,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Mark a task finished and release its claim.
    Finish {
        #[arg(long)]
        task_id: String,
        /// Refuse to finish unless the current workspace exactly matches the
        /// source digest captured by `grove inspect acquire`.
        #[arg(long, value_name = "SHA256")]
        expected_source_sha256: Option<String>,
        #[arg(long, value_name = "REASON")]
        allow_unverified: Option<String>,
    },
    /// Show task ownership, heartbeat, command, verification, and conflict state.
    Status {
        task_id: Option<String>,
        #[arg(long)]
        active: bool,
        #[arg(long)]
        json: bool,
    },
    /// Preserve a task and its work while releasing its claim.
    Abandon {
        #[arg(long)]
        task_id: String,
        #[arg(long)]
        reason: String,
    },
    /// Recover stale tasks, salvaging leased worktrees before releasing claims.
    Reap {
        #[arg(long)]
        ttl: Option<u64>,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum VerifyCmd {
    /// Query portable successful evidence from another exact clean checkout.
    Query { profile: String },
}

#[derive(Subcommand)]
pub(crate) enum WorktreeCmd {
    /// Assign a fresh, prewarmed worktree on its own branch; prints its path.
    Acquire {
        #[arg(long, default_value = "agent")]
        agent: String,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long, default_value = "HEAD")]
        base: String,
        /// Materialize only this package (`crate:name`) or repository-relative path.
        #[arg(long)]
        materialize: Vec<String>,
    },
    /// Monotonically add package or path scopes to a managed sparse checkout.
    Expand {
        path: String,
        #[arg(required = true)]
        scope: Vec<String>,
    },
    /// Convert a managed sparse checkout to a normal full checkout.
    Full { path: String },
    /// Salvage and return a managed worktree.
    Release { path: String },
    /// List every managed worktree and its lease state.
    List,
    /// Renew one Grove-managed worktree lease.
    Heartbeat { path: String },
    /// Reclaim abandoned worktrees.
    Reap {
        #[arg(long)]
        ttl: Option<u64>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Collapse a worktree branch's commits since its base into one commit.
    Squash {
        path: String,
        #[arg(long)]
        base: Option<String>,
        #[arg(short, long)]
        message: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum CacheCmd {
    /// Build the workspace and promote it to the canonical.
    Warm,
    /// Promote the current lane to the canonical.
    Promote,
    /// Show free space and lanes; optionally scan logical sizes.
    Status {
        #[arg(long)]
        details: bool,
    },
    /// Explain this workspace's cache identity and canonical reuse eligibility.
    Explain,
    /// Probe whether this cache root supports strict copy-on-write cloning.
    Cow,
    /// Reclaim orphaned lanes and evict LRU lanes to clear the watermark.
    Gc,
}

#[derive(Subcommand)]
pub(crate) enum ArtifactCmd {
    /// Copy a lane file atomically and print its SHA-256 content hash.
    Export {
        #[arg(long)]
        tag: String,
        source: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        task_id: Option<String>,
        #[arg(long, value_name = "REASON")]
        allow_unverified: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum ReleaseCmd {
    /// Release this agent's claims.
    Claims {
        #[arg(long, default_value = "agent")]
        agent: String,
        scope: Vec<String>,
    },
    /// Verify, freeze, and publish one release bundle.
    Freeze {
        #[arg(long)]
        task_id: String,
        #[arg(long)]
        profile: String,
        #[arg(long = "artifact", required = true, num_args = 1..)]
        artifacts: Vec<String>,
        #[arg(long)]
        out: String,
    },
}
