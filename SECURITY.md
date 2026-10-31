# Security

`agent-fleet` is an example project. **It has not been audited and makes no production claims.** It runs
commands you configure (coding agents, quality gates, approved commands) with your user's permissions
and it manages git worktrees. Read this file before pointing it at anything you care about.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting: on the repository page open **Security**, then
**Report a vulnerability**. If that is not available, write to kienphan.dev@gmail.com. Do not open a
public issue for a vulnerability. This is a hobby-scale project; expect a reply within a few days, not an SLA.

## Platforms

Linux and macOS. Windows is not supported (ptys, process groups, unix sockets and `/proc` assumptions).
The test suite and all numbers in the README were produced on Linux in a container. The macOS code paths
(`LOCAL_PEERCRED`/`LOCAL_PEERPID` peer checks, `ps`-based process identity, pty behaviour, and the `lsof`-based lookup of
the process behind a dashboard connection) type-check (`cargo check --target aarch64-apple-darwin`) but have **not been
run**.

## Threat model

Who and what is trusted:

| Party | Trust |
| --- | --- |
| The operator (the user running `fleet`) | Trusted. Decides approvals, answers gates, writes `fleet.toml`. |
| `fleet.toml`, plan files, task specs | Trusted as the operator's own input, but validated and size-capped. Configuration names commands: treat it as code. It is read once at daemon start from the daemon's side, never from a repository. |
| Worker commands (coding agents) | **Untrusted.** Not sandboxed (see limitations). Their terminal output, their files in `.fleet/`, their git objects and their repository contents are hostile input. |
| Repositories handed to a run | Untrusted for execution: hooks, config, filters and attributes inside them are treated as attacker-controlled. |
| Other users on the machine | Untrusted. The state directory is 0700, the socket 0600 and the peer's uid is checked. |
| Same-user processes that are not workers | Not defended against (they can do what the operator can). |
| Web pages in the operator's browser (other origins, other tabs) | **Untrusted.** They can send requests to `127.0.0.1` but have neither the token nor the CSRF token, and Host, Origin and fetch-metadata checks refuse them. |
| Clients of the web dashboard (`--web`) | The operator (a browser tab or a script that holds the web token) is trusted like the operator on the control socket. Everything else on the network path is untrusted: there is no TLS. |

What the design tries to guarantee, and where it is tested:

