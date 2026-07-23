use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "grove",
    version,
    about = "Verified local execution for parallel Rust agents: CoW lanes, claims, worktrees, receipts.",
    after_help = "First run:  grove setup\n\
                  Project:    grove setup --repo && grove cache warm\n\
                  Invoke:     Claude /grove · Codex skill · shell: grove check|test|claim\n\
                  Fleets:     optional Summoner host plugin (does not require Grove for git fleets)"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) cmd: Cmd,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
    /// Type-check the affected packages (routed from the git diff, or an explicit -p).
    Check {
        /// Check only this package, skipping diff-based routing.
        #[arg(short, long)]
        package: Option<String>,
        /// Route from the files changed since this git ref (default: HEAD plus the working tree).
        #[arg(long)]
        base: Option<String>,
        /// Route from this comma-separated file list instead of the git diff.
        #[arg(long)]
        files: Option<String>,
    },
    /// Test the affected packages (routed), or one package's target with -p.
    Test {
        /// Test only this package, skipping diff-based routing.
        #[arg(short, long)]
        package: Option<String>,
        /// Restrict to the package's library unit tests.
        #[arg(long)]
        lib: bool,
        /// Run only this integration test target.
        #[arg(long)]
        test: Option<String>,
        /// Route from the files changed since this git ref (default: HEAD plus the working tree).
        #[arg(long)]
        base: Option<String>,
        /// Route from this comma-separated file list instead of the git diff.
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
        /// Stable identity for this agent or session. No default: a shared
        /// implicit name would make unrelated sessions renew (and silently
        /// take over) each other's claims instead of conflicting. Set once
        /// per session via GROVE_AGENT if passing it each time is a burden.
        #[arg(long, env = "GROVE_AGENT")]
        agent: String,
        /// Human-readable label recorded with the claim.
        #[arg(long, default_value = "")]
        task: String,
        /// Associate the claim with this branch.
        #[arg(long)]
        branch: Option<String>,
        /// Take the scope even when it conflicts with an existing claim.
        #[arg(long)]
        force: bool,
        /// Paths or `crate:<name>` selectors to claim.
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
        /// Emit JSON instead of the human-readable board.
        #[arg(long)]
        json: bool,
        /// Stream updates continuously instead of printing once.
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
        /// Attribute the receipts to this task and verify its checkout.
        #[arg(long)]
        task_id: Option<String>,
    },
    /// Show the dependency-ordered work plan without launching agents.
    Plan {
        /// Diff against this git ref to choose the affected packages.
        #[arg(long, default_value = "HEAD")]
        base: String,
        /// Emit the plan as JSON.
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
        /// Lane tag; the same tag serializes, independent tags run concurrently.
        #[arg(long, default_value = "")]
        tag: String,
        /// The command and its arguments to run, after `--`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Run git, serializing writes that would race concurrent worktrees on
    /// shared `.git` state (config, tags, refs); reads and per-worktree writes
    /// run free. Use in place of bare `git` in a shared checkout.
    Git {
        /// The git subcommand and its arguments to run, after `--`.
        #[arg(last = true, required = true)]
        args: Vec<String>,
    },
    /// Explain what the next build would rebuild, and why Cargo considers it stale.
    WhyRebuilt {
        /// Explain a single package instead of the whole workspace.
        #[arg(long, short = 'p')]
        package: Option<String>,
        /// Answer for a brand-new worktree instead of this one: seed a throwaway
        /// lane from the canonical and report what it still rebuilds. A healthy
        /// cache rebuilds almost nothing; a large count means seeding is not
        /// delivering, which is otherwise visible only as being slow.
        #[arg(long)]
        fresh: bool,
        /// Emit the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show the resolved configuration and where the config file lives.
    Config,
    /// Report repository-local build acceleration opportunities without changing policy.
    Doctor,
    /// First-run ergonomics: install harness skills (/grove), optional repo init.
    Setup {
        /// Overwrite managed skill files that drifted.
        #[arg(long)]
        refresh: bool,
        /// Also write AGENTS.md + .grove.toml in the current directory.
        #[arg(long)]
        repo: bool,
    },
    /// Write AGENTS.md and a .grove.toml starter.
    Init,
    /// Report versioned machine capabilities.
    Capabilities,
    /// Serve grove's coordination surface over the Model Context Protocol.
    Mcp {
        #[command(subcommand)]
        action: McpCmd,
    },
    /// Manage immutable, leased inspection capsules.
    Inspect {
        #[command(subcommand)]
        action: InspectCmd,
    },
}

#[derive(Subcommand)]
pub(crate) enum McpCmd {
    /// Speak MCP over stdio: claims, tasks, status, and worktrees as tools, so
    /// any MCP-client harness coordinates through grove without shell access.
    Serve,
}

#[derive(Subcommand)]
pub(crate) enum InspectCmd {
    /// Capture the current task workspace into a standalone private repository.
    Acquire {
        /// Task whose workspace to capture.
        #[arg(long)]
        task_id: String,
        /// Lease lifetime in seconds before the capsule may be reaped.
        #[arg(long, default_value_t = 3_600)]
        ttl_secs: u64,
    },
    /// Run a command against the captured state and emit one JSON report.
    Exec {
        /// Capsule to run the command against.
        capsule_id: String,
        /// Kill the command after this many seconds.
        #[arg(long)]
        timeout_secs: Option<u64>,
        /// The command and its arguments to run, after `--`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Remove a terminal capsule after validating its authority record.
    Release {
        /// Capsule to release.
        capsule_id: String,
    },
    /// Reclaim expired, inactive capsules.
    Reap {
        /// Report what would be reaped without removing anything.
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
        /// Stable identity for this agent or session.
        #[arg(long)]
        agent: String,
        /// Human-readable label for the task.
        #[arg(long)]
        task: String,
        /// Paths or `crate:<name>` selectors the task owns.
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
        /// Task whose lane and scope this command runs under.
        #[arg(long)]
        task_id: String,
        /// What the command may do. `build` reserves the task's seeded lane and
        /// routes cargo into it for the command's lifetime. `edit` supervises
        /// without reserving a lane or builder slot; grove builds the command
        /// runs acquire lanes on demand — use it for agent sessions.
        #[arg(long, value_enum, default_value_t = ExecCapabilityArg::Build)]
        capability: ExecCapabilityArg,
        /// Kill the command's process group after this many seconds (exit 124,
        /// as timeout(1); the task record's command state distinguishes a
        /// supervisor kill from a child that exited 124 itself). The deadline
        /// survives the caller: grove enforces it even if the process that
        /// launched `task exec` is gone.
        #[arg(long)]
        timeout_secs: Option<u64>,
        /// The command and its arguments to run, after `--`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Mark a task finished and release its claim.
    Finish {
        /// Task to finish.
        #[arg(long)]
        task_id: String,
        /// Refuse to finish unless the current workspace exactly matches the
        /// source digest captured by `grove inspect acquire`.
        #[arg(long, value_name = "SHA256")]
        expected_source_sha256: Option<String>,
        /// Finish without the required receipts, recording this reason.
        #[arg(long, value_name = "REASON")]
        allow_unverified: Option<String>,
        /// Accept a verification policy that changed since `task begin` by
        /// naming the current policy digest (from the policy_changed refusal).
        #[arg(long, value_name = "SHA256")]
        accept_policy: Option<String>,
    },
    /// Show task ownership, heartbeat, command, verification, and conflict state.
    Status {
        /// Show only this task (default: every task in the repository).
        task_id: Option<String>,
        /// Show only running or recovering tasks.
        #[arg(long)]
        active: bool,
        /// Emit JSON instead of the human-readable view.
        #[arg(long)]
        json: bool,
    },
    /// Preserve a task and its work while releasing its claim.
    Abandon {
        /// Task to abandon.
        #[arg(long)]
        task_id: String,
        /// Why the task is being abandoned; recorded on the task.
        #[arg(long)]
        reason: String,
    },
    /// Recover stale tasks, salvaging leased worktrees before releasing claims.
    Reap {
        /// Override the idle seconds before a task is considered stale.
        #[arg(long)]
        ttl: Option<u64>,
        /// Report what would be recovered without releasing anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum ExecCapabilityArg {
    Build,
    Edit,
}

impl From<ExecCapabilityArg> for grove::task::ExecCapability {
    fn from(arg: ExecCapabilityArg) -> Self {
        match arg {
            ExecCapabilityArg::Build => Self::Build,
            ExecCapabilityArg::Edit => Self::Edit,
        }
    }
}

#[derive(Subcommand)]
pub(crate) enum VerifyCmd {
    /// Query portable successful evidence from another exact clean checkout.
    Query {
        /// Profile under [verification.profiles] in .grove.toml to query.
        profile: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum WorktreeCmd {
    /// Assign a fresh, prewarmed worktree on its own branch; prints its path.
    Acquire {
        /// Stable identity for this agent or session (or GROVE_AGENT).
        #[arg(long, env = "GROVE_AGENT")]
        agent: String,
        /// Create the worktree on this branch instead of a generated name.
        #[arg(long)]
        branch: Option<String>,
        /// Base the worktree on this git ref.
        #[arg(long, default_value = "HEAD")]
        base: String,
        /// Materialize only this package (`crate:name`) or repository-relative path.
        #[arg(long)]
        materialize: Vec<String>,
    },
    /// Monotonically add package or path scopes to a managed sparse checkout.
    Expand {
        /// The managed worktree to expand.
        path: String,
        /// Package (`crate:name`) or path scopes to add to the sparse checkout.
        #[arg(required = true)]
        scope: Vec<String>,
    },
    /// Convert a managed sparse checkout to a normal full checkout.
    Full {
        /// The managed worktree to convert to a full checkout.
        path: String,
    },
    /// Salvage and return a managed worktree.
    Release {
        /// The managed worktree to salvage and return.
        path: String,
    },
    /// List managed worktrees and their lease state (this repository by default).
    List {
        /// List worktrees in every repository, not just the current one.
        #[arg(long)]
        all: bool,
        /// Emit JSON instead of the human-readable table.
        #[arg(long)]
        json: bool,
    },
    /// Renew one Grove-managed worktree lease.
    Heartbeat {
        /// The managed worktree whose lease to renew.
        path: String,
    },
    /// Reclaim abandoned worktrees.
    Reap {
        /// Override the idle seconds before a worktree is reaped.
        #[arg(long)]
        ttl: Option<u64>,
        /// Report what would be reaped without removing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Collapse a worktree branch's commits since its base into one commit.
    Squash {
        /// The managed worktree whose branch to squash.
        path: String,
        /// Squash the commits made since this base ref (default: the worktree's base).
        #[arg(long)]
        base: Option<String>,
        /// Commit message for the squashed commit.
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
        /// Also scan and report logical lane sizes (slower).
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
        /// Lane tag to export the file from.
        #[arg(long)]
        tag: String,
        /// Lane-relative path of the file to export.
        source: String,
        /// Destination path for the exported file.
        #[arg(long)]
        to: String,
        /// Attribute the export to this task.
        #[arg(long)]
        task_id: Option<String>,
        /// Export without the required receipts, recording this reason.
        #[arg(long, value_name = "REASON")]
        allow_unverified: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum ReleaseCmd {
    /// Release this agent's claims.
    Claims {
        /// Stable identity whose claims to release (or GROVE_AGENT).
        #[arg(long, env = "GROVE_AGENT")]
        agent: String,
        /// Limit the release to claims covering these scopes (default: all).
        scope: Vec<String>,
    },
    /// Verify, freeze, and publish one release bundle.
    Freeze {
        /// Task whose verified state to freeze.
        #[arg(long)]
        task_id: String,
        /// Verification profile that must pass before freezing.
        #[arg(long)]
        profile: String,
        /// Lane files to include in the bundle.
        #[arg(long = "artifact", required = true, num_args = 1..)]
        artifacts: Vec<String>,
        /// Destination path for the release bundle.
        #[arg(long)]
        out: String,
    },
}
