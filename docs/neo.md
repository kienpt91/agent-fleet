# NEO: milestone N1 (the goal ledger and verification contracts) and milestone N2 (context packs and handoffs, below)

NEO is the planned cognition and governance layer of agent-fleet. **Milestones N1 and N2 exist** (N2 is described in its own
section at the end of this page). This part says what N1 does, how to use it and, as important, what is not there.

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
| `command` | the program exits with `expect_exit` (default 0) | `program` (absolute), `args`, `expect_exit` 0-255, `timeout_ms` 100 to 1800000 (default 120000) |
| `refcheck-json` | stdout is a `refcheck` JSON document without errors (the judgement of the quality gate of the same name) | `program`, `args`, `timeout_ms` |
| `file` | `exists`: a regular file is there; `sha256`: its SHA-256 equals `value`; `contains`: it is UTF-8 text containing `value` | `path` (relative, no `..`), `assert`, `value` |
| `diff` | every path the commits of the task changed since the run's base commit (`.fleet/` paths included) matches a glob | `allow`: 1 to 32 globs: `*`, `?`, `**` as a whole segment; no classes, braces or escapes |

Every check has `id` (`[a-z0-9][a-z0-9_-]*`) and `mandatory` (default true; at least one check must be mandatory).
Defaults are written out in the canonical form, so the contract id (the SHA-256 of the canonical text, shown shortened
by the CLI) does not depend on how the file was spelled.

A verdict is `passed`, `failed` (the check ran and the result was wrong) or `error` (could not be evaluated: timeout,
output limit, cannot start, unreadable or oversized file, too many changed paths). Only `passed` passes.

### Evidence

One piece per check per run: contract id, check, verdict, exit code, summary, the last 2 KiB of output (redacted, then
sanitized), the **commit** and its **tree** that were exported and checked, the task, whether the worktree was **dirty**
(had uncommitted changes) and the daemon's clock. Evidence is immutable: a new run adds new pieces.

Whether a piece **counts** is decided by the ledger, not by whoever recorded it: it counts only if it was not taken on a
dirty worktree and its commit is the one the task's last review recorded (`task_committed`). The newest counting piece of
a check is the one that decides. So a pass does not survive a new commit of the task, a manual `fleet verify` that saw
another commit proves nothing, and a goal that names several tasks is verified only when its contract passed for each of
them (evidence is per task). Pieces that do not count are kept and shown with the reason. When the ledger holds more than
4096 pieces, superseded ones are dropped from memory (not from the log), oldest first; only if all 4096 are current is
new evidence refused.

### The completion rule

In review, after the worker's output was committed and before the quality gates, fleet runs the contracts of every
goal that names the task, on a clean export of the committed work (see below). Only if every mandatory check passed,
with nothing uncommitted left in the worktree, does the review continue to the gates and then complete the task and
unblock its dependents. A failure counts as a failed gate (the retry budget applies) and the failing checks, quoted, go
into the next brief. A worktree that changed after its work was committed fails the attempt with a note to that effect.
Before a task completes, a second check consults an in-memory per-attempt marker (set only by a passing run of every
contract that guards the task at that moment; it is lost on restart, and the restart runs the contracts again). Until
then `fleet goals show` and `goals list` show the task as `unverified` (worker finished, no passing evidence), `failed`
or `pending` (worker not done); `verified` after the pass. A task that no goal with a contract names is not held back.

`fleet merge` of a task that a contract guards is refused unless the ledger has passing evidence for exactly the commit
that would be merged: a branch that moved after the proof (a leftover process, a late commit) cannot be merged on the
strength of an earlier pass.

`fleet verify` runs the same contracts on demand, on the task's *current commit*, and records evidence (and settles goal
status) but never completes, fails or retries a task. `fleet verify <goal>` runs that goal's contract only, for its
single task (or `--task`, which must be one of the goal's tasks); `fleet verify <run>/<task>` runs the contracts of every
goal that names the task. The daemon hands out a ticket for the run and the command asks about **that run**: it exits 0
only when this run's checks passed, were recorded and counted, 6 otherwise (a run that could not be recorded is not a
pass, whatever older evidence says).

### How checks run

