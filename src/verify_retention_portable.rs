use std::collections::BTreeMap;
use std::path::Path;

use super::{Receipt, Run, RunIds, StoredRun, json_files, parse, repositories};

const HEADS_PER_PROFILE: usize = 8;

/// Keep a small history of successful clean heads per profile so another clean checkout
/// can reuse recent evidence without retaining every historical run.
pub(super) fn runs(root: &Path, runs: &[StoredRun]) -> RunIds {
    let completed = completed_runs(runs);
    let mut heads = BTreeMap::<
        (
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            String,
        ),
        BTreeMap<String, (u128, String)>,
    >::new();
    for (slug, dir) in repositories(root, "receipts") {
        for path in json_files(&dir) {
            let Some(receipt) = parse::<Receipt>(&path) else {
                continue;
            };
            let Some(run) = completed.get(&(slug.clone(), receipt.run_id.clone())) else {
                continue;
            };
            let Some(inputs) = inputs(&receipt, run) else {
                continue;
            };
            let key = (
                slug.clone(),
                inputs.repository_sha256.clone(),
                receipt.profile.clone(),
                receipt.profile_sha256.clone(),
                inputs.toolchain.clone(),
                inputs.rustc_sha256.clone(),
                inputs.cargo_sha256.clone(),
                inputs.command_toolchains_sha256.clone(),
                inputs.environment_sha256.clone(),
            );
            let candidate = (run.completed_at_nanos, receipt.run_id.clone());
            let current = heads
                .entry(key)
                .or_default()
                .entry(inputs.head.clone())
                .or_insert(candidate.clone());
            if candidate > *current {
                *current = candidate;
            }
        }
    }
    heads
        .into_iter()
        .flat_map(|((slug, _, _, _, _, _, _, _, _), heads)| {
            let mut runs = heads.into_values().collect::<Vec<_>>();
            runs.sort_unstable_by(|left, right| right.cmp(left));
            runs.into_iter()
                .take(HEADS_PER_PROFILE)
                .map(move |(_, run)| (slug.clone(), run))
        })
        .collect()
}

fn completed_runs(runs: &[StoredRun]) -> BTreeMap<(String, String), &Run> {
    runs.iter()
        .map(|stored| {
            (
                (stored.slug.clone(), stored.run.run_id.clone()),
                &stored.run,
            )
        })
        .collect()
}

fn inputs<'a>(
    receipt: &'a Receipt,
    run: &Run,
) -> Option<&'a super::super::portable::PortableInputs> {
    let evidence = receipt.evidence.as_ref()?;
    let inputs = evidence.portable.as_ref()?;
    (run.schema_version == 1
        && run.passed
        && receipt.schema_version == super::super::receipt::SCHEMA_VERSION
        && inputs.schema_version == super::super::portable::SCHEMA_VERSION
        && run.repository == receipt.repository
        && run.profile == receipt.profile
        && run.profile_sha256 == receipt.profile_sha256
        && run.task_id == receipt.task_id
        && evidence.checkout.changed_paths.is_empty()
        && evidence.checkout.head.as_deref() == Some(inputs.head.as_str()))
    .then_some(inputs)
}
