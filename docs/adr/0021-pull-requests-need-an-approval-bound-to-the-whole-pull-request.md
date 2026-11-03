# 21. A pull request is opened only after an approval bound to the whole pull request, and the push is one ref

Status: accepted. Extends ADR 5.

## Context
Publishing leaves the machine. An approval for "a pull request for task t" would let the branch, the title, the base or
the body change between the human's decision and the push.

## Decision
- The approval's action is `forge pull-request.create` with the arguments repository (`host/owner/name`), base, head
  branch, **head commit SHA**, title, body and `draft|ready`, plus the task (as for every approval). The digest is recomputed
  from the repository and the arguments each time `fleet pr create` runs; any difference is another digest and another
  approval request. An approval is single use (`approval_consumed`, then `pull_request_started`, both recorded before
  anything is pushed; a publication lost to a restart is recorded as failed and never started again).
- Text is cleaned and redacted before it is approved; a body longer than 4 KiB is refused, not cut. An issue-linked task gets
  the default title `Work on issue #n`: issue text never becomes approval text on its own authority.
- Two steps so that an approval is not spent on a call that cannot work: a *preflight* (`GET repos/o/r`) first; the approval
  is consumed only after it passed and the plan, computed again, has the same digest.
- The push is `git push --porcelain --no-verify --no-follow-tags --no-recurse-submodules -- <url> <sha>:refs/heads/<branch>`
  through the hardened git runner (ADR 6): no `+`, no `--force`, no tags, no extra refs; a remote branch with other commits makes
  it fail. The URL is re-read from the repository's configuration, compared with the approved repository and **rebuilt**
  (`fleet_forge::push::push_target`); ssh remotes are refused (no agent or key is involved), a local path needs
  `allow_local_remote`. A repository whose configuration rewrites URLs (`url.*`), steers HTTP (`http.*`, proxies) or names a git
  proxy is refused, because git would apply that after the check. The branch must still be the approved commit when git starts.
- Credentials for the push are explicit: `none` (default), `gh` (git runs `gh auth git-credential` as the credential helper of
  this one call) or `helper` (one absolute-path helper). Nothing of the operator's own git configuration is read.

## Consequences
Pushing to GitHub over https works only with `push_credentials = "gh"` or `"helper"`; an operator who relies on an ssh remote
sets `push_url` to the https URL. The owner's credential helper is used only if named.
