# Grove quickstart

Grove is distributed as a prebuilt binary. Installing it does not require Rust.

## Install

macOS or Linux:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/thomgardiner/grove/releases/latest/download/grove-installer.sh | sh
```

Windows PowerShell:

```powershell
$ErrorActionPreference = "Stop"
irm https://github.com/thomgardiner/grove/releases/latest/download/grove-installer.ps1 | iex
```

The installer verifies the release checksum before placing `grove` and `grove-update` on the user
path. A source build remains available for contributors:

```sh
cargo install --git https://github.com/thomgardiner/grove --tag v0.3.3 --locked
```

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

Run the updater installed with the binary:

```sh
grove-update
grove --version
grove doctor
```

If the updater is unavailable, rerun the installer. Package-manager installations must be upgraded
through the same package manager.

## Uninstall

Remove only `grove` and `grove-update` from the directory chosen by the installer. Do not delete the
Grove cache or configuration as part of binary uninstall: they may contain active worktrees, task
leases, claims, and verification evidence. Any future state-purge command must first prove with
`grove status --json` that no live state exists.
