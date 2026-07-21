# Grove quickstart

## Install

From source (requires a Rust toolchain):

```sh
cargo install --git https://github.com/thomgardiner/grove --locked
```

Prebuilt, checksum-verified binary installers ship with the first published GitHub
release; until then, source installation is the supported path.

## First five minutes

In a Git repository containing a Rust workspace:

```sh
grove init
grove doctor
grove cache warm
grove check
grove test
```

`grove init` writes the coordination contract and a commented configuration starter. `doctor` is
read-only. The first `cache warm` builds the shared canonical; later worktrees receive isolated
copy-on-write lanes seeded from it.

For an agent task:

```sh
grove worktree acquire --agent alice
grove claim --agent alice --task parser src/parser.rs
grove check
grove test
grove release claims --agent alice
```

Use `grove status --json` to inspect live worktrees, claims, tasks, and lanes.

## Upgrade

Reinstall from source, then confirm the toolchain contract still holds:

```sh
cargo install --git https://github.com/thomgardiner/grove --locked --force
grove --version
grove doctor
```

Binary installations (once releases ship) upgrade with `grove-update`; package-manager
installations must be upgraded through the same package manager.

## Uninstall

```sh
cargo uninstall grove
```

Do not delete the Grove cache or configuration as part of binary uninstall: they may
contain active worktrees, task leases, claims, and verification evidence. Any future
state-purge command must first prove with `grove status --json` that no live state exists.
