use anyhow::{Result as AnyResult, bail};
use serde::{Deserialize, Deserializer, Serialize, de};

use crate::claim::claim_scope::normalize_scope;

#[path = "materialization_cargo.rs"]
mod materialization_cargo;
#[path = "materialization_git.rs"]
mod materialization_git;
#[path = "materialization_plan.rs"]
mod materialization_plan;
pub(super) use materialization_cargo::{Fingerprint, capture, equivalent};
pub(super) use materialization_git::{
    Add, Failure, add, cones as actual_cones, full, head, sparse,
};
pub(super) use materialization_plan::{PlanInput, expand, measure, plan};

pub(super) fn fingerprint(fingerprint: &Fingerprint) -> &str {
    &fingerprint.hash
}

pub(super) const SCHEMA_VERSION: u32 = 1;
pub(super) const PLAN_SCHEMA_VERSION: u32 = 2;

fn schema<'de, D>(deserializer: D) -> std::result::Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    let version = u32::deserialize(deserializer)?;
    if version != SCHEMA_VERSION {
        return Err(de::Error::custom(format!(
            "unsupported materialization schema {version}"
        )));
    }
    Ok(version)
}

fn plan_schema<'de, D>(deserializer: D) -> std::result::Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    let version = u32::deserialize(deserializer)?;
    if version != PLAN_SCHEMA_VERSION {
        return Err(de::Error::custom(format!(
            "unsupported materialization plan schema {version}"
        )));
    }
    Ok(version)
}

