//! Why Cargo rebuilt a unit instead of reusing it.
//!
//! Cargo already knows the answer and will say so under
//! `CARGO_LOG=cargo::core::compiler::fingerprint=info`, but the output is a
//! debug log: one line per probe, interleaved across units, carrying Rust debug
//! formatting and absolute paths. Every seeding regression Grove has shipped was
//! diagnosable from it and none were diagnosed quickly, so the parsing lives
//! here rather than in a shell pipeline someone has to remember.

use serde::Serialize;

/// One unit Cargo decided to rebuild, with the reason it gave.
#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct DirtyUnit {
    pub package: String,
    /// Best-effort unit kind ("check", "build script", "test", "doc"). Cargo
    /// prints a debug-formatted unit; only the discriminant is worth surfacing.
    pub kind: String,
    /// Cargo's own `DirtyReason`, verbatim, for when the summary is not enough.
    pub reason: String,
    /// Plain-language reading of `reason`.
    pub explanation: String,
    /// The input that went stale, when Cargo named one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed: Option<String>,
}

/// Parse the fingerprint log emitted by one Cargo invocation.
///
/// Cargo emits, per unit, an optional `stale: changed "<path>"` line, then
/// `fingerprint dirty for <package> v<version> (<path>)/<Kind>/...`, then
/// `dirty: <DirtyReason>`. The middle line is the anchor; the others are
/// attached when present and skipped when Cargo did not print them.
pub fn parse(log: &str) -> Vec<DirtyUnit> {
    let mut units = Vec::new();
    let mut pending_changed: Option<String> = None;
    let mut open: Option<DirtyUnit> = None;
    for line in log.lines() {
        if let Some(path) = after(line, "stale: changed ") {
            pending_changed = Some(unquote(path.trim()));
            continue;
        }
        if let Some(unit) = after(line, "fingerprint dirty for ") {
            // A unit with no `dirty:` line still rebuilt; do not drop it.
            if let Some(previous) = open.take() {
                units.push(previous);
            }
            open = Some(DirtyUnit {
                package: package_of(unit),
                kind: kind_of(unit),
                reason: String::new(),
                explanation: "Cargo gave no reason".to_string(),
                changed: pending_changed.take(),
            });
            continue;
        }
        if let Some(reason) = after(line, "dirty: ")
            && let Some(mut unit) = open.take()
        {
            let reason = reason.trim().to_string();
            unit.explanation = explain(&reason, unit.changed.as_deref());
            unit.reason = reason;
            units.push(unit);
        }
    }
    if let Some(previous) = open {
        units.push(previous);
    }
    units
}

/// The text following `marker`, when the line contains it. Cargo prefixes every
/// line with a timestamp and span, so anchoring on the marker beats splitting.
fn after<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    line.find(marker).map(|at| &line[at + marker.len()..])
}

