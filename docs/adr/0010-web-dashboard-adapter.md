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
upgrades), a cap of 32 connections and 4 event streams, one thread per connection (ADR 3). Malformed input is answered
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
| A worker (same user) talks to the dashboard | *Admission*: the client's end of the TCP connection is found in the kernel's socket table (`/proc/net/tcp*` and `/proc/*/fd` on Linux; `lsof` on macOS, not run there) and its pids go through the control socket's rule: a worker, a descendant of one, or an unidentifiable loopback peer (already gone, hidden) is answered 403 before its request is read. The web token is held in the daemon's memory only and printed once on its terminal: not in the state directory, not in any log or event, not in a worker's environment, arguments, brief or worktree | `web::peer`, `web_http.rs`, `e2e_web.rs` (a worker with a leaked token still gets 403) |
| Another user on the machine | Binds loopback only; the token; peers that cannot be identified are refused | as above |
| DNS rebinding | Strict `Host` check on every request (the printed address or `localhost`, with the bound port; for a remote bind only IP literals) | `web::tests::host_header_table`, `web_http.rs` |
| Cross-site requests (CSRF) | Cookie is `HttpOnly; SameSite=Strict`; every state-changing request needs `Origin` equal to the dashboard, `Sec-Fetch-Site: same-origin` when sent, a per-session CSRF token in `X-CSRF-Token` (constant-time compare) and `Content-Type: application/json`; `Sec-Fetch-Site: cross-site` is refused on reads as well; no CORS header is ever sent | `web_http.rs` |
| Token guessing | 256-bit token, constant-time comparison, failed credentials counted globally (5 in 60 s lock authentication for 60 s; clients are all local processes, so there is no useful per-address key) | `web::auth`, `web_http.rs` |
| Script injection through titles, output, branch names | The domain rejects control and invisible characters in names and titles and sanitizes output and worker text; the JSON adds `\u` escapes for `<`, `>`, `&`, controls and bidi characters; the page uses `textContent`; CSP `default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `Cache-Control: no-store` | `fleet-app/tests/hostile_text.rs`, `web::assets` source scans, `web_http.rs`, `scripts/web-smoke.mjs`, headless Chromium (README) |
| Resource exhaustion | Every size above is bounded before it is buffered; slowloris is cut by the per-request deadline; connection and stream caps; streams end after 10 min or 4 MiB and the browser reconnects with `Last-Event-ID`; a client that stops reading is dropped by the write timeout; the soak test runs with a connected stream | `web_http.rs`, `soak.rs` |
| Token on the wire | There is no TLS. Loopback only by default; a non-loopback address needs `--web-allow-remote` and prints a warning. Use an SSH tunnel for remote use | `e2e_web.rs` |

**Login.** The daemon prints `http://127.0.0.1:<port>/#token=<hex>` once to its terminal. The fragment never reaches a
server; the page POSTs it to `/api/login`, receives a session cookie (a random id, not the token) and a CSRF token, and
removes the fragment from the address bar. The token also works as `Authorization: Bearer` for scripts. It lives until
the daemon stops (there is no one-use login code: a one-use code would have to be a second secret next to the bearer
token, and an operator whose session expired would have to restart the daemon).

## Consequences
The dashboard is a convenience for the operator, not a new boundary: a same-user hostile process that reads the daemon's
memory or the terminal output can take the token and act as the operator, as it could already with the control token
(SECURITY.md). The admission check makes a worker's casual or scripted access fail; it does not stop a worker that
detaches from its process tree first. macOS support for admission (`lsof`) type-checks but was not run. The page is
verified in headless Chromium inside a container (README) but only against the demo data.
