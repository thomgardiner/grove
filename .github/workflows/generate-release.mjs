#!/usr/bin/env node

import { readFileSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const workflow = fileURLToPath(new URL("release.yml", import.meta.url));
const config = fileURLToPath(new URL("../../dist-workspace.toml", import.meta.url));
const check = process.argv.includes("--check");
const safe =
  "if: ${{ always() && needs.host.result == 'success' && needs.custom-release-qualification.result == 'success' }}";
const unsafe = "if: ${{ always() && needs.host.result == 'success' }}";
const prBuild =
  "if: ${{ fromJson(needs.plan.outputs.val).ci.github.artifacts_matrix.include != null && (needs.plan.outputs.publishing == 'true' || fromJson(needs.plan.outputs.val).ci.github.pr_run_mode == 'upload') }}";
const allowDirty = 'allow-dirty = ["ci"]';
const backup = `${workflow}.generate-backup`;
const before = readFileSync(workflow, "utf8");
const configured = readFileSync(config, "utf8");
if (before.split(safe).length - 1 !== 1 || before.includes(unsafe)) {
  console.error("release.yml does not contain exactly one qualified announce condition");
  process.exit(1);
}
if (
  configured.match(/^allow-dirty\s*=/gm)?.length !== 1 ||
  configured.split(allowDirty).length - 1 !== 1
) {
  console.error('dist-workspace.toml must contain only allow-dirty = ["ci"]');
  process.exit(1);
}

function dist(args) {
  const result = spawnSync("dist", args, { stdio: "inherit" });
  if (result.status !== 0) {
    throw new Error(`dist ${args.join(" ")} exited ${result.status ?? "without a status"}`);
  }
}

function distJson(args) {
  const result = spawnSync("dist", args, {
    encoding: "utf8",
    stdio: ["inherit", "pipe", "inherit"],
  });
  if (result.status !== 0) {
    throw new Error(`dist ${args.join(" ")} exited ${result.status ?? "without a status"}`);
  }
  return JSON.parse(result.stdout);
}

let succeeded = false;
try {
  // Prove cargo-dist's pristine output without allowing generated CI drift.
  writeFileSync(config, configured.replace(`${allowDirty}\n`, ""));
  renameSync(workflow, backup);
  dist(["generate", "--mode", "ci"]);
  dist(["generate", "--mode", "ci", "--check"]);
  dist(["plan"]);

  const generated = readFileSync(workflow, "utf8");
  const occurrences = generated.split(unsafe).length - 1;
  if (occurrences !== 1 || generated.includes(safe)) {
    throw new Error(`expected one pristine cargo-dist announce condition, found ${occurrences}`);
  }
  const release = generated.replace(unsafe, safe);
  if (release.replace(safe, unsafe) !== generated) {
    throw new Error("qualified workflow differs from cargo-dist output by more than the announce gate");
  }
  if (release.split(prBuild).length - 1 !== 1) {
    throw new Error("release workflow does not build the planned artifact matrix on pull requests");
  }

  const announce = release.indexOf("\n  announce:\n");
  if (announce < 0 || release.slice(0, announce).includes("gh release create")) {
    throw new Error("release creation is not isolated to the qualified announce job");
  }
  for (const line of release.split("\n")) {
    const action = line.match(/^\s*(?:-\s*)?uses:\s*([^#\s]+)\s*/)?.[1];
    if (action && !action.startsWith("./") && !/@[0-9a-f]{40}$/.test(action)) {
      throw new Error(`unpinned action: ${action}`);
    }
  }

  writeFileSync(config, configured);
  writeFileSync(workflow, release);
  const plan = distJson(["plan", "--output-format=json"]);
  const github = plan.ci?.github;
  const targets = github?.artifacts_matrix?.include
    ?.flatMap((entry) => entry.targets ?? [])
    .sort();
  const expectedTargets = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-musl",
  ];
  if (
    github?.pr_run_mode !== "upload" ||
    JSON.stringify(targets) !== JSON.stringify(expectedTargets)
  ) {
    throw new Error("runtime release plan does not upload the complete artifact matrix on pull requests");
  }
  if (check && release !== before) {
    throw new Error("release.yml is stale; run: node .github/workflows/generate-release.mjs");
  }
  succeeded = true;
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
} finally {
  writeFileSync(config, configured);
  if (!succeeded) writeFileSync(workflow, before);
  try {
    unlinkSync(backup);
  } catch {}
}
