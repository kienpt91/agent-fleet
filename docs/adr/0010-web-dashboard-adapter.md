# 10. The web dashboard: a hand-written loopback adapter with its own token, admission and CSRF rules

Status: accepted

## Context
Phase 2 adds a local web dashboard (runs and DAG, live task output, the gate and approval queue, send input, merge,
the event log with its chain status, the doctor checks). A browser is a new kind of client with new ways to go wrong:
any web page the operator visits can send requests to `127.0.0.1` (CSRF), a hostile DNS name can be pointed at it
(rebinding), a page renders text it is given (injection), and the daemon's workers run as the same user and can open a
TCP connection as easily as a socket.

## Decision
A new adapter in `fleet-infra` (`web/`), started by `fleet daemon --web <addr>`, off by default.

**No framework.** The server is about three thousand lines on `std::net` (a third of them unit tests): an HTTP/1.1 reader and writer with hard bounds
(request line 2 KiB, head 8 KiB, 40 headers, body 16 KiB, one deadline of 10 s for the whole request, 10 s write timeout),
one request per connection (`Connection: close`; no keep-alive, no pipelining, no chunked bodies, no `Expect`, no
upgrades), a first-byte deadline of 3 s, a cap of 32 connections, 8 per remote address and 4 event streams, one thread per connection (ADR 3). Malformed input is answered
with a status, never a panic. No new third-party crate was added; an architecture test fails if an HTTP, async or TLS
crate appears in any `Cargo.toml` or in `Cargo.lock`.

**It reuses the application layer.** Every API route is a command the control socket already has (`status`, `events`,
`task.tail`, `task.send`, `gate.resolve`, `approval.decide`, `merge`); the daemon's own handler validates and runs them
(`daemon::WEB_COMMANDS` is the allowlist; starting runs, stopping, cancelling, attaching and shutting down stay on the
socket). The adapter sees the daemon through a small `WebBackend` trait, so the web module imports none of the other
adapters and the adapter tests run against a fake. Approvals stay bound to the digest: the page must send the digest it
displayed (`digest` is mandatory in the request), the domain compares it with the approval, and a merge from the web
still returns `approval_required` until a person approved that exact action.

**Static assets** are one HTML file, one script and one stylesheet written by hand and embedded with `include_str!`.
No CDN, no external request, no inline script or style, no build step, no npm. Text from the daemon is shown with
`textContent` only; a source-scan test fails if the script contains `innerHTML` and its relatives.

**Threat model and defences**, in the order a request meets them:

