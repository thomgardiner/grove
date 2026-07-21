#!/usr/bin/env node
// Evidence-first comparison of Cargo configurations and Grove across fresh worktrees.
// It deliberately measures Cargo's safe shared-target serialization; it does not call it unsafe.

import { createHash } from "node:crypto";
import { cpus, hostname, release, totalmem } from "node:os";
import {
  closeSync,
  existsSync,
  mkdirSync,
  openSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  statfsSync,
  writeFileSync,
} from "node:fs";
import { delimiter, dirname, join, relative, resolve } from "node:path";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { createMediumFixture, mediumFixture } from "./fixture.mjs";

const GIB = 1024 ** 3;
const here = dirname(fileURLToPath(import.meta.url));
const cargo = process.env.CARGO_BIN || "cargo";
const grove = process.env.GROVE_BIN || "grove";
const fixture = process.env.BENCH_FIXTURE || null;
if (fixture && fixture !== mediumFixture.name) throw new Error(`unknown BENCH_FIXTURE: ${fixture}`);
const repo = fixture ? `synthetic:${fixture}` : (process.env.BENCH_REPO || "https://github.com/BurntSushi/ripgrep.git");
const revision = fixture ? mediumFixture.version : (process.env.BENCH_REF || "14.1.1");
const probeBin = process.env.BENCH_PROBE_BIN || (fixture ? mediumFixture.binary : "rg");
const probeArgs = process.env.BENCH_PROBE_ARGS?.split(" ").filter(Boolean) || [];
const runs = integerEnv("RUNS", 6);
const concurrency = integerEnv("CONCURRENCY", 2);
const keep = process.env.KEEP_ARTIFACTS === "1";
const dryRun = process.argv.includes("--dry-run");
const help = process.argv.includes("--help") || process.argv.includes("-h");
const minFree = Number(process.env.BENCH_START_FREE_GB || "30") * GIB;
const timestamp = new Date().toISOString().replace(/[:.]/g, "-");
const root = resolve(process.env.BENCH_DIR || join(here, ".work", `head-to-head-${timestamp}`));
const requested = (process.env.MODES || "cargo-default,cargo-isolated,cargo-shared,cargo-worktree,grove,sccache")
  .split(",")
  .map((mode) => mode.trim())
  .filter(Boolean);
const phases = (process.env.PHASES || "cold,seeded,warm,amortized")
  .split(",")
  .map((phase) => phase.trim())
  .filter(Boolean);
const workloads = [
  { name: "check", args: ["check", "--workspace", "--locked"] },
  { name: "test-binaries", args: ["nextest", "run", "--workspace", "--no-run", "--locked"] },
];
const paths = {
  source: join(root, "source"), worktrees: join(root, "worktrees"), cargoHome: join(root, "cargo-home"),
  grove: join(root, "grove"), sccache: join(root, "sccache"), logs: join(root, "logs"),
  targets: join(root, "targets"), report: join(root, "report.json"),
};
const liveWorktrees = new Set();
let probeFixture;
let seededCanonicalWarmed = false;

const profile = {
  CARGO_PROFILE_DEV_DEBUG: "0", CARGO_PROFILE_TEST_DEBUG: "0", CARGO_INCREMENTAL: "0",
  ...(process.platform === "darwin" ? { CARGO_PROFILE_DEV_SPLIT_DEBUGINFO: "off", CARGO_PROFILE_TEST_SPLIT_DEBUGINFO: "off" } : {}),
};