fn stable(values: &[String]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

fn cones(values: &[String]) -> bool {
    stable(values)
        && values.iter().all(|value| {
            matches!(normalize_scope(value), Ok(path) if path == value.as_str() && path != ".")
        })
}

fn includes(all: &[String], required: &[String]) -> bool {
    required.iter().all(|required| {
        all.iter().any(|cone| {
            required == cone
                || required
                    .strip_prefix(cone)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
    })
}

fn plan_fallback(
    mode: MaterializationMode,
    scopes: &[String],
    reason: Option<FallbackReason>,
) -> bool {
    match mode {
        MaterializationMode::Sparse => !scopes.is_empty() && reason.is_none(),
        MaterializationMode::Full => scopes.is_empty() == reason.is_none(),
    }
}

fn record_fallback(
    mode: MaterializationMode,
    scopes: &[String],
    reason: Option<FallbackReason>,
) -> bool {
    match mode {
        MaterializationMode::Sparse => !scopes.is_empty() && reason.is_none(),
        MaterializationMode::Full => reason.is_none() || !scopes.is_empty(),
    }
}

fn fingerprints(record: &MaterializationRecord) -> bool {
    let source = record.source_cargo_fingerprint.as_deref();
    let candidate = record.candidate_cargo_fingerprint.as_deref();
    match record.fallback_reason {
        Some(FallbackReason::MetadataMismatch) => {
            record.mode == MaterializationMode::Full
                && matches!((source, candidate),
                    (Some(source), Some(candidate))
                    if !source.is_empty() && !candidate.is_empty() && source != candidate)
        }
        Some(FallbackReason::MetadataFailed) => {
            record.mode == MaterializationMode::Full
                && matches!((source, candidate), (Some(source), None) if !source.is_empty())
        }
        _ => match (source, candidate) {
            (Some(source), Some(candidate)) => !source.is_empty() && source == candidate,
            (None, None) => record.mode == MaterializationMode::Full,
            _ => false,
        },
    }
}

fn reduction(reason: Option<FallbackReason>, full: (u64, u64), selected: (u64, u64)) -> bool {
    reason != Some(FallbackReason::NoReduction) || selected == full
}

fn plan_reduction(plan: &MaterializationPlan) -> bool {
    let full = (
        plan.full_tracked_files,
        plan.full_git_blob_bytes,
        plan.full_working_files,
        plan.full_working_logical_bytes,
    );
    let selected = (
        plan.selected_tracked_files,
        plan.selected_git_blob_bytes,
        plan.selected_working_files,
        plan.selected_working_logical_bytes,
    );
    plan.fallback_reason != Some(FallbackReason::NoReduction) || selected == full
}

fn byte_count(files: u64, bytes: u64) -> bool {
    files != 0 || bytes == 0
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterializationMode {
    #[default]
    Full,
    Sparse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReason {
    RootScope,
    NoReduction,
    GitUnsupported,
    SparseSetupFailed,
    MetadataFailed,
    MetadataMismatch,
    RecoveryFull,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MaterializationPlan {
    #[serde(deserialize_with = "plan_schema")]
    pub(super) schema_version: u32,
    pub(super) mode: MaterializationMode,
    pub(super) requested_scopes: Vec<String>,
    pub(super) closure_packages: Vec<String>,
    pub(super) closure_cones: Vec<String>,
    pub(super) support_cones: Vec<String>,
    pub(super) base_oid: String,
    pub(super) cargo_fingerprint: Option<String>,
    pub(super) full_tracked_files: u64,
    pub(super) full_git_blob_bytes: u64,
    pub(super) full_working_files: u64,
    pub(super) full_working_logical_bytes: u64,
    pub(super) selected_tracked_files: u64,
    pub(super) selected_git_blob_bytes: u64,
    pub(super) selected_working_files: u64,
    pub(super) selected_working_logical_bytes: u64,
    pub(super) fallback_reason: Option<FallbackReason>,
    pub(super) planned_at: u64,
}

impl MaterializationPlan {
    pub(super) fn validate(&self) -> AnyResult<()> {
        if self.schema_version != PLAN_SCHEMA_VERSION
            || self.base_oid.is_empty()
            || self.planned_at == 0
            || !plan_fallback(self.mode, &self.requested_scopes, self.fallback_reason)
            || !stable(&self.requested_scopes)
            || !stable(&self.closure_packages)
            || !cones(&self.closure_cones)
            || !cones(&self.support_cones)
            || self.selected_tracked_files > self.full_tracked_files
            || self.selected_git_blob_bytes > self.full_git_blob_bytes
            || self.selected_working_files > self.full_working_files
            || self.full_working_files > self.full_tracked_files
            || self.selected_working_files > self.selected_tracked_files
            || self.selected_working_logical_bytes > self.full_working_logical_bytes
            || !byte_count(self.full_tracked_files, self.full_git_blob_bytes)
            || !byte_count(self.selected_tracked_files, self.selected_git_blob_bytes)
            || !byte_count(self.full_working_files, self.full_working_logical_bytes)
            || !byte_count(
                self.selected_working_files,
                self.selected_working_logical_bytes,
            )
            || !plan_reduction(self)
        {
            bail!("contradictory materialization plan")
        }
        if self.mode == MaterializationMode::Sparse
            && (self.cargo_fingerprint.as_deref().is_none_or(str::is_empty)
                || (self.closure_cones.is_empty() && self.support_cones.is_empty())
                || (self.selected_tracked_files == self.full_tracked_files
                    && self.selected_git_blob_bytes == self.full_git_blob_bytes
                    && self.selected_working_files == self.full_working_files
                    && self.selected_working_logical_bytes == self.full_working_logical_bytes))
        {
            bail!("sparse materialization plan lacks proof of reduction")
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializationRecord {
    #[serde(deserialize_with = "schema")]
    pub(super) schema_version: u32,
    pub(super) mode: MaterializationMode,
    pub(super) requested_scopes: Vec<String>,
    pub(super) closure_cones: Vec<String>,
    pub(super) support_cones: Vec<String>,
    pub(super) current_cones: Vec<String>,
    pub(super) base_oid: String,
    pub(super) source_cargo_fingerprint: Option<String>,
    pub(super) candidate_cargo_fingerprint: Option<String>,
    pub(super) full_tracked_files: u64,
    pub(super) full_git_blob_bytes: u64,
    pub(super) full_working_files: u64,
    pub(super) full_working_logical_bytes: u64,
    pub(super) selected_tracked_files: u64,
    pub(super) selected_git_blob_bytes: u64,
    pub(super) working_files: u64,
    pub(super) working_logical_bytes: u64,
    pub(super) materialization_duration_ms: u64,
    pub(super) materialized_at: u64,
    pub(super) expansion_count: u64,
    pub(super) last_expanded_at: Option<u64>,
    pub(super) fallback_reason: Option<FallbackReason>,
}

impl MaterializationRecord {
    pub(crate) fn validate(&self) -> AnyResult<()> {
        if self.schema_version != SCHEMA_VERSION
            || self.base_oid.is_empty()
            || self.materialized_at == 0
            || !record_fallback(self.mode, &self.requested_scopes, self.fallback_reason)
            || !stable(&self.requested_scopes)
            || !cones(&self.closure_cones)
            || !cones(&self.support_cones)
            || !cones(&self.current_cones)
            || !fingerprints(self)
            || self.selected_tracked_files > self.full_tracked_files
            || self.selected_git_blob_bytes > self.full_git_blob_bytes
            || self.working_files > self.full_working_files
            || self.working_logical_bytes > self.full_working_logical_bytes
            || !byte_count(self.full_tracked_files, self.full_git_blob_bytes)
            || !byte_count(self.full_working_files, self.full_working_logical_bytes)
            || !byte_count(self.selected_tracked_files, self.selected_git_blob_bytes)
            || !byte_count(self.working_files, self.working_logical_bytes)
            || !reduction(
                self.fallback_reason,
                (self.full_tracked_files, self.full_git_blob_bytes),
                (self.selected_tracked_files, self.selected_git_blob_bytes),
            )
            || (self.expansion_count == 0) != self.last_expanded_at.is_none()
        {
            bail!("contradictory materialization record")
        }
        match self.mode {
            MaterializationMode::Full
                if !self.current_cones.is_empty()
                    || self.working_files != self.full_working_files
                    || self.working_logical_bytes != self.full_working_logical_bytes =>
            {
                bail!("full materialization records cannot retain sparse cones")
            }
            MaterializationMode::Sparse
                if self.current_cones.is_empty()
                    || !includes(&self.current_cones, &self.closure_cones)
                    || !includes(&self.current_cones, &self.support_cones) =>
            {
                bail!("sparse materialization record omits required cones")
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MaterializationIntent {
    #[serde(deserialize_with = "schema")]
    pub(super) schema_version: u32,
    pub(super) repo: String,
    pub(super) workspace: String,
    pub(super) cargo_dir: String,
    pub(super) branch: String,
    pub(super) base_oid: String,
    pub(super) prior: MaterializationRecord,
    pub(super) desired_mode: MaterializationMode,
    pub(super) requested_scopes: Vec<String>,
    pub(super) closure_cones: Vec<String>,
    pub(super) support_cones: Vec<String>,
    pub(super) current_cones: Vec<String>,
    pub(super) desired_cones: Vec<String>,
    pub(super) plan: Option<MaterializationPlan>,
    pub(super) created_at: u64,
}

impl MaterializationIntent {
    pub(super) fn validate(&self) -> AnyResult<()> {
        self.prior.validate()?;
        if self.schema_version != SCHEMA_VERSION
            || self.repo.is_empty()
            || self.workspace.is_empty()
            || !matches!(
                normalize_scope(&self.cargo_dir),
                Ok(path) if path == self.cargo_dir
            )
            || self.branch.is_empty()
            || self.base_oid.is_empty()
            || self.created_at == 0
            || self.base_oid != self.prior.base_oid
            || self.current_cones != self.prior.current_cones
            || !stable(&self.requested_scopes)
            || !cones(&self.closure_cones)
            || !cones(&self.support_cones)
            || !cones(&self.desired_cones)
            || self.plan.as_ref().is_some_and(|plan| {
                plan.validate().is_err()
                    || plan.base_oid != self.base_oid
                    || plan.requested_scopes != self.requested_scopes
                    || plan.closure_cones != self.closure_cones
                    || plan.support_cones != self.support_cones
                    || plan.mode != self.desired_mode
            })
        {
            bail!("contradictory materialization intent")
        }
        match self.desired_mode {
            MaterializationMode::Full if !self.desired_cones.is_empty() => {
                bail!("full conversion intent cannot narrow to sparse cones")
            }
            MaterializationMode::Sparse
                if self.prior.mode != MaterializationMode::Sparse
                    || self.plan.is_none()
                    || self.requested_scopes.is_empty()
                    || !includes(&self.desired_cones, &self.current_cones)
                    || !includes(&self.desired_cones, &self.closure_cones)
                    || !includes(&self.desired_cones, &self.support_cones) =>
            {
                bail!("sparse expansion intent is not monotonic")
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
#[path = "worktree_materialization_tests.rs"]
mod tests;