**Which commit.** The head and the tree of a task are read from the real repository, never through the worktree's own git
directory, which the worker owns: the worktree's `.git` pointer is validated before every use (a plain file naming exactly
`<repo>/.git/worktrees/<name>`, whose back-pointer and `commondir` agree), git runs with that directory pinned, replace refs
are disabled and a repository with alternates, `http-alternates` or grafts is refused. Evidence names (commit, tree) and a
merge compares the tree the repository has for the commit it merges with the proven one (ADR 16).

**Where.** Not in the worktree. The commit the review recorded is exported into a fresh directory outside the state
directory (`<temp>/fleet-verify-<uid>/<hash of the state path>/job-*/tree`, mode 0700, so nothing above a check's working
directory is the state directory; `git checkout-index` from a temporary index, so the worktree's files,
ignored and untracked files, index and local git state are never read) and the checks run there; the directory is removed
when the job ends and anything an earlier daemon left is removed at start. Symlinks of the commit that are absolute or
contain `..` are removed from the export. A Makefile, script or helper that exists only in the worktree, in an ignored
file or uncommitted is therefore never executed, and what is proven is the commit that is merged. The tree id of the
export is checked against the one recorded.

**Environment.** The operator's allowlist, minus `HOME`, `TMPDIR`, `XDG_*`, `FLEET_*`, `GIT_*` and the like, plus a `HOME`,
`TMPDIR` and `XDG_*` of the job's own (empty directories inside the job directory: no `~/.gitconfig`, `~/.npmrc`, `~/.ssh`
of the operator), git pinned to no global or system configuration and a ceiling at the job directory, and a `PATH` cut
down to absolute directories that exist, are not inside the worktree, its repository or the scratch area, and are not
writable by group or others.

**Processes.** No shell; the program an absolute path that is not inside the worktree, its repository, the export or the
scratch area; no stdin; its own process group, killed on timeout and when the leader exits; a timeout per check (default
2 min, at most 30 min) and 30 min per job; at most 2 verification jobs at a time (8 jobs of any kind); at most 4 MiB of
output per stream, **redacted over the whole capture before it is cut to its tail** and again before it is stored.
Descendants that leave the process group (`setsid`, a double fork) are found by sampling the process table every 200 ms
while the check runs (and once more before a check that timed out is stopped), tracked, and killed with start-token verification when the check is over. File assertions open
the file below the export root without following any symlink and read at most 1 MiB. The diff is `git diff base..HEAD`
of the commit: paths under `.fleet/` that the worker committed are changes like any other.

**Origin.** The children of every running job (quality gates, approved commands, verification checks, and also the undo
tool and git) are registered in one tracker shared by all those adapters, and so are the detached descendants found above.
The origin check refuses a peer that is one of them, sits in one of their groups, descends from one, or has the daemon
itself among its ancestors, exactly like a worker (fail closed), on the control socket **and** on the web dashboard's
port (one rule, see ADR 15). An ancestry that could not be read to the root of the process tree is refused as well. `approval.decide` requires the digest of the action it covers (at least 12 hex
digits; `fleet approve <id>` takes it from `fleet approval list` unless `--digest` is given), so a decision always names
what it decides. The digest guards against a stale decision; it does not keep out an admitted caller, since the CLI takes it from the list the daemon returns (the origin rule does that).

Events appended to the audit log: `goal_added`, `goal_status_changed`, `criteria_attached`, `evidence_recorded`.
`fleet verify-log` covers them like all others. There is no other file: the ledger is rebuilt by replay at start, so
goals, contracts and evidence survive a crash. On replay a `criteria_attached` whose id is not the SHA-256 of its contract
is ignored, a status event that claims `verified` without the evidence is ignored, and the ledger is settled again
(a commit that changed while the daemon was down un-verifies what it proved). The contract text is stored in the log:
contracts that contain something a redaction rule recognizes (a token, a key, the value of a secret variable of the
daemon) are refused when they are added.

## What does not exist

- The **knowledge store** (N3), the **token governor** (N4) and the **credential broker** (N5); the context compiler and
  handoff packets of N2 are described below. Nothing here meters tokens or touches credentials.
