//! Static, read-only status for acceleration work Grove does not enable itself.

use super::Watch;

pub(super) fn items() -> Vec<Watch> {
    vec![
        watch(
            "parallel-frontend",
            "nightly-only",
            "rustc parallel front-end remains nightly-only; revisit when stable support lands",
        ),
        watch(
            "relink-dont-rebuild",
            "unavailable",
            "Cargo relink-don't-rebuild is incomplete; revisit when Cargo exposes a stable capability",
        ),
        watch(
            "wild",
            "not-ready",
            "Wild 0.9 lacks incremental linking and has partial linker-script support; keep mold as the production Linux choice today",
        ),
        watch(
            "sccache",
            "conditional",
            "sccache requires rustc incremental off and cannot cache final binary crates; it fits remote or frequently cold dependency builds, not warm local lanes",
        ),
    ]
}

fn watch(id: &str, status: &str, detail: &str) -> Watch {
    Watch {
        id: id.to_string(),
        status: status.to_string(),
        detail: detail.to_string(),
    }
}
