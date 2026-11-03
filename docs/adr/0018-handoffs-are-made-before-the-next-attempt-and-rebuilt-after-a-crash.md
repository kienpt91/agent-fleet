# 18. Handoffs are event-sourced, made before the next attempt starts, and rebuilt after a crash

Status: accepted

## Context
A task's next attempt, after a failure or a daemon crash, has to be briefed from what fleet recorded, never from a model's
summary alone. A worker may leave a summary in `.fleet/handoff.json`; the daemon can die between recording the end of an
attempt and recording its handoff.

## Decision
- A `HandoffCreated` event carries the whole packet (its pack slice with the layer manifest and the stable digest, the open
  verification state, pointers to evidence, and the worker's summary stored as UNVERIFIED) and its digest. The newest packet
  per task is rebuilt by replay (`ContextState`); a packet whose digest does not match its content is ignored.
- Handoffs are made once per attempt and class: when a task completes or fails; when an attempt ends and the task goes back in
  line (retry); when the daemon finds a task running after a crash (recovery); and on demand (`fleet handoff`, recorded only if
  its content is new).
- **Atomicity without a second store.** The summary is read from the worker's file, kept in memory, and consumed only after
  the event is recorded. Before a task is briefed again, `dispatch` makes the handoff of the attempt that ended if it is
  missing, and does not start the attempt if it cannot be recorded, so `prepare` never wipes a summary that was not yet
  recorded. At start-up the orchestrator does the same for every task whose attempt ended without a handoff, re-reading the
  summary from the worktree. A test (`fleet-app/tests/neo_context.rs`) makes the store refuse the handoff, restarts, and finds it
  made with the summary; an end-to-end test kills the daemon with a running worker and checks that layers 1-5 are identical.

## Consequences
A summary is lost only if the worker's worktree is gone. A handoff can be made later than the end of its attempt, never
skipped. The cost is one more compile at dispatch for a task that was retried. The wait is bounded: if the handoff cannot be
recorded for 5 ticks in a row, the task is failed with the reason instead of being tried again by every tick (an operator can
retry it). An on-demand packet (`fleet handoff`) is not the source of the next brief while a packet made by the flow exists,
so asking for one does not replace what a worker left.
