use serde::{Deserialize, Deserializer, Serialize};

/// GNU jobserver admission applied to Grove-routed builders.
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GovernorMode {
    /// Share tokens when available and let a build continue if setup fails.
    #[default]
    BestEffort,
    /// Refuse builds unless Unix jobserver and builder admission are enforceable.
    ///
    /// CPU accounting requires at most one top-level jobserver client per admitted command.
    Strict,
    /// Invalid environment input retained so build acquisition can refuse it.
    Invalid,
}

impl<'de> Deserialize<'de> for GovernorMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = toml::Value::deserialize(deserializer)?;
        Ok(value.as_str().map(parse).unwrap_or(Self::Invalid))
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Governor {
    pub(crate) mode: GovernorMode,
    pub(crate) cpu_slots: usize,
    pub(crate) max_builders: usize,
}

impl Governor {
    pub(crate) fn best_effort(cpu_slots: usize) -> Self {
        Self {
            mode: GovernorMode::BestEffort,
            cpu_slots,
            max_builders: 1,
        }
    }
}

pub(super) fn validated(
    configured: Option<GovernorMode>,
    cpu_slots: Option<usize>,
    max_builders: Option<usize>,
) -> GovernorMode {
    let mode = mode(configured);
    if mode != GovernorMode::Strict {
        return mode;
    }
    for (key, configured) in [
        ("GROVE_CPU_SLOTS", cpu_slots),
        ("GROVE_MAX_BUILDERS", max_builders),
    ] {
        if !valid_positive(key, configured) {
            eprintln!("grove: invalid {key}; strict builds will be refused");
            return GovernorMode::Invalid;
        }
    }
    mode
}

fn mode(configured: Option<GovernorMode>) -> GovernorMode {
    let value = match std::env::var("GROVE_GOVERNOR_MODE") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return configured.unwrap_or_default(),
        Err(std::env::VarError::NotUnicode(_)) => {
            eprintln!("grove: non-Unicode GROVE_GOVERNOR_MODE; builds will be refused");
            return GovernorMode::Invalid;
        }
    };
    let mode = parse(value.trim());
    if mode == GovernorMode::Invalid {
        eprintln!("grove: invalid GROVE_GOVERNOR_MODE={value:?}; builds will be refused");
    }
    mode
}

fn valid_positive(key: &str, configured: Option<usize>) -> bool {
    match std::env::var(key) {
        Ok(value) => value.parse::<usize>().is_ok_and(|value| value > 0),
        Err(std::env::VarError::NotPresent) => configured != Some(0),
        Err(std::env::VarError::NotUnicode(_)) => false,
    }
}

fn parse(value: &str) -> GovernorMode {
    match value {
        "best_effort" => GovernorMode::BestEffort,
        "strict" => GovernorMode::Strict,
        _ => GovernorMode::Invalid,
    }
}

pub(super) fn builders(configured: Option<usize>) -> usize {
    std::env::var("GROVE_MAX_BUILDERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .or(configured)
        .unwrap_or(1)
}
