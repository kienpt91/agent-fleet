# 11. The goal ledger and verification live in two domain crates and in the application layer

Status: accepted

## Context
NEO (see [docs/neo.md](../neo.md)) adds a goal ledger and verification contracts. They are new bounded
contexts with their own rules (a status machine, a contract schema, a glob matcher, evidence), but they
are not independent of the rest: the completion of a task has to depend on them, they record events in
the same audit log and they run commands through the same hardened job runner. Two questions: where
does the pure code live, and where do the use cases live?

## Decision
- Two new domain crates, `neo-verify` (contracts, checks, glob matcher, judging, evidence) and
  `neo-goals` (the ledger aggregate). Both are pure: std and workspace domain crates only, no I/O, no
  clock, no randomness, `#![forbid(unsafe_code)]`. They may use `fleet-domain` (ids, `Value`, the
  sanitizer, the redactor limits, the quality judgement of `refcheck-json`): that is a shared kernel
  inside the domain layer, not a dependency on an outer layer. `neo-goals` may use `neo-verify`
  because a goal is verified by the state of its contract.
- The use cases are a `neo` module of `fleet-app`, not a separate `neo-app` crate. The review flow has
  to read and change orchestrator state: `commit` folds every event into the ledger, `begin_review`
  starts the contract run before the quality gates, `apply_gate_results` refuses to complete a governed
  task without a passing run. A separate crate would need either the orchestrator's internals exposed
  (`pub` fields, which weakens the single-writer rule of ADR 3) or hooks (callbacks into the other crate,
  which would be a dependency in the wrong direction). One crate keeps them private (`pub(crate)`).
- The shared vocabulary that events need is in `fleet-domain`: `GoalId`, `EvidenceId`, `GoalStatus`,
  `Verdict` and the four events. Events are the published language of the audit context (ADR 2); a
  context that cannot name its events there cannot be recorded.
- The architecture tests know the new layers: the domain crates may depend on each other inward only,
  the application layer on the three domain crates only, and none of the three reads a clock or draws
  random numbers.

## Consequences
The domain stays testable without processes, files or time (domain tests: unit, property tests of the
status machine and of the glob matcher against a reference). `fleet-app` grows by one module and now
depends on three crates instead of one. If a later milestone (context compiler, knowledge store)
needs state that is independent of the orchestrator, it can become its own crate; the goal ledger could
not, because it participates in completion.
