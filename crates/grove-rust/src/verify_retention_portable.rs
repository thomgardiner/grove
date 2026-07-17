use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use grove_core::verification::{Receipt, Run};

const HEADS_PER_PROFILE: usize = 8;

pub(super) fn runs(root: &Path) -> BTreeSet<(String, String)> {
    let completed = grove_core::verification::all_runs(root)
        .into_iter()
        .map(|stored| ((stored.slug, stored.run.run_id.clone()), stored.run))
        .collect::<BTreeMap<_, _>>();
    let mut heads = BTreeMap::new();
    for stored in grove_core::verification::all_receipts(root) {
        let Some(run) = completed.get(&(stored.slug.clone(), stored.receipt.run_id.clone())) else {
            continue;
        };
        let Some(inputs) = inputs(&stored.receipt, run) else {
            continue;
        };
        let key = (
            stored.slug.clone(),
            inputs.repository_sha256,
            stored.receipt.profile.clone(),
            stored.receipt.profile_sha256.clone(),
            inputs.toolchain,
            inputs.rustc_sha256,
            inputs.cargo_sha256,
            inputs.command_toolchains_sha256,
            inputs.environment_sha256,
        );
        let candidate = (run.completed_at_nanos, stored.receipt.run_id.clone());
        let current = heads
            .entry(key)
            .or_insert_with(BTreeMap::new)
            .entry(inputs.head)
            .or_insert(candidate.clone());
        if candidate > *current {
            *current = candidate;
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

fn inputs(receipt: &Receipt, run: &Run) -> Option<super::super::portable::PortableInputs> {
    let evidence = receipt.evidence.as_ref()?;
    let inputs: super::super::portable::PortableInputs =
        serde_json::from_value(evidence.portable.as_ref()?.clone()).ok()?;
    (run.schema_version == 1
        && run.passed
        && receipt.schema_version == grove_core::verification::RECEIPT_SCHEMA_VERSION
        && inputs.schema_version == super::super::portable::SCHEMA_VERSION
        && run.repository == receipt.repository
        && run.profile == receipt.profile
        && run.profile_sha256 == receipt.profile_sha256
        && run.task_id == receipt.task_id
        && evidence.checkout.changed_paths.is_empty()
        && evidence.checkout.head.as_deref() == Some(inputs.head.as_str()))
    .then_some(inputs)
}
