# NEO, milestone N1: the goal ledger and verification contracts

NEO is the planned cognition and governance layer of agent-fleet. **Only milestone N1 exists.** This page says what
it does, how to use it and, as important, what is not there.

## What exists

A **goal ledger**: the objective as a tree of goals, each with a status, the tasks that work on it, an optional
**verification contract** (executable acceptance criteria) and the **evidence** fleet recorded by running that
contract itself. A task that a goal with a contract names completes only after fleet's own run of the contract
passed; what an agent reports is shown as `unverified` until then.

```
fleet goals add --title <text> [--id <id>] [--parent <goal>] [--intent <text>|-]
                [--task <run>/<task>]... [--contract <file.json>]
fleet goals attach <goal> --contract <file.json>
fleet goals status <goal> <open|in_progress|blocked|abandoned> [--reason <text>]
fleet goals list | show <goal> | remaining           (all take --json)
fleet verify <goal|task> [--task <run>/<task>] [--timeout <secs>]
```

All of them talk to the daemon (one writer of the event log). Workers cannot use them: the control-plane origin check
refuses every request from a worker or a process it started, like the other commands.

### Goals

A goal has an id (`[a-z0-9][a-z0-9_-]*`, or `goal-<n>` when omitted), a parent, a title, an intent, a status, the
tasks that do its work (`run/task`, at most 16) and at most one contract. The ledger is a tree by construction (the
parent must exist when a goal is added and never changes, so there are no cycles) and is bounded: 512 goals, 8 levels,
64 children per goal, 4096 pieces of evidence.

Statuses: `open`, `in_progress`, `blocked`, `abandoned` are set by an operator. **`verified` is never set by hand.** A
goal is verified exactly when

- its own contract passed (the newest evidence of every mandatory check passed), or, without a contract, it has
  children; **and**
- every child that is not abandoned is verified.

A leaf without a contract can therefore never be verified. After every change the ledger settles to that fixed point
in both directions: new evidence can verify a goal and, in the same batch of events, its ancestors; a failing re-run,
a new child or a replaced contract takes the verification away again (`verified` goes back to `in_progress`). A
verified goal can only be abandoned by hand.

`fleet goals remaining` prints what is left: **open leaves** (goals with nothing but their own criteria between them
and `verified`), **blockers** (blocked goals; blocked or failed tasks of unfinished goals) and the **next ready tasks**
(tasks of unfinished goals that are `ready`).

### Verification contracts

A JSON file (at most 64 KiB; the contract itself at most 32 KiB, 32 checks), strictly validated: unknown fields, wrong
types, duplicate ids and out-of-range numbers are errors.

```json
{"version": 1, "checks": [
  {"id": "tests",  "kind": "command", "program": "/usr/bin/make", "args": ["test"],
   "expect_exit": 0, "timeout_ms": 600000},
  {"id": "refs",   "kind": "refcheck-json", "program": "/usr/local/bin/refcheck", "args": ["--json", "."],
   "mandatory": false},
  {"id": "readme", "kind": "file", "path": "README.md", "assert": "contains", "value": "## Usage"},
  {"id": "scope",  "kind": "diff", "allow": ["src/**", "tests/**"]}
]}
```

| Kind | Passes when | Fields |
| --- | --- | --- |
| `command` | the program exits with `expect_exit` (default 0) | `program` (absolute), `args`, `expect_exit` 0-255, `timeout_ms` 100 to 3600000 (default 120000) |
| `refcheck-json` | stdout is a `refcheck` JSON document without errors (the judgement of the quality gate of the same name) | `program`, `args`, `timeout_ms` |
| `file` | `exists`: a regular file is there; `sha256`: its SHA-256 equals `value`; `contains`: it is UTF-8 text containing `value` | `path` (relative, no `..`), `assert`, `value` |
| `diff` | every path the work changed (committed, staged, modified, untracked; not `.fleet/`) matches a glob | `allow`: 1 to 32 globs: `*`, `?`, `**` as a whole segment; no classes, braces or escapes |

Every check has `id` (`[a-z0-9][a-z0-9_-]*`) and `mandatory` (default true; at least one check must be mandatory).
Defaults are written out in the canonical form, so the contract id (the SHA-256 of the canonical text, shown shortened
by the CLI) does not depend on how the file was spelled.

