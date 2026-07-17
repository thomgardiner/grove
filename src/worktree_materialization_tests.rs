use super::*;
use crate::config::Config;

fn sparse_plan() -> MaterializationPlan {
    MaterializationPlan {
        schema_version: PLAN_SCHEMA_VERSION,
        mode: MaterializationMode::Sparse,
        requested_scopes: vec!["crate:a".into()],
        closure_packages: vec!["a".into()],
        closure_cones: vec!["crates/a".into()],
        support_cones: vec!["crates/b/src".into()],
        base_oid: "base".into(),
        cargo_fingerprint: Some("cargo".into()),
        full_tracked_files: 3,
        full_git_blob_bytes: 30,
        full_working_files: 3,
        full_working_logical_bytes: 30,
        selected_tracked_files: 2,
        selected_git_blob_bytes: 20,
        selected_working_files: 2,
        selected_working_logical_bytes: 20,
        fallback_reason: None,
        planned_at: 1,
    }
}

fn sparse_record() -> MaterializationRecord {
    MaterializationRecord {
        schema_version: SCHEMA_VERSION,
        mode: MaterializationMode::Sparse,
        requested_scopes: vec!["crate:a".into()],
        closure_cones: vec!["crates/a".into()],
        support_cones: vec!["crates/b/src".into()],
        current_cones: vec!["crates/a".into(), "crates/b/src".into()],
        base_oid: "base".into(),
        source_cargo_fingerprint: Some("cargo".into()),
        candidate_cargo_fingerprint: Some("cargo".into()),
        full_tracked_files: 3,
        full_git_blob_bytes: 30,
        full_working_files: 3,
        full_working_logical_bytes: 30,
        selected_tracked_files: 2,
        selected_git_blob_bytes: 20,
        working_files: 2,
        working_logical_bytes: 20,
        materialization_duration_ms: 1,
        materialized_at: 1,
        expansion_count: 0,
        last_expanded_at: None,
        fallback_reason: None,
    }
}

fn empty_record() -> MaterializationRecord {
    MaterializationRecord {
        schema_version: SCHEMA_VERSION,
        mode: MaterializationMode::Full,
        requested_scopes: Vec::new(),
        closure_cones: Vec::new(),
        support_cones: Vec::new(),
        current_cones: Vec::new(),
        base_oid: String::new(),
        source_cargo_fingerprint: None,
        candidate_cargo_fingerprint: None,
        full_tracked_files: 0,
        full_git_blob_bytes: 0,
        full_working_files: 0,
        full_working_logical_bytes: 0,
        selected_tracked_files: 0,
        selected_git_blob_bytes: 0,
        working_files: 0,
        working_logical_bytes: 0,
        materialization_duration_ms: 0,
        materialized_at: 0,
        expansion_count: 0,
        last_expanded_at: None,
        fallback_reason: None,
    }
}

fn expansion_intent() -> MaterializationIntent {
    let prior = sparse_record();
    MaterializationIntent {
        schema_version: SCHEMA_VERSION,
        repo: "repo-id".into(),
        workspace: "/worktree".into(),
        cargo_dir: ".".into(),
        branch: "grove/agent".into(),
        base_oid: prior.base_oid.clone(),
        requested_scopes: vec!["crate:a".into(), "crate:b".into()],
        closure_cones: vec!["crates/a".into(), "crates/b".into()],
        support_cones: Vec::new(),
        current_cones: prior.current_cones.clone(),
        desired_cones: vec!["crates/a".into(), "crates/b".into(), "crates/b/src".into()],
        plan: Some(MaterializationPlan {
            requested_scopes: vec!["crate:a".into(), "crate:b".into()],
            closure_packages: vec!["a".into(), "b".into()],
            closure_cones: vec!["crates/a".into(), "crates/b".into()],
            support_cones: Vec::new(),
            ..sparse_plan()
        }),
        prior,
        desired_mode: MaterializationMode::Sparse,
        created_at: 2,
    }
}

#[test]
fn repository_materialization_paths_are_normalized_and_deduplicated() {
    let config: Config = toml::from_str(
        "[worktree]\nmaterialize = [\"schemas\\\\generated\", \"./proto\", \"proto\"]\n",
    )
    .unwrap();
    assert_eq!(
        config.materialize().unwrap(),
        ["proto", "schemas/generated"]
    );
    assert!(Config::default().materialize().unwrap().is_empty());
}

#[test]
fn repository_materialization_paths_reject_absolute_and_escaping_values() {
    for scope in [
        "/absolute",
        "../escape",
        "nested/../../escape",
        "C:\\absolute",
    ] {
        let config: Config =
            toml::from_str(&format!("[worktree]\nmaterialize = [{scope:?}]\n")).unwrap();
        assert!(config.materialize().is_err(), "accepted {scope:?}");
    }
}

#[test]
fn fallback_and_mode_serialization_are_stable() {
    let reasons = [
        FallbackReason::RootScope,
        FallbackReason::NoReduction,
        FallbackReason::GitUnsupported,
        FallbackReason::SparseSetupFailed,
        FallbackReason::MetadataFailed,
        FallbackReason::MetadataMismatch,
        FallbackReason::RecoveryFull,
    ];
    assert_eq!(
        serde_json::to_value(reasons).unwrap(),
        serde_json::json!([
            "root_scope",
            "no_reduction",
            "git_unsupported",
            "sparse_setup_failed",
            "metadata_failed",
            "metadata_mismatch",
            "recovery_full"
        ])
    );
    assert_eq!(
        serde_json::to_value(MaterializationMode::Full).unwrap(),
        "full"
    );
    assert_eq!(
        serde_json::to_value(MaterializationMode::Sparse).unwrap(),
        "sparse"
    );
}

