# Assurance constitution (v1)

**Status:** normative for Grove · Summoner · Crucible  
**Epoch:** 1  
**Date:** 2026-07-23  

This document freezes the **invariants** the stack must preserve. Features that
violate an invariant are bugs, not tradeoffs. The overall platform score is the
**weakest dimension**, not an average of architecture quality and operational
maturity.

**10/10 operational definition:** every material assurance claim is mechanically
enforced, tested against adversarial failure, independently reproducible,
operable by engineers other than the author, and tied to measurable
organizational impact.

---

## Component roles (stable boundaries)

| Component | Owns | Does not own |
| --- | --- | --- |
| **Grove** | Worktrees, claims, process supervision, build lanes, exact-checkout verification receipts, policy pinning, inspection capsules, finish-time state integrity, resource admission *as reported by capabilities* | Orchestration, model dispatch, review semantics, whether tests are meaningful, OS same-user sandbox for hostile code |
| **Summoner** | Deterministic dispatch, trusted policy, dependency propagation, immutable run evidence, crash resume, holder review, revisions, landing | Deep execution evidence (delegates to host), Crucible arms, identity provider |
| **Crucible** | Whether verification means anything (gates, reality, mutation, coverage, flake, smells); content-bound attestations | Replacing test runners; granting merge authority |

Hooks, OS sandboxes, and org identity systems remain the **authorization /
prevention** plane. Grove **coordinates and detects**; Summoner **schedules and
binds**; Crucible **challenges the oracle**.

---

## The ten invariants

### I1 — Candidate identity

Every verification, review, revision, dependency edge, and landing decision
must refer to the **same immutable candidate**:

```
candidate = commit + tree + complete source digest
```

**Forbidden substitutes:** current `HEAD`, a mutable branch name, “clean working
tree” alone, “latest source digest” without commit/tree, a salvaged commit that
was not the reviewed candidate.

**Today (honest):** Grove snapshot digests approach this; Summoner git host
requires a clean tree and binds `sha256(commit‖tree)` — still not a full
tracked+index+untracked capture. Landing uses recorded `candidate_commit`s.

**Owner (interim):** Summoner host + Grove task/receipt  
**Executable tests:** host conformance suite (dirty mis-ID, salvage drift)

### I2 — Policy identity

Every decision must refer to one trusted policy:

```
policy = policy digest (+ future: epoch, signer/authority)
```

Repository content may **request** a profile; it must never **lower** the
operator’s trusted policy. Policy lives outside the candidate repository.

**Today:** Summoner `[trusted_policy]` is global-only, content-addressed into
manifest/report/review; not yet signed/revocable epochs.

**Owner (interim):** Summoner config  
**Executable tests:** repo cannot publish policy; digest changes with every
narrowing field

### I3 — Exact input identity

Every order must record base commit, dependency candidate commits, and any
merge result used as input. **Scheduling** dependency and **source** dependency
must be distinct (future fields `wait_for` / `consumes`). A missing consumed
candidate is a failure, never a silently dropped input.

**Today:** `after` + recorded `candidate_commit` / `base_commit`; no formal
`consumes` vs `wait_for` split yet.

**Owner (interim):** Summoner order + outcome  
**Executable tests:** land refuses missing candidate commit; dependents do not
inherit dirty candidates

### I4 — Per-profile evidence

Each required verification profile must independently bind profile definition,
candidate identity, tool identities, declared inputs, environment, result, and
logs. There must never be a single global “verified source” pointer shared by
several profiles as if they were one observation.

**Today:** profiles run separately; git host still stores one
`verify_source_*` per task for finish. Per-profile full binding is incomplete.

**Owner (interim):** Grove receipts + Summoner gate  
**Executable tests:** required_profiles all run; dirty tree cannot verify

### I5 — Verification immutability

A verifier may not modify the candidate it certifies. Capture identity before
verification; reject changes to commit, tree, source digest, policy, or
declared inputs even if the command exits zero and leaves a clean worktree.

**Today:** git host requires clean tree before/after verify; Grove has stronger
snapshot compare. Full pre/post content CAS on every host path is incomplete.

**Owner (interim):** Hosts  
**Executable tests:** dirty-after-verify refuses; Grove source_changed path

### I6 — Independent review binding

A review verdict must bind candidate, policy, verification evidence, Crucible
evidence (when present), reviewer identity, nonce, and protocol version. The
reviewer must be unable to modify the candidate or authorization evidence.

**Today:** protocol binds candidate snapshot + policy digest + nonce; git host
uses detached review worktree; `distinct_reviewer_identity` is
operator-asserted labels, not model/service principals.

**Owner (interim):** Summoner review_gate + host inspection  
**Executable tests:** integrity fail-closed on capsule mutation; identity
policy unit tests

### I7 — Integration identity

Individual candidates are not the final product. Selected candidates produce an
immutable integration candidate `I = deterministic_merge(C1…Cn)`. Aggregate
verification, Crucible, and holder review must run against **I**; the protected
target advances **specifically to I**.

**Today:** Summoner land uses a temp integration branch, optional aggregate
verify, then FF. Integration is not yet a first-class captured candidate object
with its own full evidence envelope.