function integerEnv(name, fallback) {
  const value = Number.parseInt(process.env[name] || String(fallback), 10);
  if (!Number.isInteger(value) || value < 1) throw new Error(`${name} must be a positive integer`);
  return value;
}
function fail(message) { throw new Error(message); }
function rel(path) { return relative(root, path) || "."; }
function mkdir(path) { mkdirSync(path, { recursive: true }); }
function commandAvailable(program, args = ["--version"], env = process.env) {
  const result = spawnSync(program, args, { env, stdio: "ignore" });
  return !result.error && result.status === 0;
}
function capture(program, args, cwd, env) {
  const result = spawnSync(program, args, { cwd, env, encoding: "utf8" });
  if (result.error || result.status !== 0) fail(`${program} ${args.join(" ")} failed: ${(result.error?.message || result.stderr || `exit ${result.status}`).trim()}`);
  return result.stdout.trim();
}
function run(argv, cwd, env, log, stderr = log) {
  mkdir(dirname(log));
  const output = openSync(log, "w");
  const errors = stderr === log ? output : openSync(stderr, "w");
  const started = process.hrtime.bigint();
  const result = spawnSync(argv[0], argv.slice(1), { cwd, env, stdio: ["ignore", output, errors] });
  closeSync(output);
  if (errors !== output) closeSync(errors);
  const seconds = Number(process.hrtime.bigint() - started) / 1e9;
  if (result.error || result.status !== 0) fail(`${argv.join(" ")} failed (${result.error?.message || `exit ${result.status}`}); see ${log}`);
  return { seconds, exit_code: 0, argv };
}
function runParallel(tasks) {
  return Promise.all(tasks.map(({ argv, cwd, env, log }) => new Promise((resolveTask, rejectTask) => {
    mkdir(dirname(log));
    const output = openSync(log, "w");
    const started = process.hrtime.bigint();
    const child = spawn(argv[0], argv.slice(1), { cwd, env, stdio: ["ignore", output, output] });
    child.on("error", (error) => { closeSync(output); rejectTask(error); });
    child.on("close", (status) => {
      closeSync(output);
      const seconds = Number(process.hrtime.bigint() - started) / 1e9;
      if (status !== 0) rejectTask(new Error(`${argv.join(" ")} failed (exit ${status}); see ${log}`));
      else resolveTask({ seconds, exit_code: status, argv });
    });
  })));
}
function filesystem(path) {
  const stat = statfsSync(path);
  return { block_bytes: Number(stat.bsize), free_bytes: Number(stat.bavail) * Number(stat.bsize), type: Number(stat.type) };
}
function allocation(path) {
  if (!existsSync(path)) return { exists: false, logical_bytes: 0, physical_bytes: null, physical_available: false };
  let logical = 0; let blocks = 0; let physical = true;
  const walk = (entry) => {
    const stat = statSync(entry);
    logical += stat.size;
    if (typeof stat.blocks === "number") blocks += stat.blocks; else physical = false;
    if (stat.isDirectory()) for (const child of readdirSync(entry)) walk(join(entry, child));
  };
  walk(path);
  return { exists: true, logical_bytes: logical, physical_bytes: physical ? blocks * 512 : null, physical_available: physical };
}
function owned(path) {
  const prefix = `${root}${process.platform === "win32" ? "\\" : "/"}`;
  if (!path.startsWith(prefix)) fail(`refusing to remove path outside benchmark root: ${path}`);
  rmSync(path, { recursive: true, force: true });
}
function commonEnv() {
  const env = { ...process.env, ...profile, CARGO_HOME: paths.cargoHome };
  for (const key of Object.keys(env)) if (key === "RUSTC_WRAPPER" || key === "CARGO_TARGET_DIR" || key === "CARGO_BUILD_BUILD_DIR" || key.startsWith("SCCACHE_") || key.startsWith("GROVE_")) delete env[key];
  return env;
}
function withoutSccache(base) {
  if (!commandAvailable("sccache", ["--version"], base)) return base;
  const locator = process.platform === "win32" ? "where" : "which";
  const executable = capture(locator, ["sccache"], root, base).split(/\r?\n/, 1)[0];
  const sccacheDir = resolve(dirname(executable));
  return { ...base, PATH: (base.PATH || "").split(delimiter).filter((entry) => entry && resolve(entry) !== sccacheDir).join(delimiter) };
}
function modeEnv(mode, label, base) {
  const isolated = { CARGO_TARGET_DIR: join(paths.targets, label, "target"), CARGO_BUILD_BUILD_DIR: join(paths.targets, label, "build") };
  switch (mode) {
    case "cargo-default": return base;
    case "cargo-isolated": return { ...base, ...isolated };
    case "cargo-shared": return { ...base, CARGO_TARGET_DIR: join(paths.targets, "shared", "target"), CARGO_BUILD_BUILD_DIR: join(paths.targets, "shared", "build") };
    case "cargo-worktree": return withoutSccache(base);
    case "grove": return { ...base, GROVE_CACHE_ROOT: paths.grove, GROVE_MIN_FREE_GB: "0", GROVE_REQUIRE_COW: "true" };
    case "sccache": return { ...base, ...isolated, RUSTC_WRAPPER: "sccache", SCCACHE_DIR: paths.sccache, ...(process.platform === "win32" ? { SCCACHE_SERVER_PORT: String(43000 + (process.pid % 1000)) } : { SCCACHE_SERVER_UDS: join(root, "sccache.sock") }) };
    default: fail(`unknown mode: ${mode}`);
  }
}
function invocation(mode, workspace, label, args, base) {
  const env = modeEnv(mode, label, base);
  if (mode === "cargo-worktree") return { argv: args[0] === "nextest" ? [cargo, "worktree", ...args] : [cargo, "worktree", args[0], "--", ...args.slice(1)], env };
  if (mode === "grove") return { argv: [grove, "exec", "--tag", "head-to-head", "--", cargo, ...args], env };
  return { argv: [cargo, ...args], env };
}
function createWorktree(label, env) {
  const worktree = join(paths.worktrees, label);
  run(["git", "-C", paths.source, "worktree", "add", "--quiet", "-b", `bench-${label}`, worktree, "HEAD"], paths.source, env, join(paths.logs, `setup-${label}.log`));
  liveWorktrees.add(worktree);
  return worktree;
}
function removeWorktree(worktree, env) {
  if (existsSync(worktree)) run(["git", "-C", paths.source, "worktree", "remove", "--force", worktree], paths.source, env, join(paths.logs, `teardown-${relative(paths.worktrees, worktree)}.log`));
  liveWorktrees.delete(worktree);
}
function prepareProbeFixture() {
  probeFixture = join(root, "behavior-fixture"); mkdir(join(probeFixture, "nested"));
  writeFileSync(join(probeFixture, "alpha.txt"), "needle\nneedle haystack\n");
  writeFileSync(join(probeFixture, "nested", "beta.txt"), "haystack\nneedle\n");
}
function probeCommand() { return ["run", "--quiet", "--bin", probeBin, "--", ...(probeArgs.length ? probeArgs : (fixture ? [] : ["--json", "--no-ignore", "--sort", "path", "needle", probeFixture]))]; }
function outputHash(path) {
  let output = readFileSync(path, "utf8");
  if (!fixture && !probeArgs.length) output = output.trimEnd().split("\n").map((line) => { const record = JSON.parse(line); if (record.data?.stats) delete record.data.stats.elapsed; if (record.data) delete record.data.elapsed_total; return JSON.stringify(record); }).join("\n");
  return { bytes: Buffer.byteLength(output), sha256: createHash("sha256").update(output).digest("hex") };
}
function behaviorReceipt(mode, workspace, label, base) {
  const task = invocation(mode, workspace, `${label}-probe`, probeCommand(), base);
  const stdout = join(paths.logs, `${label}.probe.stdout`);
  const stderr = join(paths.logs, `${label}.probe.stderr`);
  const runResult = run(task.argv, workspace, task.env, stdout, stderr);
  return { command_exit_code: runResult.exit_code, output: outputHash(stdout), stdout_log: rel(stdout), stderr_log: rel(stderr) };
}
function compileUnits(log) { return (readFileSync(log, "utf8").match(/^\s*(Checking|Compiling)\s/mg) || []).length; }
function percentile(values, fraction) { const sorted = [...values].sort((a, b) => a - b); return sorted[Math.max(0, Math.ceil(sorted.length * fraction) - 1)]; }
function summarize(results) {
  const groups = new Map();
  for (const result of results) { const key = `${result.mode}:${result.phase}:${result.concurrency}:${result.workload}`; groups.set(key, [...(groups.get(key) || []), result.seconds]); }
  return [...groups].map(([key, values]) => { const [mode, phase, workers, workload] = key.split(":"); return { mode, phase, concurrency: Number(workers), workload, samples: values.length, median_seconds: percentile(values, .5), p95_seconds: percentile(values, .95) }; });
}
function validate() {
  if (help) { console.log("Usage: RUNS=6 CONCURRENCY=2 node benchmark/head_to_head.mjs [--dry-run]\nModes: cargo-default,cargo-isolated,cargo-shared,cargo-worktree,grove,sccache\nPhases: cold,seeded,warm,amortized"); process.exit(0); }
  const modes = new Set(["cargo-default", "cargo-isolated", "cargo-shared", "cargo-worktree", "grove", "sccache"]);
  const phaseNames = new Set(["cold", "seeded", "warm", "amortized"]);
  for (const mode of requested) if (!modes.has(mode)) fail(`unknown mode in MODES: ${mode}`);
  for (const phase of phases) if (!phaseNames.has(phase)) fail(`unknown phase in PHASES: ${phase}`);
  if (phases.includes("seeded")) for (const mode of requested) if (mode !== "cargo-default" && mode !== "grove") fail(`seeded phase defines seeding only for cargo-default and grove; remove ${mode} from MODES or drop the seeded phase`);
  if (!requested.includes("cargo-default")) fail("MODES must include cargo-default for behavior equivalence");
  if (existsSync(root)) fail(`BENCH_DIR must not already exist: ${root}`);
  if (!dryRun) for (const [program, args] of [["git", ["--version"]], [cargo, ["--version"]], [cargo, ["nextest", "--version"]]]) if (!commandAvailable(program, args)) fail(`required command is unavailable: ${program} ${args.join(" ")}`);
}
function capabilities(base) {
  const entries = [];
  const add = (mode, available, reason = null) => entries.push({ mode, available, ...(reason ? { reason } : {}) });
  add("cargo-default", true); add("cargo-isolated", true); add("cargo-shared", true);
  add("cargo-worktree", commandAvailable(cargo, ["worktree", "--help"], base), "cargo-worktree subcommand unavailable");
  add("grove", commandAvailable(grove, ["--version"], base), "grove unavailable");
  add("sccache", commandAvailable("sccache", ["--version"], base), "sccache unavailable (recorded, not silently skipped)");
  return entries;
}
function resetModeCache(mode, base) {
  if (mode === "grove") owned(paths.grove);
  if (mode === "cargo-shared") owned(join(paths.targets, "shared"));
  if (mode === "sccache") {
    spawnSync("sccache", ["--stop-server"], { cwd: root, env: modeEnv(mode, "reset", base), stdio: "ignore" });
    owned(paths.sccache);
    rmSync(join(root, "sccache.sock"), { force: true });
  }
}
function storageFor(mode, workspace, label) {
  if (mode === "cargo-default") return { target: allocation(join(workspace, "target")) };
  if (mode === "cargo-worktree") return { target: { physical_available: false, reason: "cargo-worktree owns its target layout" } };
  if (mode === "grove") return { grove_cache: allocation(paths.grove) };
  if (mode === "sccache") return { target: allocation(join(paths.targets, label)), sccache: allocation(paths.sccache) };
  return { target: allocation(join(paths.targets, mode === "cargo-shared" ? "shared" : label)) };
}
async function sampleGroup(mode, phase, workload, sample, activeConcurrency, base) {
  // Seeded measures Grove's product scenario: the Nth fresh worktree seeding
  // copy-on-write from a canonical warmed once on the source checkout. The
  // canonical must persist across seeded samples, so nothing is reset here.
  if (phase === "seeded") {
    if (mode === "grove" && !seededCanonicalWarmed) {
      run([grove, "cache", "warm"], paths.source, modeEnv("grove", "seeded-warm", base), join(paths.logs, "seeded-warm.log"));
      seededCanonicalWarmed = true;
    }
  } else {
    resetModeCache(mode, base);
  }
  const labels = Array.from(
    { length: activeConcurrency },
    (_, index) => `${mode}-${phase}-${workload.name}-${sample}-${activeConcurrency}w-${index}`,
  );
  const worktrees = labels.map((label) => createWorktree(label, base));
  const tasks = worktrees.map((workspace, index) => {
    const label = labels[index]; const task = invocation(mode, workspace, label, workload.args, base); const log = join(paths.logs, `${label}.build.log`);
    return { ...task, cwd: workspace, log, label };
  });
  try {
    // Every non-cold sample first establishes equivalent warm state by running the
    // workload once in that same fresh, isolated worktree. The timed command below
    // is therefore a retained-lane repeat for every mode, rather than comparing
    // Cargo's individually prebuilt worktrees with Grove's first canonical seed.
    const fresh = phase === "cold" || phase === "seeded" ? null : (activeConcurrency === 1
      ? tasks.map((task) => run(task.argv, task.cwd, task.env, join(paths.logs, `${task.label}.prime.log`)))
      : await runParallel(tasks.map((task) => ({ ...task, log: join(paths.logs, `${task.label}.prime.log`) }))));
    const outcomes = activeConcurrency === 1 ? tasks.map((task) => run(task.argv, task.cwd, task.env, task.log)) : await runParallel(tasks);
    const receipts = worktrees.map((workspace, index) => behaviorReceipt(mode, workspace, labels[index], base));
    const baseline = receipts[0].output.sha256;
    if (receipts.some((receipt) => receipt.output.sha256 !== baseline)) fail(`${mode} produced divergent behavior within one concurrent group`);
    return outcomes.map((outcome, index) => {
      const fresh_seconds = fresh?.[index].seconds ?? 0;
      return { mode, phase, workload: workload.name, sample, worker: index + 1, concurrency: activeConcurrency, seconds: outcome.seconds, fresh_seconds, warmup_seconds: phase === "amortized" ? fresh_seconds : 0, amortized_seconds: phase === "amortized" ? outcome.seconds + fresh_seconds : outcome.seconds, command_exit_code: outcome.exit_code, compile_units: compileUnits(tasks[index].log), behavior: receipts[index], build_log: rel(tasks[index].log), storage: storageFor(mode, worktrees[index], labels[index]) };
    });
  } finally { for (const tree of worktrees) removeWorktree(tree, base); }
}
function report(results, available, free, base) {
  const groveStatus = available.includes("grove") ? (() => { try { return capture(grove, ["cache", "status", "--json"], paths.source, modeEnv("grove", "status", base)); } catch { return null; } })() : null;
  const tools = { cargo: capture(cargo, ["--version"], root, base), rustc: capture("rustc", ["-Vv"], root, base), nextest: capture(cargo, ["nextest", "--version"], root, base), ...(available.includes("grove") ? { grove: capture(grove, ["--version"], root, base) } : {}), ...(available.includes("sccache") ? { sccache: capture("sccache", ["--version"], root, modeEnv("sccache", "status", base)) } : {}) };
  const baseline = results.filter((result) => result.mode === "cargo-default");
  for (const result of results) { const match = baseline.find((candidate) => candidate.workload === result.workload); result.behavior.matches_cargo_default = Boolean(match && match.behavior.output.sha256 === result.behavior.output.sha256); }
  if (results.some((result) => !result.behavior.matches_cargo_default)) fail("behavior receipt differs from cargo-default");
  const data = { schema_version: 3, source: { repository: repo, revision, commit: capture("git", ["-C", paths.source, "rev-parse", "HEAD"], paths.source, base) }, harness: { runs, concurrency, requested_modes: requested, active_modes: available, phases, cleanup_artifacts: !keep, profile, warmup: phases.includes("seeded") ? "seeded phase: grove cache warm once on the source checkout before seeded grove samples; cargo-default seeds nothing" : null, claim: "Comparison is for concurrent isolated worktrees. Cargo shared-target mode is measured as Cargo's serialized shared configuration, not described as unsafe." }, environment: { machine: { platform: process.platform, release: release(), arch: process.arch, hostname: hostname(), cpus: cpus().map((cpu) => cpu.model), total_memory_bytes: totalmem() }, filesystem: { benchmark_root: filesystem(root), start: free }, tools, cow: { requested_for_grove: true, grove_cache_status_json: groveStatus } }, capabilities: capabilities(base), results, summary: summarize(results) };
  writeFileSync(paths.report, `${JSON.stringify(data, null, 2)}\n`); console.log(`report: ${paths.report}`);
}
function cleanup(base) {
  for (const worktree of liveWorktrees) { const result = spawnSync("git", ["-C", paths.source, "worktree", "remove", "--force", worktree], { cwd: paths.source, env: base, stdio: "ignore" }); if (result.status !== 0) return; }
  if (existsSync(paths.source)) spawnSync("git", ["-C", paths.source, "worktree", "prune"], { cwd: paths.source, env: base, stdio: "ignore" });
  if (!keep) for (const path of [paths.worktrees, paths.targets, paths.grove, paths.sccache, join(root, "sccache.sock"), paths.cargoHome, paths.source]) if (existsSync(path)) owned(path);
}
function prepareSource(base) {
  if (!fixture) {
    run(["git", "clone", "--depth", "1", "--branch", revision, repo, paths.source], root, base, join(paths.logs, "clone.log"));
    return;
  }
  createMediumFixture(paths.source);
  run([cargo, "generate-lockfile", "--offline"], paths.source, base, join(paths.logs, "fixture-lock.log"));
  run(["git", "init", "--quiet"], paths.source, base, join(paths.logs, "fixture-init.log"));
  run(["git", "config", "user.email", "benchmark@example.invalid"], paths.source, base, join(paths.logs, "fixture-config.log"));
  run(["git", "config", "user.name", "Grove benchmark"], paths.source, base, join(paths.logs, "fixture-config.log"));
  run(["git", "add", "."], paths.source, base, join(paths.logs, "fixture-add.log"));
  run(["git", "commit", "--quiet", "-m", `benchmark fixture ${revision}`], paths.source, base, join(paths.logs, "fixture-commit.log"));
}
async function main() {
  validate(); const base = commonEnv(); const capabilityList = capabilities(base); const available = requested.filter((mode) => capabilityList.find((entry) => entry.mode === mode)?.available);
  if (dryRun) { console.log(JSON.stringify({ requested_modes: requested, active_modes: available, phases, runs, concurrency }, null, 2)); return; }
  mkdir(dirname(root)); const free = filesystem(dirname(root)); if (free.free_bytes < minFree) fail(`only ${(free.free_bytes / GIB).toFixed(1)} GiB free; need BENCH_START_FREE_GB=${minFree / GIB}`);
  mkdir(paths.logs); mkdir(paths.worktrees); mkdir(paths.cargoHome); prepareProbeFixture();
  try {
    prepareSource(base);
    run([cargo, "fetch", "--locked"], paths.source, base, join(paths.logs, "fetch.log"));
    const results = [];
    const levels = [...new Set([1, concurrency])];
    // Rotate mode order per sample so no mode always runs first and absorbs
    // one-time costs (crate-source extraction, OS caches) for the others.
    for (const phase of phases) for (const workload of workloads) for (let sample = 1; sample <= runs; sample += 1) for (let offset = 0; offset < available.length; offset += 1) { const mode = available[(sample - 1 + offset) % available.length]; for (const workers of levels) results.push(...await sampleGroup(mode, phase, workload, sample, workers, base)); }
    report(results, available, free, base);
  } finally { cleanup(base); }
}
main().catch((error) => { process.stderr.write(`benchmark: ${error.message}\n`); process.exitCode = 1; });