| Property | How | Tests |
| --- | --- | --- |
| A worker is kept off the control plane (best effort, see limitations) | Only the daemon creates approvals (merges, or from a worker's `question.json`); decisions need a control request. Three independent checks: (1) a per-run 256-bit capability token in `control/token` (0600, fresh at every daemon start) that every request except `ping` must carry, compared in constant time; (2) at accept, before any request is read, the peer process must be identified and must not be a worker, a descendant of one, or anything else the daemon started (quality gates, approved commands, git, the undo tool, and the process groups they still occupy): an unknown pid, a process that is already gone or an unreadable ancestry all deny (fail closed), the decision is bound to the connection and the request is checked again; (3) workers get an allowlisted environment with no socket path, state directory or token, and `XDG_STATE_HOME` is forced to a per-task directory so a habitual `fleet` call finds no daemon | `fleet-infra::server`/`client`/`daemon` tests (token, fail-closed admit, pipelined requests), `e2e_security.rs` |
| An approval covers one exact action, once | SHA-256 digest of (tool, command, arguments, task id); the digest is recomputed at use. Merge approvals include the exact commit and the merge is performed on that commit (the branch is verified to still point at it, otherwise nothing is merged). An approval is consumed when the action *starts* (recorded before the command is launched), so a daemon restart never runs it again; a command that was running when the daemon stopped is reported as `interrupted, outcome unknown`. Worker-requested commands must be absolute paths that lie outside the worktree and its repository; their working directory is the worker's worktree (see limitations). Approvals expire | `fleet-domain::approval`, `fleet-domain::fleet`, `fleet-app/tests/{merging,decisions,recovery}.rs`, `fleet-infra::git`, `fleet-infra::jobs`, `e2e_security.rs` |
| Worker output cannot attack the terminal or the logs | Streaming sanitizer: every escape sequence removed, controls and invisible Unicode made visible, `\r` becomes a line break; separate passthrough filter for `attach` that drops OSC/DCS/APC/queries; redaction of key shapes, configured rules and secret-looking daemon variables before storage, applied to terminal output, quality-gate output, approved-command output and the text of a worker's `result.json`/`question.json` (summary, question, choices). The exact command and arguments of an approval request are not altered (the digest binds them) and are shown to the person deciding. Worker text that reaches another agent's brief is flattened or quoted so it cannot pose as fleet's own statements | `fleet-domain::sanitize`, `redact`, `output`, `brief`; `fleet-infra::pty`, `jobs`, `contract_fs`; soak test |
| Memory stays bounded | Ring buffer per session with a hard cap; line cap; caps on messages, frames, files, connections, sessions, runs, tasks, notes, gate output, event log size | `fleet-domain::output`, `fleet-infra/tests/soak.rs`, limits tests in every adapter |
| Reading worker-written files cannot hang or leak | lstat + `O_NOFOLLOW|O_NONBLOCK` + fstat re-check + size cap; `.fleet` opened relative to the worktree with `openat`; FIFOs, symlinks, directories and oversize files are rejected | `fleet-infra::fsx`, `contract_fs`, `e2e_security.rs` |
| git never runs code planted in a worktree | Empty environment + allowlist; `GIT_CONFIG_GLOBAL/SYSTEM=/dev/null`; `-c` flags that disable hooks, fsmonitor, pager, editor, ssh, credential helpers, signing; per-key overrides for filters, textconv, merge drivers, pagers; `--no-verify`; names validated before they reach a command line | `fleet-infra::git` (one test per vector with a sentinel env var and marker file), `e2e_security.rs` |
| Isolation of the user's checkout | Work happens on `fleet/<run>/<task>` branches in `<repo>/.fleet-worktrees`; the user's branch, index and working tree are never touched; the integration branch has its own worktree | `fleet-infra::git`, `e2e_flow.rs` |
| A task's acceptance criteria are checked by fleet, not by the worker (NEO N1) | A task that a goal with a verification contract names completes only after fleet's own run of every mandatory check passed (in review, before the quality gates). The checks run on a **clean export of the reviewed commit** (`git checkout-index` from a temporary index into a fresh 0700 directory under `<state>/verify`; the worktree's uncommitted, ignored and planted files, its index and local git state are never read, links that leave the export are removed), with a throwaway `HOME`/`TMPDIR`/`XDG_*`, git pinned to no configuration, a `PATH` of trusted directories only, no shell, an absolute program outside the worktree, its repository and the export, null stdin, their own process group, per-check and per-job time caps, a cap of 2 concurrent verification jobs and output caps; output is redacted over the whole capture before it is cut and again before storing. Evidence (verdict, bounded output, commit, tree, dirty flag, task) is immutable and logged; the ledger counts it only for the commit the task's review recorded, per task, never when the worktree was dirty; `verified` is derived, never set; a guarded task is merged only as the commit that was proven. Contracts are parsed with a strict schema and size caps and refused when they contain something a redaction rule recognizes; CLI output is escaped and check output quoted | `neo-verify`, `neo-goals`, `fleet-app/tests/neo_verify.rs`, `fleet-infra::verify`, `fleet-infra::git` (export), `fleet-infra::fsx`, `e2e_neo.rs` |
| Everything the daemon starts is kept off the control plane like workers (control socket and web port, one mechanism) | One `JobTracker` is shared by every adapter that starts a child (quality gates, approved commands, verification checks, the undo tool, git). It lists the process group of each child while it lives and the descendants that leave the group (found by sampling the process table every 200 ms, registered by pid, killed with start-token verification when the child ends); `Orchestrator::live_sessions` returns it together with the live worker sessions. The origin check refuses a peer that is one of those, or in one of those groups, **or that has the daemon itself among its ancestors** (anything the daemon started that is still attached, listed or not), or that cannot be identified (fail closed). The control socket checks at accept and per request; the web port checks every connection after its request was read and before anything is acted on, through the same handler. `approval.decide` requires the digest of the action it covers on every path, including `POST /api/approval/decide` | `fleet-infra::jobs`, `fleet-infra::daemon`, `fleet-domain::supervision`, `fleet-app/tests/neo_verify.rs`, `fleet-app/tests/decisions.rs`, `e2e_neo.rs`, `e2e_web.rs` |
| Tamper-evident audit log | Hash chain over canonical event text plus a head file created together with the log; truncation, edits, deletions and reordering are detected at start and by `fleet verify-log`; a log with records but no head file is refused. Deleting *both* files starts a fresh history and is not detected | `fleet-domain::chain`, `fleet-infra::eventlog`, `e2e_recovery.rs` |
| No signal reaches the wrong process | Pid plus process start time (on Linux scoped by the boot id) must match before any signal; only a process group leader is signalled as a group; orphans of an earlier daemon run are checked the same way | `fleet-domain::supervision`, `fleet-infra::procid`, `fleet-infra::pty` |
| Control socket hygiene | 0700 directory (owner checked, symlinks refused), 0600 socket, `SO_PEERCRED`/`LOCAL_PEERCRED` uid check, flock-based single daemon, stale socket detection, bounded JSON lines, bounded frames, connection cap | `fleet-infra::server`, `statedir`, `client`, `e2e_security.rs` |
| The web dashboard serves the operator only | Off unless `fleet daemon --web`; loopback unless `--web-allow-remote` (then a warning is printed). Each connection: a first-byte and a whole-request deadline, then the request is read within hard bounds, then **admission** (before anything is acted on: the process holding the client end of the TCP connection must not be a worker, a quality gate, an approved command, git or the undo tool, nor started by any of them, nor in the process group of one that is running, and must be identifiable; the same rule as the control socket's; answered 403), strict `Host` check against rebinding, then authentication: a 256-bit per-start token (memory only, constant-time compare, printed once, and only to a terminal unless `--web-print-login-url`) as `Authorization: Bearer` or, exchanged by `POST /api/login`, a random session id sent in an `X-Fleet-Session` header. **No cookie is ever set or read**: the id lives in the page's `sessionStorage` (per tab and per origin including the port). Every state-changing request also needs `Origin`, `Sec-Fetch-Site`, the session's CSRF token and a JSON content type; `/api/send` accepts no control characters. The UI offers only `status`, `events`, `task.tail`, `task.send`, `gate.resolve`, `approval.decide` and `merge` and, read only, `goal.list`, `goal.show`, `goal.remaining` and `verify.status` (it cannot add a goal, attach a contract, change a status or start a verification); approvals still need the displayed digest (mandatory in the request) and a merge still needs its approval. Hand-written HTTP/1.1 with bounded line, head, header count and body, connection, per-remote-address and stream caps; event streams end when their session does. CSP `default-src 'none'; script-src 'self'; ...`, no inline code, `textContent` only, JSON with `<>&`, controls and bidi escaped | `fleet-infra::web::{http,auth,peer,assets}`, `fleet-infra::daemon` and `jobs` (the origin rule above), `fleet-infra/tests/web_http.rs`, `fleet-cli/tests/e2e_web.rs`, `fleet-app/tests/hostile_text.rs`, `architecture.rs`, soak with a connected stream, `scripts/web-smoke.mjs` |

## Known limitations (read these)

- **Workers are not sandboxed.** A worker runs as you. It can read your `HOME`, write the repository's shared
  `.git` directory, start any program and open network connections. fleet limits what it *shares*
  (environment, socket, state directory) and what it can make *the daemon* do, not what the worker can do to
  your files. Use a container, a VM or another user for real isolation.
- **The control plane is not a security boundary against a hostile same-user process.** Workers run as you. fleet makes
  accidental and casual access fail (no socket path, state directory or token in a worker's environment or worktree,
  `XDG_STATE_HOME` redirected, per-run token required, peer ancestry checked at accept and per request, unknown peers
  denied), but a worker can read `/proc/<daemon pid>/cmdline` and the 0600 token file as the same user, then detach
  from its process tree (`setsid`, `systemd-run --user`, `at`, a terminal multiplexer) and connect as a plain process of
  yours. Only running workers under a different user, in a container or in a VM closes that gap. Where the platform
  exposes no peer credentials the 0700 directory and the token are the only barriers.
- **Approvals bind what fleet executes, not what a worker does.** For a merge or an approved external command the daemon
  runs exactly the approved action (merge: the approved commit; command: the approved absolute path and arguments). A
  worker that asks "may I push?" and then pushes anyway is not stopped. An approved command runs with the worker's
  worktree as its working directory, so a command that executes files from its working directory (`make`, `npm run`,
  `git` with repository configuration, a script that sources `./env`) executes whatever the worker left there:
  approve only commands that do not. The worktree contents are not part of the digest.
- **Redaction is pattern based.** It removes built-in key shapes, your configured rules and literals and values of daemon
  variables whose names look secret, from terminal output, gate output, approved-command output and worker-written
  summaries and questions. A secret that matches none of these is stored as printed. Error text from git and the
  approval request text a worker wrote (command, arguments) are not redacted.
- **The audit log is tamper evident, not tamper proof.** Whoever can rewrite both `events.log` and `events.head`, or delete
  both, can forge or erase history. The head file lives next to the log, so it cannot anchor the log against an attacker
  with write access to the state directory.
- **The web credentials are defence in depth against same-user hostile code, not a boundary.** Neither the control
  socket's token nor the web token nor a web session keeps out code that runs as you and is willing to do real work. The
  web token exists only in the daemon's memory and on the terminal that started it, but a process of yours can read
  process memory (where `ptrace` is allowed), the terminal's scrollback, the browser's memory or its `sessionStorage`,
  and then act as the operator. What fleet does: workers, quality gates, approved commands, git, the undo tool and
  everything they started are refused at the TCP connection (and at the socket), they never receive the token, and a
  session id is not an ambient credential (it is not a cookie, so the browser does not hand it to other ports; a dev
  server a worker started on another port of the same host never sees it). What it does not do: a process that has
  left the tree of all of these (a double fork adopted by init that also changed process group, `setsid`, a unit started
  with `systemd-run --user`, a terminal multiplexer) is indistinguishable from the operator's own `curl`. Run workers as
  another user, in a container or in a VM for a real boundary.
- **No TLS.** The dashboard speaks plain HTTP, so it is meant for the loopback interface. The token, the session id and
  every page cross a non-loopback network in clear text; `--web-allow-remote` exists for trusted networks and prints a
  warning. For remote use put an SSH tunnel (`ssh -L 7878:127.0.0.1:7878 host`) in front of a loopback dashboard
  instead. Remote peers cannot be identified (they are not processes of this machine), so for them only the token
  protects; their failed guesses are counted per address and each address is limited to 8 open connections.
- **The web token lives until the daemon stops, and where it is printed matters.** It is not rotated, it is never a
  one-time code (scripts use it as a bearer token), and a login URL scrolled back in a terminal stays valid. The daemon
  prints the URL only when stderr is a terminal (or with `--web-print-login-url`): if you redirect stderr to a file,
  `nohup.out`, a journal or a terminal logger, the URL, and with it the token, is kept there, readable by every
  process of yours (`/proc/<daemon>/fd/2` too). Opening the URL with `xdg-open`/`open` puts it in that program's argument
  list, visible to processes of yours, and the browser may keep the address (with the fragment) in its history until the
  page has replaced it. A session is valid for 12 hours (2 hours idle), is forgotten when the daemon restarts, and ends an
  open event stream within a second when it ends. A stream opened with the bearer token is not tied to a session and
  runs until its limits (10 minutes or 4 MiB, then the page reconnects).
- **Failed token guesses lock the token paths, per client.** Five wrong tokens within a minute lock *bearer and login*
  attempts of that client for a minute. Live sessions keep working during a lock and an unknown session id (a stale tab, a
  restarted daemon) is not counted. All local processes are one client, so a local process that passes admission can keep
  the operator from logging in again (not from using a session they already have) by guessing wrongly; a remote address
  only locks itself. The control socket is unaffected.
- **Connection slots can still be held for a few seconds.** Connections are capped (32, plus 4 event streams and 8 per
  remote address). A connection that sends nothing is closed after 3 seconds and costs no process-table lookup (admission
  runs after the request was read); a local process that passes admission can still keep the slots busy by opening
  connections as fast as they are closed, which makes the operator's page wait or get 503. Slots are not reserved for
  authenticated sessions (nothing distinguishes a connection before it sends its credentials).
- **Admission looks at processes, not at intent.** On Linux it needs `/proc/net/tcp*` and the readable `/proc/<pid>/fd` of
  the client (same user: readable); where that is hidden every loopback client is refused. On macOS it runs `lsof` and has
  not been run there. A client that exits before the lookup is refused (fail closed). Every process that holds the client's end
  of the connection is judged; one that has vanished by then (a child forked and exec'd while the socket was open) is skipped
  only if another holder is there and passes, and one that is alive and refused refuses the connection. The daemon's own
  threads are not refused (they are the daemon).
- **The page shows what workers wrote.** Text is sanitized by the domain and rendered as text, so it cannot run or break
  the layout, but an operator reading a summary or a question can still be misled by its content (prompt injection
  reaches people too). The exact action and digest of an approval are shown as the daemon holds them.
- **`merge` and the health panel run on the daemon.** A merge from the web runs git synchronously on the engine thread like
  `fleet merge`; the health panel re-verifies the whole event log (up to 64 MiB) at most every five seconds when it is open.
- **Quality gates run on the tree the worker edited.** A worker can influence a gate (for example by editing the
  gate's script if it lives in the repository). Keep gate commands outside the repository.
- **A path-planted git driver can race.** A merge driver or filter planted by a *concurrently running* worker
  between the config listing and the git command is not overridden; it still runs with the allowlisted environment only.
- **The engine thread runs some git commands synchronously.** Committing a worker's output at review time and merging
  (including bringing dependency branches into a task's branch) run `git` on the single engine thread, each call with
  a 120 second timeout. A worker that leaves a pathological tree (millions of files) can stall supervision, approval
  expiry and request handling for that long. Not changed: moving these into background jobs would need a larger
  redesign of the review flow; limit worktree sizes with your own gates.
- **No resource limits for workers.** fleet sets no `RLIMIT_*`, cgroup or disk quota on workers (that needs `unsafe` or a
  sandbox), so a worker can fill the disk, fork without bound or use all memory.
- **Worker text in briefs.** Worker summaries, questions and tool output are flattened or quoted before they reach the brief
  of another task, so they cannot start headings or imitate fleet's notes, but a language model reading them can still
  be influenced by their content (prompt injection between tasks is possible by nature).
- **agent-undo is resolved once.** The optional `agent-undo` binary is looked up at daemon start in absolute `PATH` entries
  only, stored as a canonical absolute path and refused at every call if it lies inside the worktree or its repository;
  it runs with working directory `/`. A binary that an attacker can already replace in a trusted `PATH` directory is
  not defended against.
- **No SIGINT/SIGTERM handler.** Catching signals needs `unsafe` or another dependency. Stopping the daemon with a
  signal ends it immediately; workers lose their terminals and the next daemon start reconciles them. Use
  `fleet shutdown` for a graceful stop.
- **Output threads can outlive a session.** If a worker leaves a process outside its process group holding the pty
  open, one reader thread stays blocked until that process exits; its memory is bounded.
- **`SHELL` is added by the pty library.** `portable-pty` always sets `SHELL` in a worker's environment (from the
  passwd database) even after the environment was cleared.
- **Unix socket path length.** `sun_path` allows about 100 bytes; very deep `--state-dir` paths are rejected with
  advice to use a shorter one.
- **Event log size.** The log stops accepting events at 64 MiB; the daemon then refuses new work. There is no rotation.
- **Verification contracts: what they protect against and what they do not (NEO N1).** They protect against a worker that
  plants uncommitted, ignored or local files (a Makefile, a helper, a git configuration) to make its own check pass or to run
  code when the daemon verifies, against checks reading the operator's home configuration, against processes of checks,
  gates and approved commands using the control socket, and against stale or mismatched evidence. They do **not**
  sandbox hostile code that is *part of the commit*: `make test`, `cargo test` or `npm test` run the committed build
  scripts and tests as you, with network access and every file your user can read. Same-user processes remain defence in
  depth, not a boundary: a process can read the capability token of a daemon whose state directory it can find and detach
  completely before connecting, and a descendant that leaves the process group is found only if a 200 ms sample sees it
  (there are no cgroups). Use a container, a VM or another user for real isolation. A weak check passes a weak result.
  Contract text is stored in the audit log and shown by `fleet goals show`; contracts that contain something a redaction
  rule recognizes are refused, but a secret that does not look like one is not. Output of checks is redacted by pattern
  like all other output. A daemon killed during a verification loses that run (the next start removes its directory and
  runs the contracts of tasks still in review again).
- **Quality gates and approved commands share the job-origin protection but not the export.** They run in the worktree, as
  before; their processes (like those of the undo tool and git) are listed in the shared tracker and refused by the origin
  check on the control socket and on the web port.

## Dependency policy

- Every crate version in `Cargo.lock` (direct and transitive, 121 external packages) was published on or before
  **2026-10-24T00:00:00Z**, at least a week before the day the lock file was last resolved. The check is
  mechanical: `node check-crates-age.mjs <repo> 2026-10-24T00:00:00Z` queries crates.io for every locked version
  and must report `too new: 0`. Four crates resolved to newer releases and were pinned back with
  `cargo update --precise` (`log`, `rustix`, `serial2`, `unicode-width`).
- Direct dependencies are pinned with `=` in `Cargo.toml`; `Cargo.lock` is committed; CI uses `--locked`.
- Dependabot (cargo and github-actions, weekly) has a 7 day cooldown so new releases age before they are proposed.
- GitHub Actions are pinned to a commit SHA; the workflow has `permissions: contents: read` and
  `persist-credentials: false`; it uses the runner's preinstalled rustup and no third-party actions.
- There is no async runtime, no web or HTTP framework (the dashboard's server is hand-written on `std::net`, see ADR 10),
  no TLS stack, no CLI framework and no regex engine. An architecture test fails if one appears.

Direct dependencies and why they are there:

| Crate | Why |
| --- | --- |
| `serde`, `serde_json` | Config/plan files and control-socket documents. |
| `toml` | `fleet.toml` and plan files. |
| `sha2` | Approval digests and the audit hash chain. |
| `portable-pty` | Ptys and process spawning with a controlling terminal. |
| `nix` | Safe wrappers for signals, peer credentials, `flock`, `openat`, `poll` (so the code base needs no `unsafe`). Pulled twice: 0.31.3 directly and 0.28.0 by `portable-pty`. |
| `ratatui`, `crossterm` | The terminal dashboard, attach and raw mode. Only in `fleet-cli`. |

**Crates with a `build.rs`** (build scripts run at compile time): `anyhow 1.0.104`, `libc 0.2.191`, `libm 0.2.16`,
`memoffset 0.9.1`, `nix 0.28.0`, `nix 0.31.3`, `num-traits 0.2.19`, `parking_lot_core 0.9.12`,
`portable-atomic 1.15.0`, `proc-macro2 1.0.107`, `quote 1.0.47`, `rustix 1.1.5`, `rustversion 1.0.23`,
`serde 1.0.229`, `serde_core 1.0.229`, `serde_json 1.0.154`, `signal-hook 0.3.18`, `thiserror 1.0.69`,
`thiserror 2.0.21`, `zmij 1.0.23`, plus Windows-only `winapi 0.3.9`, `winapi-i686-pc-windows-gnu 0.4.0`,
`winapi-x86_64-pc-windows-gnu 0.4.0`, `winreg 0.10.1` (not compiled on Linux or macOS).

**Procedural-macro crates:** `darling_macro 0.24.1`, `derive_more-impl 2.1.1`, `document-features 0.2.12`,
`indoc 2.0.8`, `instability 0.3.14`, `palette_derive 0.7.7`, `rustversion 1.0.23`, `serde_derive 1.0.229`,
`strum_macros 0.28.0`, `thiserror-impl 1.0.69`, `thiserror-impl 2.0.21`.

The domain and application crates have no dependencies at all beyond each other
(`crates/fleet-cli/tests/architecture.rs` checks this), and every crate has `#![forbid(unsafe_code)]`.
Third-party crates contain `unsafe` internally (`libc`, `nix`, `portable-pty` and others).

## Secrets

No secrets are committed. Test fixtures that look like keys are assembled at run time. The daemon never passes
its environment to children: workers, gates, git and approved commands start from an empty environment plus an
allowlist, and fleet's own variables never include the socket path or the state directory. Stored output is
redacted and sanitized. No tokens are logged. A final scan of the working tree and the full git history (key
shapes, private-key headers, long hex/base64 strings, absolute home-directory paths) is part of the release
checklist in the README.
