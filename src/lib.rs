//! grove — agentic Rust build tooling.
//!
//! Library surface so the binary and the test suite share one implementation:
//! [`cache`] owns lanes, the warm canonical, and the self-bounding GC; [`seed`]
//! does the copy-on-write cloning; [`impact`] routes a git diff to affected packages.

pub mod cache;
pub mod impact;
pub mod seed;