- The scheduler does **not** use the ledger to choose tasks: it only enforces the completion rule. `fleet goals remaining`
  is a view; there is no `fleet goals` command without a subcommand.
- Check kinds from the design that are not implemented: test selectors and knowledge assertions. A command check can
  run a test runner with a selector.
- Claim checking of an agent's report against the repository, and checks of JS/TS references beyond what a
  `refcheck-json` check you configure does.
- Status changes by tasks starting or finishing: `in_progress` and `blocked` are set by an operator.

## Honest limits

What this protects against: a worker that plants files in its worktree (uncommitted, ignored, a Makefile, a helper script,
a local git configuration) to make its own check pass or to run code when the daemon verifies; a check that reads the
operator's home configuration; a process started by a check, a gate or an approved command that tries to approve, resolve
gates, add goals or attach evidence through the control socket; a worker that commits a path under `.fleet/`; a stale pass
(an older commit, another task, a run that was never recorded); a secret in a contract; a secret printed after a lot of
output; a hung check that would hold every job slot.

What it does not protect against: **hostile code that is part of the commit.** A check that runs `make test`, `cargo test`
or `npm test` runs the *committed* Makefile, build scripts and tests, which are the worker's, as your user, with network
access and every file your user can read. The export gives it no operator home, no state directory, no socket path and
no inherited secrets, and the origin check refuses it on the control socket and on the web port, but the same-user boundary is defence in
depth, not a wall: such a process can read the capability token of a daemon whose state directory it can find (for
example through `/proc`) and detach completely before it connects. Use a container, a VM or another user for real
isolation, as for workers. A weak check passes a weak result.

