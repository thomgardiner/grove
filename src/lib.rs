//! grove — agentic Rust build tooling.
//!
//! Library surface so the binary and the test suite share one implementation:
//! [`cache`] owns lanes, the warm canonical, and the self-bounding GC; [`seed`]
//! does the copy-on-write cloning; [`impact`] routes a git diff to affected packages.

pub mod api;
pub mod cache;
pub mod claim;
pub mod config;
pub mod git;
pub mod impact;
pub mod project;
pub mod seed;
pub mod status;
pub mod task;
pub mod watch;
pub mod worktree;
