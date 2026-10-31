# 13. Fleet runs the contract itself, and completion depends on it

Status: accepted

## Context
A worker that says "done, all tests pass" has said nothing fleet can rely on. The design requires that
no goal becomes verified, and no task with acceptance criteria completes, on an agent's statement alone.

## Decision
- **The rule.** A task that a goal (not abandoned) with a contract names is *governed*. In review, after
  the worker's output is committed, fleet runs every such contract in the task's worktree *before* the
  quality gates. Only if every mandatory check passed does the review go on to the gates and then
  complete. A failure is a failed gate: it uses the retry budget and its failing checks, quoted line
  by line, go into the next brief (the existing failure loop). What the worker reported plays no part.
  `apply_gate_results` enforces the same rule a second time: results that would complete a governed task
  without a recorded pass are dropped (this covers a goal added while the gates were running). A worker
  report that is not backed by evidence is shown as `unverified`.
- **Order of contracts and gates.** Contracts first: they are the task's own acceptance criteria and cheap
  to explain; the gates are generic. A task whose contract fails never spends time in the gates.
- **How checks run.** A `VerifyJob` goes through the existing `JobRunner` port, on a job thread, with the
  rules of approved commands and gates: no shell, the program an absolute path that is not inside the
  task's worktree or its repository (`trusted_program`), the *worker* environment allowlist and nothing
  else (no inherited secrets, no daemon variables), no stdin, their own process group killed on timeout and
  at the end, a timeout per check (default 120 s, at most 1 h) and 1 h for the whole job, captured output
  capped at 4 MiB, output redacted before it is judged and again before it is stored. File assertions open
  the file below the worktree root with `O_NOFOLLOW` at every component (`open_beneath`) and read at most
  1 MiB. The diff constraint is judged from the changed paths git reported (measured once, before the
  commands run, so a test that writes artifacts does not count as the worker's change).
- **Judging is pure.** The adapter observes (exit kind, output, file facts, changed paths); `neo_verify::judge`
  decides. `refcheck-json` reuses `evaluate_refcheck`, the judgement of the quality gate. A check that
  could not be evaluated is an `error`, never a pass.
- **No regular-expression engine.** Path globs (`*`, `?`, `**` as a whole segment; no classes, braces,
  escapes) are matched by a segment-level automaton with the greedy single-restart wildcard algorithm for
  one segment: no backtracking blow-up, worst case `segments * path * pattern`, all three capped. It is
  tested against an exponential reference matcher on random inputs.
- **Manual runs.** `fleet verify <goal|task>` starts the same job with origin `manual`: it records
  evidence and settles goal status, but never completes, fails or retries a task.

## Consequences
A governed task is only as trustworthy as its contract: a weak check passes a weak result, and a command
check that runs `make test` runs the *worker's* Makefile as you (the program is trusted, what it executes
is not). Contracts do not replace sandboxing; see SECURITY.md. A verification that cannot start (the
worktree cannot be read, no job slot for ever) fails the task or waits; it never completes it.