/// Cargo prints the path Debug-formatted, so it arrives quoted and with its
/// escapes intact. On Windows that means every separator is a doubled
/// backslash, and reporting `C:\\src\\lib.rs` at someone would be worse than
/// useless when the whole point is naming the file that changed.
fn unquote(value: &str) -> String {
    let trimmed = value
        .strip_prefix('"')
        .unwrap_or(value)
        .trim_end_matches('"');
    let mut out = String::with_capacity(trimmed.len());
    let mut chars = trimmed.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            // Anything else was not an escape Cargo produced; keep it verbatim
            // rather than silently eating a character out of a path.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// `wr v0.1.0 (/path)/Check { .. }` names package `wr`. The version marker is
/// the only reliable delimiter: package names and paths both contain slashes
/// and parentheses on some platforms.
fn package_of(unit: &str) -> String {
    match unit.find(" v") {
        Some(at) => unit[..at].to_string(),
        None => unit.split_whitespace().next().unwrap_or(unit).to_string(),
    }
}

fn kind_of(unit: &str) -> String {
    for (marker, label) in [
        ("custom_build", "build script"),
        ("/Build", "build"),
        ("/Check", "check"),
        ("/Test", "test"),
        ("/Doc", "doc"),
        ("/Bench", "bench"),
    ] {
        if unit.contains(marker) {
            return label.to_string();
        }
    }
    "unit".to_string()
}

/// Translate Cargo's `DirtyReason` into something that names the fix. The
/// variants matter to Grove specifically: a seeded lane that reports
/// `MissingFile` lost outputs during seeding, and one that reports
/// `ChangedEnv`/`RustflagsChanged` has a policy or environment mismatch between
/// the canonical and the lane rather than a real source change.
fn explain(reason: &str, changed: Option<&str>) -> String {
    let named = |what: &str| match changed {
        Some(path) => format!("{what}: {path}"),
        None => what.to_string(),
    };
    if reason.contains("MissingFile") {
        return named("an expected output was missing, so the unit could not be reused");
    }
    if reason.contains("ChangedFile") || reason.contains("StaleItem") {
        return named("an input file changed");
    }
    if reason.contains("StaleDependency") || reason.contains("UnitDependency") {
        return "a dependency was rebuilt, so this rebuilt with it".to_string();
    }
    if reason.contains("RustflagsChanged") {
        return "RUSTFLAGS differ from the cached build".to_string();
    }
    if reason.contains("ProfileConfigurationChanged") {
        return "the build profile differs from the cached build".to_string();
    }
    if reason.contains("ChangedEnv") || reason.contains("EnvVarChanged") {
        return "an environment variable the build reads changed".to_string();
    }
    if reason.contains("PrecalculatedComponentsChanged") {
        return "the dependency graph or feature set changed".to_string();
    }
    if reason.contains("FreshBuild") {
        return "nothing was cached for this unit yet".to_string();
    }
    named("changed")
}

/// The environment that makes Cargo explain itself.
pub const LOG_ENV: (&str, &str) = ("CARGO_LOG", "cargo::core::compiler::fingerprint=info");

/// What Cargo reused versus recompiled, from `--message-format=json`.
///
/// This, not the dirty log, is the authoritative count. Cargo emits a
/// `fingerprint dirty for` line only when a *stale* fingerprint exists; a unit
/// with no cached state at all is simply compiled, silently. So a lane that
/// seeded badly enough to have no fingerprints reports zero dirty units while
/// rebuilding everything — the precise failure this is meant to catch.
#[derive(Serialize, Debug, Default, PartialEq, Eq)]
pub struct Freshness {
    pub reused: usize,
    pub rebuilt: usize,
}

impl Freshness {
    pub fn total(&self) -> usize {
        self.reused + self.rebuilt
    }
}

/// Count `compiler-artifact` messages on Cargo's JSON stdout.
pub fn freshness(stdout: &str) -> Freshness {
    let mut counts = Freshness::default();
    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if message.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") {
            continue;
        }
        match message.get("fresh").and_then(|f| f.as_bool()) {
            Some(true) => counts.reused += 1,
            // An artifact message without `fresh` was produced by this run.
            _ => counts.rebuilt += 1,
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured verbatim from `CARGO_LOG=...fingerprint=info cargo check` after
    /// editing a source file, so the parser is pinned to real Cargo output
    /// rather than to a guess about its shape.
    const REAL: &str = r#"
   0.002971000s  INFO prepare_target{force=false package_id=wr v0.1.0 (/tmp/wrtest) target="wr"}: cargo::core::compiler::fingerprint: stale: changed "/tmp/wrtest/src/lib.rs"
   0.002979583s  INFO prepare_target{force=false package_id=wr v0.1.0 (/tmp/wrtest) target="wr"}: cargo::core::compiler::fingerprint:           (vs) "/tmp/wrtest/target/debug/.fingerprint/wr-4f264a12e627eb89/dep-lib-wr"
   0.003028458s  INFO prepare_target{force=false package_id=wr v0.1.0 (/tmp/wrtest) target="wr"}: cargo::core::compiler::fingerprint: fingerprint dirty for wr v0.1.0 (/tmp/wrtest)/Check { test: false }/TargetInner { name_inferred: true, ..: lib_target("wr", ["lib"], "/tmp/wrtest/src/lib.rs", Edition2021) }
   0.003039708s  INFO prepare_target{force=false package_id=wr v0.1.0 (/tmp/wrtest) target="wr"}: cargo::core::compiler::fingerprint:     dirty: FsStatusOutdated(StaleItem(ChangedFile { reference: "/tmp/wrtest/target/debug/.fingerprint/wr-4f264a12e627eb89/dep-lib-wr", stale: "/tmp/wrtest/src/lib.rs" }))
"#;

    #[test]
    fn parses_a_real_changed_file_rebuild() {
        let units = parse(REAL);
        assert_eq!(units.len(), 1, "{units:?}");
        assert_eq!(units[0].package, "wr");
        assert_eq!(units[0].kind, "check");
        assert_eq!(units[0].changed.as_deref(), Some("/tmp/wrtest/src/lib.rs"));
        assert!(units[0].explanation.starts_with("an input file changed"));
        assert!(units[0].reason.contains("ChangedFile"));
    }

    /// The seeding-regression shape: a lane seeded from the canonical whose
    /// outputs did not survive reports a missing file, not a source edit.
    #[test]
    fn a_missing_output_reads_as_a_seeding_failure_not_an_edit() {
        let log = "x: fingerprint dirty for serde v1.0.0 (/reg)/Build\n\
                   x:     dirty: FsStatusOutdated(StaleItem(MissingFile(\"/lane/out\")))";
        let units = parse(log);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].package, "serde");
        assert_eq!(units[0].kind, "build");
        assert!(
            units[0].explanation.contains("expected output was missing"),
            "{}",
            units[0].explanation
        );
    }

    #[test]
    fn a_unit_without_a_reason_line_is_still_reported() {
        let units = parse("a: fingerprint dirty for solo v0.1.0 (/p)/Check { test: false }");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].package, "solo");
        assert_eq!(units[0].explanation, "Cargo gave no reason");
    }

    #[test]
    fn consecutive_units_do_not_bleed_into_each_other() {
        let log = "a: fingerprint dirty for one v0.1.0 (/p)/Check\n\
                   a:     dirty: RustflagsChanged\n\
                   b: stale: changed \"/p/src/two.rs\"\n\
                   b: fingerprint dirty for two v0.2.0 (/p)/Build\n\
                   b:     dirty: FsStatusOutdated(StaleItem(ChangedFile {}))";
        let units = parse(log);
        assert_eq!(units.len(), 2, "{units:?}");
        assert_eq!(units[0].package, "one");
        assert_eq!(units[0].changed, None);
        assert!(units[0].explanation.contains("RUSTFLAGS"));
        assert_eq!(units[1].package, "two");
        assert_eq!(units[1].changed.as_deref(), Some("/p/src/two.rs"));
    }

    #[test]
    fn a_log_with_no_dirty_units_is_empty_not_an_error() {
        assert!(parse("INFO nothing interesting here").is_empty());
    }

    /// The blind spot that made the dirty log alone unsafe: a cold build emits
    /// no dirty lines at all, so only the artifact count reveals the rebuild.
    #[test]
    fn freshness_counts_reused_and_rebuilt_artifacts() {
        let stdout = concat!(
            r#"{"reason":"compiler-artifact","target":{"name":"a"},"fresh":true}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"b"},"fresh":false}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
            "\n",
            "not json at all\n",
        );
        let counts = freshness(stdout);
        assert_eq!(counts.reused, 1);
        assert_eq!(counts.rebuilt, 1);
        assert_eq!(counts.total(), 2);
    }

    #[test]
    fn a_cold_build_reports_rebuilt_even_though_no_unit_was_dirty() {
        let stdout = r#"{"reason":"compiler-artifact","target":{"name":"wr"},"fresh":false}"#;
        assert_eq!(freshness(stdout).rebuilt, 1);
        // Cargo logs nothing dirty when there was no prior fingerprint.
        assert!(parse("").is_empty());
    }

    #[test]
    fn freshness_of_an_empty_or_noisy_stream_is_zero() {
        assert_eq!(freshness(""), Freshness::default());
        assert_eq!(freshness("warning: something\n{}"), Freshness::default());
    }

    /// The Windows form: cargo Debug-formats the path, so every separator
    /// arrives as an escaped pair. Reporting it raw would name a file that
    /// does not exist.
    #[test]
    fn a_windows_path_is_unescaped_to_its_real_form() {
        let log = concat!(
            r#"x: stale: changed "C:\\Users\\me\\repo\\src\\lib.rs""#,
            "\n",
            r#"x: fingerprint dirty for whyrb v0.1.0 (C:\\Users\\me\\repo)/Check { test: false }"#,
            "\n",
            r#"x:     dirty: FsStatusOutdated(StaleItem(ChangedFile {}))"#,
        );
        let units = parse(log);
        assert_eq!(units.len(), 1, "{units:?}");
        assert_eq!(
            units[0].changed.as_deref(),
            Some(r"C:\Users\me\repo\src\lib.rs")
        );
    }
}
