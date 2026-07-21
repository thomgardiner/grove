//! Verification DAG parsing, resource validation, and profile identity hashing.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::config;

#[derive(Clone)]
pub(super) struct Node {
    pub(super) id: String,
    pub(super) needs: Vec<usize>,
    pub(super) cpu: usize,
    pub(super) memory_mib: u64,
}

pub(super) struct Plan {
    pub(super) nodes: Vec<Node>,
    pub(super) max_parallel: usize,
    pub(super) cpu_slots: usize,
    pub(super) memory_mib: Option<u64>,
}

pub(super) fn validate(profile: &config::VerificationProfile) -> Result<()> {
    Plan::new(profile).map(|_| ())
}

pub(super) fn profile_sha256(profile: &config::VerificationProfile) -> String {
    let mut hash = Sha256::new();
    hash.update(b"grove.verification-profile.v3\0");
    hash.update([u8::from(profile.continue_on_failure.unwrap_or(false))]);
    hash.update([u8::from(profile.portable)]);
    option_usize(&mut hash, profile.max_parallel);
    option_usize(&mut hash, profile.cpu_slots);
    option_u64(&mut hash, profile.memory_mib);
    for name in &profile.portable_env {
        string(&mut hash, name);
    }
    hash.update([0xfd]);
    for command in &profile.commands {
        option_string(&mut hash, command.id.as_deref());
        for need in &command.needs {
            string(&mut hash, need);
        }
        hash.update([0xfe]);
        option_usize(&mut hash, command.cpu);
        option_u64(&mut hash, command.memory_mib);
        for argument in &command.argv {
            string(&mut hash, argument);
        }
        hash.update([u8::from(command.allow_zero_tests.unwrap_or(false))]);
        hash.update([0xff]);
    }
    crate::hex(&hash.finalize())
}

fn option_string(hash: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            hash.update([1]);
            string(hash, value);
        }
        None => hash.update([0]),
    }
}

fn option_usize(hash: &mut Sha256, value: Option<usize>) {
    match value {
        Some(value) => {
            hash.update([1]);
            hash.update((value as u64).to_le_bytes());
        }
        None => hash.update([0]),
    }
}

fn option_u64(hash: &mut Sha256, value: Option<u64>) {
    match value {
        Some(value) => {
            hash.update([1]);
            hash.update(value.to_le_bytes());
        }
        None => hash.update([0]),
    }
}

fn string(hash: &mut Sha256, value: &str) {
    hash.update((value.len() as u64).to_le_bytes());
    hash.update(value.as_bytes());
}

impl Plan {
    pub(super) fn new(profile: &config::VerificationProfile) -> Result<Self> {
        let max_parallel = profile.max_parallel.unwrap_or(1);
        if max_parallel == 0 {
            bail!("verification max_parallel must be at least one")
        }
        let cpu_slots = profile.cpu_slots.unwrap_or(max_parallel);
        if cpu_slots == 0 {
            bail!("verification cpu_slots must be at least one")
        }
        let mut ids = BTreeMap::new();
        for (index, command) in profile.commands.iter().enumerate() {
            let id = command
                .id
                .clone()
                .unwrap_or_else(|| format!("command-{}", index + 1));
            if id.is_empty()
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            {
                bail!("verification command IDs may only use letters, digits, '-' and '_'")
            }
            if ids.insert(id.clone(), index).is_some() {
                bail!("verification command ID {id:?} is duplicated")
            }
        }
        let mut nodes = Vec::with_capacity(profile.commands.len());
        for (index, command) in profile.commands.iter().enumerate() {
            let cpu = command.cpu.unwrap_or(1);
            if cpu == 0 || cpu > cpu_slots {
                bail!(
                    "verification command {} has an invalid CPU request",
                    index + 1
                )
            }
            let memory_mib = command.memory_mib.unwrap_or(0);
            if profile.memory_mib.is_some_and(|budget| memory_mib > budget) {
                bail!(
                    "verification command {} exceeds the memory budget",
                    index + 1
                )
            }
            let mut needs = Vec::new();
            let mut seen = BTreeSet::new();
            for need in &command.needs {
                let dependency = ids.get(need).copied().with_context(|| {
                    format!("verification command {} needs unknown {need:?}", index + 1)
                })?;
                if !seen.insert(dependency) {
                    bail!(
                        "verification command {} repeats dependency {need:?}",
                        index + 1
                    )
                }
                needs.push(dependency);
            }
            nodes.push(Node {
                id: command
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("command-{}", index + 1)),
                needs,
                cpu,
                memory_mib,
            });
        }
        validate_acyclic(&nodes)?;
        Ok(Self {
            nodes,
            max_parallel,
            cpu_slots,
            memory_mib: profile.memory_mib,
        })
    }

    pub(super) fn legacy_serial(&self) -> bool {
        self.max_parallel == 1 && self.nodes.iter().all(|node| node.needs.is_empty())
    }
}

