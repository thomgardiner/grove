//! Cross-checkout lookup for portable verification evidence.

use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

use crate::{cache, config, snapshot};

use super::{PortableInputs, Receipt, dag, evidence_lock, portable, profile, receipt};

/// Machine-readable result of looking up successful clean-checkout receipts from any
/// local cache bucket. A clean miss is intentional: deployment callers can fall back
/// to their usual gate without treating absence of cached evidence as an error.
#[derive(Serialize)]
pub struct PortableQueryReport {
    pub schema_version: u32,
    pub profile: String,
    pub eligible: bool,
    pub matched: bool,
    pub matches: Vec<PortableMatch>,
}

#[derive(Serialize)]
pub struct PortableMatch {
    pub run_id: String,
    pub head: String,
    pub profile_sha256: String,
    pub completed_at_nanos: u128,
    pub receipts: Vec<PortableReceipt>,
}

#[derive(Serialize)]
pub struct PortableReceipt {
    pub command_index: usize,
    pub argv: Vec<String>,
    pub started_at: u64,
    pub ended_at: u64,
}

/// Find reusable evidence for this exact clean checkout. Local workspace, branch,
/// task, and agent identity are diagnostic fields only; portable inputs, command
/// configuration, and content-addressed snapshots are the proof boundary.
pub(super) fn run(root: &Path, workspace: &Path, name: &str) -> Result<PortableQueryReport> {
    let workspace = cache::canonical_path(workspace);
    let _workspace_lock = snapshot::workspace_lock(root, &workspace)?;
    let _evidence_lock = evidence_lock(root)?;
    let (configured, _) = profile(name)?;
    let Some(inputs) = portable::capture(&workspace, &configured)? else {
        return Ok(miss(name));
    };
    let current = snapshot::capture(&workspace)?;
    if current.head()? != inputs.head {
        return Ok(miss(name));
    }
    let expected = current.reference();
    let expected_sha256 = dag::profile_sha256(&configured);
    let receipts = receipt::all_receipts(root);
    let runs = receipt::all_runs(root);
    let query = Query {
        root,
        name,
        profile: &configured,
        profile_sha256: &expected_sha256,
        inputs: &inputs,
        expected: &expected,
        receipts: &receipts,
    };
    let mut matches = Vec::new();
    for stored in &runs {
        if runs
            .iter()
            .filter(|other| other.slug == stored.slug && other.run.run_id == stored.run.run_id)
            .count()
            != 1
        {
            continue;
        }
        if let Some(found) = matching_run(&query, stored) {
            matches.push(found);
        }
    }
    matches.sort_by(|left, right| {
        (right.completed_at_nanos, &right.run_id).cmp(&(left.completed_at_nanos, &left.run_id))
    });
    Ok(PortableQueryReport {
        schema_version: 1,
        profile: name.to_string(),
        eligible: true,
        matched: !matches.is_empty(),
        matches,
    })
}

struct Query<'a> {
    root: &'a Path,
    name: &'a str,
    profile: &'a config::VerificationProfile,
    profile_sha256: &'a str,
    inputs: &'a PortableInputs,
    expected: &'a snapshot::Ref,
    receipts: &'a [receipt::StoredReceipt],
}

fn miss(name: &str) -> PortableQueryReport {
    PortableQueryReport {
        schema_version: 1,
        profile: name.to_string(),
        eligible: false,
        matched: false,
        matches: Vec::new(),
    }
}

fn matching_run(query: &Query<'_>, stored: &receipt::StoredRun) -> Option<PortableMatch> {
    let run = &stored.run;
    valid_run(query, stored)?;
    let by_index = index_receipts(query, stored)?;
    let receipts = receipt_output(query, &by_index)?;
    Some(PortableMatch {
        run_id: run.run_id.clone(),
        head: query.inputs.head.clone(),
        profile_sha256: query.profile_sha256.to_string(),
        completed_at_nanos: run.completed_at_nanos,
        receipts,
    })
}

fn valid_run(query: &Query<'_>, stored: &receipt::StoredRun) -> Option<()> {
    let run = &stored.run;
    if run.schema_version != 1
        || !run.passed
        || run.profile != query.name
        || run.profile_sha256 != query.profile_sha256
        || run.command_count != query.profile.commands.len()
        || run.receipt_count != query.profile.commands.len()
        || stored.slug != cache::repo_slug(&run.repository)
    {
        None
    } else {
        Some(())
    }
}

fn index_receipts<'a>(
    query: &'a Query<'a>,
    stored: &receipt::StoredRun,
) -> Option<BTreeMap<usize, &'a Receipt>> {
    let related = query
        .receipts
        .iter()
        .filter(|stored_receipt| {
            stored_receipt.slug == stored.slug && stored_receipt.receipt.run_id == stored.run.run_id
        })
        .collect::<Vec<_>>();
    if related.len() != stored.run.receipt_count {
        return None;
    }
    let mut by_index = BTreeMap::new();
    for stored_receipt in related {
        let receipt = &stored_receipt.receipt;
        if !valid_receipt(query, &stored.run, receipt)
            || by_index.insert(receipt.command_index, receipt).is_some()
        {
            return None;
        }
    }
    Some(by_index)
}

fn valid_receipt(query: &Query<'_>, run: &receipt::Run, receipt: &Receipt) -> bool {
    let Some(evidence) = receipt.evidence.as_ref() else {
        return false;
    };
    receipt.schema_version == receipt::SCHEMA_VERSION
        && receipt.repository == run.repository
        && receipt.task_id == run.task_id
        && receipt.profile == query.name
        && receipt.profile_sha256 == query.profile_sha256
        && receipt.passed
        && evidence.checkout.changed_paths.is_empty()
        && evidence.checkout.head.as_deref() == Some(query.inputs.head.as_str())
        && evidence.portable.as_ref() == Some(query.inputs)
        && snapshot_matches(query, &run.repository, &evidence.input)
        && snapshot_matches(query, &run.repository, &evidence.output)
}

fn snapshot_matches(query: &Query<'_>, repository: &str, reference: &snapshot::Ref) -> bool {
    snapshot::validate(query.root, repository, reference)
        .map(|snapshot| snapshot.reference() == *query.expected)
        .unwrap_or(false)
}

fn receipt_output(
    query: &Query<'_>,
    by_index: &BTreeMap<usize, &Receipt>,
) -> Option<Vec<PortableReceipt>> {
    query
        .profile
        .commands
        .iter()
        .enumerate()
        .map(|(index, command)| {
            let receipt = by_index.get(&index)?;
            (receipt.argv == command.argv).then(|| PortableReceipt {
                command_index: index,
                argv: receipt.argv.clone(),
                started_at: receipt.started_at,
                ended_at: receipt.ended_at,
            })
        })
        .collect()
}
