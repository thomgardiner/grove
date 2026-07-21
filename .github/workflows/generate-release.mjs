#!/usr/bin/env node

import { readFileSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const workflow = fileURLToPath(new URL("release.yml", import.meta.url));
const qualification = fileURLToPath(new URL("release-qualification.yml", import.meta.url));
const qualifier = fileURLToPath(new URL("qualify-release-assets.mjs", import.meta.url));
const config = fileURLToPath(new URL("../../dist-workspace.toml", import.meta.url));
const check = process.argv.includes("--check");
const safe =
  "if: ${{ always() && needs.host.result == 'success' && needs.custom-release-qualification.result == 'success' && needs.attest-qualified-release-assets.result == 'success' }}";
const unsafe = "if: ${{ always() && needs.host.result == 'success' }}";
const prBuild =
  "if: ${{ fromJson(needs.plan.outputs.val).ci.github.artifacts_matrix.include != null && (needs.plan.outputs.publishing == 'true' || fromJson(needs.plan.outputs.val).ci.github.pr_run_mode == 'upload') }}";
const allowDirty = 'allow-dirty = ["ci"]';
const distInstaller =
  "curl --proto '=https' --tlsv1.2 -LsSf https://github.com/axodotdev/cargo-dist/releases/download/v0.32.0/cargo-dist-installer.sh | sh";
const distInstallerSha256 =
  "b657cf8c04a8b7bc28f39d220f7e6dd11bbd2bdb072c552262bd9ccf597261b5";
const distPowerShellInstaller =
  "irm https://github.com/axodotdev/cargo-dist/releases/download/v0.32.0/cargo-dist-installer.ps1 | iex";
const distPowerShellInstallerSha256 =
  "a3435e9944f1a1297add11c6a8ac1f543c14a5ea88879ee05b24ff8218d46d87";
const generatedPlan =
  '          echo "dist ran successfully"\n          cat plan-dist-manifest.json';
const qualifiedPlan =
  '          node .github/workflows/generate-release.mjs --plan plan-dist-manifest.json\n          echo "dist ran successfully"\n          cat plan-dist-manifest.json';
const generatedBootstrap = `        run: "${distInstaller}"`;
const qualifiedBootstrap = `        run: |\n          set -euo pipefail\n          installer="\${RUNNER_TEMP}/cargo-dist-installer.sh"\n          curl --proto '=https' --tlsv1.2 -fLsS \\\n            https://github.com/axodotdev/cargo-dist/releases/download/v0.32.0/cargo-dist-installer.sh \\\n            -o "\${installer}"\n          echo "${distInstallerSha256}  \${installer}" | shasum -a 256 -c -\n          sh "\${installer}"`;
const generatedPermissions = 'permissions:\n  "contents": "write"';
const qualifiedPermissions = 'permissions:\n  "contents": "read"';
const announceRunner = `${safe}\n    runs-on: "ubuntu-22.04"\n    env:`;
const qualifiedAnnounceRunner =
  `${safe}\n    runs-on: "ubuntu-22.04"\n    permissions:\n      "contents": "write"\n    env:`;
const generatedAnnounceAssets = `      - name: "Download GitHub Artifacts"
        uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c
        with:
          pattern: artifacts-*
          path: artifacts
          merge-multiple: true
      - name: Cleanup
        run: |
          # Remove the granular manifests
          rm -f artifacts/*-dist-manifest.json`;
const qualifiedAnnounceAssets = `      - name: Download qualified release assets
        uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c
        with:
          name: qualified-release-assets
          path: artifacts`;
const generatedBuildPermissions = `    permissions:
      "attestations": "write"
      "contents": "read"
      "id-token": "write"`;
const qualifiedBuildPermissions = `    permissions:
      "contents": "read"`;
const generatedAttestation = `      - name: Attest
        uses: actions/attest@f7c74d28b9d84cb8768d0b8ca14a4bac6ef463e6
        with:
          subject-path: "target/distrib/*\${{ join(matrix.targets, ', ') }}*"
`;
const announceMarker = "  # Create a GitHub Release while uploading all files to it\n  announce:";
const trustedAttestation = `  attest-qualified-release-assets:
    needs:
      - plan
      - custom-release-qualification
    if: \${{ !github.event.pull_request && needs.plan.outputs.publishing == 'true' && needs.custom-release-qualification.result == 'success' }}
    runs-on: "ubuntu-22.04"
    permissions:
      "attestations": "write"
      "contents": "read"
      "id-token": "write"
    steps:
      - name: Download qualified release assets
        uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c
        with:
          name: qualified-release-assets
          path: artifacts
      - name: Attest qualified release assets
        uses: actions/attest@f7c74d28b9d84cb8768d0b8ca14a4bac6ef463e6
        with:
          subject-path: "artifacts/*"

${announceMarker}`;
const generatedAnnounceNeeds = `  announce:
    needs:
      - plan
      - host
      - custom-release-qualification`;
const qualifiedAnnounceNeeds = `${generatedAnnounceNeeds}
      - attest-qualified-release-assets`;
const backup = `${workflow}.generate-backup`;

function hardenPlan(plan) {
  const entries = plan.ci?.github?.artifacts_matrix?.include ?? [];
  let unix = 0;
  let windows = 0;
  for (const entry of entries) {
    if (entry.install_dist?.run === distInstaller) {
      entry.install_dist.run = [
        "set -euo pipefail",
        'installer="${RUNNER_TEMP}/cargo-dist-installer.sh"',
        `curl --proto '=https' --tlsv1.2 -fLsS https://github.com/axodotdev/cargo-dist/releases/download/v0.32.0/cargo-dist-installer.sh -o "\${installer}"`,
        `echo "${distInstallerSha256}  \${installer}" | shasum -a 256 -c -`,
        'sh "${installer}"',
      ].join("\n");
      unix += 1;
    } else if (entry.install_dist?.run === distPowerShellInstaller) {
      entry.install_dist.run = [
        "$ErrorActionPreference = 'Stop'",
        "$installer = Join-Path $env:RUNNER_TEMP 'cargo-dist-installer.ps1'",
        "Invoke-WebRequest 'https://github.com/axodotdev/cargo-dist/releases/download/v0.32.0/cargo-dist-installer.ps1' -OutFile $installer",
        "$actual = (Get-FileHash $installer -Algorithm SHA256).Hash.ToLowerInvariant()",
        `if ($actual -ne '${distPowerShellInstallerSha256}') { throw 'cargo-dist installer checksum mismatch' }`,
        "& $installer",
      ].join("\n");
      windows += 1;
    }
  }
  if (unix !== 3 || windows !== 1) {
    throw new Error(
      `expected three Unix and one Windows cargo-dist installers, found ${unix} and ${windows}`,
    );
  }
  return plan;
}

if (process.argv[2] === "--plan") {
  const path = process.argv[3];
  if (!path || process.argv.length !== 4) {
    throw new Error("usage: generate-release.mjs --plan PATH");
  }
  const plan = hardenPlan(JSON.parse(readFileSync(path, "utf8")));
  writeFileSync(path, `${JSON.stringify(plan)}\n`);
  process.exit(0);
}

const before = readFileSync(workflow, "utf8");
const configured = readFileSync(config, "utf8");
const qualifiedAssets = readFileSync(qualification, "utf8");
const unixCapabilityContract = 'const c=JSON.parse(process.argv[1]),v=process.argv[2],s=c.status,i=c.inspection; if(c.grove_version!==v||c.schema_version!==1||s.repository_schema!==1||s.task_status_schema!==3||s.task_record_schema!==5||i.binding_schema!==1||i.execution_schema!==1||i.finish_source_cas!==true||i.process_tree!=="unix_process_group_best_effort"||i.filesystem!=="read_only_permissions_and_digest"||i.output!=="captured_logs_json_report")process.exit(1)';
const windowsCapabilityContract = "if ($capabilities.grove_version -ne $releases[0].app_version -or $capabilities.schema_version -ne 1 -or $capabilities.status.repository_schema -ne 1 -or $capabilities.status.task_status_schema -ne 3 -or $capabilities.status.task_record_schema -ne 5 -or $capabilities.inspection.binding_schema -ne 1 -or $capabilities.inspection.execution_schema -ne 1 -or $capabilities.inspection.finish_source_cas -ne $true -or $capabilities.inspection.process_tree -ne 'windows_job_object' -or $capabilities.inspection.filesystem -ne 'read_only_permissions_and_digest' -or $capabilities.inspection.output -ne 'captured_logs_json_report')";
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
for (const required of [
  "  macos-installer:\n    needs: checksums",
  "          - runner: macos-14\n            target: aarch64-apple-darwin",
  "          - runner: macos-15-intel\n            target: x86_64-apple-darwin",
  "        run: node .github/workflows/qualify-release-assets.mjs raw-artifacts qualified-artifacts",
  "      - name: Upload qualified release assets",
]) {
  if (qualifiedAssets.split(required).length - 1 !== 1) {
    console.error(`release qualification must contain exactly one ${JSON.stringify(required)}`);
    process.exit(1);
  }
}
if (qualifiedAssets.split("          name: qualified-release-assets").length - 1 !== 4) {
  console.error("release qualification must stage once and consume the qualified asset in every installer job");
  process.exit(1);
}
if (
  qualifiedAssets.split(unixCapabilityContract).length - 1 !== 2 ||
  qualifiedAssets.split(windowsCapabilityContract).length - 1 !== 1
) {
  console.error("release qualification must verify Grove's exact installed capability contract");
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
  let release = generated.replace(unsafe, safe);
  if (release.replace(safe, unsafe) !== generated) {
    throw new Error("qualified workflow differs from cargo-dist output by more than the announce gate");
  }
  for (const [label, source, replacement] of [
    ["plan installer hardening", generatedPlan, qualifiedPlan],
    ["bootstrap installer hardening", generatedBootstrap, qualifiedBootstrap],
    ["least-privilege workflow permissions", generatedPermissions, qualifiedPermissions],
    ["announce write permission", announceRunner, qualifiedAnnounceRunner],
    ["qualified announce assets", generatedAnnounceAssets, qualifiedAnnounceAssets],
    ["unprivileged pull-request build", generatedBuildPermissions, qualifiedBuildPermissions],
    ["build-job attestation removal", generatedAttestation, ""],
    ["qualified-set attestation job", announceMarker, trustedAttestation],
    ["attestation announce dependency", generatedAnnounceNeeds, qualifiedAnnounceNeeds],
  ]) {
    const occurrences = release.split(source).length - 1;
    if (occurrences !== 1) {
      throw new Error(`expected one ${label} source, found ${occurrences}`);
    }
    release = release.replace(source, replacement);
  }
  if (release.split(prBuild).length - 1 !== 1) {
    throw new Error("release workflow does not build the planned artifact matrix on pull requests");
  }
  const localBuild = release
    .split("\n  build-local-artifacts:\n")[1]
    ?.split("\n  build-global-artifacts:\n")[0];
  if (
    !localBuild?.includes(qualifiedBuildPermissions) ||
    localBuild.includes('"id-token": "write"') ||
    localBuild.includes('"attestations": "write"') ||
    localBuild.includes("actions/attest@")
  ) {
    throw new Error("pull-request artifact builds must have contents:read only and no attestation step");
  }
  const attestation = release
    .split("\n  attest-qualified-release-assets:\n")[1]
    ?.split(`\n${announceMarker}\n`)[0];
  if (
    !attestation?.includes("!github.event.pull_request") ||
    !attestation.includes("needs.custom-release-qualification.result == 'success'") ||
    !attestation.includes('"id-token": "write"') ||
    !attestation.includes('"attestations": "write"') ||
    !attestation.includes("actions/attest@") ||
    attestation.split("name: qualified-release-assets").length - 1 !== 1 ||
    !attestation.includes('subject-path: "artifacts/*"') ||
    attestation.includes("pattern:") ||
    attestation.includes("merge-multiple:") ||
    attestation.includes("actions/checkout@") ||
    release.split('"id-token": "write"').length - 1 !== 1 ||
    release.split('"attestations": "write"').length - 1 !== 1
  ) {
    throw new Error("attestation must bind only the exact qualified asset set in one tag-only job");
  }
  const announceJob = release.split("\n  announce:\n")[1];
  if (
    !announceJob?.includes("- attest-qualified-release-assets") ||
    !announceJob.includes("needs.attest-qualified-release-assets.result == 'success'")
  ) {
    throw new Error("announcement must require successful qualified-set attestation");
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
  const plan = hardenPlan(distJson(["plan", "--output-format=json"]));
  const qualifierTest = spawnSync(process.execPath, [qualifier, "--self-test"], {
    env: { ...process.env, DIST_PLAN: JSON.stringify(plan) },
    stdio: "inherit",
  });
  if (qualifierTest.status !== 0) {
    throw new Error(
      `release asset qualifier self-test exited ${qualifierTest.status ?? "without a status"}`,
    );
  }
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