| Threat | Defence | Test |
| --- | --- | --- |
| A worker, or anything the daemon started for one, talks to the dashboard | *Admission*, after the request was read and before anything is acted on: the client's end of the TCP connection is found in the kernel's socket table (`/proc/net/tcp*` and `/proc/*/fd` on Linux; `lsof` on macOS, not run there) and every holder goes through the control socket's rule. That rule refuses a live worker's session, its process group and descendants, **and everything the daemon itself started**: a process with the daemon among its ancestors, or listed by the shared job tracker (the process groups of gates, approved commands, verification checks, the undo tool and git, and their detached descendants), see [ADR 15](0015-one-origin-rule-for-everything-the-daemon-starts.md). An unidentifiable loopback peer is refused. The web token is held in the daemon's memory only and printed once, to a terminal: not in the state directory, not in any log or event, not in a worker's environment, arguments, brief or worktree | `fleet-domain::supervision`, `fleet-infra::daemon` (origin tests), `jobs` (the tracker), `web::peer`, `web_http.rs`, `e2e_web.rs` (a worker, a gate child and a descendant of an approved command, each with a leaked token, get 403) |
| A process on the same host reads the session from the browser's traffic to another port | **No cookie.** A cookie is scoped to the host, not the port, and `SameSite` ignores ports, so a session cookie would be sent by the browser to every other server on `127.0.0.1` (a dev server a worker started, for example). The session id is returned in the login reply, kept in the page's `sessionStorage` (per tab, per origin including the port) and sent in an `X-Fleet-Session` header; the server never reads a `Cookie` header, so a stolen or planted old-style cookie is no credential. Event streams, which `EventSource` could not authenticate with a header, are read with `fetch` | `web_http.rs::a_session_id_sent_as_a_cookie_is_not_a_credential`, `web::assets` source scan, `scripts/web-smoke.mjs` |
| Another user on the machine | Binds loopback only; the token; peers that cannot be identified are refused | as above |
| DNS rebinding | Strict `Host` check on every request (the printed address or `localhost`, with the bound port; for a remote bind only IP literals) | `web::tests::host_header_table`, `web_http.rs` |
| Cross-site requests (CSRF) | The credential is not ambient (no cookie), every state-changing request still needs `Origin` equal to the dashboard, `Sec-Fetch-Site: same-origin` when sent, a per-session CSRF token in `X-CSRF-Token` (constant-time compare) and `Content-Type: application/json`; `Sec-Fetch-Site: cross-site` is refused on reads as well; no CORS header is ever sent | `web_http.rs` |
| Token guessing | 256-bit token, constant-time comparison, failed guesses counted per client (all local processes are one client, a remote address is its own) and 5 within 60 s lock *bearer and login* for that client for 60 s. Unknown session ids are refused without counting and live sessions keep working, so a stale tab or a restarted daemon cannot lock the operator out and a remote host can lock only itself | `web::auth`, `web_http.rs` |
| Script injection through titles, output, branch names | The domain rejects control and invisible characters in names and titles and sanitizes output and worker text; the JSON adds `\u` escapes for `<`, `>`, `&`, controls and bidi characters; the page uses `textContent`; CSP `default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `Cache-Control: no-store` | `fleet-app/tests/hostile_text.rs`, `web::assets` source scans, `web_http.rs`, `scripts/web-smoke.mjs`, headless Chromium (README) |
| Keystroke injection through "send input" | The web path refuses control characters (escape sequences, carriage return, backspace) and invisible characters in the text, other than tab and line breaks | `web_http.rs::text_sent_to_a_worker_may_not_carry_terminal_control_characters` |
| Resource exhaustion | Every size above is bounded before it is buffered; a connection that sends nothing is cut after 3 s and causes no process-table scan (admission runs after the read); slowloris is cut by the per-request deadline; connection, per-remote-address and stream caps; streams end after 10 min or 4 MiB and when their session ends, and the page reconnects with `Last-Event-ID`; a client that stops reading is dropped by the write timeout; the soak test runs with a connected stream. Slots are not reserved for authenticated clients (not knowable before the request arrives): a local process that passes admission can still keep them busy | `web_http.rs`, `soak.rs` |
| Token on the wire and in logs | There is no TLS. Loopback only by default; a non-loopback address needs `--web-allow-remote` and prints a warning. Use an SSH tunnel for remote use. The login URL is printed only when stderr is a terminal (or with `--web-print-login-url`) so it does not land in a log by accident | `e2e_web.rs`, `daemon::tests` |

**Login.** The daemon prints `http://127.0.0.1:<port>/#token=<hex>` once to its terminal (and says so instead, without the token,
when stderr is not a terminal). The fragment never reaches a server; the page POSTs it to `/api/login`, receives a session id
and a CSRF token in the reply, keeps them in `sessionStorage` and removes the token from the address bar. `#token=<hex>&next=/run/r1`
lands on a page. The token also works as `Authorization: Bearer` for scripts. It lives until the daemon stops. A one-time login
code with a short life, which the review suggested, was **not** adopted: scripts and the demo use the token as a bearer token, a
code would make them need a second secret, and the operator whose session expired would have to restart the daemon (a control
socket command that mints a fresh code would solve that; it is not built). A URL that was captured stays valid until the
daemon stops; SECURITY.md says where that can happen.

## Consequences
The dashboard's credentials (the token, a session id) are defence in depth against code of the same user, **not a security
boundary**, and so are the control socket's token and peer checks: a process of yours that can read the daemon's memory, the
terminal or the browser can take the token and act as the operator, as it could already with the control token (SECURITY.md).
Admission makes accidental, scripted and descendant access by workers and by everything the daemon runs for them fail; it does
not stop a process that has left the tree and the process group of all of these (`setsid`, `systemd-run --user`). macOS support for
admission (`lsof`) type-checks but was not run. The page is verified in headless Chromium inside a container (README) but only against
the demo data.
