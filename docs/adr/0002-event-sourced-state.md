# 2. State is rebuilt from a hash-chained event log

Status: accepted

## Context
The daemon must survive being killed. It also must leave an audit trail of who approved what.
Keeping a state snapshot next to a log invites the two to disagree.

## Decision
Every change is a domain event. The event log is the only persistent state: on start the daemon
replays it into `FleetState`. Events are appended to the store first and applied to memory
afterwards (`Orchestrator::commit`), so a failed write changes nothing. Each record carries the
hash of the previous one; a second file records the head (count and last hash) to catch truncation.
Events carry only bounded, sanitized text.

## Consequences
Crash recovery is replay plus reconciliation of sessions that were running. Tamper evidence is real
but limited: someone who can rewrite the whole file and the head file can forge a consistent chain.
The log is capped (64 MiB) and the daemon refuses new work at the cap rather than growing without bound.