**Owner (interim):** Summoner land  
**Executable tests:** conflict leaves target unchanged; no-op aggregate refused
unless explicitly allowed

### I8 — No silent downgrade

If a required capability is unavailable, execution **stops** (or enters an
**explicit** lower-assurance mode with lowered status). Infrastructure absence
must never silently reduce the authorization bar.

**Today:** trusted policy `required_host` / `required_capabilities`; held review
gated on host capabilities; git fallback prints a weaker-host notice. Capability
advertising on git host is honest only under the clean-tree contract.

**Owner (interim):** Summoner host resolution + Grove capabilities  
**Executable tests:** policy refuses wrong host; missing capability refuse

### I9 — Crash-safe transitions

Every critical operation must be safe if the process dies immediately before or
after it (acquire, publish, spawn, receipt, review, finish CAS, journal,
integration, target update). Resume either completes the transition or explains
why it cannot — never infers success from incomplete state.

**Today:** journals, manifests, Grove task records; not exhaustive crash-window
coverage for every transition.

**Owner (interim):** Summoner resume + Grove recovery  
**Executable tests:** fleet resume / hard-kill recovery suites

### I10 — Final authority is explicit

No generic “success” implies authorization to merge or deploy. Keep distinct
outcomes (at least): `completed`, `verified`, `reviewed` / `approved`,
`integrated`, and (future) `authorized_for_merge`, `merged`, `deployed`.

**Today:** Summoner outcomes distinguish completed vs verified vs review
results; not a full deploy authorization ladder.

**Owner (interim):** Summoner report / outcome  
**Executable tests:** no verification → completed not verified

---

## Weakest-link scorecard (epoch 1)

Scores are **conservative** and **gated by the weakest** column that matters
for production use of the full stack.

| Dimension | Score | Notes |
| --- | --- | --- |
| Architecture | 8 | Clear three-plane split; host SPI still maturing |
| Correctness | 6 | Dual-review P0 dirty-path closed for git; per-profile/integration objects incomplete |
| Security & assurance | 5 | Threat model not published; no external assessment; identity labels not principals |
| Reliability | 5 | Resume suites exist; not exhaustive crash-window + SLOs |
| Evidence integrity | 6 | Strong pieces; not one third-party-verifiable envelope yet |
| Performance & economics | 3 | No fleet-scale public benchmark/cost story |
| Product maturity | 6 | Releases exist; coordinated multi-crate compatibility incomplete |
| Adoption | 2 | Not multi-team without author |
| Impact | 1 | Not measured vs baseline |
| Leadership leverage | 2 | Single-author ownership |

**Platform production proposition (weakest link): ~2–5** until adoption, impact,
and independent ownership move. **Technical ceiling without org leverage: ~6–7.**

That is intentional honesty: Staff+ architecture ≠ Principal case.

---

## What we will not build to chase the score

- More agent personas / debate hierarchies  
- Kubernetes / multi-host distributed scheduler as a substitute for proof  
- Large dashboards, vector DBs, proprietary model gateways  
- Broad language support without host/adapter conformance  
- Autonomous deployment  

Next leap: **stronger proof**, not more autonomy.

---

## Execution order (constitution freeze → leverage)

1. **This constitution** — invariants named, owners interim, tests listed  
2. **Unify candidate + policy identity** — Grove candidate object; signed policy epoch; per-profile evidence; immutable integration candidate  
3. **Assurance envelope** — one verifiable artifact composing Grove + Crucible + Summoner  
4. **Conformance + fault harnesses** — hosts, crashes, corruption, downgrade  
5. **Runtime boundary** — sandbox/resource capability reporting that cannot lie  
6. **Coordinated releases** — compatibility, migration, rollback, install  
7. **Independent pilots** — other teams without live author support  
8. **Publish impact evidence**  
9. **Delegate ownership**  
10. **Org autonomy standard** — libraries implement policy; they are not the policy  

---

## Normative test map (IDs)

| ID | Suite / location (current or planned) |
| --- | --- |
| I1 | `tests/fault_injection.rs`, Grove receipt tests; planned: host conformance |
| I2 | `config` / `order` trusted_policy tests |
| I3 | `tests/land.rs`, dependency outcome tests |
| I4 | `gate.rs` required_profiles; planned: per-profile receipt schema tests |
| I5 | `fault_injection` dirty verify; Grove `source_changed` |
| I6 | review integrity; planned: review mutation suite |
| I7 | `tests/land.rs` conflict/FF; planned: integration candidate object |
| I8 | host policy refuse; planned: capability matrix table tests |
| I9 | fleet resume / recovery integration tests |
| I10 | anti_reward completed-vs-verified |

When an invariant has no automated test, the scorecard must not claim that
invariant is met.

---

## Related documents

- Grove: [docs/reference.md](docs/reference.md), [README.md](README.md)
- Summoner: https://github.com/thomgardiner/summoner/blob/master/ASSURANCE.md
- Crucible: https://github.com/thomgardiner/crucible/blob/main/ASSURANCE.md

**Epoch bumps** when an invariant’s meaning changes incompatibly. Implementations
may advance under the same epoch only if they remain behaviorally compatible
with these definitions.