A verdict is `passed`, `failed` (the check ran and the result was wrong) or `error` (could not be evaluated: timeout,
output limit, cannot start, unreadable or oversized file, too many changed paths). Only `passed` passes.

### Evidence

One piece per check per run: contract id, check, verdict, exit code, summary, the last 2 KiB of output (redacted, then
sanitized), the **worktree commit** the check saw, the task, and the daemon's clock. Evidence is immutable: a new run
adds new pieces; the newest piece of a check is the one that counts. `fleet goals show` prints the newest evidence of
every check and the last 20 pieces.

### The completion rule

In review, after the worker's output was committed and before the quality gates, fleet runs the contracts of every
goal that names the task, in the task's worktree. Only if every mandatory check passed does the review continue to the
gates and then complete the task and unblock its dependents. A failure counts as a failed gate (the retry budget
applies) and the failing checks, quoted, go into the next brief. A second check at the point of completion refuses a
governed task without a recorded pass. Until then `fleet goals show` and `goals list` show the task as `unverified`
(worker finished, no passing evidence), `failed` or `pending` (worker not done); `verified` after the pass. A task that
no goal with a contract names is not held back.

`fleet verify` runs the same contracts on demand and records evidence (and settles goal status) but never completes,
fails or retries a task. `fleet verify <goal>` runs that goal's contract only, in the worktree of its single task (or
`--task`); `fleet verify <run>/<task>` runs the contracts of every goal that names the task. It waits for the result
(default 900 s) and exits 0 when every contract passed and 6 otherwise.

### How checks run, and what is stored

Commands run like quality gates and approved commands: no shell; the program an absolute path that is **not inside the
task's worktree or its repository**; the worker environment allowlist and nothing else (no inherited secrets, no
daemon variables); no stdin; their own process group, killed on timeout and when the leader exits; a timeout per check
and one hour per run; at most 4 MiB of output captured; output redacted before it is judged and again before it is
stored. File assertions open the file below the worktree root without following any symlink and read at most 1 MiB.
The diff is measured once, before the commands run. Everything shown by the CLI is escaped, and check output is quoted.

Events appended to the audit log: `goal_added`, `goal_status_changed`, `criteria_attached`, `evidence_recorded`.
`fleet verify-log` covers them like all others. There is no other file: the ledger is rebuilt by replay at start, so
goals, contracts and evidence survive a crash. The contract text is stored in the log: **do not put secrets in a
contract.**

## What does not exist

- The **context compiler** and handoff packets (N2), the **knowledge store** (N3), the **token governor** (N4) and
  the **credential broker** (N5). Nothing here meters tokens, builds context packs or touches credentials.
- The scheduler does **not** use the ledger to choose tasks: it only enforces the completion rule. `fleet goals remaining`
  is a view; there is no `fleet goals` command without a subcommand.
- Check kinds from the design that are not implemented: test selectors and knowledge assertions. A command check can
  run a test runner with a selector.
- Claim checking of an agent's report against the repository, and checks of JS/TS references beyond what a
  `refcheck-json` check you configure does.
- Status changes by tasks starting or finishing: `in_progress` and `blocked` are set by an operator.

## Honest limits

- A contract is only as good as its checks: a weak check passes a weak result. The agent can read the contract in the
  worktree-independent file you wrote only if you tell it; fleet does not give it the contract (that is the context
  compiler's job, N2).
- Commands are not sandboxed. The program is trusted (it lies outside the worktree) but **what it runs is not**: a check
  that runs `make test` runs the worker's `Makefile` as you, with the allowlisted environment. Use a container or another
  user for real isolation, like for the workers.
- Output is redacted by patterns; a secret that does not look like one is stored as printed.
- Evidence names the commit it saw; the files checked after commands ran (`file` checks run after earlier commands of the
  same contract in order) can be changed by those commands.
- A daemon killed during a verification loses that run (nothing was recorded); the restarted daemon runs the contracts of
  every task still in review again.

See the decisions in [docs/adr/0011](adr/0011-neo-domain-crates-and-application-module.md),
[0012](adr/0012-goals-and-evidence-are-event-sourced.md) and [0013](adr/0013-verification-by-fleet-gates-completion.md),
and the vocabulary in [domain.md](domain.md).
