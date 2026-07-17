use super::*;

fn reject_plan(edit: impl FnOnce(&mut MaterializationPlan)) {
    let mut plan = sparse_plan();
    edit(&mut plan);
    assert!(plan.validate().is_err());
}

fn reject_record(edit: impl FnOnce(&mut MaterializationRecord)) {
    let mut record = sparse_record();
    edit(&mut record);
    assert!(record.validate().is_err());
}

fn reject_intent(edit: impl FnOnce(&mut MaterializationIntent)) {
    let mut intent = expansion_intent();
    edit(&mut intent);
    assert!(intent.validate().is_err());
}

#[test]
fn contradictory_plans_are_rejected() {
    reject_plan(|plan| plan.schema_version = 0);
    reject_plan(|plan| plan.base_oid.clear());
    reject_plan(|plan| plan.planned_at = 0);
    reject_plan(|plan| plan.requested_scopes.push("crate:a".into()));
    reject_plan(|plan| plan.closure_packages.push("a".into()));
    reject_plan(|plan| plan.closure_cones.push("crates/a".into()));
    reject_plan(|plan| plan.support_cones.push("crates/b/src".into()));
    reject_plan(|plan| plan.selected_tracked_files = 4);
    reject_plan(|plan| plan.selected_git_blob_bytes = 31);
    reject_plan(|plan| plan.selected_working_files = 4);
    reject_plan(|plan| plan.selected_working_logical_bytes = 31);
    reject_plan(|plan| plan.full_working_files = 4);
    reject_plan(|plan| plan.selected_working_files = 3);
    reject_plan(|plan| {
        plan.full_tracked_files = 0;
        plan.full_git_blob_bytes = 1;
    });
    reject_plan(|plan| {
        plan.selected_tracked_files = 0;
        plan.selected_git_blob_bytes = 1;
    });
    reject_plan(|plan| {
        plan.full_working_files = 0;
        plan.full_working_logical_bytes = 1;
    });
    reject_plan(|plan| {
        plan.selected_working_files = 0;
        plan.selected_working_logical_bytes = 1;
    });
    reject_plan(|plan| plan.cargo_fingerprint = Some(String::new()));
    reject_plan(|plan| {
        plan.closure_cones.clear();
        plan.support_cones.clear();
    });
    reject_plan(|plan| plan.requested_scopes.clear());
    reject_plan(|plan| plan.mode = MaterializationMode::Full);
    reject_plan(|plan| {
        plan.mode = MaterializationMode::Full;
        plan.fallback_reason = Some(FallbackReason::NoReduction);
        plan.selected_tracked_files = plan.full_tracked_files;
    });
    reject_plan(|plan| {
        plan.mode = MaterializationMode::Full;
        plan.fallback_reason = Some(FallbackReason::NoReduction);
        plan.selected_git_blob_bytes = plan.full_git_blob_bytes;
    });
    reject_plan(|plan| {
        plan.mode = MaterializationMode::Full;
        plan.fallback_reason = Some(FallbackReason::NoReduction);
        plan.selected_working_files = plan.full_working_files;
    });
    reject_plan(|plan| {
        plan.mode = MaterializationMode::Full;
        plan.fallback_reason = Some(FallbackReason::NoReduction);
        plan.selected_working_logical_bytes = plan.full_working_logical_bytes;
    });
    reject_plan(|plan| {
        plan.selected_tracked_files = plan.full_tracked_files;
        plan.selected_git_blob_bytes = plan.full_git_blob_bytes;
        plan.selected_working_files = plan.full_working_files;
        plan.selected_working_logical_bytes = plan.full_working_logical_bytes;
    });
}

