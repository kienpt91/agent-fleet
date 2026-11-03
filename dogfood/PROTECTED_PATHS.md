# Protected paths: what NEO may never change by itself

**The principle: a system must not be able to loosen its own guardrails.** If the agent that writes
the code can also edit the code that approves it, the check that verifies it, the scanner that looks for
its secrets, the pipeline that tests it and the policy that describes all of them, then every one of
those protections lasts exactly as long as the agent chooses to leave it alone. So the files below are
off limits to every task of the self-development roadmap. A change to any of them needs **one of**:

1. a commit **authored by a person** (a person's own clone, a person's own review), or
2. an **explicit human approval of the exact diff** (read the diff, not the agent's summary of it)
   **plus an independent security review** of that diff by someone other than the author, who is
   not the agent that proposed it.

Nothing in the list is secret and none of it is a judgement that the agent is malicious: a worker
that is merely wrong, or that was fed hostile text by a file it read, is the case this protects against.

The machine-readable form is [`protected-paths.json`](protected-paths.json) (one entry per line, in the
glob dialect of the `diff` check of a verification contract: `*` and `?` inside a path segment, `**` as a
whole segment, case-sensitive). The table below is written from it; a test
(`crates/fleet-infra/tests/dogfood.rs`) fails when the two differ in a path, when an entry matches no file
(unless it is marked a *guard*, a name that does not exist today but must never be created by a task) and when
a contract's deny list is not exactly this list. Every entry has its reason in one line.

## What is protected, and why

Entries whose reason starts with "Does not exist today" are guards: they keep a task from *creating*
the file, which would change how the repository is built, checked or steered.

### Governance: CI, policy, scanner and the images checks run in

| Path | Why |
| --- | --- |
| `.github/**` | CI workflows and dependabot are the independent second judge of every push; weakening them removes it. |
| `.github/workflows/**` | The CI jobs, among them ssh-tests, which runs the sshd tests with a missing server as a failure; a job removed or made optional removes a gate. |
| `SECURITY.md` | The threat model and the dependency policy state what is promised; a task must not edit the promise it is held to. |
| `scripts/**` | The secret scanner, the layered test runner, the coverage script and the demos define what 'tests pass', 'coverage met' and 'no secrets' mean. |
| `scripts/test-layers.sh` | Defines which suites make up each test layer, including the remote layer that must run when an sshd exists; a dropped suite is a silent pass. |
| `scripts/ssh-env.sh` | Starts the throwaway sshd of the remote tests and sets FLEET_SSH_TESTS=1 so that a missing server fails the test instead of skipping it. |
| `scripts/coverage.sh` | Defines how coverage is measured per layer and which code is left out of the measurement. |
| `docker/**` | The development images (pinned by digest) are the environment every check runs in. |
| `docker/Dockerfile.ssh` | The image with an OpenSSH server that the remote tests run against; changing the server changes what host key pinning is proven against. |
| `docs/adr/**` | The decisions of record about the guardrails; a task must not rewrite the reasons it is bound by. |
| `examples/fleet.example.toml` | A daemon configuration names the commands fleet runs; examples are copied into real configurations. |
| `examples/agents/**` | Scripts that fleet starts as workers and gates in the demos and the end-to-end tests. |
| `examples/neo/**` | Example verification contracts, used by the demo and read as the model for real ones. |
| `examples/fanout-contract.json` | The example verification contract of the fan-out demo, read as a model for real ones. |
| `examples/github/**` | Recorded gh answers that the GitHub demo serves through a fake gh; examples are copied into real set-ups. |

### The guardrails of self-development itself

| Path | Why |
| --- | --- |
| `dogfood/**` | This list, the contracts, the plan, the runbook and the check scripts: the guardrails of self-development must not be editable by what they guard. |

### Git and secrets

| Path | Why |
| --- | --- |
| `lefthook.yml` | Runs the secret scanner before every commit and push. |
| `.secret-scan-allow` | One added line silences a scanner rule for a file. |
| `.gitignore` | Holds the block of secret file names that must never be committed. |
| `.gitattributes` | Does not exist today; attributes select filters, diff and merge drivers for git. |
| `.gitmodules` | Does not exist today; a submodule pulls foreign code into the tree. |

### Build, toolchain and dependencies (a dependency change needs a person)

| Path | Why |
| --- | --- |
| `.cargo/**` | Does not exist today; cargo configuration can replace registries, set rustflags and define aliases. |
| `rust-toolchain.toml` | Pins the compiler and the clippy and rustfmt components. |
| `rustfmt.toml` | Does not exist today; formatter configuration changes what fmt --check accepts. |
| `.rustfmt.toml` | Does not exist today; same as rustfmt.toml. |
| `clippy.toml` | Does not exist today; lint configuration changes what clippy -D warnings accepts. |
| `.clippy.toml` | Does not exist today; same as clippy.toml. |
| `Cargo.toml` | The workspace members, the pinned direct dependencies and the lint levels (unsafe forbidden, unwrap denied). |
| `Cargo.lock` | The resolved dependency set; every version must pass the 7 day age check, which needs a human. |
| `crates/*/Cargo.toml` | Per-crate dependencies, the layering and the inherited lint levels (also of crates added later). |
| `crates/*/build.rs` | Does not exist today; a build script runs code at compile time. |
| `crates/fleet-domain/src/lib.rs` | Crate attributes: forbid(unsafe_code) and the test-only lint relaxations. |

### Instructions that steer the agent

| Path | Why |
| --- | --- |
| `.omp/**` | Does not exist today; project rules, extensions and MCP servers of the agent CLI would steer every later task. |
| `.mcp.json` | Does not exist today; an MCP server definition gives the agent new tools in every later task. |
| `AGENTS.md` | Does not exist today; instructions that agent CLIs load into every later task. |

### Approval, admission and the rules of the state machine

The code that decides who may approve, who counts as a worker, what a task needs to complete or merge,
what is passed to children and how names are validated. The whole application, adapter and CLI layers
are on the list as directories (`crates/fleet-app/**`, `crates/fleet-infra/**`, `crates/fleet-cli/**`):
they hold the job origin rule (ADR 15), the control socket and the web admission, the approval digest,
the completion rule, the safe file reads, the git hardening and the daemon wiring, and the end-to-end
and security tests that pin all of that. The crates and modules that arrived later are on the list as
well: the SSH remote worktrees (host key pinning, the remote host, the remote command line, ADR 27), the forge
(`gh` as the only transport, the pull request digest and the push rule, ADR 20 to 22), fan-out and its pick
approval (ADR 23), the context compiler with its secret rules and the check of stored packs (ADR 17, 18), messaging (channel authorization, loop limits, secret-shaped text, ADR 24 to 26), and the read-only
web workbench with its Markdown allowlist (ADR 19). Several of these also have an entry of their own inside a
directory that is protected already (for example `crates/fleet-infra/src/ssh/known_hosts.rs` inside
`crates/fleet-infra/**`): the directory entry is the net, the named entry carries the reason, and a later
narrowing of the directory would not silently release it.

| Path | Why |
| --- | --- |
| `crates/fleet-domain/src/approval.rs` | Approvals: the digest binding, expiry and consume-once rules. |
| `crates/fleet-domain/src/decision.rs` | Decision gates: what only a person may answer. |
| `crates/fleet-domain/src/fleet.rs` | The fleet state: which approvals and gates exist and which actions they allow. |
| `crates/fleet-domain/src/run.rs` | The task state machine: what a task needs before it completes or merges. |
| `crates/fleet-domain/src/supervision.rs` | The origin rules (who counts as a worker or a job), process identity and kill escalation. |
| `crates/fleet-domain/src/env.rs` | The environment allowlist: what workers, gates and git inherit. |
| `crates/fleet-domain/src/ids.rs` | Validation of every name that reaches a path, a ref or a command line. |
| `crates/fleet-domain/src/limits.rs` | The hard caps that keep memory and inputs bounded. |
| `crates/fleet-domain/src/brief.rs` | Argument placeholders and the quoting that keeps worker text from posing as fleet's own words. |
| `crates/fleet-domain/src/contract.rs` | The worker contract: which files a worker may write in .fleet/ and how they are read. |
| `crates/fleet-domain/src/remote.rs` | Remote hosts: what a host description may contain, where a remote worktree lives and the one place a remote command line is assembled. |
| `crates/fleet-domain/src/msg/**` | Message policy: channels declared by the operator and never by a worker, loop and storm limits, rate limits and the refusal of text shaped like a secret. |
| `crates/fleet-domain/src/repo/**` | The read-only workbench rules: validated repository paths, the Markdown element and attribute allowlist the web renders, and the diff limits. |
| `crates/fleet-forge/**` | The forge model: the pull request digest an approval is bound to, where a branch may be pushed, the rate gate and the rules for untrusted issue text. |
| `crates/fleet-fanout/**` | Fan-out groups: the state machine, the comparison and the ranking rule; a pick is a merge and needs its own approval. |
| `crates/fleet-app/**` | The use cases: the completion rule, merge only of the proven commit, approval handling, the NEO module, the port contract suites. |
| `crates/fleet-app/src/remote.rs` | Remote tasks: the host port and the session host; a task on a host gets no message channel and its handoff is read back through it. |
| `crates/fleet-app/src/fanout/pick.rs` | fleet pick, the one way a group's result reaches the integration branch: an approval bound to the digest of the exact merge. |
| `crates/fleet-app/src/forge/**` | The forge use cases: a pull request is opened only under an approval bound to its digest, and one executor defines what a request means. |
| `crates/fleet-app/src/orchestrator/messaging.rs` | Decides and delivers messages under the policy of the channel and the daemon; the daemon is the only party that writes an inbox. |
| `crates/fleet-infra/**` | The adapters: job origin, control socket and web admission, git hardening, safe file reads, the export and check runner, the event log, the daemon wiring and their tests. |
| `crates/fleet-infra/src/ssh/**` | The OpenSSH adapter: the fixed client command line, host key pinning, quoting of the remote command and the verification of what a host returns. |
| `crates/fleet-infra/src/ssh/known_hosts.rs` | Host key pinning: one known_hosts file per host, written only by fleet host trust, holding exactly the key the operator confirmed. |
| `crates/fleet-infra/src/ssh/trust.rs` | Learning a host's key: it only reports what the host presents; a person at a terminal confirms the fingerprint before anything is pinned. |
| `crates/fleet-infra/src/hosts_file.rs` | Edits the hosts tables of fleet.toml for fleet host add and remove; a host entry decides where a worker's command runs. |
| `crates/fleet-infra/src/git/push.rs` | The push of a task branch: the remote URL is read again and compared with the repository the approval names before anything is sent. |
| `crates/fleet-infra/src/git/remote_sync.rs` | The local half of the remote worktree protocol: the one-commit bundle that goes out and the checks on what comes back. |
| `crates/fleet-infra/src/forge_gh.rs` | The only transport to GitHub is gh with a minimal environment: fleet never sees, stores or passes a token. |
| `crates/fleet-infra/src/msgfs.rs` | The outbox and inbox directories of a worktree: every path below the worktree is untrusted and only the daemon writes an inbox. |
| `crates/fleet-infra/src/web/**` | The web adapter: Host and Origin checks, tokens, sessions, CSRF, rate limits, the refusal of worker processes and the read-only repository routes. |
| `crates/fleet-infra/web/**` | The pages the dashboard serves, among them the DOM builder with its element and attribute allowlist for rendered Markdown. |
| `crates/fleet-infra/tests/ssh_remote.rs` | The tests against a real sshd: pinning, a changed key, quoting through a real login shell, hostile bundles; weakening one weakens the pinning claim. |
| `crates/fleet-infra/tests/dogfood.rs` | The consistency test of the self-development data: it fails when the protected list, the contracts and the plan drift apart. |
| `crates/fleet-cli/**` | What the operator sees when approving (digest and exact action), the commands, the attach filter, the end-to-end and security tests. |
| `crates/fleet-cli/tests/e2e_remote.rs` | The end-to-end remote tests through the real binary and daemon, including the refusal of message channels for a remote task. |

### Verification, evidence, the ledger, redaction and the architecture tests

| Path | Why |
| --- | --- |
| `crates/fleet-domain/src/events.rs` | The events the log records and replay applies, including the goal and evidence events. |
| `crates/fleet-domain/src/chain.rs` | The hash chain that makes the audit log tamper evident. |
| `crates/fleet-domain/src/value.rs` | Canonical JSON: the exact bytes that approval digests, the hash chain, contract ids and pack digests are computed over; a change makes them ambiguous. |
| `crates/fleet-domain/src/quality.rs` | How a gate result is judged, including the refcheck-json rules. |
| `crates/fleet-domain/src/redact.rs` | Secret redaction before anything is stored. |
| `crates/fleet-domain/src/sanitize.rs` | Removes escape sequences and invisible characters from worker output. |
| `crates/fleet-domain/src/output.rs` | Bounded capture of worker output through the sanitizer and the redactor. |
| `crates/neo-verify/**` | Verification contracts, the glob matcher, judging and evidence: the rules for what counts as proof. |
| `crates/neo-goals/**` | The goal ledger: when a goal is verified and which evidence counts. |
| `crates/neo-context/**` | The context compiler: which layers may hold agent text, the secret rules, the budgets and the digests that stored packs are verified against. |
| `crates/neo-context/src/secret.rs` | The secret rules of the compiler: file names and contents that a pack names but never shows. |
| `crates/neo-context/src/pack.rs` | The canonical text, the manifest and the digests of a pack: what fleet context verify computes again. |
| `crates/fleet-app/src/neo/context*.rs` | Compiling at dispatch, the handoff flow and the commands that verify a stored pack against the hash-chained log (exit 11 on a mismatch). |
| `crates/fleet-infra/src/context_store.rs` | Stored context packs and handoffs on disk: the files that are re-hashed and compared with what the log recorded. |
| `crates/fleet-cli/tests/architecture.rs` | The architecture tests: layering, no I/O in the domain, no new dependency kinds, forbid(unsafe_code) everywhere. |

## What a task may change (and why that is enough to be useful)

Everything not listed, and nothing the contract of the task does not allow: every contract of
[`contracts/`](contracts/README.md) also carries a `scope` check that allows only the file (or the
documents, or the `src` and `tests` of one new crate) the task is about. Today that is, for example,
`crates/fleet-domain/src/{dag,error,status}.rs`, README.md and the documents directly in `docs/` (their
measured numbers are guarded by a separate check), new documents, and the modules of crates that a person
scaffolded for the milestones: `crates/neo-knowledge/src/**`, `crates/neo-governor/src/**`,
`crates/neo-broker/src/**` (each crate's `Cargo.toml` stays protected through `crates/*/Cargo.toml`, so a
task can never add a dependency). Anything that needs a protected file, such as registering a new crate,
wiring it into the daemon or adding an event, is a **human-only task** ([`human-only.md`](human-only.md)).

## Reviewed exceptions: a test-only change in a protected source file

The list is broad on purpose (`crates/fleet-infra/**` as a whole), and the work that was actually run on this repository before the
list existed was a fix of a flaky test inside two files of it ([trials/](trials/README.md)). Forbidding that wholesale would make
the list unusable for the commonest maintenance task, and loosening it would defeat it. The way out is the second route of the
principle: **a human approves the exact diff**, with the machine narrowing what that diff can be.

- The only such route is the contract template `contracts/flaky-test.json`, rendered by hand for **one named Rust file**
  (`crates/<crate>/src/**.rs` or `crates/<crate>/tests/**.rs`; never a build script, a manifest or the architecture tests).
- It replaces the glob deny check by `check-protected.sh --except <that file>` (exact path, printed as a REVIEWED EXCEPTION, symlinks and
  odd paths still refused) and adds `check-test-only.sh` (everything above the file's first test module is byte-identical to the base),
  a `kept` check (the test is still there) and a stress loop.
- It does **not** make the file less protected for any other task, and it is never part of the roadmap's goal list: you render it, for
  one task, on purpose. What it cannot see is production code placed after a test module: that is the diff you read before you approve
  the merge, and for `crates/fleet-infra` the independent security review you owe the change anyway.

## How the list is enforced

Four layers, each independent of the others:

1. **The `protected-diff` check of every contract** (the `diff` check with a `deny` list, run by fleet
   itself on the committed work): `git diff <run base> <task head>` of the real repository, without rename
   detection, so a rename away from a protected path lists the old path. A path that matches fails the task,
   whatever else it did. This is the authoritative layer; a diff too large to list is an error, never a pass.
2. **`check-protected.sh`** ([`check-protected.sh`](check-protected.sh), a command check of the same
   contract and the tool for the person who reviews). Stricter than layer 1: case-insensitive (a
   case-insensitive checkout maps `Security.md` to `SECURITY.md`), refuses paths git has to quote (newline,
   tab, quote, backslash, non-ASCII), a segment with a leading or trailing space, a `.git` segment, and any
   symlink or submodule added or changed (a link can point into a protected path). It runs from a copy that
   lives outside the repository (`install-tools.sh`), so a task cannot edit the checker it is judged by.
3. **The human at the merge.** `fleet merge` stays an approval bound to the exact commit; run
   `check-protected.sh` on that commit yourself and read the diff.
4. **The tests and CI that are themselves on the list**: the architecture tests (layering, `forbid(unsafe_code)`,
   no new kind of dependency), the secret scanner on the tree and the history, and the pipeline.

## What this does not do

- It does not make the worker harmless. The worker is the same user as you and is not sandboxed (README,
  Limitations): it can edit files outside its worktree, among them the main checkout and the shared `.git`
  directory. The contracts judge what the task *committed*; they cannot judge what a process did to your
  files. That is why RUNBOOK.md runs this on a throwaway clone and keeps the check scripts outside the repository.
- It does not protect a path that is not on the list. New kinds of guardrail files must be added by a person
  (and to `gen-contracts.sh`'s output) before they exist; the guards are a start, not a proof.
- It does not review the *content* of allowed files. A task can write a wrong test or a misleading document in
  the one file it may change; the contract proves the checks passed, a person reads the diff.
- The list protects itself only through the `dogfood/**` entry in a contract that was rendered from it by a
  person. A contract is data in the daemon's event log: a mistake there (a deny list that is too short) is a
  mistake of the person who rendered it. Regenerate with `gen-contracts.sh` and let the test compare.
