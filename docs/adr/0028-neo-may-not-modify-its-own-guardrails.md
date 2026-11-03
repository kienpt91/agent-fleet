# 28. NEO may not modify its own guardrails

Status: accepted. Builds on ADR 13 (verification by fleet gates completion), ADR 14 (checks run on an export) and ADR 15
(one origin rule). Amends the diff check of ADR 14 with a deny list.

## Context
The next step for NEO is to develop its own repository: agents write the code of N3 to N5 and fleet verifies it with the
goal ledger and the contracts of N1. Everything N1 guarantees (a task completes only on evidence fleet recorded itself,
only the proven commit is merged, workers and everything the daemon started are kept off the control plane, secrets are
scanned and redacted) lives in files of this repository. If a task could change those files, the guarantees would hold
exactly as long as the agent chose not to touch them: a contract does not protect against a change to the code that
judges contracts, an approval does not protect against a change to what an approval is bound to, and a scanner that the
task may edit finds only what the task allows. The worker is also the same user as the operator and is not sandboxed, so
"the agent cannot reach that file" is not available as a defence.

## Decision
- **A list of protected paths exists and is data** (`dogfood/protected-paths.json`, with the reasons in
  `dogfood/PROTECTED_PATHS.md`): the approval and admission code, the verification and evidence code, the secret scanner
  and hooks, CI and dependabot, SECURITY.md, the examples that define commands, the dependency manifests and lock file,
  the toolchain and lint configuration, the architecture tests and the `dogfood/` directory itself, and the guardrail code
  that the later features brought: the SSH host key pinning and the remote host (ADR 27), the forge push and approval
  rules (ADR 20 to 22), the message policy, channel authorization and secret shapes (ADR 24 to 26), the fan-out pick
  (ADR 23), the context compiler's secret rules and the check of stored packs (ADR 17, 18), the web admission and the
  read-only workbench allowlist (ADR 19), and the scripts, image and CI job that run the test layers. Names that do not
  exist but would change how the repository is built, checked or steered (`.cargo/`, `.gitattributes`, `.omp/`, ...) are
  on it as guards.
- **A change to a protected path needs a person**: a commit authored by one, or an explicit human approval of the exact
  diff plus an independent security review. No task of the roadmap may change one. A task that needs one is **human-only**;
  the plan format has no such flag, so it is marked in the task, uses a worker that fails at once and a contract that no
  change can satisfy.
- **Every self-development contract forbids the list.** The `diff` check of a contract takes a `deny` list next to `allow`
  (`neo-verify`): a changed path that matches it fails the check whatever `allow` says, and a deny list alone is a
  constraint (a diff cut at 10000 paths is an error, never a pass). Canonical text and contract ids of contracts that
  only have `allow` do not change. The same list is checked a second time by `check-protected.sh` (case-insensitive,
  refuses symlinks, submodules and paths that git quotes), and the contract's other checks (formatting, lints, the layered
  and architecture tests, the secret scan, `Cargo.lock` unchanged, no measured number of the documentation changed) are the
  same for every task.
- **One narrow, explicit exception exists: a test-only change in one protected Rust file.** The trial runs that preceded this
  decision fixed flaky tests that live inside `crates/fleet-infra`, which the list protects as a whole. A contract template
  (`flaky-test`), rendered by hand for one named file, replaces the glob deny check by a script check that excepts exactly that
  path (and refuses a build script, a manifest, the architecture tests, links and odd names), checks that everything above the file's
  test module is byte-identical to the base, that the test is still there and that it survives 25 runs under load. This is the
  second route of the principle: a human approves the exact diff. It is never part of the roadmap's goals.
- **Merge conflicts are checked for too.** fleet's commits are authored `fleet <fleet@localhost>` and taken into the branch
  you publish by `git cherry-pick -n` under your own identity; a conflict leaves markers. Every contract runs
  `check-conflict-markers.sh --tree`, and the procedure runs `--git` before the commit, which is the case only a person can catch.
- **The checkers are not the repository's.** A contract program may not live in the repository, so the scripts a contract
  runs are installed from a reviewed commit into a directory outside it, and the container wrapper is the operator's own.
- **A human lands every merge.** `merge` and `external-command` stay sensitive actions; the run's integration branch
  is reviewed and merged into the real branch by a person.

## Consequences
- The roadmap is slower than it could be: wiring a new crate into events, use cases, adapters or the CLI is always a
  person's work, and so is every dependency change. Agents write pure domain modules and documents, one named file per task.
- The list is a tripwire for the people who maintain it, too: tests (`crates/fleet-infra/tests/dogfood.rs`) fail when an
  entry matches no file, when a guard starts to exist, when a contract's deny list differs from the list, and when a
  human-only task has a contract that could pass.
- What it does not do: it judges committed work. A same-user worker can still edit files outside its worktree, so the real
  runs belong on a throwaway clone, in a container or under another OS user (README, Limitations). It does not review the
  content of the files a task may change, and a deny list that is too short is a mistake of whoever wrote it, which is why a
  test compares it with the list.
- The agent runs in `--approval-mode write`: headless it cannot run a shell command, so it edits blind and fleet's own contract is the
  whole proof (observed in trial runs that predate this scaffold; two real tasks completed with every check passing). That is the safe
  default and the reason the checks must live outside what the agent can touch. **The scaffold itself has not been run with a real
  agent**; only the scripted dry run, the shell tests and the consistency tests exist.
- The list was written before those features landed and was extended when they did; a guardrail file that a later feature adds is
  on it only after a person adds it, and the consistency test (`crates/fleet-infra/tests/dogfood.rs`) only checks that what is listed exists.
- The ADR number is 28 because `main` took 17 to 27 (context compiler, workbench, GitHub, fan-out, messages, SSH) while this was written.
