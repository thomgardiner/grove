#!/usr/bin/env node

import { createHash } from "node:crypto";
import {
  copyFileSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { basename, join, resolve, win32 } from "node:path";

function artifactNames(plan) {
  if (plan.releases?.length !== 1) throw new Error("release plan must contain exactly one release");
  const planned = plan.releases[0].artifacts;
  if (!planned?.length || new Set(planned).size !== planned.length) {
    throw new Error("release plan has no artifacts or contains duplicate artifact names");
  }
  for (const name of planned) {
    if (name !== basename(name) || name !== win32.basename(name)) {
      throw new Error(`release plan artifact is not a basename: ${name}`);
    }
  }
  return planned;
}

function internalNames(plan) {
  const entries = plan.ci?.github?.artifacts_matrix?.include ?? [];
  const local = entries.map(({ targets }) => {
    if (!targets?.length) throw new Error("release plan matrix entry has no targets");
    return `${targets.join("-")}-dist-manifest.json`;
  });
  const required = ["plan-dist-manifest.json", "global-dist-manifest.json", ...local];
  if (new Set(required).size !== required.length) {
    throw new Error("release plan produces duplicate internal manifest names");
  }
  return { required, optional: ["dist-manifest.json"] };
}

function checksum(root, name) {
  return createHash("sha256").update(readFileSync(resolve(root, name))).digest("hex");
}

function parseChecksums(root, manifest, expectedTargets) {
  const entries = readFileSync(resolve(root, manifest), "utf8")
    .split(/\r?\n/)
    .filter(Boolean)
    .map((line) => {
      const match = /^([0-9a-fA-F]{64}) [ *]([^\r\n]+)$/.exec(line);
      if (!match) throw new Error(`invalid checksum entry in ${manifest}`);
      const name = match[2];
      if (name !== basename(name) || name !== win32.basename(name) || !expectedTargets.has(name)) {
        throw new Error(`unexpected checksum target in ${manifest}: ${name}`);
      }
      return { digest: match[1].toLowerCase(), name };
    });
  if (!entries.length || new Set(entries.map(({ name }) => name)).size !== entries.length) {
    throw new Error(`empty or duplicate checksum entries in ${manifest}`);
  }
  return entries;
}

export function qualify(plan, rawDirectory, stagedDirectory) {
  const raw = resolve(rawDirectory);
  const staged = resolve(stagedDirectory);
  const planned = artifactNames(plan);
  const internal = internalNames(plan);
  const allowed = new Set([...planned, ...internal.required, ...internal.optional]);
  const actual = readdirSync(raw);
  const missing = [...planned, ...internal.required].filter((name) => !actual.includes(name));
  const extras = actual.filter((name) => !allowed.has(name));
  if (missing.length || extras.length) {
    throw new Error(`downloaded artifacts differ from plan; missing=${missing.sort()} extra=${extras.sort()}`);
  }
  for (const name of actual) {
    if (!lstatSync(resolve(raw, name)).isFile()) throw new Error(`artifact is not a regular file: ${name}`);
  }

  const checksumFiles = planned.filter((name) => name.endsWith(".sha256"));
  const expectedTargets = new Set(checksumFiles.map((name) => name.slice(0, -7)));
  if (!expectedTargets.size || !planned.includes("sha256.sum")) {
    throw new Error("release plan is missing SHA-256 artifacts");
  }
  for (const manifest of checksumFiles) {
    const entries = parseChecksums(raw, manifest, expectedTargets);
    const target = manifest.slice(0, -7);
    if (entries.length !== 1 || entries[0].name !== target) {
      throw new Error(`checksum file does not name its planned artifact: ${manifest}`);
    }
    if (checksum(raw, target) !== entries[0].digest) throw new Error(`checksum mismatch for ${target}`);
  }
  const unified = parseChecksums(raw, "sha256.sum", expectedTargets);
  if (unified.length !== expectedTargets.size || unified.some(({ name }) => !expectedTargets.has(name))) {
    throw new Error("unified checksum does not cover exactly the planned artifacts");
  }
  for (const { digest, name } of unified) {
    if (checksum(raw, name) !== digest) throw new Error(`checksum mismatch for ${name}`);
  }

  mkdirSync(staged);
  for (const name of planned) copyFileSync(resolve(raw, name), resolve(staged, name));
  const published = readdirSync(staged);
  if (published.length !== planned.length || published.some((name) => !planned.includes(name))) {
    throw new Error("staged release assets differ from the qualified plan");
  }
}

function selfTest(realPlan) {
  const root = mkdtempSync(join(tmpdir(), "grove-release-assets-"));
  try {
    const raw = join(root, "raw");
    mkdirSync(raw);
    const plan = realPlan ?? {
      releases: [{ artifacts: ["demo.tar.xz", "demo.tar.xz.sha256", "sha256.sum"] }],
      ci: { github: { artifacts_matrix: { include: [{ targets: ["aarch64-apple-darwin"] }] } } },
    };
    const planned = artifactNames(plan);
    const checksumFiles = planned.filter((name) => name.endsWith(".sha256"));
    const targets = checksumFiles.map((name) => name.slice(0, -7));
    for (const name of planned) {
      if (!name.endsWith(".sha256") && name !== "sha256.sum") {
        writeFileSync(join(raw, name), `synthetic ${name}\n`);
      }
    }
    for (const [manifest, target] of checksumFiles.map((name) => [name, name.slice(0, -7)])) {
      writeFileSync(join(raw, manifest), `${checksum(raw, target)}  ${target}\n`);
    }
    writeFileSync(
      join(raw, "sha256.sum"),
      targets.map((target) => `${checksum(raw, target)}  ${target}`).join("\n") + "\n",
    );
    for (const name of [...internalNames(plan).required, "dist-manifest.json"]) {
      writeFileSync(join(raw, name), "{}\n");
    }
    qualify(plan, raw, join(root, "staged"));
    writeFileSync(join(raw, "unexpected.txt"), "unexpected");
    try {
      qualify(plan, raw, join(root, "rejected"));
      throw new Error("unexpected release input was accepted");
    } catch (error) {
      if (!error.message.includes("extra=unexpected.txt")) throw error;
    }
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

if (process.argv[2] === "--self-test") {
  if (process.argv.length !== 3) throw new Error("usage: qualify-release-assets.mjs --self-test");
  selfTest(process.env.DIST_PLAN ? JSON.parse(process.env.DIST_PLAN) : undefined);
} else {
  if (process.argv.length !== 4 || !process.env.DIST_PLAN) {
    throw new Error("usage: DIST_PLAN=JSON qualify-release-assets.mjs RAW_DIR STAGED_DIR");
  }
  qualify(JSON.parse(process.env.DIST_PLAN), process.argv[2], process.argv[3]);
}