fn validate_acyclic(nodes: &[Node]) -> Result<()> {
    let mut dependents = vec![Vec::new(); nodes.len()];
    let mut incoming: Vec<_> = nodes.iter().map(|node| node.needs.len()).collect();
    for (index, node) in nodes.iter().enumerate() {
        for dependency in &node.needs {
            dependents[*dependency].push(index);
        }
    }
    let mut ready: VecDeque<_> = incoming
        .iter()
        .enumerate()
        .filter_map(|(index, count)| (*count == 0).then_some(index))
        .collect();
    let mut visited = 0;
    while let Some(index) = ready.pop_front() {
        visited += 1;
        for dependent in &dependents[index] {
            incoming[*dependent] -= 1;
            if incoming[*dependent] == 0 {
                ready.push_back(*dependent);
            }
        }
    }
    if visited != nodes.len() {
        bail!("verification command dependencies contain a cycle")
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{State, launchable};
    use super::*;

    fn command(id: &str) -> config::VerificationCommand {
        config::VerificationCommand {
            id: Some(id.into()),
            argv: vec!["true".into()],
            allow_zero_tests: Some(false),
            needs: Vec::new(),
            cpu: None,
            memory_mib: None,
        }
    }

    fn profile(commands: Vec<config::VerificationCommand>) -> config::VerificationProfile {
        config::VerificationProfile {
            commands,
            portable: false,
            portable_env: Vec::new(),
            continue_on_failure: Some(false),
            max_parallel: Some(2),
            cpu_slots: Some(2),
            memory_mib: Some(64),
        }
    }

    #[test]
    fn rejects_unknown_cycles_and_resource_overcommit_before_execution() {
        let mut unknown = profile(vec![command("a")]);
        unknown.commands[0].needs = vec!["missing".into()];
        assert!(Plan::new(&unknown).is_err());

        let mut cycle = profile(vec![command("a"), command("b")]);
        cycle.commands[0].needs = vec!["b".into()];
        cycle.commands[1].needs = vec!["a".into()];
        assert!(Plan::new(&cycle).is_err());

        let mut overcommitted = profile(vec![command("a")]);
        overcommitted.cpu_slots = Some(1);
        overcommitted.commands[0].cpu = Some(2);
        assert!(Plan::new(&overcommitted).is_err());
    }

    #[test]
    fn scheduler_rejects_overflowing_resource_totals() {
        let states = [State::Running, State::Pending];
        let mut cpu = profile(vec![command("first"), command("second")]);
        cpu.cpu_slots = Some(usize::MAX);
        cpu.commands[0].cpu = Some(1);
        cpu.commands[1].cpu = Some(usize::MAX);
        assert!(launchable(&states, &Plan::new(&cpu).unwrap(), 1, 0, true).is_none());

        let mut memory = profile(vec![command("first"), command("second")]);
        memory.commands[0].cpu = Some(1);
        memory.commands[0].memory_mib = Some(1);
        memory.commands[1].cpu = Some(1);
        memory.commands[1].memory_mib = Some(u64::MAX);
        memory.memory_mib = Some(u64::MAX);
        assert!(launchable(&states, &Plan::new(&memory).unwrap(), 1, 1, true).is_none());
    }
}
