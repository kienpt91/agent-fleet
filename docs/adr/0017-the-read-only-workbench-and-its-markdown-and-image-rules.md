# 17. The read-only workbench: the repository at recorded commits, a Markdown subset and four image types

Status: accepted. Builds on ADR 10 (web adapter), ADR 15 (one origin rule) and ADR 16 (the repository, not the worktree).

## Context
Phase 5.1 adds a review workbench to the dashboard: explorer, file viewer, diff (unified and side by side), Markdown
preview and image viewer in a dock with tabs and splits. Everything shown (names, contents, diffs, Markdown) comes from
repositories that workers write, so it is hostile. The workbench must not open a write path, a second listener, a
looser CSP, or a way to read anything outside the recorded commits.

## Decision
- **Addressing.** A request names a task and a commit selector: `head`, `base`, or a full commit id that the daemon
  recorded for that task (its base, head or merge commit). Refs, revision syntax, short ids and paths on disk are not
  accepted. The orchestrator resolves the task to a `RepoTarget` from its own state (`Command::Repo` answers with
  `Reply::Repo`); the reading is done on the caller's thread by `RepoBrowser` through the `RepoReader` port, so a slow git
  never stalls the engine. The five commands (`repo.tree|file|blob|markdown|diff`) are in `WEB_COMMANDS` and read only.
- **Reading.** `GitVcs` implements `RepoReader` with the hardened runner (ADR 6, 16): the repository's own directory
  (never a worktree, a linked worktree is refused), fixed argv, `GIT_LITERAL_PATHSPECS`, `ls-tree -z -l`, a size check with
  `cat-file -s` before `cat-file blob <oid>`, no filters, textconv, external diff, pager or replace objects, and explicit
  `--no-ext-diff --no-textconv --submodule=short --ignore-submodules=none -M -l200`. Directory listings, the raw and
  numstat lists and the patch have their own output caps; more output stops git and is reported as truncation.
- **Paths** are `RepoPath` values: relative, no `.`/`..`/empty/`.git` component, no backslash, NUL, control, invisible or
  bidi character, at most 1024 bytes, 255 per component and 32 deep. Nothing is normalized. Names that fail the rules are
  listed with an escaped display name and cannot be requested.
- **Content.** Text is decoded leniently (replacement, a notice), CRLF is reported, lines are clipped with a marker, files
  above 1 MiB (text) are placeholders, binary is decided by a NUL in the first 8000 bytes. Links are shown as text and never
  followed; submodules are shown as pointers. Diffs are bounded in files (300), hunks, lines per file (4000), total lines
  (20000) and bytes (3 MiB); totals come from `--numstat`. Results are cached by (commit, path, range) in a cache bounded
  in entries (64), bytes (16 MiB) and entry size (2 MiB).
- **Markdown** is parsed on the server by a hand-written subset (`fleet_domain::repo::markdown`) into a closed tree. Raw
  HTML is text, only `http`/`https` links and repository-relative links survive (resolved against the document's directory
  and never leaving the repository), images load only from repository paths, work is bounded and linear. The page turns the
  tree into DOM nodes from a fixed element and attribute allowlist; external links open only after an explicit click and
  an in-page confirmation, with `noopener noreferrer`.
- **Images** are served only when the first bytes are a valid PNG, JPEG, GIF or WebP header (SVG and everything else are
  text or binary placeholders), at most 4 MiB, with `X-Content-Type-Options: nosniff`, `Content-Security-Policy:
  default-src 'none'; sandbox`, `Content-Disposition: inline` and no filename. The page fetches them with its session
  header and shows them through a `data:` URL (already allowed by `img-src`), so the page CSP is unchanged.
- **Changed words.** Edited line pairs (the i-th removed and i-th added line of a run) are compared by token with Myers' O(ND)
  algorithm in the domain (`worddiff`): at most 1024 tokens and 8 KiB per line, at most 64 differing tokens, 64 pairs per run and a
  work budget per diff; beyond that nothing is marked. Ranges are UTF-16 offsets and the page renders them with text nodes only.
- **Evidence.** The page is exercised in headless Chromium against a real daemon (`scripts/workbench-demo.sh`), including hostile
  names and contents; memory under concurrent large reads reaches a plateau and does not grow (e2e test). The caps were lowered after a
  first measurement (20000 diff lines, three concurrent reads).
- **Limits.** At most three repository reads run at once (503 with `Retry-After` otherwise); the request target limit
  became 4096 bytes so that a maximal path fits when percent-encoded. No new listener, no new dependency.
- **UI.** Three scripts (`app.js`, `workbench.js`, `panes.js`) and two stylesheets, all scanned by tests for forbidden
  constructs; the layout is saved in `sessionStorage` under `fleet.layout` and validated strictly on load.

## Alternatives considered
- Rendering Markdown in the page: simpler, but the property tests (allowlists, URL schemes, linear work) are much stronger
  on a typed tree in the domain.
- `blob:` URLs for images: would need `blob:` in the CSP.
- Reading through the worktree: rejected by ADR 16.
- A third-party diff or Markdown library: rejected (no new dependencies).

## Consequences
`.gitattributes` in the user's own checkout can still decide that a file counts as binary in a diff (no code runs).
Homoglyph names are shown as they are (only control, invisible and bidi characters are escaped). The workbench shows the
committed state; uncommitted worktree changes are invisible by design.
