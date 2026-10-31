# Verification contracts for self-development

Every task of the self-development roadmap has a goal with a **verification contract**: checks that fleet
runs itself, on an export of the task's committed work, before the task can complete or merge (README, "NEO
N1"; [docs/neo.md](../../docs/neo.md)). The contracts here are **templates**: they contain `@NAME@`
placeholders for what depends on your machine and on the run, and `render-contract.sh` fills them in.
An unrendered template is refused by `fleet goals add` (`@TOOLS@/check-protected.sh` is not an absolute
program path), so a forgotten substitution cannot turn into a contract that checks nothing.

## The files

| File | For | Allowed to change (the `scope` check) |
| --- | --- | --- |
| `single-file.json` | the default: one named file | exactly `@FILE_PATH@` |
| `docs-only.json` | a task that edits several documents | `README.md` and `docs/**` |
| `crate-src.json` | a task that builds one crate a person scaffolded | `crates/@CRATE@/src/**` and `crates/@CRATE@/tests/**` |
| `human-only.json` | a task that touches a protected path | nothing: it can never pass (see below) |
| `common-checks.json` | not a contract: the shared fragment, the first nine checks of the three above | |

How a task picks one: **start from `single-file`**. Use `docs-only` only when the task really is several
documents, `crate-src` only for the one task per module that must add a test file next to its source, and
`human-only` for every task that touches a protected path ([PROTECTED_PATHS.md](../PROTECTED_PATHS.md)). Never
widen a scope to make a task pass; if the work needs another file, that is a different task. The choice for each
task of the roadmap is a row of [`goals.neo-roadmap.tsv`](../goals.neo-roadmap.tsv); `apply-goals.sh` renders and adds them.

## The reusable fragment (every contract except `human-only` starts with it)

All checks are mandatory. They run in order in a private export of the commit, with no network wish of their own.

| Check | What it does | Requirement it stands for |
| --- | --- | --- |
| `protected-diff` | `diff` with `deny` = every entry of `protected-paths.json`: fails if any commit of the task changed a protected path | the task must not touch a guardrail |
| `protected-script` | `check-protected.sh --repo <repo> <base>..fleet/<run>/<task>`: case-insensitive, refuses symlinks, submodules and unusual paths | the same, stricter, from a checker outside the repository |
| `lock-unchanged` | `file` `sha256` of `Cargo.lock` equals the hash at the run's base commit (and `Cargo.lock` is in the deny list too) | no dependency change without a person |
| `docs-claims` | `check-docs-claims.sh`: no changed line of README.md or docs/ states a measured number (test counts, coverage, check counts, crate counts, soak figures) | those numbers come from the human-run verification |
| `secret-scan` | `check-secret-tree.sh`: the repository's `scripts/secret-scan.sh --tree` on the export | no secret enters the tree |
| `fmt` | `cargo fmt --all -- --check` in the container | formatting |
| `clippy` | `cargo clippy --workspace --all-targets --locked --offline -- -D warnings` | lints (workspace denies `unwrap`, `expect`, indexing; `unsafe` is forbidden) |
| `architecture` | `cargo test --locked --offline -p fleet-cli --test architecture` | layering, no I/O in the domain, no new kind of dependency |
| `layers` | `sh scripts/test-layers.sh`: domain, application, adapters, soak and end-to-end layers | the tests, including the new ones |

then the task's own `scope` check: a `diff` with `allow` (the table above).

`protected-diff` is the authority: it runs inside fleet, on the real repository's objects. The two scripts add
what a glob cannot (case folding, links, odd names) and run from copies installed outside the repository.
`secret-scan` covers the tree of the commit; run `sh scripts/secret-scan.sh --history` yourself before you
merge, it needs the whole history.

## What `human-only.json` is

Two mandatory checks that cannot both pass: `no-change-at-all` (`diff` with `deny: ["**"]`) and `never-exists` (a
file that must exist at a path nothing may create). If nothing changed, the file is missing; if the file exists,
something changed. A test (`crates/fleet-infra/tests/dogfood.rs`) proves it with fleet's own judge for every combination.

## Placeholders

| Placeholder | Must be | Notes |
| --- | --- | --- |
| `@TOOLS@` | absolute directory outside the repository | where `install-tools.sh` put the check scripts, for example `~/.fleet-dogfood/bin` |
| `@CONTAINER_RUN@` | absolute path of your container wrapper, outside the repository | from `../container-run.example.sh`; it takes one argument, a shell command, and runs it in the development container |
| `@REPO@` | absolute path of the repository the run belongs to | the throwaway clone |
| `@BASE@` | the run's base commit | a commit hash, not a branch: it must not move during the run |
| `@RUN@`, `@TASK@` | the run id and the task id | the task's branch is `fleet/<run>/<task>` |
| `@LOCK_SHA256@` | derived by `render-contract.sh` from `Cargo.lock` at `@BASE@` | never typed |
| `@FILE_PATH@`, `@CRATE@` | a relative path without wildcards; a crate name | from the `params` column of the goal list |

Values may contain only letters, digits and `. _ / -` (a base may also contain `^ ~`): nothing that could add a
field to the JSON. No value is a secret and none may be.

```sh
# one contract by hand
sh dogfood/render-contract.sh dogfood/contracts/single-file.json ~/.fleet-dogfood/contracts/g-x.json \
  TOOLS=$HOME/.fleet-dogfood/bin CONTAINER_RUN=$HOME/.fleet-dogfood/bin/container-run REPO=$HOME/work/clone \
  BASE=$(git -C ~/work/clone rev-parse HEAD) RUN=neo-roadmap TASK=starter-error-tests \
  FILE_PATH=crates/fleet-domain/src/error.rs
fleet goals add --id g-x --title "..." --task neo-roadmap/starter-error-tests --contract ~/.fleet-dogfood/contracts/g-x.json
```

## The commands and the container

The `command` checks run **programs**, never a shell: absolute path, not inside the repository, its worktree, the
export or the scratch area. So the cargo checks go through your **container wrapper**, one program that takes a
command string: `@CONTAINER_RUN@ 'cargo fmt --all -- --check'`. The wrapper is yours, outside the repository
(`../container-run.example.sh` is the template): it starts the development container of the README
(`fleet-dev`, volumes `fleet-cargo` and `fleet-target`, filled once by `cargo fetch --locked`), with the current
directory (the export) mounted, **no network** and no environment of yours; no secret is passed in. Checks run with
a PATH of absolute directories and a home of their own, so the wrapper uses absolute paths and never relies on your
shell setup.

To change the fragment or the protected list: edit `protected-paths.json` and/or `gen-contracts.sh`, run
`sh dogfood/gen-contracts.sh`, commit; a human does this, these files are protected.

## What these contracts do not prove

A weak check passes a weak result: the tests the task writes are judged only by compiling and passing. Checks run
as you, on the committed build scripts and tests, with the container as the only isolation; the container has no
network but is not a security boundary against a hostile test either (README, Limitations). Evidence says what
fleet saw on one commit; a person still reads the diff before approving the merge.
