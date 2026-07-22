//! grove — agentic Rust build tooling.
//!
//! Library surface so the binary and the test suite share one implementation:
//! [`cache`] owns lanes, the warm canonical, and the self-bounding GC; [`seed`]
//! does the copy-on-write cloning; [`impact`] routes a git diff to affected packages.

pub mod api;
pub mod artifact;
pub mod cache;
pub mod capabilities;
pub mod claim;
pub mod config;
pub mod doctor;
pub mod fingerprint;
pub use grove_core::{events, git, snapshot};
pub mod governor;
pub mod impact;
pub mod init;
pub mod inspection;
pub mod inspection_process;
pub mod inspection_snapshot;
pub mod materialization_cargo;
pub mod materialization_git;
pub mod project;
pub mod recovery;
pub mod release;
pub mod seed;
pub mod status;
pub mod task;
pub mod topology;
pub(crate) mod topology_partition;
pub mod verify;
pub mod watch;
pub mod worktree;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
