# 17. Fan-out groups: comparison from known facts, advisory ranking, a pick that is a merge

Status: accepted

## Context
One prompt can be given to several agents and the results compared (`fleet fanout`, `fleet compare`, `fleet pick`).
Everything a comparison shows is either something the daemon knows or something an agent or a repository wrote.
The ranking of members is a temptation to automate the choice.

## Decision
- **A new pure domain crate, `fleet-fanout`** (std, `fleet-domain`, `neo-verify`): the `Groups` aggregate (a projection
  of the event log: `group_created`, `member_finished`, `winner_picked`, `group_closed`), diff statistics and scope
  reports, the comparison facts and the ranking rule. The events live in `fleet-domain`, as for the goal ledger (ADR 11).
  Group events that follow from task events are derived by `Groups::follow_up` inside `Orchestrator::commit`; a restart
  derives what was lost with `Groups::reconcile`.
- **The members are ordinary tasks** with the same spec, one worker each, no dependencies. The run's `max_parallel` and the
  daemon's session limit are the only concurrency rule. Tasks, the goal that carries the shared contract and the group are
  recorded in one batch: nothing is created when anything is refused.
- **A comparison is facts only.** Status, attempts and time come from the event log; the diff is asked of the repository
  for the commit review recorded (`Vcs::diff_summary`, ADR 16: never through a worktree); evidence status per check is
  read from the ledger for exactly that commit **and** tree; scope is the touched paths (both ends of a rename) judged
  against the allow globs of the contract's `diff` checks. What the worker wrote (its summary) is shown, flattened and
  quoted, and is not an input of anything derived.
- **The ranking is advisory and has no hidden inputs.** It is an ordered list of named criteria (default: completed,
  verified, in-scope, diff, time), then task id; `[compare] rank` in `fleet.toml` (validated strictly) or `--rank` reorders it; the formula and each member's deciding criterion
  are printed. Nothing acts on it.
- **A pick is a merge.** `fleet pick` requests the normal merge approval, bound to the digest of an action that carries
  the group and the evidence statement (`UNVERIFIED ... --allow-unverified` when it is). The branch must still point at the
  commit review recorded; failed evidence is never overridden; unverified evidence needs the flag. After the merge the
  other members are `superseded`; their worktrees and branches stay until `fleet cleanup [--branches]` or `--remove-others`.
  A plain `fleet merge` of a member is refused, so the pick is the only way a group's result is merged. Pick needs every
  member to have ended (cancel or stop the others first).
- **Read only elsewhere.** The dashboard (`/api/groups`, `/api/group`, `/api/group/compare`, text nodes only) and the
  terminal dashboard (`g`) show groups and comparisons; `group.fanout` and `group.pick` are not on the web allowlist.

## Consequences
Contractless groups are `unverified` throughout and can only be picked with `--allow-unverified`. The group's goal is
verified only if every member passed and is abandoned when the group is resolved. Diffs of 16 members cost 16 cached git
calls on the daemon's thread. Not done: a fan-out into several runs, automatic picking, ranking weights.
