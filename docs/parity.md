# Feature parity and roadmap

## 1. Purpose and method

This page compares agent-fleet with the publicly documented feature set of one well-known worktree-based agent
IDE, Orca (https://www.onorca.dev). Its purpose is to make plain which capabilities of that kind of tool
agent-fleet has, which it only partly has and which it will not have, and to fix acceptance criteria for the
phases that are still to be built.

- agent-fleet is an independent, original implementation. It is not affiliated with, endorsed by or derived from
  the project above. No code, text, interface naming or images were taken from it.
- The comparison is based on a one-time read of two public pages (the product's documentation start page and the
  README of its public repository) on 2026-10-31. It may be out of date, it may miss features those pages do not
  mention, and the capability names below are neutral paraphrases, not the product's own terms.
- Only what those pages mention is listed. Where a feature was visible only as a one-line summary, the notes say
  "listed in public docs summary": nothing beyond that line is claimed about how it works.
- The Evidence column only names things that exist in this repository: a test as `file::test_function` (paths are
  relative to `crates/`), a command, a section of the README, or `scripts/demo.sh`. The word `none` means there is
  no evidence. Test names were checked to exist by searching the source; they were not run for this page.

## 2. Status vocabulary

| Status | Meaning |
| --- | --- |
| implemented | Works, and a test or demo output that exercises it exists in this repository and was actually run by the project (the test suite, or `scripts/demo.sh`). |
| partial | Some of the capability works with evidence; the rest is missing or untested. The notes say which part. |
| planned | Assigned to a phase in section 4. No evidence yet. |
| not planned | Deliberately out of scope. Section 5 gives reasons. |

agent-fleet is a headless daemon plus a CLI, a terminal dashboard and (phase 2) a local web dashboard. It has no
desktop GUI application, no editor, no embedded browser, no Linear integration, SSH worktrees only through the system's OpenSSH client (phase 3c) and no
mobile app.

## 3. Matrix

### Core orchestration

| Capability | Status in agent-fleet | Evidence | Notes |
| --- | --- | --- | --- |
| Parallel isolated git worktrees | implemented | `fleet-cli/tests/e2e_flow.rs::max_parallel_two_runs_tasks_side_by_side`, `fleet-infra/src/git.rs::user_checkout_is_never_touched` | One worktree per task under `<repo>/.fleet-worktrees/`. The user's checkout is not touched. |
| Per-task branches | implemented | `fleet-cli/tests/e2e_flow.rs::demo_flow_end_to_end` | Branch `fleet/<run>/<task>`; see the README section "A real run". |
| Fan one prompt across several agents and compare | implemented | `fleet-cli/tests/e2e_fanout.rs::compare_shows_facts_from_the_daemon_the_printed_rule_and_the_overlap`, `fleet-cli/tests/e2e_fanout.rs::a_pick_goes_through_the_approval_merges_that_commit_and_supersedes_the_rest`, `fleet-cli/tests/e2e_fanout.rs::an_approval_is_not_good_for_a_branch_that_moved_after_it`, `fleet-cli/tests/e2e_fanout.rs::hostile_member_output_is_inert_in_the_cli_the_terminal_dashboard_and_the_web`, `fleet-app/tests/fanout.rs::a_pick_asks_for_the_approval_of_the_exact_action_and_then_merges_that_commit`, `fleet-infra/tests/git_diff.rs::a_rename_is_one_change_that_touches_two_paths`, `scripts/fanout-demo.sh` | `fleet fanout`, `fleet compare`, `fleet pick`, `[compare] rank` in `fleet.toml`, read-only web and terminal views (ADR 17; README section "Fan out one prompt to several agents and compare"). Run with scripted fake agents only; no real agent, and nothing on macOS. Compared 2026-10-31. |
| Terminal (pty) sessions per agent | implemented | `fleet-infra/src/pty.rs::runs_a_command_and_captures_sanitized_output`, `fleet-cli/tests/e2e_cli.rs::attach_with_piped_stdin_echoes_input_and_detaches_leaving_the_worker_running` | Each worker has its own pty, session and process group. `fleet attach` is a raw-terminal passthrough, tested with piped stdin only. |
| Terminal splits and tabs | planned | `scripts/web-smoke.mjs` (the dock hosts the read-only panes only) | Phase 5.2 puts live terminal panes in the dock. The dock (tabs, splits, keyboard resizing, validated saved layout) hosts runs, task output, goals, approvals and the review panes today. |
| Scrollback that survives restarts | partial | `fleet-infra/tests/contracts.rs::file_output_logs_satisfy_the_contract` | Bounded sanitized output is saved per attempt. It is not restored into an interactive terminal emulator. |
| Any CLI agent | implemented | `fleet-app/tests/lifecycle.rs::dispatch_builds_the_command_from_the_worker_definition_with_an_allowlisted_environment`, `scripts/demo.sh` | A worker is any command from `fleet.toml`. Only scripted fake agents were run; no specific commercial agent was tested. |
| Task DAG | implemented | `fleet-domain/src/dag.rs::dependencies_come_first`, `fleet-domain/src/dag.rs::cycles_are_rejected_with_the_path`, `fleet-app/tests/lifecycle.rs::dependencies_gate_scheduling_and_their_branches_are_merged_into_dependents` | Dependencies, cycle rejection, concurrency limits per run and globally. |
| Worker supervision and timeouts | implemented | `fleet-app/tests/supervision.rs::the_total_timeout_sends_term_first_and_ends_the_session_as_a_timeout`, `fleet-cli/tests/e2e_flow.rs::a_silent_worker_hits_its_idle_timeout` | Idle and total timeouts, TERM then KILL of the process group, pid-reuse check. |
| Bounded automatic retries | implemented | `fleet-app/tests/supervision.rs::a_nonzero_exit_is_retried_up_to_max_retries_and_then_the_task_fails` | Failure text goes into the next brief. |
| Restart recovery | implemented | `fleet-app/tests/recovery.rs::a_task_running_when_the_daemon_died_is_requeued_without_using_a_retry`, `fleet-cli/tests/e2e_recovery.rs::a_killed_daemon_is_recovered_orphans_are_stopped_and_decisions_survive` | State is rebuilt from the event log; surviving sessions are stopped and their tasks re-queued. |
| Decision gates (agent asks a person) | implemented | `fleet-app/tests/decisions.rs::a_question_opens_a_gate_and_blocks_the_task`, `scripts/demo.sh` | Gate `g1` in the README demo. |
| Approvals bound to the exact action | implemented | `fleet-domain/src/approval.rs::consume_requires_the_exact_approved_action`, `fleet-cli/tests/e2e_security.rs::an_approval_cannot_be_used_after_the_branch_head_changed` | Digest of the exact action, expiry, single use. Not listed in the public docs summary; included because it is a core agent-fleet property. |
| Quality gates before completion | implemented | `fleet-app/tests/lifecycle.rs::gates_run_after_the_worker_and_must_pass_before_completion`, `fleet-cli/tests/e2e_flow.rs::a_clean_refcheck_report_passes_the_gate` | Exit-code and `refcheck-json` formats. |
| Merge into an integration branch | implemented | `fleet-app/tests/merging.rs::after_approval_the_merge_succeeds_and_is_recorded`, `fleet-cli/tests/e2e_flow.rs::demo_flow_end_to_end` | `--no-ff` into `fleet/<run>/integration`, behind an approval. |
| Merge conflict handling | implemented | `fleet-app/tests/merging.rs::a_merge_conflict_aborts_opens_a_gate_and_blocks_the_task` | The merge is aborted and a decision gate opens; the integration branch is unchanged. |
| Agent undo | partial | `fleet-app/tests/recovery.rs::undo_previews_by_default_and_applies_only_when_asked`, `fleet-cli/tests/e2e_cli.rs::a_fake_agent_undo_gets_checkpoints_and_undo_requests` | Delegates to an optional external `agent-undo` tool. Only a fake tool was exercised. |
| Agent-to-agent messaging | implemented | `fleet-cli/tests/e2e_messages.rs::two_workers_exchange_messages_through_a_channel_and_a_worker_cannot_use_fleet_msg`, `fleet-cli/tests/e2e_messages.rs::a_ping_pong_mutes_the_channel_opens_a_gate_and_a_person_unmutes_it`, `fleet-cli/tests/e2e_messages.rs::a_restart_neither_loses_nor_repeats_a_delivery`, `fleet-cli/tests/e2e_messages.rs::a_message_for_a_task_that_starts_too_late_expires_by_ttl`, `fleet-cli/tests/e2e_messages.rs::the_operator_can_write_into_a_to_lead_channel_and_workers_cannot_write_sideways`, `fleet-cli/tests/e2e_messages.rs::secret_shaped_hostile_and_unauthorised_messages_stay_inert`, `fleet-cli/tests/e2e_messages.rs::hostile_subjects_reach_a_brief_only_as_one_quoted_line_behind_their_label`, `fleet-cli/tests/e2e_web.rs::agent_messages_are_served_read_only_and_cannot_be_sent_or_muted_over_http`, `fleet-infra/tests/contracts.rs::file_inbox_satisfies_the_contract`, `fleet-infra/src/msgfs.rs::an_inbox_replaced_by_a_link_while_the_worker_runs_is_refused_and_nothing_is_written`, `scripts/msg-demo.sh`, `fleet-cli/tests/e2e_messages.rs::a_ping_pong_between_two_members_mutes_only_the_one_that_crossed_the_limit_while_a_third_keeps_talking`, `fleet-cli/tests/e2e_messages.rs::junk_files_are_recorded_once_and_then_only_counted_until_the_task_is_muted`, `fleet-cli/tests/e2e_messages.rs::the_notice_is_typed_without_enter_and_only_when_switched_on`, `fleet-app/tests/messaging_security.rs` | Mediated, policy-bound and file-based: workers write `.fleet/outbox`, the daemon decides, records and writes `.fleet/inbox`; it is not live terminal injection (typing a one-line notice, without Enter, is opt-in and off by default). Channels are declared by the operator in a plan; limits, loop and storm protection, secret-shape refusal, the mute kill switch and per-channel approvals are tested at the domain, application and adapter layers and through the real binary; the terminal and web dashboards show messages read only, labelled as unverified peer content. Only scripted agents were run: what a real model does with injected text is not verified. See ADRs 24 to 26. |
| Goal ledger and proof of completion (verification contracts, evidence bound to a commit) | implemented | `fleet-cli/tests/e2e_neo.rs::a_failing_contract_then_a_passing_one`, `fleet-cli/tests/e2e_neo.rs::a_worker_report_is_unverified_until_fleets_own_run_finished`, `fleet-app/tests/neo_verify.rs::a_task_with_a_contract_completes_only_after_fleets_own_run_passed`, `scripts/neo-demo.sh` | Not in the public docs summary; included because it is part of agent-fleet (docs/neo.md). Checks run on a clean export of the committed work, not in a sandbox. Milestones N1 and N2 (context packs and handoffs, opt-in per run) exist. |
| Agents driving the orchestrator through its CLI | not planned | `fleet-cli/tests/e2e_security.rs::a_worker_cannot_approve_resolve_or_read_status_but_the_operator_can` | Workers are refused on purpose. Listed in public docs summary for the other product. |

### Review, editing and integrations

| Capability | Status in agent-fleet | Evidence | Notes |
| --- | --- | --- | --- |
| Source-control view and diff view | implemented | `fleet-infra/tests/repo_read.rs::renames_are_detected_with_their_similarity_and_old_path`, `fleet-cli/tests/e2e_web.rs::the_workbench_shows_the_recorded_commit_of_a_task_from_the_real_repository`, `scripts/web-smoke-panes.mjs` | Read-only diff of a task's commit against the run's base, unified and side by side (ADR 19). No staging, no working-tree status. Rust tests, a fake DOM and `scripts/workbench-demo.sh` (headless Chromium, 13 loads, screenshots in `docs/img`); no desktop browsers. |
| Diff annotation (comments sent back to the agent) | not planned | none | A person can answer a gate or send text to a live worker (see the web rows and `fleet send`). |
| File editor | planned | none | Phase 5.4 (edits through a daemon API with path containment). Nothing edits files today. |
| File explorer and file viewer | implemented | `fleet-infra/tests/repo_read.rs::the_git_reader_satisfies_the_contract`, `fleet-cli/tests/e2e_web.rs::clients_cannot_name_paths_commits_or_tasks_the_daemon_did_not_record`, `scripts/web-smoke-panes.mjs` | Read-only tree and file view at the recorded commit, never the worktree (ADR 19). Also loaded in headless Chromium (`scripts/workbench-demo.sh`); no desktop browsers. |
| Rich previews (Markdown, images; PDF) | partial | `fleet-domain/src/repo/markdown.rs` (property and adversarial tests), `fleet-cli/tests/e2e_web.rs::the_workbench_shows_the_recorded_commit_of_a_task_from_the_real_repository`, `scripts/web-smoke-panes.mjs` | Markdown subset and png/jpeg/gif/webp images only (SVG excluded). PDF is planned for phase 5.5 (first step: the system viewer). Also loaded in headless Chromium (`scripts/workbench-demo.sh`); no desktop browsers. |
| GitHub integration (PRs, issues) | partial (implemented and tested against a fake `gh`; a real account is owner-verified) | `fleet-app/tests/forge.rs::an_approved_pull_request_is_preflighted_pushed_created_and_recorded`, `forge.rs::changing_anything_after_the_approval_invalidates_it`, `fleet-infra/tests/forge_gh.rs::gh_gets_a_minimal_environment_and_never_a_token`, `forge_gh.rs::a_rate_limit_blocks_further_calls_without_starting_gh_and_comes_back_after_the_wait`, `forge_push.rs::exactly_the_approved_commit_goes_to_exactly_one_ref_of_the_remote`, `fleet-cli/tests/e2e_forge.rs`, `scripts/github-demo.sh` | Read issues and pull requests (checks, reviews), issue to task (untrusted text), pull request creation behind an approval bound to repository, base, head branch and commit, title, body and draft flag; read-only dashboard and TUI views. `pr comment` and review actions are not implemented. Compared 2026-10-31. Verified against `gh` 2.79.0 help text only; a real account, real HTTPS push credentials and macOS are not verified (README "What was not verified"). |
| Linear integration (issues, boards) | planned | none | Phase 3. |
| SSH remote worktrees | implemented | `fleet-infra/tests/ssh_remote.rs` (a real sshd: pinning, wrong key, forwarding, quoting, kill escalation, keepalive, jump host, the `RemoteHost` contract), `fleet-cli/tests/e2e_remote.rs` (the real binary and daemon), `scripts/ssh-demo.sh` (output in the README), ADR 27 | Compared 2026-10-31. Phase 3c: `fleet host add/list/trust/remove/doctor`, `[hosts.<name>]`, `task add --host`; host key pinning by an explicit operator step, no agent forwarding, bounded remote output, bundle transfer verified before use. Criteria 6-10 of phase 3 are met by tests against a local sshd; not verified on macOS, with real accounts or with real agents (owner-verified). Agent forwarding is not offered at all. A remote task of a compiled run gets the same pack and brief as a local one and its handoff comes back; message channels do not reach remote tasks (a plan that names one is refused with a clear error, tested). |
| Self-hosted server or cloud VM runtime | not planned | none | The daemon is local only (unix socket, loopback web). Listed in public docs summary. |
| Embedded browser with element capture and design mode | planned | none | Phase 4. |
| Mobile companion | planned | none | Phase 4. |
| Desktop computer-use for agents | not planned | none | Listed in public docs summary. |

### Interface and everyday use

| Capability | Status in agent-fleet | Evidence | Notes |
| --- | --- | --- | --- |
| Terminal dashboard (TUI) | implemented | `fleet-cli/src/tui/ui.rs::full_render_header_and_task_row_are_exact`, `fleet-cli/src/tui/ui.rs::hostile_text_never_reaches_the_buffer_as_control_characters` | Runs, DAG rows, sanitized output, gates, approvals. |
| Keyboard bindings | implemented | `fleet-cli/src/tui/keys.rs::approve_and_deny_only_in_approvals_focus`, `fleet-cli/src/tui/keys.rs::answer_input_flow` | Fixed keys in the TUI only; not configurable. |
| Command palette and quick open | not planned | none | Listed in public docs summary. |
| Attention list for gates and approvals | implemented | `fleet-cli/src/tui/ui.rs::shows_approval_digest_prefix_and_gate_text`, README "A real run" ("waiting for you") | Shown in `fleet status` and the TUI. |
| Desktop or push notifications | not planned | none | Listed in public docs summary. |
| Scriptable CLI with JSON output | implemented | `fleet-cli/tests/e2e_cli.rs::every_read_command_prints_a_json_document_with_its_schema` | Stable schemas and exit codes. |
| Usage and cost tracking, account switching | not planned | none | Listed in public docs summary. |
| Plugin system and extensions | not planned | none | Listed in public docs summary. |
| Auto-update and packaged installers | not planned | none | Build from source with `cargo build --release`. |
| Platforms | partial | `fleet-infra/tests/soak.rs::memory_stays_flat_while_chatty_workers_flood_their_terminals` | Linux tested in a container. macOS code type-checks but was not run. Windows is not supported. |

### Local web dashboard (phase 2)

Test paths are relative to `crates/`. `scripts/web-demo.sh` drives the real daemon with curl and loads the real page in a headless Chromium
inside the `docker/Dockerfile.web` image (output in the README section "Web dashboard"); `scripts/web-smoke.mjs` runs the page
script against a hand-written fake DOM under node. Neither was run on macOS or in a desktop browser.

| Capability | Status in agent-fleet | Evidence | Notes |
| --- | --- | --- | --- |
| Web view of runs and the task DAG | implemented | `fleet-cli/tests/e2e_web.rs::full_decision_flow_over_http`, `scripts/web-demo.sh` (Chromium: DAG page shows the task titles and status classes), `scripts/web-smoke.mjs` | Read-only. The DAG is drawn with SVG elements created by the script. |
| Live worker output in the browser | implemented | `fleet-cli/tests/e2e_web.rs::live_output_sse_and_send_input`, `fleet-infra/tests/web_http.rs::an_event_stream_delivers_output_then_end_and_closes`, `fleet-infra/tests/soak.rs::memory_stays_flat_while_chatty_workers_flood_their_terminals` | Server-sent events, same sanitized, redacted, bounded output as the terminal views; streams are capped in number, lifetime and bytes. The live output panel itself was exercised only against the fake DOM (`scripts/web-smoke.mjs`), not in a browser. |
| Approve, deny and answer gates from the web | implemented | `fleet-cli/tests/e2e_web.rs::full_decision_flow_over_http`, `fleet-infra/tests/web_http.rs::bodies_are_validated_strictly_and_rejected_before_the_daemon_sees_them` | The approval's exact action and digest are shown to the person deciding and the digest is mandatory in the request; a wrong digest is refused. |
| Send input to a worker and merge from the web | implemented | `fleet-cli/tests/e2e_web.rs::live_output_sse_and_send_input`, `fleet-cli/tests/e2e_web.rs::full_decision_flow_over_http` | Merge still returns `approval_required` until a person approved that exact action. |
| Goals, evidence and verification status in the browser (read only) | implemented | `fleet-cli/tests/e2e_web.rs::goals_and_evidence_are_served_read_only`, `fleet-cli/tests/e2e_web.rs::goal_documents_cannot_be_changed_over_http`, `fleet-infra/tests/web_http.rs::the_goal_ledger_and_verification_are_served_read_only_through_the_daemons_own_commands`, `scripts/web-demo.sh` (Chromium: goals page with an `unverified` badge), `scripts/web-smoke.mjs` | Remaining work, goal tree, checks, evidence with commit and tree digests; no way to add goals, attach contracts or start a verification from the page. |
| Event log and daemon health view | implemented | `fleet-cli/tests/e2e_web.rs::health_and_log_panels`, `fleet-infra/tests/web_http.rs::health_and_log_documents_are_cached_and_report_tampering` | Chain verification status comes from re-verifying the log (cached for five seconds); the health panel is the `fleet doctor` checklist. |
| Authenticated access (token, header session, CSRF, Host and Origin checks) | implemented | `fleet-infra/tests/web_http.rs::the_api_needs_credentials_and_the_bearer_token_works`, `fleet-infra/tests/web_http.rs::state_changing_requests_need_origin_csrf_token_and_json`, `fleet-infra/tests/web_http.rs::a_wrong_host_is_refused_before_authentication`, `fleet-infra/tests/web_http.rs::failed_credentials_lock_authentication_and_the_lock_applies_to_the_right_token_too`, `fleet-infra/tests/web_http.rs::a_session_id_sent_as_a_cookie_is_not_a_credential`, `fleet-infra/tests/web_http.rs::a_lockout_does_not_cut_off_a_logged_in_session_and_unknown_sessions_are_not_failures`, `fleet-cli/tests/e2e_web.rs::login_flow` | Loopback listener only by default; plain HTTP, no TLS (SECURITY.md). The session is a header, never a cookie. |
| Refusal of requests originating from workers | implemented | `fleet-cli/tests/e2e_web.rs::a_worker_cannot_use_the_web_ui_even_with_the_token`, `fleet-cli/tests/e2e_web.rs::a_quality_gate_child_cannot_use_the_web_ui_or_the_socket`, `fleet-cli/tests/e2e_web.rs::a_process_started_by_an_approved_command_cannot_use_the_web_ui`, `fleet-cli/tests/e2e_web.rs::a_verification_check_process_cannot_use_the_web_ui`, `fleet-infra/tests/origin.rs::processes_started_by_the_daemon_process_are_refused_on_both_interfaces_though_nobody_listed_them`, `fleet-cli/tests/e2e_web.rs::web_token_never_reaches_workers_or_disk`, `fleet-infra/tests/web_http.rs::a_refused_process_gets_403_and_its_request_never_reaches_the_daemon` | Same worker-origin rule as the control socket, applied to the process behind the TCP connection (Linux: tested; macOS: `lsof`-based, type-checked only). A worker that detaches from its process tree is the documented gap. |
| Hostile text cannot inject markup or script | implemented | `fleet-app/tests/hostile_text.rs::markup_quotes_and_ampersands_in_a_title_come_back_byte_for_byte`, `fleet-cli/tests/e2e_web.rs::hostile_task_text_is_inert_in_the_json`, `fleet-infra/src/web/assets.rs::the_script_never_turns_text_into_markup_or_code`, `scripts/web-smoke.mjs`, `scripts/workbench-demo.sh` | Worker output and file text are treated as untrusted: sanitized by the domain, JSON-escaped, shown with `textContent`, CSP without inline code. The review workbench was also driven with hostile names and contents through the real page in headless Chromium (`scripts/workbench-demo.sh`: the DOM has no injected element, attribute or link); the dashboard's older pages were not. |

## 4. Phase roadmap

### Phase 1 (done): daemon, CLI, terminal dashboard

A single daemon supervising workers in git worktrees; the CLI (`plan`, `start`, `status`, `gate`, `approve`,
`merge`, `tail`, `send`, `attach`, `events`, `verify-log`, `doctor`, `cleanup`, `undo`); the TUI; the hash-chained
event log. Evidence: the test suites listed above and the demo in the README.

### Phase 2 (done): local web dashboard

Scope: an HTTP server inside the daemon, bound to loopback and protected by a token, serving a browser view of
runs and the task DAG, live sanitized output, gates and approvals (decide), sending input to a worker, merging
(through the same approval), the event log and health. It uses the same use cases and the same worker-origin rule
as the control socket; it adds no new authority for workers. ADR 10 and SECURITY.md carry the threat model.

Deliberately omitted: creating runs or loading plans from the web, stopping, cancelling or retrying tasks from the
web, editing `fleet.toml`, any third-party web framework or async runtime, TLS, and a diff or file view. Remote
(non-loopback) access exists only behind `--web-allow-remote`, with a warning, and is not recommended: use an SSH
tunnel.

### Phase 3: GitHub and Linear integration, SSH remote worktrees

Acceptance criteria (each must be a test or a documented manual check recorded in the phase's ADR):

1. Tokens for GitHub or Linear are never written to the event log, the output logs, status documents or any
   worker environment; a test plants a recognizable token and asserts it appears in none of them.
2. Creating a pull request requires an approval bound to the digest of branch, title and body; changing any of the
   three after approval invalidates it. An approval is single use.
3. Read access to issues and pull requests is rate-limit aware: on a limit response the daemon backs off and keeps
   running; tests cover a limit response and a timeout.
4. With no network, the daemon, CLI and dashboards still work for local runs; integration commands fail with a
   clear error and change no state.
5. Text from issues, pull requests and comments is treated as untrusted (sanitized, redacted, quoted in briefs).
6. SSH worktrees pin the host key (no trust-on-first-use without an explicit operator step) and refuse a changed
   key.
7. SSH agent forwarding is off by default and only enabled by an explicit per-host setting. (Phase 3c implements the stricter variant: there is no setting that enables it; the key of the host entry is the only credential.)
8. Remote workers are subject to the same admission, environment allowlist, sanitization, redaction and output
   bounds as local workers; a test runs the same hostile-output cases against a remote session.
9. Remote output and remote command duration are bounded (size and time limits) and a stalled connection ends in
   a failed attempt, not a hung daemon.
10. SSH tests run against a local `sshd` in a container; no test needs a real remote machine.
11. The daemon adds no listener other than the loopback ones that already exist.

### Phase 4: embedded browser and design capture, mobile companion

Acceptance criteria:

1. The browser used for capture cannot reach the control socket or the web dashboard, and cannot read the
   dashboard token; a test attempts both from the capture context and expects refusal.
2. Element capture output (HTML, styles, any text) is sanitized and size-bounded before it reaches an agent brief
   or the event log; hostile markup cannot pose as fleet's own statements.
3. Captured data is never executed or interpreted as a command by the daemon.
4. The mobile companion connects only over an authenticated tunnel; unauthenticated or replayed requests are
   refused, and a lost device's access can be revoked.
5. The companion can do no more than the web dashboard can do (including approvals bound to digests), and
   worker-origin requests are still refused.
6. No new inbound listener beyond loopback by default; anything wider is opt-in, documented in SECURITY.md and
   covered by a test that checks the bound addresses.

### Documentation and verification criteria (every phase)

1. An ADR in `docs/adr` records the decision, the alternatives and the threat-model change.
2. SECURITY.md is updated: threat-model table, tests column and known limitations. Claims name only tests that
   exist.
3. Tests exist at each layer touched: domain unit tests, application tests with the in-memory ports, adapter
   contract tests, and end-to-end tests through the real binary.
4. Coverage targets: domain at least 95 percent, application at least 90 percent, overall at least 85 percent,
   measured with `scripts/coverage.sh`; the measured numbers are published, not assumed.
5. No new dependency (direct or transitive) younger than 7 days, checked mechanically as described in
   SECURITY.md; direct dependencies stay pinned exactly.
6. This page is updated: statuses change only when the named test or demo output exists and was run.
7. The README stays honest: it states what was and was not run, including platforms.

## 5. Not planned

- (Reversed: the graphical workbench is in scope, see the plan below.) It is built as a web UI served by the daemon
  (phase 5.x); read-only review panes exist (phase 5.1, ADR 19); the terminal panes, interactive terminal, editor, PDF
  viewing and a thin desktop shell are **planned** in phases 5.2 to 5.6 (the shell is verified by the owner on macOS, not
  in CI). Diff annotation (comments back to the agent) is planned for phase 5.2.
- Sandboxing of workers: it needs containers, VMs or another user and cannot be done honestly with the current
  no-`unsafe`, small-dependency design; SECURITY.md states that workers are not sandboxed and recommends outside
  isolation.
- Plugins and extensions: they would execute third-party code inside the trusted daemon.
- Usage and cost tracking, account switching: this depends on each agent vendor's private interfaces.
- Windows support: ptys, process groups, unix sockets and `/proc` assumptions.
- Self-hosted server or cloud VM runtime, auto-update and packaged installers, command palette: outside the scope
  of a small local daemon.
