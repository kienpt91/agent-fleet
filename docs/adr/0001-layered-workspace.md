# 1. A layered cargo workspace with a std-only domain crate

Status: accepted

## Context
The orchestrator mixes pure rules (task lifecycle, retries, approvals, output sanitizing) with
processes, ptys, git and sockets. Tests that need a real git repository and a real pty are slow and
hard to make deterministic, and security-relevant rules (what an approval covers, when a worker's
output is escaped) should be testable without any of that.

## Decision
Four crates with a strict inward dependency rule: `fleet-domain` (std only) <- `fleet-app`
(use cases and ports) <- `fleet-infra` (adapters) <- `fleet-cli`. The domain performs no I/O, reads no
clock and generates no ids: time, hashing and ids reach it through arguments and small traits.

## Consequences
The compiler enforces the dependency rule. Domain tests are fast and deterministic; application tests
run the whole orchestrator against in-memory fakes; adapters are tested against the same contract
suites as their fakes and then end to end through the real binary. The price is some mapping code
(`Value` <-> JSON, port traits and their fakes).
