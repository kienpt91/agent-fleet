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
  is consumed only after it passed, the plan, computed again, has the same digest, and the adapter says it can take the
  publication now (`ForgeJobs::can_start`: `gh` trusted, the rate gate open, a free slot). A refusal at that point costs nothing.
- The prompt shows what the digest covers: `fleet approval list` prints the repository, base, branch, commit, the draft or ready
  flag, the title and then the body (at most 4 KiB, quoted line by line), each on its own line and none cut short.
- The push is `git push --porcelain --no-verify --no-follow-tags --no-recurse-submodules -- <url> <sha>:refs/heads/<branch>`
  through the hardened git runner (ADR 6): no `+`, no `--force`, no tags, no extra refs; a remote branch with other commits makes
  it fail. The URL is re-read from the repository's configuration, compared with the approved repository and **rebuilt**
  (`fleet_forge::push::push_target`); ssh remotes are refused (no agent or key is involved), a local path needs
  `allow_local_remote`. A repository whose configuration rewrites URLs (`url.*`), steers HTTP (`http.*`, proxies) or names a git
  proxy is refused (and so is one whose configuration cannot be read), as a message to the operator. The refusal is not what
  protects the push: git runs in a **private, throwaway git directory** with an empty configuration and the repository's object
  store as its alternate, so a worker that rewrites the shared `.git/config` after the check changes nothing. The branch must
  still be the approved commit when git starts.
- The repository requests go to is `[github] repo` when set. Otherwise it is read from the run's remote, a line of
  `.git/config` a worker can rewrite: it is recorded when the run is created (`forge_repo_recorded`), a later different answer is
  refused until the operator pins `repo`, and every `issue to-task` and `pr create` reply and approval request says which
  repository it is and where that came from. Pin `repo` whenever workers run.
- Credentials for the push are explicit: `none` (default), `gh` (git runs `gh auth git-credential` as the credential helper of
  this one call) or `helper` (one absolute-path helper). Nothing of the operator's own git configuration is read.

## Consequences
The projection that guards against doing the same thing twice (an issue becomes one task, a commit one pull request) and the list of
publications without an outcome are not capped; only the lists the views show keep the newest 512.

Pushing to GitHub over https works only with `push_credentials = "gh"` or `"helper"`; an operator who relies on an ssh remote
sets `push_url` to the https URL. The owner's credential helper is used only if named.
