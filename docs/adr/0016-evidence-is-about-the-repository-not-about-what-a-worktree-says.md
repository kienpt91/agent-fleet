# 16. Evidence and merges are about the repository's commit, not about what a worktree says

Status: accepted. Amends ADR 14 (how a commit is chosen and how evidence is bound) and ADR 15 (what protects git).

## Context
A review of the merged tree found that verification and the merge gate read the head and the tree of a task through the
git directory of its worktree. A worker owns the worktree, and with it the `.git` file that says where that directory is.
A worker that rewrites the file to point at a repository of its own (one that borrows objects from the real one through
`objects/info/alternates` and carries a `refs/replace/<head>` entry) makes git report a head and a tree of its choosing
from inside the worktree. Evidence recorded for such a tree, and a merge decided on it, would not describe what the
`fleet/<run>/<task>` branch of the real repository contains.

## Decision
- **The commit and the tree of a task come from the real repository.** `GitVcs` resolves the task branch in the
  repository with `rev-parse` and reads its tree there (`real_branch_head`, `real_tree_of`), never through the worktree.
  The worktree is used for what only it can do: `status`, `add`, `commit`, and the temporary index of an export.
- **The worktree's pointer is validated before every use.** `check_worktree` requires: the path
  `<repo>/.fleet-worktrees/<run>/<task>`; a `.git` that is a plain file (not a link) whose only content is
  `gitdir: <repo>/.git/worktrees/<name>` with an absolute path that is a real directory directly below the repository's
  `worktrees` directory; an entry whose `gitdir` points back at the worktree, whose `commondir` is the repository's own
  `.git`, and whose `HEAD` is a branch (`fleet/<run>/<task>` or the integration branch). git is then run with
  `GIT_DIR`, `GIT_COMMON_DIR` and `GIT_WORK_TREE` set to what was validated, so a later change of the file does not change
  which directory a call uses.
- **Replace refs and foreign object sources are off.** Every git call sets `GIT_NO_REPLACE_OBJECTS=1` and
  `core.useReplaceRefs=false`. A repository that has `objects/info/alternates`, `objects/info/http-alternates` or
  `info/grafts` is refused when it is opened (a person adds alternates deliberately, and those are the files through which
  a forged object source would reach fleet).
- **Evidence and the merge gate are bound to (commit, tree).** The ledger counts a piece only when its commit equals the
  task's recorded commit and its tree equals the tree recorded with it; `fleet merge` asks the repository for the tree of
  the commit it is about to merge (`Vcs::commit_tree`) and compares that with the proven one, so evidence about a different
  tree for the same commit id does not count. An export checks that the tree written from the index equals the tree read
  from the repository.
- **Verification checks run outside the state directory.** The exports live in `<temp>/fleet-verify-<uid>/<hash of the
  state path>/job-*`, created private; nothing above a check's working directory leads to the control token or socket.

## Consequences
A worker that repoints its worktree gets a review that fails, no evidence, no completion and no merge; the tests that pin
this are in `git::tests` (a forged gitdir, every other way to point a worktree elsewhere, replace refs, alternates and
grafts), `fleet-app/tests/neo_verify.rs` (a repository whose tree for the proven commit differs) and `e2e_neo.rs` (the
real binary with a worker that forges its gitdir). What stays: a worker can still make git inside its worktree do things
its own repository state allows (it owns the files there); the defence is that nothing fleet decides on is read from there.
The cost: a few more git calls per review and a refusal for repositories that use alternates (for example a clone made with
`--reference`); a run in such a repository is refused with a message that names the file.
