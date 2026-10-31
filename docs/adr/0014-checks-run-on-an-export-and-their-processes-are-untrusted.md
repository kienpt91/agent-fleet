# 14. Checks run on an export of the commit, and the processes of jobs are untrusted

Status: accepted. Amends ADR 13 (where a check runs and how evidence counts) and ADR 5 (a decision names its digest). The
registry of job processes described below is shared by all child-starting adapters and used by the web port too: see ADR 15.

## Context
An independent security review of N1 found that a contract command such as `make test` runs files a worker wrote, as a
child of the *daemon*. Two consequences: the control-socket origin check only refuses descendants of live worker
sessions, so a process started by a check was admitted (the same held for quality gates and approved commands, which
N1 inherited and made easier to reach); and `approval.decide` took an optional digest, so such a process could approve
without naming what it approved. Also, checks ran in the live worktree, so uncommitted, ignored or planted files were
executed and tested while the commit that is merged was something else, and evidence was counted regardless of the
commit, task or run it came from.

## Decision
- **The processes of every job are worker origin.** The job runner registers the process group of each child it starts
  (gates, approved commands, verification checks) for as long as the child and its group exist; the orchestrator adds
  them to the live sessions the control socket's origin check uses (`Ports.jobs.live()`), so a peer that is one of them or
  descends from one is refused, failing closed like workers. Descendants that leave the group are sampled from the process
  table every 200 ms (and at the end), registered by pid, and killed with start-token verification when the check ends.
  This is a sample, not a guarantee (see SECURITY.md); without cgroups or a sandbox there is none.
- **Decisions name their digest.** `approval.decide` requires the digest (>= 12 hex digits) of the action; `fleet approve`
  fills it from `approval list` unless `--digest` is given. A call without one is invalid and records nothing.
- **Checks run on an export.** A verification job exports the reviewed commit with `git checkout-index` from a temporary
  index into a fresh 0700 directory under `<state>/verify/`, after checking the tree id; commands run there with a home,
  temporary and XDG directories of their own, a `PATH` of trusted directories only and git pinned to no configuration;
  the directory is removed afterwards and swept at start. Links that could leave the export are removed. The adapter
  never runs a check without a host that can export (it fails every check instead of falling back to the worktree).
- **Evidence is about one commit of one task.** `evidence_recorded` carries run, task, commit, tree and a `dirty` flag.
  The ledger counts a piece only if it is not dirty and its commit equals the commit of the task's last
  `task_committed`; goals with several tasks need the contract to pass for each; a new commit un-verifies; merging a
  guarded task needs passing evidence for exactly the commit it would merge. A worktree with uncommitted changes when a
  review verifies fails the attempt.
- **Resource bounds.** At most 2 verification jobs at once, 30 minutes per check and per job (they were 1 hour), output
  redacted over the whole capture before the tail is cut, evidence cap by dropping superseded pieces.
- **Ledger replay does not trust derived fields**: ids are recomputed, `verified` is refused unless derivable, the ledger
  is settled again at start.

## Consequences
What a check proves is the committed tree, run by fleet in a place the worker did not prepare, as a process fleet treats
as hostile. What it does not give is a sandbox: the committed code still runs as the operator with network and file
access, and a same-user process can still read the capability token and detach completely. That remains defence in depth;
real isolation is a container, a VM or another user. The cost: a check that needs ignored build outputs or network
fetches of its own must build them itself inside the export (a fresh checkout, like CI does), and a verification takes
longer than running in place.
