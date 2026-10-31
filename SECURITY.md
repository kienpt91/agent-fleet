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
(`LOCAL_PEERCRED`/`LOCAL_PEERPID` peer checks, `ps`-based process identity, pty behaviour) type-check
(`cargo check --target aarch64-apple-darwin`) but have **not been run**.

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

What the design tries to guarantee, and where it is tested:

| Property | How | Tests |
| --- | --- | --- |
| A worker cannot grant itself an approval or answer a gate | Only the daemon creates approvals (merges, or from a worker's `question.json`); only control requests decide them; requests from a worker process or a descendant of one are refused (peer pid ancestry); workers get no socket path, no state dir and an allowlisted environment | `e2e_security.rs`, `fleet-domain` approval tests, `fleet-app` decisions tests, `fleet-infra::pty` environment test |
| An approval covers one exact action | SHA-256 digest of (tool, command, arguments, task id); merge approvals include the commit being merged; expiry; single use; the digest is recomputed at use | `fleet-domain::approval`, `fleet-app/tests/merging.rs`, `e2e_security.rs` |
| Worker output cannot attack the terminal or the logs | Streaming sanitizer: every escape sequence removed, controls and invisible Unicode made visible, `\r` becomes a line break; separate passthrough filter for `attach` that drops OSC/DCS/APC/queries; redaction of key shapes, configured rules and secret-looking daemon variables before storage | `fleet-domain::sanitize`, `redact`, `output`; `fleet-infra::pty`; soak test |
| Memory stays bounded | Ring buffer per session with a hard cap; line cap; caps on messages, frames, files, connections, sessions, runs, tasks, notes, gate output, event log size | `fleet-domain::output`, `fleet-infra/tests/soak.rs`, limits tests in every adapter |
| Reading worker-written files cannot hang or leak | lstat + `O_NOFOLLOW|O_NONBLOCK` + fstat re-check + size cap; `.fleet` opened relative to the worktree with `openat`; FIFOs, symlinks, directories and oversize files are rejected | `fleet-infra::fsx`, `contract_fs`, `e2e_security.rs` |
| git never runs code planted in a worktree | Empty environment + allowlist; `GIT_CONFIG_GLOBAL/SYSTEM=/dev/null`; `-c` flags that disable hooks, fsmonitor, pager, editor, ssh, credential helpers, signing; per-key overrides for filters, textconv, merge drivers, pagers; `--no-verify`; names validated before they reach a command line | `fleet-infra::git` (one test per vector with a sentinel env var and marker file), `e2e_security.rs` |
| Isolation of the user's checkout | Work happens on `fleet/<run>/<task>` branches in `<repo>/.fleet-worktrees`; the user's branch, index and working tree are never touched; the integration branch has its own worktree | `fleet-infra::git`, `e2e_flow.rs` |
| Tamper-evident audit log | Hash chain over canonical event text plus a head file; truncation, edits, deletions and reordering are detected at start and by `fleet verify-log` | `fleet-domain::chain`, `fleet-infra::eventlog`, `e2e_recovery.rs` |
| No signal reaches the wrong process | Pid plus process start time must match before any signal; only a process group leader is signalled as a group; orphans of an earlier daemon run are checked the same way | `fleet-domain::supervision`, `fleet-infra::procid`, `fleet-infra::pty` |
| Control socket hygiene | 0700 directory (owner checked, symlinks refused), 0600 socket, `SO_PEERCRED`/`LOCAL_PEERCRED` uid check, flock-based single daemon, stale socket detection, bounded JSON lines, bounded frames, connection cap | `fleet-infra::server`, `statedir`, `client`, `e2e_security.rs` |

## Known limitations (read these)

- **Workers are not sandboxed.** A worker runs as you. It can read your `HOME`, write the repository's shared
  `.git` directory, start any program and open network connections. fleet limits what it *shares*
  (environment, socket, state directory) and what it can make *the daemon* do, not what the worker can do to
  your files. Use a container, a VM or another user for real isolation.
- **The origin check is best effort.** Control requests whose peer process is a worker, or a descendant of one,
  are refused. A process that detaches completely from the worker's tree (double fork and `setsid`) and
  discovers the socket path can connect as the same user. Where the platform offers no peer credentials the
  0700 directory is the only barrier. Run workers under a different user for a hard guarantee.
- **Approvals bind what fleet executes, not what a worker does.** For a merge or an approved external command
  the daemon runs exactly the approved action. A worker that asks "may I push?" and then pushes anyway is not
  stopped: it can do anything its own permissions allow.
- **Redaction is pattern based.** It removes built-in key shapes, your configured rules/literals and values of
  daemon variables whose names look secret. A secret that matches none of these is stored as printed.
- **The audit log is tamper evident, not tamper proof.** Whoever can rewrite both `events.log` and `events.head`
  can forge a consistent history.
- **Quality gates run on the tree the worker edited.** A worker can influence a gate (for example by editing the
  gate's script if it lives in the repository). Keep gate commands outside the repository.
- **A path-planted git driver can race.** A merge driver or filter planted by a *concurrently running* worker
  between the config listing and the git command is not overridden; it still runs with the allowlisted environment only.
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
- There is no async runtime, no web framework, no CLI framework and no regex engine.

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
