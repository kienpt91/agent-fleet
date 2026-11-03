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
| `gh` and `git push` (GitHub integration, off by default) | Trusted code of the operator, run with a minimal environment. Their answers are untrusted text: issue titles, bodies, labels and authors are written by strangers. fleet never holds a token: signing in is `gh`'s. |

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
| A task's acceptance criteria are checked by fleet, not by the worker (NEO N1) | A task that a goal with a verification contract names completes only after fleet's own run of every mandatory check passed (in review, before the quality gates). The checks run on a **clean export of the reviewed commit** (`git checkout-index` from a temporary index into a fresh 0700 directory outside the state directory (`<temp>/fleet-verify-<uid>/<hash>/job-*`, so nothing above a check's working directory leads to the control token); the commit and tree come from the real repository, never through the worktree's git directory, whose `.git` pointer is validated before every use (replace refs are disabled, alternates and grafts refused, ADR 16); the worktree's uncommitted, ignored and planted files, its index and local git state are never read, links that leave the export are removed), with a throwaway `HOME`/`TMPDIR`/`XDG_*`, git pinned to no configuration, a `PATH` of trusted directories only, no shell, an absolute program outside the worktree, its repository and the export, null stdin, their own process group, per-check and per-job time caps, a cap of 2 concurrent verification jobs and output caps; output is redacted over the whole capture before it is cut and again before storing. Evidence (verdict, bounded output, commit, tree, dirty flag, task) is immutable and logged; the ledger counts it only for the commit the task's review recorded, per task, never when the worktree was dirty; `verified` is derived, never set; a guarded task is merged only as the commit that was proven, and only when the repository's tree for it is the proven tree. Contracts are parsed with a strict schema and size caps and refused when they contain something a redaction rule recognizes; CLI output is escaped and check output quoted | `neo-verify`, `neo-goals`, `fleet-app/tests/neo_verify.rs`, `fleet-infra::verify`, `fleet-infra::git` (export), `fleet-infra::fsx`, `e2e_neo.rs` |
| Everything the daemon starts is kept off the control plane like workers (control socket and web port, one mechanism) | One `JobTracker` is shared by every adapter that starts a child (quality gates, approved commands, verification checks, the undo tool, git). It lists the process group of each child while it lives and the descendants that leave the group (found by sampling the process table every 200 ms while the child runs and once more before a timed-out child is stopped, registered by pid, killed with start-token verification when the child ends); `Orchestrator::live_sessions` returns it together with the live worker sessions. The origin check refuses a peer that is one of those, or in one of those groups, **or that has the daemon itself among its ancestors** (anything the daemon started that is still attached, listed or not), or that cannot be identified, or whose ancestry could not be read to the root of the process tree (fail closed). The control socket checks at accept and per request; the web port checks every connection after its request was read and before anything is acted on, through the same handler. `approval.decide` requires the digest of the action it covers on every path, including `POST /api/approval/decide` (the CLI fills the digest from the list the daemon returns, so it guards against a stale decision, not against an admitted caller). git runs on the engine thread for most calls (see limitations): those are protected by the ancestry rule only, the tracker listing covers verification-host git | `fleet-infra::jobs`, `fleet-infra::daemon`, `fleet-domain::supervision`, `fleet-app/tests/neo_verify.rs`, `fleet-app/tests/decisions.rs`, `e2e_neo.rs`, `e2e_web.rs` |
| Tamper-evident audit log | Hash chain over canonical event text plus a head file created together with the log; truncation, edits, deletions and reordering are detected at start and by `fleet verify-log`; a log with records but no head file is refused. Deleting *both* files starts a fresh history and is not detected | `fleet-domain::chain`, `fleet-infra::eventlog`, `e2e_recovery.rs` |
| No signal reaches the wrong process | Pid plus process start time (on Linux scoped by the boot id) must match before any signal; only a process group leader is signalled as a group; orphans of an earlier daemon run are checked the same way | `fleet-domain::supervision`, `fleet-infra::procid`, `fleet-infra::pty` |
| Control socket hygiene | 0700 directory (owner checked, symlinks refused), 0600 socket, `SO_PEERCRED`/`LOCAL_PEERCRED` uid check, flock-based single daemon, stale socket detection, bounded JSON lines, bounded frames, connection cap | `fleet-infra::server`, `statedir`, `client`, `e2e_security.rs` |
| The web dashboard serves the operator only | Off unless `fleet daemon --web`; loopback unless `--web-allow-remote` (then a warning is printed). Each connection: a first-byte and a whole-request deadline, then the request is read within hard bounds, then **admission** (before anything is acted on: the process holding the client end of the TCP connection must not be a worker, a quality gate, an approved command, git or the undo tool, nor started by any of them, nor in the process group of one that is running, and must be identifiable; the same rule as the control socket's; answered 403), strict `Host` check against rebinding, then authentication: a 256-bit per-start token (memory only, constant-time compare, printed once, and only to a terminal unless `--web-print-login-url`) as `Authorization: Bearer` or, exchanged by `POST /api/login`, a random session id sent in an `X-Fleet-Session` header. **No cookie is ever set or read**: the id lives in the page's `sessionStorage` (per tab and per origin including the port). Every state-changing request also needs `Origin`, `Sec-Fetch-Site`, the session's CSRF token and a JSON content type; `/api/send` accepts no control characters. The UI offers only `status`, `events`, `task.tail`, `task.send`, `gate.resolve`, `approval.decide` and `merge` and, read only, `goal.list`, `goal.show`, `goal.remaining` and `verify.status` (it cannot add a goal, attach a contract, change a status or start a verification); approvals still need the displayed digest (mandatory in the request) and a merge still needs its approval. Hand-written HTTP/1.1 with bounded line, head, header count and body, connection, per-remote-address and stream caps; event streams end when their session does. CSP `default-src 'none'; script-src 'self'; ...`, no inline code, `textContent` only, JSON with `<>&`, controls and bidi escaped | `fleet-infra::web::{http,auth,peer,assets}`, `fleet-infra::daemon` and `jobs` (the origin rule above), `fleet-infra/tests/web_http.rs`, `fleet-cli/tests/e2e_web.rs`, `fleet-app/tests/hostile_text.rs`, `architecture.rs`, soak with a connected stream, `scripts/web-smoke.mjs` |
| Repository content shown in the workbench (names, files, diffs, Markdown) and the image blob API | Read from the real repository at commits the daemon recorded (never a worktree); `RepoPath` validation; escaped names; bounded text, trees and diffs; links shown, never followed; Markdown parsed to a closed tree (raw HTML is text, only http/https and repository links, no remote images); images only by magic number (png, jpeg, gif, webp) and within a declared size of 8192 per side and 32 megapixels (read from the header, so a tiny file cannot make a browser allocate gigabytes), with `nosniff`, `default-src 'none'; sandbox`; read-only commands in `WEB_COMMANDS`; at most three reads at once; worker origin refused | `fleet-domain/src/repo/{path,view,diff,markdown}.rs`, `fleet-infra/tests/repo_read.rs`, `fleet-domain/src/repo/view.rs` (declared sizes, damaged headers), `fleet-app/tests/browse.rs::an_image_that_declares_a_huge_canvas_is_never_served_and_is_a_placeholder_in_its_file_document`, `fleet-infra/tests/web_http.rs::an_image_is_sent_with_a_closed_policy_nosniff_and_one_of_four_types`, `fleet-cli/tests/e2e_web.rs::clients_cannot_name_paths_commits_or_tasks_the_daemon_did_not_record`, `fleet-cli/tests/e2e_web.rs::a_worker_cannot_read_the_repository_through_the_workbench`, `fleet-cli/tests/e2e_web.rs::many_large_diffs_and_files_keep_memory_flat_and_every_cap_holds`, ADR 19 |
| fleet never sees or stores a GitHub token | `gh` is the only transport and keeps its own credential store; there is no token setting; the child environment is `HOME`, `PATH`, `GH_PROMPT_DISABLED`, `GH_NO_UPDATE_NOTIFIER` and optionally `GH_HOST` and the named non-secret `env_pass` variables; token variables of the daemon are never copied (and are redaction literals); `git push` gets only `PATH` and `HOME`. A planted sentinel token appears in no log, event, brief, status document, web response or child environment (ADR 20); what `gh` prints to stderr, what `git push` prints and the reason stored in a `pull_request_failed` event pass through the daemon's redactor, and an error about a remote URL never repeats its user name, password, query or fragment. That the token stays out of *fleet* does not keep it from a worker: see the known limitation below | `fleet-infra/tests/forge_gh.rs::gh_gets_a_minimal_environment_and_never_a_token`, `forge_push.rs::the_child_has_no_token_no_agent_and_no_askpass`, `fleet-cli/tests/e2e_forge.rs` |
| `gh` is the program the operator installed, and only that | Absolute canonical path resolved once; relative `PATH` entries skipped; refused while inside any run's repository or worktree; fixed argv, no shell, cwd `/`; hard timeout, 2 MiB output cap, group listed in the job tracker (refused as origin) | `forge_gh.rs::gh_is_found_once_in_an_absolute_path_entry_and_relative_entries_are_never_used`, `a_gh_inside_a_repository_or_a_worktree_is_not_trusted`, `a_gh_that_hangs_is_stopped_at_the_timeout_and_leaves_nothing_running` |
| A pull request is opened only as approved | Approval bound to (repository, base, head branch, head commit, title, body, draft); `fleet approval list` shows each of them on its own line, the flag before the long fields, and the body in full (at most 4 KiB) quoted line by line; recomputed at use; single use; the preflight, and the adapter's own checks (trusted `gh`, open rate gate, a free slot), pass before the approval is spent; the push is `<sha>:refs/heads/<branch>` without force, tags or extra refs, to a URL rebuilt from the approved repository, run in a private throwaway git directory so that git never reads the repository's (worker-writable) configuration (ADR 21). Limits: push credentials are the operator's `gh` or named helper; an ssh remote is refused; a repository whose config rewrites URLs or sets http options is refused as well, and a configuration that cannot be read is refused too (fails closed); the approval gate is process-level, see the known limitation below | `fleet-app/tests/forge.rs`, `forge_push.rs`, `fleet-forge::publish` |
| The repository GitHub requests go to is the operator's | `[github] repo` pins it. When it is not set, the repository is read from the run's configured remote, which a worker can rewrite: it is recorded when the run is created (`forge_repo_recorded` event), a later different answer is refused until the operator pins `repo`, and every `issue to-task` and `pr create` reply and approval request names the repository and where it came from | `fleet-app/tests/forge.rs::a_remote_that_changes_after_the_run_began_is_refused_until_the_repository_is_pinned`, `the_repository_a_task_or_pull_request_goes_to_is_said_with_where_it_came_from` |
| Issue text is data, not instructions | Cleaned, redacted, quoted and labelled `(untrusted)` in the brief, never in a contract or approval; shown as sanitized text in the CLI and with `textContent` in the dashboard (ADR 22). Prompt-injection phrases stay readable inside the quote: they cannot be filtered, they only have no authority | `fleet-forge::untrusted`, `fleet-app/tests/forge.rs::an_issue_becomes_a_task_whose_text_is_quoted_labelled_and_never_an_instruction`, `forge_gh.rs::hostile_issue_text_is_cleaned_and_secret_shapes_are_redacted_before_anything_sees_it` |
| GitHub rate limits are respected and offline is harmless | 403/429 and the remaining-requests header close the rate gate with doubling, jittered back-off; no process while closed; a missing, unauthenticated or unreachable `gh` fails the command with a clear error before any state changes | `forge_gh.rs::a_rate_limit_blocks_further_calls_without_starting_gh_and_comes_back_after_the_wait`, `fleet-app/tests/forge.rs::a_missing_or_unready_adapter_fails_clearly_and_changes_no_state` |
| The web dashboard serves the operator only | Off unless `fleet daemon --web`; loopback unless `--web-allow-remote` (then a warning is printed). Each connection: a first-byte and a whole-request deadline, then the request is read within hard bounds, then **admission** (before anything is acted on: the process holding the client end of the TCP connection must not be a worker, a quality gate, an approved command, git or the undo tool, nor started by any of them, nor in the process group of one that is running, and must be identifiable; the same rule as the control socket's; answered 403), strict `Host` check against rebinding, then authentication: a 256-bit per-start token (memory only, constant-time compare, printed once, and only to a terminal unless `--web-print-login-url`) as `Authorization: Bearer` or, exchanged by `POST /api/login`, a random session id sent in an `X-Fleet-Session` header. **No cookie is ever set or read**: the id lives in the page's `sessionStorage` (per tab and per origin including the port). Every state-changing request also needs `Origin`, `Sec-Fetch-Site`, the session's CSRF token and a JSON content type; `/api/send` accepts no control characters. The UI offers only `status`, `events`, `task.tail`, `task.send`, `gate.resolve`, `approval.decide` and `merge` and, read only, `goal.list`, `goal.show`, `goal.remaining`, `verify.status`, `msg.list` and `msg.show` (it cannot add a goal, attach a contract, change a status or start a verification, send, mute or unmute a message); approvals still need the displayed digest (mandatory in the request) and a merge still needs its approval. Hand-written HTTP/1.1 with bounded line, head, header count and body, connection, per-remote-address and stream caps; event streams end when their session does. CSP `default-src 'none'; script-src 'self'; ...`, no inline code, `textContent` only, JSON with `<>&`, controls and bidi escaped | `fleet-infra::web::{http,auth,peer,assets}`, `fleet-infra::daemon` and `jobs` (the origin rule above), `fleet-infra/tests/web_http.rs`, `fleet-cli/tests/e2e_web.rs`, `fleet-app/tests/hostile_text.rs`, `architecture.rs`, soak with a connected stream, `scripts/web-smoke.mjs` |
| Messages between agents are mediated and policy bound | Workers cannot call the daemon (origin check), so a worker writes `.fleet/outbox/<n>.json`; the daemon reads it with lstat/`O_NOFOLLOW`/`O_NONBLOCK` descriptors, size and count caps and a strict schema, and writes `.fleet/inbox/` only through descriptors it checked (a link in place of `.fleet` or `inbox` is refused and the message waits; files it did not write are removed). Channels are declared by the operator in a plan; a task in no channel sends and receives nothing. Nothing is typed into a terminal unless `[messages] notify_input` is on; then it is one fixed line of printable ASCII **without Enter**, so a peer can cause keystrokes in the recipient's input line (timing is the sender's), but never a submitted line or a confirmed prompt. Bounds: body size, per-task and per-channel totals, per-minute rates, fan-out, ttl, a cap per run, a share of each recipient's inbox per sender. Every decision is an event in the hash chain | `fleet-domain::msg`, `fleet-app/tests/messaging.rs`, `fleet-app/tests/messaging_security.rs`, `fleet-infra::msgfs`, `fleet-infra::pty` (`a_notice_is_printable_ascii_bounded_and_never_presses_enter`), `e2e_messages.rs` |
| Prompt injection between agents | Message text is untrusted peer content: sanitized, redacted, clipped and shown as `from task <id> (unverified peer message)`; kinds are a closed set (a file claiming kind `approval` is invalid; "I approve" is text); it is never put into a contract, an approval (approvals name the id and a digest), a gate, acceptance criteria; a brief carries the sender label and, for at most three waiting messages, one flattened 80-character subject each (that is peer text inside a document the worker reads as its instructions: it is labelled, quoted and bounded, not removed). A model can still be influenced by what it reads | `fleet-domain::msg::{message,inbox,board}`, `e2e_messages.rs::{secret_shaped_hostile_and_unauthorised_messages_stay_inert,hostile_subjects_reach_a_brief_only_as_one_quoted_line_behind_their_label}` |
| Collusion and evasion of guardrails between agents | Secret-shaped text (the rules of `scripts/secret-scan.sh`, applied to the subject and the body, line by line, also after removing invisible characters) is refused and not stored; duplicates suppressed; a reply depth and a pair budget that count genuine back-and-forth between two tasks over every channel of a run and do not trust `in_reply_to`; the operator can mute any channel, task or one task on one channel; approvals can be required per channel. Collusion over an allowed channel is bounded and visible, not prevented. **The secret check is a shape check**: a credential split across lines, across the subject and the body or across several messages, an encoded or homoglyph form, and any secret that matches no shape pass it | `fleet-domain::msg::{secrets,loops,board}`, `fleet-app/tests/messaging_security.rs` |
| Message storms, loops and cost | Reply depth (default 6) and pair budget (default 12 changes of direction), counted per pair of tasks over all channels of a run; a loop mutes the sender that crossed the limit on that channel (the whole channel only when nobody else is left on it), so one member cannot silence the others; one-way traffic and a ring of three or more tasks are bounded by quotas and rates, not by the loop rule (which looks at pairs); per-task/channel totals and rates, a cap per run, fan-out cap, a share of every recipient's inbox per sender, at most 16 messages per channel waiting for approval and their own shares of the decision pool (128 approvals, 32 gates) so they cannot starve questions and merge approvals. Refused messages: a flood rule (20 a minute) and a budget of refusals per task (`max_refused_per_task`, default 100) mute the task until a person unmutes it; files that are not messages are recorded once a minute per task and counted otherwise. Automatic mute of a channel with nobody left; each mute opens a decision gate naming ids only; refused messages keep no body. The event log has a hard size cap (64 MiB, after which the daemon stops recording): the rules above bound what a task can add per run, they do not make the log growth zero | `fleet-domain::msg::{board,loops,ratelimit}`, `fleet-app/tests/messaging_security.rs`, `e2e_messages.rs` |

## Known limitations (read these)

- **Remote workers** run with the full rights of their remote account, and a returned tree is verified for form (paths, size,
  ancestry, object integrity), not for *content*: a hostile tree is as hostile as a local worker's output. See "Remote hosts"
  below for the list of what is and is not fleet's job.
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
- **The GitHub approval is a gate in fleet, not a credential boundary.** `gh` keeps its credential in its own store
  (`hosts.yml` or the system keyring), and a worker runs as you: it can run `gh pr create`, `gh api` or `git push` with
  that credential without asking fleet, and fleet's approval, environment scrubbing and "never holds a token" apply to
  what *fleet* starts, not to what a worker starts. To make the approval mean something, run workers as another user or in
  a container that has no access to your `gh` configuration, and give `gh` a fine-grained token limited to the one
  repository and to the permissions the integration needs (issues and pull requests; contents only for the push).
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
- **Messages between agents limit blast radius, they do not make a model immune.** A message is peer text that the
  recipient's model reads; nothing stops a model from obeying it. What fleet does: the text is labelled, sanitized,
  bounded and kept out of every trusted place (contracts, approvals, gates, goals), nothing is typed into a terminal
  unless you switch `notify_input` on (one fixed line, no Enter, but the sender chooses when it appears), and every
  decision is in the log. What it does not do: understand the text. The secret-shape refusal is per line and ASCII
  based; a secret split into pieces, encoded or written with look-alike characters is delivered. The brief of a task
  contains up to three flattened 80-character peer subjects (labelled) for messages that wait for it. A worker that is
  allowed on a channel can use its quotas and its inbox share, and an operator who sets generous limits gets
  generous traffic: the defaults are small on purpose. Not exercised: real agents, real model behaviour under
  injected messages, macOS.
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
  (the first sample is 200 ms after the child starts, so `setsid` plus a parent that exits earlier is not seen; there are no
  cgroups), and when the process table cannot be read the sweeper says so once on stderr and finds nothing. Use a container, a VM or another user for real isolation. A weak check passes a weak result.
  Contract text is stored in the audit log and shown by `fleet goals show`; contracts that contain something a redaction
  rule recognizes are refused, but a secret that does not look like one is not. Output of checks is redacted by pattern
  like all other output. A daemon killed during a verification loses that run (the next start removes its directory and
  runs the contracts of tasks still in review again).
- **Repositories with alternates or grafts are refused.** Because a forged object source is how a worker's own git
  directory could make fleet see another tree, a repository that has `objects/info/alternates`, `http-alternates` or
  `info/grafts` is not used (a clone made with `--reference` is one); the error names the file. Replace refs are ignored
  for everything fleet reads. A worker can still run git inside its own worktree as it likes; nothing fleet records or
  merges is read from there (ADR 16).
- **The origin check needs a complete ancestry.** A peer whose process ancestry could not be read to the root (more than 64
  levels deep, or a parent that exited while it was read) is refused and can retry. A daemon that is pid 1 of its
  container is handled (everything inside is below it); a process that entered the container from outside is not.
- **Quality gates and approved commands share the job-origin protection but not the export.** They run in the worktree, as
  before; their processes (like those of the undo tool and git) are listed in the shared tracker and refused by the origin
  check on the control socket and on the web port.

## Remote hosts (SSH remote worktrees)

A task that names a host (`task add --host build`, or `host = "build"` in a plan) runs its worker on that machine through the
system's OpenSSH client. ADR 27 has the design; this section is what it means for security.

**What the remote worker can do.** It runs **as the remote user of that host entry, with that user's full rights on that
machine**: read and write everything that user can, start any program, open network connections (including to other machines
that user can reach), keep processes running after the attempt and read whatever the host exposes to that account. It is not
sandboxed there any more than a local worker is here. `allowed_workers` limits only which *command* fleet starts; the worker
that was started can run anything. What fleet gives up when a worker runs remotely: nothing it had for local workers (its output,
its files and its commits are untrusted exactly the same); what is new is that the machine itself is part of the attack surface.

**What fleet does about the transport.**

| Property | How | Tests |
| --- | --- | --- |
| The host is the host the operator confirmed | One plain key per host in `<state>/hosts/<name>.known_hosts` (0600), written only by `fleet host trust`, which shows the fingerprint and needs a terminal and the word `yes`; ssh runs with `StrictHostKeyChecking=yes`, only that file, `GlobalKnownHostsFile=/dev/null`, `UpdateHostKeys=no`; an unpinned host is not connected to at all; a changed key is refused with a message that says so and nothing is sent. Trust on first use does not exist. | `ssh_remote.rs::first_use_without_a_pin_is_refused_before_any_connection`, `a_changed_host_key_is_refused_with_a_clear_error_and_nothing_is_sent`, `e2e_remote.rs::adding_listing_trusting_checking_and_removing_a_host` |
| Only the operator's key for that host is used, and nothing else | `IdentitiesOnly=yes` with the key file named in the host entry, `IdentityAgent=none`, `-F /dev/null` (no user ssh config), no agent socket, display or askpass in the client's environment, `BatchMode`, password and keyboard-interactive and host-based and GSSAPI methods off | `ssh_remote.rs::a_key_the_server_does_not_accept_is_refused`, `forwarding_agent_display_and_environment_do_not_reach_the_remote`, `fleet-infra/src/ssh/argv.rs` tests |
| No forwarding, no local command, no multiplexing | `ForwardAgent=no`, `ForwardX11=no`, `ClearAllForwardings=yes`, `PermitLocalCommand=no`, `Tunnel=no`, `ControlMaster=no`, `ControlPath=none`; a `-R` that is added to the command line is overridden and the server's port stays closed (the test server allows all forwarding on purpose) | `ssh_remote.rs::forwarding_agent_display_and_environment_do_not_reach_the_remote` |
| No local shell and no proxy unless the operator wrote one | The client is started with a fixed argument list (`ssh/argv.rs`, the only place that writes ssh options; `ssh/exec.rs` the only place that starts it). A jump host is configured by name; its hop is a second client with the jump host's own key and pin (a `ProxyCommand` built from validated shell-inert words; `ProxyJump` would use the user's default known_hosts and keys). Host names, users, ports and paths are validated to narrow character sets | `fleet-cli/tests/architecture.rs::only_the_executor_starts_the_ssh_client_and_no_shell_is_involved`, `ssh_remote.rs::a_jump_host_hop_uses_the_same_strict_options_and_its_processes_are_refused_as_origin`, `fleet-domain/src/remote.rs` tests |
| The remote command line cannot be bent by a hostile worker command, argument or path | A closed set of calls (`RemoteCall`); every word through one quoting function; the worker command must be on the host's list; remote paths are `<base>/<run>/<task>` with ids that are `[a-z0-9_-]` and a base without `.` or `..`; scripts are constants that take data as positional parameters; the whole call runs under `env -i` | property tests in `fleet-domain/src/remote.rs`, `ssh_remote.rs::hostile_arguments_reach_the_remote_program_unchanged`, `random_hostile_arguments_survive_the_real_shell` |
| The remote environment holds only what the operator wrote, and its values are on no command line | `[hosts.<name>.env]` plus `FLEET_*`, `PATH` and `TERM`; nothing is copied from the daemon's environment (not even what `[env] allow` lets local workers see), the worker's `env_pass` and `env_set` are not sent, no `SendEnv`. The values travel on the stdin of a small call into a 0600 file next to the worktree, which the launcher reads and deletes before it starts the worker, so they appear in neither the local ssh process's arguments nor the remote shell's (a line break in a value is refused). They are in the worker's own environment on the host, readable there by that account | `fleet-app/tests/remote.rs`, `ssh_remote.rs::the_remote_worker_sees_only_explicit_variables_and_no_local_secret`, `a_secret_variable_is_in_no_command_line_and_the_file_it_travels_in_is_deleted`, `e2e_remote.rs::a_remote_worker_gets_only_the_hosts_explicit_variables_never_the_daemons_environment` |
| What a host returns is verified before anything uses it | The bundle is fetched into a scratch repository that holds only the commit that was sent and the incoming objects (a refused bundle leaves nothing in your repository), must name exactly one ref, pass `git fsck` there, descend from the commit that was sent, touch no `.git`-like or `.fleet` path and stay within the commit, path, file and byte limits; the local worktree must be clean and the branch unmoved. Only the verified **tree** is used, as one new commit by fleet. Gates run locally in that worktree and verification contracts on an export of the commit; evidence and merges use the commit read from the local repository (ADR 16) | `fleet-infra/src/git/remote_sync.rs` tests (unrelated history, `.fleet`, a `.git` tree entry, extra refs, a moved branch, a dirty worktree, garbage), `ssh_remote.rs::a_hostile_remote_that_returns_unrelated_history_or_forbidden_paths_is_refused`, `remote_sync::tests::a_refused_bundle_leaves_no_object_in_the_repository`, `an_old_malformed_object_in_the_repository_does_not_stop_an_honest_import`, `a_checkout_that_fails_halfway_leaves_the_worktree_as_it_was`, `e2e_remote.rs::quality_gates_run_locally_on_the_verified_commit_never_on_the_host` |
| A remote worker's output is as bounded and as clean as a local one's | It is the output of a local pty process, through the same sanitizer, ring buffer, redaction and origin rule | `ssh_remote.rs::sessions::a_remote_worker_runs_in_a_local_terminal_with_sanitized_bounded_redacted_output`, `e2e_remote.rs::a_flood_of_output_from_the_host_is_bounded_like_local_output` |
| A hung or slow host ends an attempt and never hangs the daemon | `ConnectTimeout`, `ServerAliveInterval/CountMax`; every non-terminal call has a total time, a silence limit and byte caps; a host that could not be reached is not tried again for 120 s; stderr is not progress (only bytes sent and bytes of output reset the silence limit); sending the code and the brief (which runs on the engine thread) shares one 120 s budget; the worker's own idle and total limits apply to the terminal session; collecting and signalling run on helper threads; **preparing does not**: a dispatch to a slow or hostile host blocks the engine thread (every command and every tick, local workers' supervision included) for up to the 120 s budget, and again after 120 s for the next task of that host; see "What stays the operator's responsibility" | `ssh_remote.rs::a_connection_that_never_answers_ends_as_a_timed_out_call_not_a_hang`, `exec.rs::a_client_that_only_chatters_on_stderr_is_stalled_all_the_same`, `sessions::a_frozen_server_ends_the_attempt_through_the_keepalive_not_a_hang`, `e2e_remote.rs::a_remote_worker_that_hangs_is_stopped_by_the_idle_limit_and_dies_on_the_host` |
| A stopped remote worker is stopped on the host | Stop sends TERM then KILL to the remote process group (recorded in a process-id file) and to the local client; at the end of every session the group is killed before the work is committed | `ssh_remote.rs::a_remote_worker_that_ignores_term_and_hangup_is_stopped_by_the_remote_kill`, `sessions::stopping_a_remote_worker_that_ignores_term_and_hangup_kills_it_on_the_host_too` |
| Leftovers of a dead daemon are cleaned and reported | Each remote worktree is listed in `<state>/remote/live/` while it exists; at start the group is killed and the directory removed (bounded time), the result goes to the activity feed; a host that is down keeps its entries for the next start | `ssh_remote.rs::reconciliation_removes_what_a_dead_daemon_left_and_says_so`, `e2e_remote.rs::a_daemon_that_died_leaves_a_remote_worker_and_worktree_that_the_next_start_cleans_up` |
| The ssh client and what it starts are kept off the control plane | Non-terminal calls register their process group in the shared `JobTracker` (descendants followed, including a jump host's client); the terminal client is a live worker session; both are refused as origin on the control socket and the web port (ADR 15) | `ssh_remote.rs::a_jump_host_hop_uses_the_same_strict_options_and_its_processes_are_refused_as_origin`, `sessions::stopping_a_remote_worker_...` (live session), `fleet-infra/src/ssh/exec.rs` tests |
| The ssh program is not one a worker planted | Resolved once at daemon start: absolute entries of `PATH` (or `[remote] ssh`), canonical path, not inside a git work tree or below `.fleet-worktrees`, not writable by group or others, its directory neither, owned by root or the user; OpenSSH 7.6 or newer | `fleet-infra/src/ssh/binary.rs` tests, `ssh/doctor.rs` |

**What stays the operator's responsibility.**
- **The remote account.** Give fleet its own unprivileged user on the host, with a key that is only for fleet and a base
  directory that nobody else uses. A host entry for `root`, for your own login or for an account with other keys, agents or
  cloud credentials gives a worker all of that (`fleet host doctor` says so when the user is root). A worker can read the
  other files of that account, including `~/.ssh` and anything in `/tmp`.
- **The key.** The identity file is a long-lived credential for that account; keep it 0600, give it a passphrase-less key
  *only* because fleet cannot answer a prompt (`BatchMode`), and use `command=`/`from=` restrictions in the host's
  `authorized_keys` where you can. A worker on **this** machine can read the identity file (it is your file); a stolen key gets
  its holder onto the host as that account.
- **Who can pin a key.** `fleet host add|trust|remove` do not talk to the daemon and so have no worker-origin check: `trust` needs a terminal and the word `yes`, which keeps *scripts* and accidents out, not a hostile process of yours (a worker can allocate a pty and type `yes`, as `scripts/ssh-demo.sh` does with `script(1)`, or edit `<state>/hosts/` and `fleet.toml` directly: same-user code is not defended against, see above). Pin keys from a terminal you opened yourself.
- **Secrets in `[hosts.*.env]`.** They are protected from the process table (see above) but sit in `fleet.toml` and in the remote worker's environment. Prefer a restricted credential for that host over an account-wide secret.
- **The remote base directory.** `fleet host doctor` and every prepare refuse a base or run directory that is not owned by the remote user or that group or others can write. Remote `git` calls use a fixed set of `-c` overrides but read the worktree's own `.git/config`; do not share the account.
- **Comparing the fingerprint.** `fleet host trust` is only as good as the comparison you make over another channel. A key you
  confirm without comparing is trust on first use, by your decision.
- **The host's own security:** its login shell must be POSIX (`fleet host doctor` refuses others), its sshd configuration
  (`ForceCommand`, `AcceptEnv`) and its patching are outside fleet. A worker that daemonizes or calls `setsid` on the host
  escapes the process-group kill (as a local one does); fleet removes the directory but cannot hunt such processes.
- **Network exposure.** Traffic is as private as ssh makes it; the host learns your task's source tree (the whole tree of the
  task branch is sent) and anything the worker is given. Do not send trees that must not leave this machine.
- **A slow or hostile host stalls the daemon.** Sending the code runs on the engine thread. A host that accepts the connection and then reads or answers more slowly than the silence limit allows can make `status`, `stop` and supervision wait for up to 120 s per dispatch to it (then it is skipped for 120 s). Use hosts you trust to be responsive; moving the transfer to a helper thread is not done.
- **Disk and CPU on the host.** A bundle is bounded (`max_bundle_mib`) and the returned tree is bounded (100k files, 1 GiB), but
  a worker can fill the host's disk or run it out of memory like it can here. The bundle is checked for size as a file only (`max_bundle_mib`); fleet puts no memory or object-size limit on `git` while it unpacks it in the scratch repository, so a hostile host can make that one `git` process (bounded by the 120 s git timeout) use disk and memory beyond the bundle's size. Nothing of a refused bundle stays in your repository.
- **Not verified here:** macOS as the local or the remote system, real accounts, real agents, an OpenSSH other than the one in
  the test image (9.2), `ssh-agent`-based setups (deliberately unsupported).

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

## NEO N2: compiled context

| What | Rule | Where it is enforced and tested |
| --- | --- | --- |
| Agent-written text in a pack | Only in layer 7, introduced by a line that starts with `UNVERIFIED (` (fleet's words only) and quoted line by line with `> `; optionally excluded (`[context] unverified = "exclude"`). Layers 1-5 are built from fleet's state and operator input; layer 6 shows repository files at a recorded commit (which files follows the paths a worker committed; content is never from the worktree). The other lines of layer 7 are fleet's own record. | `neo-context/tests/compiler.rs`, `properties.rs`; `fleet-cli/tests/e2e_context.rs` (forged `.fleet/context.*` and `handoff.json`) |
| Framing forgery | Length-prefixed headers, a digest in each header, and a backslash in front of every content line that starts with `@@` or `\`. | `neo-context` pack and compiler tests (hostile bodies, a whole pack inside a note) |
| Secrets | Every string is redacted (daemon redactor: key prefixes, JWT, Stripe, npm, daemon secret values) and a line that matches a secret rule is replaced whole; files whose name or content match the secret rules (the commit scanner's, matched case-insensitively, plus `.npmrc`, `.pgpass`, `.git-credentials`, `.aws/`, `.ssh/`, `*.p8` and similar; `secret-scan: allow` not honoured) are named, never shown. The rules run in time linear in a line's length. They are a filter, not a guarantee: unquoted `NAME=value`, AWS secret keys, base64 or wrapped keys are not recognised. | `neo-context/src/secret.rs` and `text.rs` tests with run-time-built key shapes and worst-case lines |
| Stored packs | `context show`, `diff` and `verify` re-hash the stored text and compare it with the `context_compiled` event of the hash-chained log before showing anything; a mismatch, a missing newest pack or an unreadable file is `tampered` (exit 11) with no content. The digest comes from the log, not from file names. A knowledge file inside a run's repository is refused. | `fleet-app/tests/neo_context.rs` (edited layer, edited manifest, truncated, swapped, rolled back, planted), `e2e_context.rs` (real store, exit 11) |
| Files read for layer 6 | From the real repository at the recorded commit, through the hardened git adapter (ADR 16); regular files only, at most 12 files, 64 KiB scanned and 4 KiB shown per file. | `fleet-infra` git tests |
| Worker-written context files | `.fleet/context.md`, `context.json` and `handoff.json` are never trusted: fleet keeps its own copy in the state directory (0700/0600, bounded retention) and recompiles from state; `handoff.json` is read with the `result.json` rules and shown as UNVERIFIED. | `contract_fs` tests, `e2e_context.rs` |
| Digests | The audit log records the pack digest and manifest (`context_compiled`), and handoffs (`handoff_created`) under the hash chain. | `fleet-app/tests/neo_context.rs` |
