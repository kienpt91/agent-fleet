# 20. `gh` is the only transport to GitHub and fleet never holds a token

Status: accepted

## Context
Phase 3a adds issues and pull requests. The roadmap sketched tokens "in the OS keychain or a 0600 file". Any token fleet
holds is a token the daemon's own children, logs, events, briefs, status documents and web responses could leak, and
the daemon runs next to workers that execute untrusted code as the same user.

## Decision
- The only transport is the system `gh` program (`gh api --hostname <host> --include --method <GET|POST> ... <endpoint>`),
  and signing in is `gh`'s own business (its credential store). fleet has no token setting, no token file and no token
  variable; `[github]` rejects unknown keys, and `env_pass` refuses secret-looking names and `GH_*`, `GITHUB_*`, `GIT_*`,
  `SSH_*`, `FLEET_*`.
- `gh` is resolved once at start-up to an absolute, canonical path (relative `PATH` entries are skipped, never resolved
  against a working directory) and refused while it lies inside any run's repository or worktree.
- Fixed argv, no shell. The endpoint is built from validated owner, name, numbers and the closed vocabulary `open|closed|all`.
  The working directory is `/`, so `gh` finds no repository configuration; the repository is always in the endpoint.
- Minimal environment: `HOME`, `PATH` (absolute entries), `GH_PROMPT_DISABLED=1`, `GH_NO_UPDATE_NOTIFIER=1`, `GH_HOST`
  for another host, and the variables named in `env_pass`. `GH_TOKEN`, `GITHUB_TOKEN`, `GH_ENTERPRISE_TOKEN`,
  `GITHUB_ENTERPRISE_TOKEN`, `GH_HOST` and `GH_REPO` of the daemon's environment never reach a child.
- The child runs through the one job tracker (ADR 15): it and its descendants are refused on the control socket and the
  dashboard. Hard timeout, output capped at 2 MiB, answer parsed as JSON under a size cap and read through strict schemas
  (required typed fields, caps, terminal control and secret shapes removed), a rate gate that backs off with jitter and
  never starts a process while blocked.
- What `gh` prints to stderr, and what `git push` prints, is cleaned and passed through the daemon's redactor before it is
  part of an error, a reply or a `pull_request_failed` event.
- The values of secret-looking variables of the daemon's environment are already redaction literals (`[redact] env_values`),
  so a token that did leak into an answer is redacted before it is stored.

## Consequences
"fleet never holds a token" is a statement about fleet, not a credential boundary. `gh`'s credentials are readable by every
process of the operator's user, workers included (workers run as the same user, see ADR 4): a worker can call `gh` or
`git push` itself without fleet's approval. The approval gate controls what *fleet* publishes. To make it mean more, run
workers as another user or in a container without access to the `gh` configuration, and give `gh` a fine-grained token for the
one repository.

fleet cannot sign in for you and cannot work where `gh` cannot. On Linux desktops where `gh` keeps its token in a session
keyring, the minimal environment can hide the keyring (name `DBUS_SESSION_BUS_ADDRESS` in `env_pass`). `gh` is trusted code
of the operator, not a sandbox boundary: a replaced `gh` binary outside the repositories is outside this defence.
Calls are made through `gh api` against documented REST endpoints; what a real `gh` prints today was checked only against its
help text (see README, "What was not verified").
