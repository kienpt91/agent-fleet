# 27. SSH remote worktrees: only the worker's process is remote, and nothing the host says is believed

Status: accepted. Builds on ADR 4 (output is untrusted), ADR 6 (git is hardened), ADR 7 (configuration is daemon side), ADR 14
and 15 (jobs are untrusted, one origin rule) and ADR 16 (evidence is about the repository). Phase 3c of the roadmap.

## Context
A task should be able to run its worker on another machine (a bigger one, one with the right toolchain) without fleet giving
up what it guarantees for local workers: the output is sanitized, bounded and redacted; the worker's environment is explicit;
the commit that is gated, verified and merged is one fleet itself read from its own repository; nothing the worker or the
machine says decides anything. The transport has to be the system's OpenSSH client (no library, no new dependency, the
operator's own audited tool), and it must not inherit anything from the operator's ssh setup: agent, config, known hosts,
default keys.

## Decision
**Only the process is remote.** The task keeps its local worktree, branch, gates, verification contracts, approvals and merge.
`RemoteSessions` (fleet-app) is a `SessionHost` around the ordinary pty session host. For a task that names a host it runs
four steps through the `RemoteHost` port:

1. *Prepare.* fleet makes a **root commit with exactly the tree of the task branch's head** (no history, no other branch), keeps
   it under a quarantine ref and writes it to a git bundle (at most `max_bundle_mib`). The bundle streams over ssh stdin to a
   script that creates `<base_dir>/<run>/<task>` as a fresh repository from it. The brief goes the same way.
2. *Launch.* The adapter returns the **local** command `ssh -tt ... <remote command>`; the pty session host runs it. So the
   sanitizer, the ring buffer, redaction, the output bounds, idle and total limits, kill escalation and the origin rule apply to a
   remote worker unchanged: the remote terminal is just the output of a local process.
3. *Collect* (after the local client ended; on a helper thread, the orchestrator never waits for a host). The host commits what
   the worker left (never `.fleet`) and sends a bundle of the new commits; the worker's `result.json`/`question.json` come through
   bounded reads into the local `.fleet/`. The bundle is **never trusted**: it must name exactly the one ref the protocol
   uses; it is fetched into a **scratch bare repository** that holds only the commit that was sent and the incoming objects, where `git fsck` checks them (git does not check the objects of a bundle by itself; checking in the scratch repository means old malformed objects of your own repository cannot refuse an honest host and a refused bundle leaves nothing behind); only then is the verified tip fetched into the quarantine ref of the real repository; the tip must descend from the commit that was sent; no changed path may be `.git`-like or
   under `.fleet`; commits, changed paths, files and bytes are bounded; the local worktree must be clean and the task branch
   unchanged since the export. **Only the verified tree is used**: the task branch gets one new commit authored by fleet with that
   tree on top of the old head. Commit objects, messages and authors of the host never enter local history.
4. *Clean up.* The worker's process group is killed first (it can ignore the hang-up that ending the terminal causes), the
   remote worktree and the process-id file are removed, the quarantine refs are dropped. What cannot be done (host down) is listed
   in `<state>/remote/live/` and retried at the next daemon start, with a report in the activity feed.

**The ssh client is a fixed argument list.** `ssh/argv.rs` is the only place that writes ssh options: `-F /dev/null`, strict host
key checking against one pinned file, `GlobalKnownHostsFile=/dev/null`, `IdentitiesOnly` with the host's key file, `IdentityAgent=none`,
`BatchMode`, no password methods, `ForwardAgent/X11=no`, `ClearAllForwardings`, `PermitLocalCommand=no`, `ControlMaster=no`,
timeouts and keepalives, `LogLevel=ERROR`. The binary is resolved once to an absolute path (relative `PATH` entries are skipped;
a binary inside a git work tree, below `.fleet-worktrees` or writable by others is refused). A jump host is configured by name and
the hop is a second client with its **own** key and pin, generated as a `ProxyCommand` of validated, shell-inert words:
`ProxyJump` would start the hop with default settings (the user's known_hosts and keys). Host, user, port and paths are validated
to narrow character sets before they are used.

**The remote command line is built from a closed set.** `fleet_domain::remote::RemoteCall` has ten variants and no way to pass a command
line. Each is `/usr/bin/env -i <explicit variables> <program> ...` where the program is `git` (only to print its version), the
worker command the operator listed for that host (`allowed_workers`), or `/bin/sh -c <constant script>` with data as positional
parameters. Every word goes through one `quote` function (property-tested against a lexer and, in the sshd tests, against
the real login shell). `fleet host doctor` round-trips hostile text through the host's login shell and refuses a shell that does
not read POSIX quotes.

**Host keys are pinned by a person.** `<state>/hosts/<name>.known_hosts` (0600) holds exactly one plain key. It is written only by
`fleet host trust`, which scans the host (a client that accepts the key into a scratch file and offers no credentials), shows the
fingerprint, requires a terminal on stdin and stdout and the word `yes`. Every other connection is strict: an unpinned host makes
no connection at all; a changed key is refused with a message that says so.

**The environment of a remote worker is the host's explicit values.** `[hosts.<name>.env]` plus `FLEET_RUN`, `FLEET_TASK`,
`FLEET_WORKTREE`, `PATH` and `TERM`. Nothing is copied from the daemon's environment; the worker's `env_pass` and `env_set` are local
settings and are not sent; nothing travels through `SendEnv`/`AcceptEnv`.

**One origin rule still.** Non-terminal calls (prepare, collect, signal, clean-up, doctor) run through `ssh/exec.rs` with the shared
`JobTracker`: process group registered, descendants followed by the sweeper, refused on the control socket and the web port. The
terminal client is a live session like any worker.

## Consequences
- The remote history of a worker is lost: one local commit per attempt, with the verified tree. That is deliberate: it is the only
  part of a bundle that fleet can check by itself.
- A bundle of a large repository's *tree* is the price of an attempt (bounded, `max_bundle_mib`, default 256 MiB). Nothing is cached
  between attempts.
- Preparing runs synchronously on the engine thread. It is bounded by one 120 s budget for the whole of sending (silence limit 60 s; stderr does not count as progress), and a host that timed out or could not be reached is skipped for 120 s. That is a bound, not a fix: a hostile host can still hold the daemon for 120 s per two minutes. Collecting and signalling are asynchronous. Moving the transfer to a helper thread (a `starting` state for the task) was considered and not done in this phase.
- Variables of a remote worker travel in a 0600 file that the launcher reads and deletes, never on a command line.
- A worker that writes a question and keeps running is noticed when it ends or is stopped by its limits, not while it runs (the
  question file is on the host until collected).
- Quality gates and verification contracts run locally, on the verified commit, in the local worktree and in an export. A remote
  `exit 0` or a remote `result.json` is a claim, as local ones are.
- Not verified: macOS as the local or remote system, real accounts, real agents, hosts whose login shell is not POSIX (refused by
  the doctor), OpenSSH older than 7.6 (refused by the doctor), servers with `ForceCommand`.

Tests: `fleet-domain/src/remote.rs` (quoting and the closed set, property tests), `fleet-infra/src/ssh/*` (argv, binary, executor,
pins), `fleet-infra/src/git/remote_sync.rs` (hostile bundles), `fleet-infra/tests/ssh_remote.rs` (a real sshd: pinning, wrong key,
forwarding, environment, quoting through the login shell, kill escalation, keepalive, jump host, origin, the contract suite),
`fleet-cli/tests/e2e_remote.rs` (the binary and the daemon end to end), `fleet-app/tests/remote.rs` and `fleet-app/src/remote.rs`.
