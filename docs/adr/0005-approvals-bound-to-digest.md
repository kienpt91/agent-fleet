# 5. Approvals are bound to the digest of one exact action

Status: accepted

## Context
"Approve" must mean "approve this", not "approve whatever happens next". Workers must not be able to
create or grant approvals.

## Decision
An approval stores the action (tool, command, arguments) and the task id and a SHA-256 digest over their
canonical encoding. Using it re-computes the digest of the action about to run; any difference refuses.
Approvals expire and are single use. A merge approval covers the integration branch, the task branch and the
exact commit being merged. Only the daemon creates approvals (for a merge, or from a worker's
`question.json` of kind `approval`), only a person decides them over the control socket, and the daemon
refuses control requests from worker processes (origin check on the peer's process ancestry).

## Consequences
A worker can ask but cannot grant. A same-user process that detaches completely from the worker's
process tree and finds the socket path could still connect; the 0700 directory and the peer uid check do
not help against the same user. Run workers under a different user for a hard guarantee.
