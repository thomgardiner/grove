//! Compatibility facade for Grove's coordination core and Rust acceleration.

pub use grove_core::{canonical_path, events, git, repo_slug, scope, snapshot, write_atomic};
pub use grove_rust::*;
