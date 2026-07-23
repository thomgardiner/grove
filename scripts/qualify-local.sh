#!/usr/bin/env bash
# Run Grove's entire CI job set on this machine before any push or tag.
#
# The rule this enforces: public CI is a confirmation, not a debug loop. Every
# job below is the same command CI runs, in the same order, so a green run here
# means the only things left that can fail are genuinely OS-specific (MSVC, the
# Windows Job Object, PowerShell). Those are the sole excuse for a CI surprise;
# everything else should have failed here first.
set -euo pipefail

cd "$(dirname "$0")/.."
scratch=$(mktemp -d "${TMPDIR:-/tmp}/grove-qualify-XXXXXX")
trap 'rm -rf "${scratch}"' EXIT
# Resolve symlinks: macOS puts $TMPDIR under /var, which is a link to
# /private/var, and inspection refuses a cache root with a redirecting
# ancestor. CI's RUNNER_TEMP is already a real path, so leaving this unresolved
# would fail locally for a reason CI never sees.
scratch=$(cd "${scratch}" && pwd -P)

version=$(node -e 'const t=require("fs").readFileSync("Cargo.toml","utf8");process.stdout.write(t.match(/^version = "(.+?)"/m)[1])')
echo "==> qualifying grove ${version}"

echo "==> quality: fmt, clippy, advisories, doctests"
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
if command -v cargo-deny >/dev/null; then
  cargo deny check advisories
else
  echo "warning: cargo-deny absent; CI still runs 'cargo deny check advisories'" >&2
fi
cargo test --doc --workspace --locked

echo "==> test: ignored-test allowlist"
cargo nextest list --workspace --locked --message-format json > "${scratch}/nextest-list.json"
node - "${scratch}/nextest-list.json" <<'NODE'
const list = require(process.argv[2]);
const ignored = [];
for (const suite of Object.values(list['rust-suites'])) {
  for (const [name, test] of Object.entries(suite.testcases)) {
    if (test.ignored) ignored.push(name);
  }
}
ignored.sort();
const expected = [
  'bench_clone_large_tree',
  'held_child',
  'inspection_child_floods_output',
  'inspection_child_mutates',
  'inspection_child_passes',
  'inspection_child_sleeps',
  'supervised_child',
  'verifier_barrier',
];
if (process.platform !== 'win32') {
  expected.push('inspection_snapshot::tests::hardening::malicious_config_child');
}
expected.sort();
if (JSON.stringify(ignored) !== JSON.stringify(expected)) {
  console.error('ignored tests changed', { expected, ignored });
  process.exit(1);
}
NODE

echo "==> test: full suite"
cargo nextest run --workspace --locked --no-fail-fast --no-tests fail

echo "==> distribution: deterministic release plan"
if command -v dist >/dev/null; then
  node .github/workflows/generate-release.mjs --check
else
  echo "warning: cargo-dist absent; CI still checks the generated release plan" >&2
fi

echo "==> source-install: install from the tree and smoke the installed binary"
cargo install --path . --locked --root "${scratch}/install" >/dev/null
grove="${scratch}/install/bin/grove"
"${grove}" --version | grep -F "grove ${version}"
capabilities=$("${grove}" capabilities)
node -e '
const c = JSON.parse(process.argv[1]);
const want = process.argv[2];
if (
  c.grove_version !== want || c.schema_version !== 1 ||
  c.status.task_status_schema !== 4 || c.inspection.binding_schema !== 1 ||
  c.inspection.finish_source_cas !== true || c.coordination.git_write_serialization !== true ||
  !c.task.exec_capabilities.includes("edit") || c.task.verification_policy_pinned !== true
) { console.error("unexpected capabilities", c); process.exit(1); }
' "${capabilities}" "${version}"