#[test]
fn contradictory_records_are_rejected() {
    reject_record(|record| record.schema_version = 0);
    reject_record(|record| record.base_oid.clear());
    reject_record(|record| record.materialized_at = 0);
    reject_record(|record| record.requested_scopes.push("crate:a".into()));
    reject_record(|record| record.closure_cones.push("crates/a".into()));
    reject_record(|record| record.support_cones.push("crates/b/src".into()));
    reject_record(|record| record.current_cones.push("crates/b/src".into()));
    reject_record(|record| record.fallback_reason = Some(FallbackReason::RecoveryFull));
    reject_record(|record| record.candidate_cargo_fingerprint = Some("different".into()));
    reject_record(|record| record.source_cargo_fingerprint = Some(String::new()));
    reject_record(|record| record.candidate_cargo_fingerprint = Some(String::new()));
    reject_record(|record| {
        record.source_cargo_fingerprint = Some(String::new());
        record.candidate_cargo_fingerprint = Some(String::new());
    });
    reject_record(|record| record.current_cones.clear());
    reject_record(|record| record.current_cones = vec!["crates/b/src".into()]);
    reject_record(|record| record.current_cones = vec!["crates/a".into()]);
    reject_record(|record| record.selected_tracked_files = 4);
    reject_record(|record| record.selected_git_blob_bytes = 31);
    reject_record(|record| {
        record.full_tracked_files = 0;
        record.full_git_blob_bytes = 1;
    });
    reject_record(|record| {
        record.selected_tracked_files = 0;
        record.selected_git_blob_bytes = 1;
    });
    reject_record(|record| {
        record.working_files = 0;
        record.working_logical_bytes = 1;
    });
    reject_record(|record| record.last_expanded_at = Some(2));
    reject_record(|record| record.expansion_count = 1);
    reject_record(|record| {
        record.mode = MaterializationMode::Full;
        record.current_cones.clear();
        record.fallback_reason = Some(FallbackReason::MetadataMismatch);
    });
    reject_record(|record| {
        record.mode = MaterializationMode::Full;
        record.current_cones.clear();
        record.fallback_reason = Some(FallbackReason::MetadataFailed);
    });
    reject_record(|record| {
        record.mode = MaterializationMode::Full;
        record.current_cones.clear();
        record.source_cargo_fingerprint = Some(String::new());
        record.candidate_cargo_fingerprint = None;
        record.fallback_reason = Some(FallbackReason::MetadataFailed);
    });
    reject_record(|record| {
        record.mode = MaterializationMode::Full;
        record.current_cones.clear();
        record.fallback_reason = Some(FallbackReason::NoReduction);
        record.selected_tracked_files = record.full_tracked_files;
    });
    reject_record(|record| {
        record.mode = MaterializationMode::Full;
        record.current_cones.clear();
        record.fallback_reason = Some(FallbackReason::NoReduction);
        record.selected_git_blob_bytes = record.full_git_blob_bytes;
    });
    reject_record(|record| {
        record.mode = MaterializationMode::Full;
        record.fallback_reason = None;
    });
    assert!(empty_record().validate().is_err());
}

#[test]
fn contradictory_intents_are_rejected() {
    reject_intent(|intent| intent.prior.base_oid.clear());
    reject_intent(|intent| intent.schema_version = 0);
    reject_intent(|intent| intent.repo.clear());
    reject_intent(|intent| intent.workspace.clear());
    reject_intent(|intent| intent.branch.clear());
    reject_intent(|intent| intent.base_oid.clear());
    reject_intent(|intent| intent.created_at = 0);
    reject_intent(|intent| intent.base_oid = "different".into());
    reject_intent(|intent| intent.current_cones = vec!["different".into()]);
    reject_intent(|intent| intent.requested_scopes.push("crate:b".into()));
    reject_intent(|intent| intent.closure_cones.push("crates/a".into()));
    reject_intent(|intent| intent.support_cones = vec!["x".into(), "x".into()]);
    reject_intent(|intent| intent.current_cones.push("crates/b/src".into()));
    reject_intent(|intent| intent.desired_cones.push("crates/b/src".into()));
    reject_intent(|intent| intent.requested_scopes.clear());
    reject_intent(|intent| intent.desired_cones = intent.current_cones.clone());
    reject_intent(|intent| intent.support_cones = vec!["crates/c".into()]);
    reject_intent(|intent| {
        intent.prior.mode = MaterializationMode::Full;
        intent.prior.current_cones.clear();
        intent.current_cones.clear();
    });
    reject_intent(|intent| {
        intent.desired_mode = MaterializationMode::Full;
        intent.desired_cones = vec!["crates/a".into()];
    });
    assert!(MaterializationPlan::default().validate().is_err());
}

#[test]
fn noncanonical_cones_are_rejected() {
    for cone in ["../escape", "/absolute", "."] {
        reject_plan(|plan| plan.closure_cones = vec![cone.into()]);
        reject_record(|record| {
            record.closure_cones = vec![cone.into()];
            record.support_cones.clear();
            record.current_cones = vec![cone.into()];
        });
        reject_intent(|intent| intent.closure_cones = vec![cone.into()]);
    }
}