#[test]
fn legacy_materialization_is_full_without_a_fallback_reason() {
    #[derive(serde::Deserialize)]
    struct LegacyLease {
        #[serde(default)]
        materialization: Option<MaterializationRecord>,
    }
    let lease: LegacyLease = serde_json::from_str("{}").unwrap();
    assert_eq!(
        lease
            .materialization
            .as_ref()
            .map_or(MaterializationMode::Full, |record| record.mode),
        MaterializationMode::Full
    );

    let encoded = serde_json::to_value(empty_record()).unwrap();
    let explicit: MaterializationRecord = serde_json::from_value(encoded).unwrap();
    assert!(explicit.validate().is_err());
}

#[test]
fn malformed_and_future_records_fail_closed() {
    assert!(serde_json::from_str::<MaterializationRecord>("{}").is_err());
    assert!(
        serde_json::from_str::<MaterializationRecord>(r#"{"schema_version":1,"mode":"sparse"}"#)
            .is_err()
    );
    for mut value in [
        serde_json::to_value(sparse_plan()).unwrap(),
        serde_json::to_value(sparse_record()).unwrap(),
        serde_json::to_value(expansion_intent()).unwrap(),
    ] {
        value["schema_version"] = 999.into();
        assert!(schema_value(value).is_err());
    }
    let mut old_plan = serde_json::to_value(sparse_plan()).unwrap();
    old_plan["schema_version"] = SCHEMA_VERSION.into();
    assert!(serde_json::from_value::<MaterializationPlan>(old_plan).is_err());
}

fn schema_value(value: serde_json::Value) -> anyhow::Result<()> {
    if value.get("repo").is_some() {
        serde_json::from_value::<MaterializationIntent>(value)?;
    } else if value.get("working_files").is_some() {
        serde_json::from_value::<MaterializationRecord>(value)?;
    } else {
        serde_json::from_value::<MaterializationPlan>(value)?;
    }
    Ok(())
}

#[test]
fn valid_sparse_expansion_and_full_conversion_validate() {
    sparse_plan().validate().unwrap();
    sparse_record().validate().unwrap();
    let mut closure_only = sparse_plan();
    closure_only.support_cones.clear();
    closure_only.validate().unwrap();
    let mut file_reduction = sparse_plan();
    file_reduction.selected_git_blob_bytes = file_reduction.full_git_blob_bytes;
    file_reduction.validate().unwrap();
    let mut byte_reduction = sparse_plan();
    byte_reduction.selected_tracked_files = byte_reduction.full_tracked_files;
    byte_reduction.validate().unwrap();
    let mut full_plan = sparse_plan();
    full_plan.mode = MaterializationMode::Full;
    full_plan.fallback_reason = Some(FallbackReason::NoReduction);
    full_plan.selected_tracked_files = full_plan.full_tracked_files;
    full_plan.selected_git_blob_bytes = full_plan.full_git_blob_bytes;
    full_plan.selected_working_files = full_plan.full_working_files;
    full_plan.selected_working_logical_bytes = full_plan.full_working_logical_bytes;
    full_plan.validate().unwrap();
    let mut full_record = sparse_record();
    full_record.mode = MaterializationMode::Full;
    full_record.current_cones.clear();
    full_record.working_files = full_record.full_working_files;
    full_record.working_logical_bytes = full_record.full_working_logical_bytes;
    full_record.fallback_reason = None;
    full_record.validate().unwrap();

    let mut ordinary_full = full_record.clone();
    ordinary_full.source_cargo_fingerprint = None;
    ordinary_full.candidate_cargo_fingerprint = None;
    ordinary_full.validate().unwrap();

    let mut no_reduction = full_record.clone();
    no_reduction.fallback_reason = Some(FallbackReason::NoReduction);
    no_reduction.selected_tracked_files = no_reduction.full_tracked_files;
    no_reduction.selected_git_blob_bytes = no_reduction.full_git_blob_bytes;
    no_reduction.validate().unwrap();

    let mut covered = sparse_record();
    covered.current_cones = vec!["crates".into()];
    covered.validate().unwrap();

    let mut covered_intent = expansion_intent();
    covered_intent.prior = covered;
    covered_intent.current_cones = vec!["crates".into()];
    covered_intent.desired_cones = vec!["crates".into()];
    covered_intent.validate().unwrap();

    let mut mismatch = full_record.clone();
    mismatch.fallback_reason = Some(FallbackReason::MetadataMismatch);
    mismatch.candidate_cargo_fingerprint = Some("different".into());
    mismatch.validate().unwrap();

    let mut failed = full_record;
    failed.fallback_reason = Some(FallbackReason::MetadataFailed);
    failed.candidate_cargo_fingerprint = None;
    failed.validate().unwrap();
    let intent = expansion_intent();
    intent.validate().unwrap();
    let roundtrip: MaterializationIntent =
        serde_json::from_value(serde_json::to_value(&intent).unwrap()).unwrap();
    assert_eq!(roundtrip, intent);
    let mut full = intent;
    full.desired_mode = MaterializationMode::Full;
    full.desired_cones.clear();
    full.plan = None;
    full.validate().unwrap();
}

#[path = "worktree_materialization_validation_tests.rs"]
mod validation;

#[test]
fn records_have_no_claim_coupling() {
    for value in [
        serde_json::to_value(sparse_record()).unwrap(),
        serde_json::to_value(sparse_plan()).unwrap(),
        serde_json::to_value(expansion_intent()).unwrap(),
    ] {
        let object = value.as_object().unwrap();
        assert!(!object.contains_key("claim_id"));
        assert!(!object.contains_key("resolved_scope"));
        assert!(!object.contains_key("overlap"));
    }
}
