# 5. Approvals are bound to the digest of one exact action and are used once

Status: accepted (revised after a security review)

## Context
"Approve" must mean "approve this", not "approve whatever happens next". Workers must not be able to
create or grant approvals. A restart of the daemon must not run an approved command a second time.

## Decision
An approval stores the action (tool, command, arguments) and the task id and a SHA-256 digest over their
canonical encoding. Using it re-computes the digest of the action about to run; any difference refuses.
Approvals expire. An approval is **consumed when the action starts**, and that is recorded in the event log
(`approval_consumed`, `external_command_started`) before the command is launched, so a restart can never
run it again; a command found started-but-unfinished at startup is recorded as interrupted (outcome unknown).
A merge approval covers the integration branch, the task branch and the exact commit; the merge is performed
on that commit after verifying that the branch still points at it. A command a worker asks for must be an
absolute path outside the worktree and its repository; it runs with the worktree as working directory, whose
contents are not part of the digest (documented limitation).

## Consequences
A worker can ask but cannot grant (see ADR 0009 for how the control plane keeps workers out). The outcome of a
command that was running while the daemon died is unknown and is never retried automatically.