# The release qualification asserts the capability contract inline, in four
# places, against the *installed* binary. Bumping a schema without updating
# them fails only after a tag is pushed, which is the worst time to find out.
# Run CI's own contract string here, against this build.
echo "==> release contract: the assertion the release qualification will run"
node - "${capabilities}" "${version}" <<'NODE'
const { readFileSync } = require('node:fs');
const source = readFileSync('.github/workflows/generate-release.mjs', 'utf8');
const match = source.match(/^const unixCapabilityContract = '(.*)';$/m);
if (!match) {
  console.error('could not find the unix capability contract in generate-release.mjs');
  process.exit(1);
}
// The contract reads process.argv[1] and [2]; give it exactly that shape.
const contract = new Function('process', match[1].replace(/\\\\"/g, '"'));
const argv = [null, process.argv[2], process.argv[3]];
let failed = false;
contract({ argv, exit: (code) => { if (code) failed = true; } });
if (failed) {
  console.error('installed capabilities do not satisfy the release contract');
  console.error(process.argv[2]);
  process.exit(1);
}
NODE

# The ci.yml source-install smoke hardcodes the version and task-status schema,
# separately from the dynamic checks above, so a bump that forgets them fails
# only on public CI (it did once). Assert ci.yml matches this build.
schema=$(node -e 'process.stdout.write(String(JSON.parse(process.argv[1]).status.task_status_schema))' "${capabilities}")
ci_versions=$(grep -oE "grove [0-9]+\.[0-9]+\.[0-9]+" .github/workflows/ci.yml | sort -u)
ci_schemas=$(grep -oE "task_status_schema[^0-9]*[0-9]+" .github/workflows/ci.yml | grep -oE "[0-9]+$" | sort -u)
if [ "${ci_versions}" != "grove ${version}" ]; then
  echo "ci.yml source-install smoke asserts [${ci_versions}], not 'grove ${version}'; update .github/workflows/ci.yml" >&2
  exit 1
fi
if [ "${ci_schemas}" != "${schema}" ]; then
  echo "ci.yml source-install smoke asserts task_status_schema [${ci_schemas}], not ${schema}; update .github/workflows/ci.yml" >&2
  exit 1
fi

# The same non-ASCII, space-bearing path CI uses: quoting bugs surface here.
repo="${scratch}/grove installed lifecycle 雪"
mkdir -p "${repo}"
(
  cd "${repo}"
  git init -q
  git config user.email ci@example.invalid
  git config user.name 'Grove CI'
  printf 'candidate\n' > candidate.txt
  git add candidate.txt
  git commit -q -m initial
  export GROVE_CACHE_ROOT="${scratch}/grove-cache"
  begun=$("${grove}" task begin --agent ci --task installed-lifecycle --scope candidate.txt)
  task_id=$(node -e 'process.stdout.write(JSON.parse(process.argv[1]).task.id)' "$begun")
  acquired=$("${grove}" inspect acquire --task-id "$task_id" --ttl-secs 60)
  capsule_id=$(node -e 'process.stdout.write(JSON.parse(process.argv[1]).capsule_id)' "$acquired")
  source_sha256=$(node -e 'process.stdout.write(JSON.parse(process.argv[1]).source_sha256)' "$acquired")
  executed=$("${grove}" inspect exec "$capsule_id" --timeout-secs 10 -- sh -c 'test -f candidate.txt')
  node -e 'const r=JSON.parse(process.argv[1]); if(!r.authorized||!r.source_unchanged||!r.capsule_unchanged)process.exit(1)' "$executed"
  "${grove}" inspect release "$capsule_id"
  finished=$("${grove}" task finish --task-id "$task_id" --expected-source-sha256 "$source_sha256" --allow-unverified 'local qualification lifecycle')
  node -e 'const r=JSON.parse(process.argv[1]); if(r.source_sha256!==process.argv[2]||r.task.lifecycle!=="finished")process.exit(1)' "$finished" "$source_sha256"
)

echo "LOCAL QUALIFICATION: GREEN (grove ${version})"
echo "Uncovered by this script: Windows MSVC linking, the Windows Job Object"
echo "process tree, and the PowerShell smoke. Those run on CI only."
