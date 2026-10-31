# 12. Goals, contracts and evidence are events in the existing log

Status: accepted

## Context
The goal ledger and the evidence have to survive a crash and be as hard to forge quietly as the rest of
the audit trail. A second store (a ledger file, an evidence directory) would be a second source of truth,
with its own write ordering, its own recovery and no hash chain; a verification that completed a task
while its evidence sat in another file could disagree with the log after a crash.

## Decision
Four events in the existing hash-chained log (ADR 2): `goal_added`, `goal_status_changed`,
`criteria_attached` (carries the contract as a JSON document and its SHA-256) and `evidence_recorded`
(one per check run: contract id, check, verdict, exit code, bounded redacted output, worktree commit,
time from the daemon's clock). The ledger (`neo-goals::Ledger`) is a projection like `FleetState`: built
by replay at start, updated by `Orchestrator::commit` after the store accepted the batch. A command
returns the events it causes, *including* the status changes that new evidence causes (the ledger
settles on a copy first), so the log alone replays to the same ledger; the property tests replay random
histories and compare.
"File-backed" therefore means the event log file and its adapters (`FileEventStore`, the safe file
helpers for reading contract files); there is no separate evidence store.
The contract travels inside `criteria_attached` as a document (`Document`, compared by canonical text)
so that the log does not need a JSON parser in the domain.
`verified` is never set by a command: `set_status` refuses it, and the settle step derives it from the
evidence and the children, in both directions (a failing re-run, a new child or a new contract takes
it away).

## Consequences
Restart and crash recovery need no new code: a task still in review after a crash runs its contracts
again; evidence of a run that was cut off was never recorded. Evidence is bounded (2 KiB of output per
check, 4096 pieces in the ledger, then new verification is refused) and the log is capped at 64 MiB like
before. The contract text is part of the log, so **a contract must not contain secrets** (arguments are
stored and shown); check output is redacted before it is recorded, but the operator's own text is not.
