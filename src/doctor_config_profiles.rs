//! Profile names discovered from the manifest and effective Cargo configs.

use std::collections::BTreeSet;

use super::Inputs;

pub(super) fn names(inputs: &Inputs) -> BTreeSet<String> {
    let mut names = ["bench", "dev", "release", "test"]
        .into_iter()
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    for document in std::iter::once(&inputs.manifest).chain(inputs.configs.iter()) {
        if let Some(profiles) = document
            .value
            .get("profile")
            .and_then(toml::Value::as_table)
        {
            names.extend(profiles.keys().cloned());
        }
    }
    names
}
