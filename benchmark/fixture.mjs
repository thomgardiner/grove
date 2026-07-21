// Deterministic, dependency-free Rust workspace used as a second benchmark fixture.
// Keep it generated: committing its sources would only add benchmark ballast to Grove.

import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

export const mediumFixture = {
  name: "medium",
  version: "v1",
  crates: 24,
  binary: "bench-app",
};

function write(path, text) {
  mkdirSync(join(path, ".."), { recursive: true });
  writeFileSync(path, text);
}

function member(index) { return `crates/unit-${String(index).padStart(2, "0")}`; }

export function createMediumFixture(root) {
  const members = Array.from({ length: mediumFixture.crates }, (_, index) => member(index));
  write(join(root, "Cargo.toml"), `[workspace]\nresolver = "2"\nmembers = [\n${members.map((name) => `  "${name}",`).join("\n")}\n  "app",\n]\n`);
  for (let index = 0; index < mediumFixture.crates; index += 1) {
    const crate = `unit-${String(index).padStart(2, "0")}`;
    const dependency = index === 0 ? "" : `unit-${String(index - 1).padStart(2, "0")} = { path = "../unit-${String(index - 1).padStart(2, "0")}" }\n`;
    const previous = index === 0 ? "0_u64" : `unit_${String(index - 1).padStart(2, "0")}::value(seed)`;
    write(join(root, member(index), "Cargo.toml"), `[package]\nname = "${crate}"\nversion = "0.1.0"\nedition = "2024"\n\n[dependencies]\n${dependency}`);
    write(join(root, member(index), "src", "lib.rs"), `pub fn value(seed: u64) -> u64 {\n    let prior = ${previous};\n    prior.rotate_left(${index % 31}) ^ ${index + 1}_u64.wrapping_mul(seed.wrapping_add(${index}_u64))\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn deterministic() { assert_eq!(super::value(7), super::value(7)); }\n}\n`);
  }
  const last = `unit-${String(mediumFixture.crates - 1).padStart(2, "0")}`;
  write(join(root, "app", "Cargo.toml"), `[package]\nname = "${mediumFixture.binary}"\nversion = "0.1.0"\nedition = "2024"\n\n[dependencies]\n${last} = { path = "../crates/${last}" }\n`);
  write(join(root, "app", "src", "main.rs"), `fn main() { println!("fixture-result:{}", ${last.replace("-", "_")}::value(42)); }\n`);
}