- The agent does not receive the contract (that is the context compiler's job, N2).
- Output is redacted by patterns; a secret that does not look like one is stored as printed.
- Descendants are found by sampling: a process that detaches and whose whole chain of parents is gone between two samples,
  or that starts after the last one, is not found; the group kill takes every process that stayed in the group. There is no
  cgroup or sandbox here.
- A daemon killed during a verification loses that run (nothing was recorded) and leaves its job directory, which the next
  start removes; the restarted daemon runs the contracts of every task still in review again.
- The `PATH` cut-down keeps world-writable and in-repository directories out, but does not prove that the directories it
  keeps hold only honest programs.

See the decisions in [docs/adr/0011](adr/0011-neo-domain-crates-and-application-module.md),
[0012](adr/0012-goals-and-evidence-are-event-sourced.md), [0013](adr/0013-verification-by-fleet-gates-completion.md) and [0014](adr/0014-checks-run-on-an-export-and-their-processes-are-untrusted.md),
and the vocabulary in [domain.md](domain.md).

# Milestone N2: the context compiler and handoff packets

What exists after N2, how it is checked, and what does not exist.

## What exists

- **`neo-context`**, a pure std-only domain crate: a deterministic compiler (no model, clock or randomness) that turns fleet's own
  state into a **context pack** of eight layers in a fixed order: invariants, charter, plan position, task and acceptance
  contract, verified knowledge, working set, recent events, scratch. Each layer has a hard token budget (a share of the
  profile's `max_tokens`); the estimate is `ceil(bytes x safety% / (chars_per_token x 100))` over UTF-8 bytes. When the pack
  is too large, layers 7, 6 and 5 give way in that order, then sections of the charter; layers 1, 3 and 4 are never cut and
  an overflow is an error, never a silent cut. The pack renders to a canonical text with length-prefixed, escaped framing,
  plus a canonical JSON manifest (tokens, SHA-256 digest, what was dropped per layer); the pack digest covers both.
- **Trust.** Layers 1-5 hold only text fleet or its operator wrote. Text an agent or a program wrote (notes, worker
  summaries, the output and summaries of checks, the worker's committed files) appears in layer 7 only, labelled
  `UNVERIFIED` and quoted line by line, or is left out by `[context] unverified = "exclude"`. Every string is sanitized and
  redacted, and a line that matches a secret rule is replaced whole; files whose name or content matches the secret rules
  are named, not shown. Layer 6 shows files as the repository has them at the run's base commit, read from the real
  repository, never from the worktree; *which* files it shows first follows the paths the task's commits changed, so layer 6
  is not part of the stable digest. Layer 5 facts that carry `paths` are scoped by the base commit's tree and the paths the
  task's contracts allow, never by what the worker committed.
- **What decides layers 1-5.** For the same plan, goal ledger, results of fleet's own checks and configuration they are the
  same on every attempt and after a crash (the *stable digest*); they change when that state does (layer 3 lists the plan and
  layer 4 what fleet's own runs said). A pack lists goals in the order of their ids, not in the order they were added.
- **Stored packs are checked.** `fleet context show`, `diff` and `verify` take the digest from the hash-chained log
  (`context_compiled`), never from file names or sequence numbers in the store, and re-hash the stored text (layer digests,
  stable digest, pack digest) and compare the stored manifest with the logged one. A mismatch, a pack the log recorded that the
  store lacks, or an unreadable file is a hard error with exit status 11 (`tampered`) and no content. A pack the store forgot
  by retention (the newest 8 per task are kept) is `not found`.
- **Handoff packets** (`HandoffCreated`): made when a task completes or fails, when it goes back in line (retry), when the
  daemon finds it running after a crash, and on demand (`fleet handoff <task>`). The next pack is compiled from fleet's state
  plus the handoff; a worker's `.fleet/handoff.json` summary is stored and shown only as UNVERIFIED. An on-demand packet is
  only the source of the next brief when no packet made by the flow exists. A task whose handoff cannot be recorded waits at
  most 5 ticks and is then failed with the reason.
- **Opt-in per run**: `context = "compiled"` in the plan's `[run]` table (or `fleet run create --context compiled`). Runs
  without it are briefed exactly as before. A compiled run's worker gets `.fleet/context.md` and `.fleet/context.json`
  and a short brief that points at them; `ContextCompiled` is appended to the hash-chained log. Fleet never reads those
  two files back.
- Commands: `fleet context show|diff|verify|explain` and `fleet handoff`.
- Configuration: `[context]` (`default_profile`, `unverified`, `knowledge`, `constraints`, `[context.glossary]`) and
  `[context.profile.<name>]` (`max_tokens`, `chars_per_token`, `safety_percent`); `[workers.<name>] profile`.

## What does not exist

- The **knowledge store** (N3): layer 5 is fed only from a static, human-written file with explicit sources, which must not
  be inside the run's repository.
- The **token governor** (N4) and the **credential broker** (N5). Token counts are estimates, not provider usage.
- The compiler makes the input identical and checkable; it does not make a model obey it, and no claim is made about
  hallucination. What is tested: determinism, layer order, budgets, layers 1-5 free of agent-written text, labelled
  UNVERIFIED text in layer 7, tamper detection of stored packs, and crash re-briefing.
- The secret rules are the commit gate's rules made case-insensitive plus more credential file names and directories; they are
  a filter, not a guarantee. Not recognised: unquoted `NAME=value` assignments, AWS secret keys, base64 or line-wrapped keys,
  a secret that is only a password in prose. Files are scanned up to 64 KiB.
- Someone who can write both the state directory and the audit log can still rewrite both: the log's hash chain and head
  file are tamper-evident, not tamper-proof (ADR 2).

## N2 status

N2 as built: everything above, the crash-safe handoff of ADR 18, the end-to-end tests in
`crates/fleet-cli/tests/e2e_context.rs` and the demo `scripts/neo-n2-demo.sh` (its output is in the README).

## Agent messages and the context compiler

Agent-to-agent messages (ADR 24 to 26) are not part of a context pack: the compiler reads fleet's own state (plan, goal
ledger, results of fleet's own checks, recorded events) and no message text enters layers 1 to 8, a verification contract,
an approval or any field a check or a status is computed from. What a worker is told about its channels and about the few
messages waiting for it is the `## Messages` section of the brief, built by the same code for a plain brief and for the
short brief that points at a compiled pack: every sender is labelled `from task <id> (unverified peer message)` (or
`from operator`), each subject is one flattened, quoted, bounded line, and the words of a message are read from
`.fleet/inbox/` only. Message ids and digests may be cited as provenance; their words are never evidence.
