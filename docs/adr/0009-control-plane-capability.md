# 9. The control plane: fail-closed admission, a per-run token and a hidden default

Status: accepted

## Context
Workers run as the operator's user, so the socket's file mode cannot keep them out. A first version checked the
peer's process ancestry per request and allowed the request when the peer could no longer be inspected, so a
worker child could pipeline requests and exit before they were evaluated.

## Decision
Defence in depth, none of it a hard boundary against a deliberate same-user attacker:
1. **Admission at accept.** Before any request is read the daemon identifies the peer process; an unknown pid, a
   process that is gone, an unreadable ancestry or a worker (or a descendant of one) is refused with one `forbidden`
   line and the connection is closed. Every request is checked again.
2. **A per-run capability token.** 256 random bits in `control/token` (0600), regenerated at every daemon start and
   required, constant-time compared, in every request except `ping`. Workers never receive it (not in their
   environment, arguments or worktree).
3. **A hidden default.** Workers get `XDG_STATE_HOME` forced to a per-task directory, so a habitual `fleet` call by an
   agent finds no daemon instead of the operator's.
4. **Bounded connections.** A per-request deadline (not per read), an immediate close on refusal, and pty input through
   a bounded queue so a worker that does not read its stdin cannot hang a connection.

## Consequences
Casual and accidental access fails; a hostile worker that reads `/proc` and the token file and detaches from its
process tree can still act as the operator. SECURITY.md says so. Hard isolation needs another user, a container or a VM.
