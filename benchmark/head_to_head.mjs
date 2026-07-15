#!/usr/bin/env node
// Fresh-worktree benchmark for Cargo, cargo-worktree, and Grove; or a stable-checkout
// compiler-cache benchmark for Cargo and sccache.
// It keeps logs and report.json, then removes every build artifact it created by default.

import { createHash } from "node:crypto";
import {
  closeSync,
  existsSync,
  mkdirSync,
  openSync,
  readFileSync,
  rmSync,
  statfsSync,
  writeFileSync,
} from "node:fs";
import { delimiter, dirname, join, relative, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const GIB = 1024 ** 3;
const here = dirname(fileURLToPath(import.meta.url));
const cargo = process.env.CARGO_BIN || "cargo";
const grove = process.env.GROVE_BIN || "grove";
const repo = process.env.BENCH_REPO || "https://github.com/BurntSushi/ripgrep.git";
const revision = process.env.BENCH_REF || "14.1.1";
const track = process.env.BENCH_TRACK || "fresh-worktree";
const probeBin = process.env.BENCH_PROBE_BIN || "rg";
const probeArgs = process.env.BENCH_PROBE_ARGS?.split(" ").filter(Boolean) || [];
const runs = Number.parseInt(process.env.RUNS || "6", 10);
const keep = process.env.KEEP_ARTIFACTS === "1";
const minFree = Number(process.env.BENCH_START_FREE_GB || "30") * GIB;
const timestamp = new Date().toISOString().replace(/[:.]/g, "-");
const root = resolve(process.env.BENCH_DIR || join(here, ".work", `head-to-head-${timestamp}`));
const paths = {
  source: join(root, "source"),
  worktrees: join(root, "worktrees"),
  cargoHome: join(root, "cargo-home"),
  grove: join(root, "grove"),
  sccache: join(root, "sccache"),
  logs: join(root, "logs"),
  report: join(root, "report.json"),
};
const requested = (process.env.MODES || (track === "stable-sccache" ? "cargo,sccache" : "cargo,cargo-worktree,grove"))
  .split(",")
  .map((mode) => mode.trim())
  .filter(Boolean);
const workloads = [
  { name: "check", args: ["check", "--workspace", "--locked"] },
  {
    name: "test-binaries",
    args: ["nextest", "run", "--workspace", "--no-run", "--locked"],
  },
];
const liveWorktrees = new Set();
let cargoWorktreeEnv;
let probeFixture;
const profile = {
  CARGO_PROFILE_DEV_DEBUG: "0",
  CARGO_PROFILE_TEST_DEBUG: "0",
  CARGO_INCREMENTAL: "0",
};

if (process.platform === "darwin") {
  profile.CARGO_PROFILE_DEV_SPLIT_DEBUGINFO = "off";
  profile.CARGO_PROFILE_TEST_SPLIT_DEBUGINFO = "off";
}

function fail(message) {
  throw new Error(message);
}

function commandAvailable(program, args = ["--version"], env = process.env) {
  const result = spawnSync(program, args, { env, stdio: "ignore" });
  return !result.error && result.status === 0;
}

function capture(program, args, cwd, env) {
  const result = spawnSync(program, args, { cwd, env, encoding: "utf8" });
  if (result.error || result.status !== 0) {
    const detail = result.error?.message || result.stderr || `exit ${result.status}`;
    fail(`${program} ${args.join(" ")} failed: ${detail.trim()}`);
  }
  return result.stdout.trim();
}

function run(argv, cwd, env, stdout, stderr = stdout) {
  mkdirSync(dirname(stdout), { recursive: true });
  const out = openSync(stdout, "w");
  const err = stderr === stdout ? out : openSync(stderr, "w");
  const started = process.hrtime.bigint();
  const result = spawnSync(argv[0], argv.slice(1), {
    cwd,
    env,
    stdio: ["ignore", out, err],
  });
  const elapsed = Number(process.hrtime.bigint() - started) / 1e9;
  closeSync(out);
  if (err !== out) {
    closeSync(err);
  }
  if (result.error || result.status !== 0) {
    const detail = result.error?.message || `exit ${result.status}`;
    fail(`${argv.join(" ")} failed (${detail}); see ${stdout}`);
  }
  return elapsed;
}

function quiet(argv, cwd, env) {
  const result = spawnSync(argv[0], argv.slice(1), { cwd, env, stdio: "ignore" });
  if (result.error || result.status !== 0) {
    const detail = result.error?.message || `exit ${result.status}`;
    fail(`${argv.join(" ")} failed during cleanup (${detail})`);
  }
}

function relativePath(path) {
  return relative(root, path) || ".";
}

function compileUnits(log) {
  return (readFileSync(log, "utf8").match(/^\s*(Checking|Compiling)\s/mg) || []).length;
}

function outputHash(path) {
  let output = readFileSync(path, "utf8");
  if (probeArgs.length === 0) {
    output = output
      .trimEnd()
      .split("\n")
      .map((line) => {
        const record = JSON.parse(line);
        if (record.data?.stats) {
          delete record.data.stats.elapsed;
        }
        if (record.data) {
          delete record.data.elapsed_total;
        }
        return JSON.stringify(record);
      })
      .join("\n");
  }
  return {
    bytes: Buffer.byteLength(output),
    sha256: createHash("sha256").update(output).digest("hex"),
  };
}

function percentile(values, fraction) {
  const sorted = [...values].sort((left, right) => left - right);
  return sorted[Math.max(0, Math.ceil(sorted.length * fraction) - 1)];
}

function filesystem(path) {
  const stat = statfsSync(path);
  return {
    block_bytes: Number(stat.bsize),
    free_bytes: Number(stat.bavail) * Number(stat.bsize),
    type: Number(stat.type),
  };
}

function owned(path) {
  const prefix = `${root}${process.platform === "win32" ? "\\" : "/"}`;
  if (!path.startsWith(prefix)) {
    fail(`refusing to remove path outside benchmark root: ${path}`);
  }
  rmSync(path, { recursive: true, force: true });
}

function createWorktree(label, env) {
  const worktree = join(paths.worktrees, label);
  const log = join(paths.logs, `setup-${label}.log`);
  run(
    ["git", "-C", paths.source, "worktree", "add", "--quiet", "-b", `bench-${label}`, worktree, "HEAD"],
    paths.source,
    env,
    log,
  );
  liveWorktrees.add(worktree);
  return worktree;
}

function removeWorktree(worktree, env) {
  if (!existsSync(worktree)) {
    liveWorktrees.delete(worktree);
    return;
  }
  quiet(["git", "-C", paths.source, "worktree", "remove", "--force", worktree], paths.source, env);
  liveWorktrees.delete(worktree);
}

function commonEnv() {
  const env = {
    ...process.env,
    ...profile,
    CARGO_HOME: paths.cargoHome,
  };
  for (const key of ["RUSTC_WRAPPER", "CARGO_TARGET_DIR", "CARGO_BUILD_BUILD_DIR"]) {
    delete env[key];
  }
  for (const key of Object.keys(env)) {
    if (key.startsWith("SCCACHE_") || key.startsWith("GROVE_")) {
      delete env[key];
    }
  }
  return env;
}

function withoutSccache(base) {
  if (cargoWorktreeEnv) {
    return cargoWorktreeEnv;
  }
  if (!commandAvailable("sccache", ["--version"], base)) {
    cargoWorktreeEnv = base;
    return cargoWorktreeEnv;
  }
  const locator = process.platform === "win32" ? "where" : "which";
  const executable = capture(locator, ["sccache"], root, base).split(/\r?\n/, 1)[0];
  const sccacheDir = resolve(dirname(executable));
  const path = (base.PATH || "")
    .split(delimiter)
    .filter((entry) => entry && resolve(entry) !== sccacheDir)
    .join(delimiter);
  cargoWorktreeEnv = { ...base, PATH: path };
  if (commandAvailable("sccache", ["--version"], cargoWorktreeEnv)) {
    fail("could not hide sccache from cargo-worktree; refuse an accidental combined benchmark");
  }
  return cargoWorktreeEnv;
}

function sccacheEnv(base) {
  const env = {
    ...base,
    RUSTC_WRAPPER: "sccache",
    SCCACHE_DIR: paths.sccache,
  };
  if (process.platform === "win32") {
    env.SCCACHE_SERVER_PORT = String(43000 + (process.pid % 1000));
  } else {
    env.SCCACHE_SERVER_UDS = join(root, "sccache.sock");
  }
  return env;
}

function prepareProbeFixture() {
  probeFixture = join(root, "behavior-fixture");
  mkdirSync(join(probeFixture, "nested"), { recursive: true });
  writeFileSync(join(probeFixture, "alpha.txt"), "needle\nneedle haystack\n");
  writeFileSync(join(probeFixture, "nested", "beta.txt"), "haystack\nneedle\n");
}

function probeCommand() {
  return [
    "run",
    "--quiet",
    "--bin",
    probeBin,
    "--",
    ...(probeArgs.length > 0
      ? probeArgs
      : ["--json", "--no-ignore", "--sort", "path", "needle", probeFixture]),
  ];
}

function invocation(mode, workspace, output, args, base) {
  switch (mode) {
    case "cargo":
      return {
        argv: [cargo, ...args],
        env: {
          ...base,
          CARGO_TARGET_DIR: join(output, "target"),
          CARGO_BUILD_BUILD_DIR: join(output, "build"),
        },
      };
    case "sccache":
      return {
        argv: [cargo, ...args],
        env: sccacheEnv(base),
      };
    case "cargo-worktree":
      return {
        // Built-in commands require `--` before Cargo flags; the external `nextest`
        // subcommand forwards its own `run` verb directly.
        argv:
          args[0] === "nextest"
            ? [cargo, "worktree", ...args]
            : [cargo, "worktree", args[0], "--", ...args.slice(1)],
        env: withoutSccache(base),
      };
    case "grove":
      return {
        argv: [grove, "exec", "--tag", "head-to-head", "--", cargo, ...args],
        env: {
          ...base,
          GROVE_CACHE_ROOT: paths.grove,
          GROVE_MIN_FREE_GB: "0",
          GROVE_REQUIRE_COW: "true",
        },
      };
    default:
      fail(`unknown mode: ${mode}`);
  }
}

function prepareGrove(base) {
  const env = {
    ...base,
    GROVE_CACHE_ROOT: paths.grove,
    GROVE_MIN_FREE_GB: "0",
    GROVE_REQUIRE_COW: "true",
  };
  run([grove, "cache", "warm"], paths.source, env, join(paths.logs, "warm-grove.log"));
}

function prepareModes(modes, base) {
  if (modes.includes("grove")) {
    prepareGrove(base);
  }
}

function behaviorGate(mode, workspace, output, base, label) {
  const probe = invocation(mode, workspace, output, probeCommand(), base);
  const stdout = join(paths.logs, `${label}.probe.stdout`);
  run(probe.argv, workspace, probe.env, stdout, join(paths.logs, `${label}.probe.stderr`));
  return outputHash(stdout);
}

function stableEnv(mode, base) {
  const env = {
    ...base,
    CARGO_TARGET_DIR: join(root, "stable-target"),
  };
  return mode === "sccache" ? sccacheEnv(env) : env;
}

function cleanStable(base, label) {
  run([cargo, "clean"], paths.source, stableEnv("cargo", base), join(paths.logs, `${label}.clean.log`));
}

function stableTask(mode, args, base) {
  return { argv: [cargo, ...args], env: stableEnv(mode, base) };
}

function prepareStableSccache(base) {
  for (const workload of workloads) {
    const task = stableTask("sccache", workload.args, base);
    run(task.argv, paths.source, task.env, join(paths.logs, `prime-sccache-${workload.name}.log`));
  }
  cleanStable(base, "prime-sccache");
}

function stableSample(mode, workload, index, base) {
  const label = `${mode}-${workload.name}-${index}`;
  cleanStable(base, label);
  const task = stableTask(mode, workload.args, base);
  const buildLog = join(paths.logs, `${label}.build.log`);
  if (mode === "sccache") {
    capture("sccache", ["--zero-stats"], root, sccacheEnv(base));
  }
  const seconds = run(task.argv, paths.source, task.env, buildLog);
  let sccacheStats;
  if (mode === "sccache") {
    const stats = capture("sccache", ["--show-stats", "--stats-format", "json"], root, sccacheEnv(base));
    const statsPath = join(paths.logs, `${label}.sccache.json`);
    writeFileSync(statsPath, `${stats}\n`);
    sccacheStats = relativePath(statsPath);
  }
  const stdout = join(paths.logs, `${label}.probe.stdout`);
  run(
    [cargo, ...probeCommand()],
    paths.source,
    stableEnv("cargo", base),
    stdout,
    join(paths.logs, `${label}.probe.stderr`),
  );
  const behavior = outputHash(stdout);
  return {
    mode,
    workload: workload.name,
    sample: index,
    seconds,
    compile_units: compileUnits(buildLog),
    behavior_bytes: behavior.bytes,
    behavior_sha256: behavior.sha256,
    build_log: relativePath(buildLog),
    ...(sccacheStats ? { sccache_stats: sccacheStats } : {}),
  };
}

function sample(mode, workload, index, base) {
  const label = `${mode}-${workload.name}-${index}`;
  const worktree = createWorktree(label, base);
  const output = join(root, "outputs", label);
  try {
    const task = invocation(mode, worktree, output, workload.args, base);
    const buildLog = join(paths.logs, `${label}.build.log`);
    const seconds = run(task.argv, worktree, task.env, buildLog);
    const behavior = behaviorGate(mode, worktree, output, base, label);
    const result = {
      mode,
      workload: workload.name,
      sample: index,
      seconds,
      compile_units: compileUnits(buildLog),
      behavior_bytes: behavior.bytes,
      behavior_sha256: behavior.sha256,
      build_log: relativePath(buildLog),
    };
    if (mode === "cargo-worktree" && index === 1) {
      const inspect = capture(cargo, ["worktree", "inspect"], worktree, withoutSccache(base));
      const inspectPath = join(paths.logs, "cargo-worktree-inspect.txt");
      writeFileSync(inspectPath, `${inspect}\n`);
      result.cargo_worktree_inspect = relativePath(inspectPath);
    }
    return result;
  } finally {
    removeWorktree(worktree, base);
  }
}

function summarize(results) {
  const groups = new Map();
  for (const result of results) {
    const key = `${result.mode}:${result.workload}`;
    const values = groups.get(key) || [];
    values.push(result.seconds);
    groups.set(key, values);
  }
  return [...groups].map(([key, values]) => {
    const [mode, workload] = key.split(":");
    return {
      mode,
      workload,
      samples: values.length,
      median_seconds: percentile(values, 0.5),
      p95_seconds: percentile(values, 0.95),
    };
  });
}

function cleanArtifacts(base) {
  let worktreesRemoved = true;
  for (const worktree of liveWorktrees) {
    const result = spawnSync("git", ["-C", paths.source, "worktree", "remove", "--force", worktree], {
      cwd: paths.source,
      env: base,
      stdio: "ignore",
    });
    if (result.error || result.status !== 0) {
      worktreesRemoved = false;
    }
  }
  if (existsSync(paths.source)) {
    spawnSync("git", ["-C", paths.source, "worktree", "prune"], { cwd: paths.source, env: base, stdio: "ignore" });
  }
  if (track === "stable-sccache" && commandAvailable("sccache", ["--version"], base)) {
    spawnSync("sccache", ["--stop-server"], { cwd: root, env: sccacheEnv(base), stdio: "ignore" });
  }
  if (!keep && worktreesRemoved) {
    for (const path of [
      paths.worktrees,
      join(root, "outputs"),
      join(root, "prime"),
      join(root, "stable-target"),
      paths.grove,
      paths.sccache,
      join(root, "sccache.sock"),
      paths.cargoHome,
      paths.source,
    ]) {
      if (existsSync(path)) {
        owned(path);
      }
    }
  } else if (!keep) {
    process.stderr.write(`benchmark: preserving ${root}; an owned worktree could not be removed\n`);
  }
}

function validateInputs() {
  if (!Number.isInteger(runs) || runs < 1) {
    fail("RUNS must be a positive integer");
  }
  if (!["fresh-worktree", "stable-sccache"].includes(track)) {
    fail(`unknown BENCH_TRACK: ${track}`);
  }
  if (existsSync(root)) {
    fail(`BENCH_DIR must not already exist: ${root}`);
  }
  if (!requested.includes("cargo")) {
    fail("MODES must include cargo so behavior equivalence has a baseline");
  }
  const known = new Set(track === "stable-sccache" ? ["cargo", "sccache"] : ["cargo", "cargo-worktree", "grove"]);
  for (const mode of requested) {
    if (!known.has(mode)) {
      fail(`unknown mode in MODES: ${mode}`);
    }
  }
  const required = [
    ["git", ["--version"]],
    [cargo, ["--version"]],
    [cargo, ["nextest", "--version"]],
  ];
  if (track === "fresh-worktree") {
    required.push([grove, ["--version"]]);
  } else {
    required.push(["sccache", ["--version"]]);
  }
  for (const [program, args] of required) {
    if (!commandAvailable(program, args)) {
      fail(`required command is unavailable: ${program} ${args.join(" ")}`);
    }
  }
}

function runSamples(active, base, sampleFn) {
  const results = [];
  let baseline = null;
  for (const workload of workloads) {
    for (let index = 1; index <= runs; index += 1) {
      for (let offset = 0; offset < active.length; offset += 1) {
        const mode = active[(index - 1 + offset) % active.length];
        const result = sampleFn(mode, workload, index, base);
        if (mode === "cargo" && baseline === null) {
          baseline = result.behavior_sha256;
        }
        if (result.behavior_sha256 !== baseline) {
          fail(`${mode} ${workload.name} produced a different ${probeBin} output from cargo`);
        }
        results.push(result);
        process.stdout.write(`${mode.padEnd(15)} ${workload.name.padEnd(15)} ${result.seconds.toFixed(3)}s\n`);
      }
    }
  }
  return results;
}

function writeReport(results, active, skipped, free, base) {
  const tools = {
    cargo: capture(cargo, ["--version"], root, base),
    nextest: capture(cargo, ["nextest", "--version"], root, base),
  };
  if (track === "fresh-worktree") {
    tools.grove = capture(grove, ["--version"], root, base);
    tools.cargo_worktree = commandAvailable(cargo, ["worktree", "--help"])
      ? "available (cargo-worktree does not expose --version)"
      : null;
  } else {
    tools.sccache = capture("sccache", ["--version"], root, sccacheEnv(base));
  }
  const report = {
    schema_version: 2,
    source: { repository: repo, revision, commit: capture("git", ["-C", paths.source, "rev-parse", "HEAD"], paths.source, base) },
    harness: {
      track,
      runs,
      requested_modes: requested,
      active_modes: active,
      skipped_modes: skipped,
      cleanup_artifacts: !keep,
      profile,
      warmup:
        track === "fresh-worktree"
          ? "grove cache warm before measured samples"
          : "sccache primed in the stable checkout; cargo clean before each measured sample",
      behavior_gate:
        probeArgs.length > 0
          ? `cargo run --quiet --bin ${probeBin} -- ${probeArgs.join(" ")} output matches cargo`
          : "rg JSON search of an owned deterministic fixture matches cargo after timing fields are removed",
    },
    environment: {
      platform: process.platform,
      arch: process.arch,
      filesystem: {
        start: free,
        before_cleanup: filesystem(root),
      },
      tools,
    },
    results,
    summary: summarize(results),
  };
  writeFileSync(paths.report, `${JSON.stringify(report, null, 2)}\n`);
  process.stdout.write(`report: ${paths.report}\n`);
}

async function main() {
  validateInputs();
  mkdirSync(dirname(root), { recursive: true });
  const free = filesystem(dirname(root));
  if (free.free_bytes < minFree) {
    fail(
      `only ${(free.free_bytes / GIB).toFixed(1)} GiB free; need BENCH_START_FREE_GB=${minFree / GIB} before benchmarking`,
    );
  }
  mkdirSync(paths.logs, { recursive: true });
  mkdirSync(paths.worktrees, { recursive: true });
  mkdirSync(paths.cargoHome, { recursive: true });
  prepareProbeFixture();

  const base = commonEnv();
  const skipped = [];
  const active = [];
  for (const mode of ["cargo", ...requested.filter((mode) => mode !== "cargo")]) {
    if (mode === "cargo-worktree" && !commandAvailable(cargo, ["worktree", "--help"])) {
      skipped.push({ mode, reason: "cargo-worktree is not installed" });
    } else {
      active.push(mode);
    }
  }
  if (runs % active.length !== 0) {
    fail(`RUNS=${runs} must be a multiple of the ${active.length} active modes for balanced order`);
  }

  try {
    run(["git", "clone", "--depth", "1", "--branch", revision, repo, paths.source], root, base, join(paths.logs, "clone.log"));
    run([cargo, "fetch", "--locked"], paths.source, base, join(paths.logs, "fetch.log"));
    const sampleFn = track === "stable-sccache" ? stableSample : sample;
    if (track === "stable-sccache") {
      prepareStableSccache(base);
    } else {
      prepareModes(active, base);
    }
    writeReport(runSamples(active, base, sampleFn), active, skipped, free, base);
  } finally {
    cleanArtifacts(base);
  }
}

main().catch((error) => {
  process.stderr.write(`benchmark: ${error.message}\n`);
  process.exitCode = 1;
});
