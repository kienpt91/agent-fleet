# 7. Configuration is read once, from the daemon's side

Status: accepted

## Context
Configuration names commands to run. Reading it from the repository a worker can modify, on every dispatch,
would let a worker change what the daemon executes.

## Decision
`fleet.toml` is read once at daemon start from `--config` or the state directory. Plans (`fleet plan`)
may name workers but never define commands. The state directory may not lie inside a repository used by a
run.

## Consequences
Changing workers or gates needs a daemon restart. A plan file cannot smuggle in a command.
