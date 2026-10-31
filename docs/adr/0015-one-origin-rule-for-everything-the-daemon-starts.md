# 15. One origin rule for everything the daemon starts, on the control socket and on the web port

Status: accepted. Reconciles ADR 9 (control plane), ADR 10 (web dashboard) and ADR 14 (jobs are untrusted).

## Context
Two changes solved the same gap separately. The gap: the origin check refused only the live worker sessions, but the
daemon also starts quality gates, approved commands, verification checks, the undo tool and git, which run or are steered
by files a worker wrote. A process such as one of those could connect to the control socket or to the dashboard and be
admitted. One change registered the process groups of the children of the job runner (and sampled their detached
descendants); the other refused any peer with the daemon among its ancestors and listed the groups of gates, commands, the
undo tool and git in a process-wide list. Both were right about part of the problem and neither covered all of it: the first
did not list the undo tool and git and had no catch-all for a child nobody had listed, the second did not follow
descendants that leave their group.

## Decision
**One registry and one rule, used by every entry point.**
- `jobs::JobTracker` is the only registry. The daemon creates one and hands a clone to every adapter that starts a child:
  `ThreadJobRunner` (gates, approved commands, verification checks), `GitVcs` (both instances) and `AgentUndo`. Each
  registers the process group of the child it starts for as long as the child and its group exist, and follows the
  child's descendants with the `Sweeper` (sampled every 200 ms, registered by pid, killed with start-token verification
  when the child ends). `JobRunner::live` and `Orchestrator::live_sessions` return the registry together with the live
  worker sessions.
- `decide_origin` (daemon) refuses a peer that (1) cannot be identified, (2) is a live session, a registered process or in a
  registered group (`is_worker_origin`), or (3) has the daemon among its ancestors (`is_started_by_daemon`: the generic
  fail-closed catch-all for anything the daemon started that is still attached, whether or not anybody listed it).
- The control socket calls it at accept and per request. The web port calls the same function through `WebBackend::admit`
  for every holder of the client's end of a connection, after the request was read and before anything is acted on
  (fail closed for a loopback peer that cannot be identified).
- `approval.decide` requires the digest of the action on every path, including `POST /api/approval/decide`, where the page
  must send the digest it displayed.

## Consequences
A process that left the daemon's process tree and the group of every job and was not seen by a sample (`setsid` plus a
parent that exits within 200 ms, a unit started with `systemd-run --user`) is indistinguishable from the operator's own
client. That is the documented limit of a same-user defence; real isolation is another user, a container or a VM.
The tests that prove each part: the origin table in `daemon::tests` (ancestry, group, detached pid, unknown peer), the
tracker and sweeper tests in `jobs`, `fleet-app/tests/neo_verify.rs` (the orchestrator reports job processes), `e2e_neo.rs`
and `e2e_web.rs` (processes started by gates, approved commands and verification checks, also detached ones, are
refused through the real binary on both interfaces); switching either mechanism off makes the matching tests fail.
