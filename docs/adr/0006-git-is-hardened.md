# 6. Every git call runs with an empty environment and neutralized configuration

Status: accepted

## Context
Worktrees live inside a repository the workers can write. A planted hook, `core.fsmonitor`, filter, merge
driver or `core.sshCommand` would run with the daemon's permissions the next time the daemon runs git.

## Decision
git starts with an empty environment plus an allowlist, `GIT_CONFIG_GLOBAL/SYSTEM=/dev/null`, command-line
`-c` options that disable hooks, fsmonitor, pagers, editors, ssh, credential helpers and signing, and
per-key overrides (found by listing the config, which executes nothing) for filters, textconv, merge drivers
and per-command pagers. Commits and merges use `--no-verify`. Refs and paths come from validated ids.

## Consequences
Tests plant each vector and assert that no marker file appears. A driver planted between the config listing
and the git command would still run, with the allowlisted environment only.
