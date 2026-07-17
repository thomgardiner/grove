use super::plan::Plan;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum State {
    Pending,
    Running,
    Passed,
    Failed,
    Blocked,
}

pub(super) fn block_dependents(states: &mut [State], plan: &Plan, continue_on_failure: bool) {
    let stopped = !continue_on_failure && states.contains(&State::Failed);
    for (index, node) in plan.nodes.iter().enumerate() {
        if states[index] == State::Pending
            && (stopped
                || node.needs.iter().any(|dependency| {
                    matches!(states[*dependency], State::Failed | State::Blocked)
                }))
        {
            states[index] = State::Blocked;
        }
    }
}

pub(super) fn launchable(
    states: &[State],
    plan: &Plan,
    used_cpu: usize,
    used_memory: u64,
    continue_on_failure: bool,
) -> Option<usize> {
    if !continue_on_failure && states.contains(&State::Failed) {
        return None;
    }
    states.iter().enumerate().find_map(|(index, state)| {
        let node = &plan.nodes[index];
        (*state == State::Pending
            && node
                .needs
                .iter()
                .all(|dependency| states[*dependency] == State::Passed)
            && used_cpu
                .checked_add(node.cpu)
                .is_some_and(|cpu| cpu <= plan.cpu_slots)
            && plan.memory_mib.is_none_or(|budget| {
                used_memory
                    .checked_add(node.memory_mib)
                    .is_some_and(|memory| memory <= budget)
            })
            && states
                .iter()
                .filter(|state| **state == State::Running)
                .count()
                < plan.max_parallel)
            .then_some(index)
    })
}
