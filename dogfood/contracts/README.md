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
| `flaky-test.json` | a flaky unit test, **a reviewed exception** (see below) | exactly `@FILE_PATH@`, and only below its test module |
| `human-only.json` | a task that touches a protected path | nothing: it can never pass (see below) |
| `common-checks.json` | not a contract: the shared fragment, the first ten checks of the three templates above | |

How a task picks one: **start from `single-file`**. Use `docs-only` only when the task really is several
documents, `crate-src` only for the one task per module that must add a test file next to its source, and
`flaky-test` only for a flaky test (see "Reviewed exceptions"), and `human-only` for every other task that touches a protected path ([PROTECTED_PATHS.md](../PROTECTED_PATHS.md)). Never
widen a scope to make a task pass; if the work needs another file, that is a different task. The choice for each
task of the roadmap is a row of [`goals.neo-roadmap.tsv`](../goals.neo-roadmap.tsv); `apply-goals.sh` renders and adds them.

## The reusable fragment (every contract except `human-only` and, in part, `flaky-test` starts with it)

All checks are mandatory. They run in order in a private export of the commit, with no network wish of their own.

| Check | What it does | Requirement it stands for |
| --- | --- | --- |
| `protected-diff` | `diff` with `deny` = every entry of `protected-paths.json`: fails if any commit of the task changed a protected path | the task must not touch a guardrail |
| `protected-script` | `check-protected.sh --repo <repo> <base>..fleet/<run>/<task>`: case-insensitive, refuses symlinks, submodules and unusual paths | the same, stricter, from a checker outside the repository |
| `lock-unchanged` | `file` `sha256` of `Cargo.lock` equals the hash at the run's base commit (and `Cargo.lock` is in the deny list too) | no dependency change without a person |
| `docs-claims` | `check-docs-claims.sh`: no changed line of README.md or docs/ states a measured number (test counts, coverage, check counts, crate counts, soak figures) | those numbers come from the human-run verification |
| `conflict-markers` | `check-conflict-markers.sh --tree`: no file of the export has a line starting with `<<<<<<<` or `>>>>>>>` | a task that commits an unresolved merge |
| `secret-scan` | `check-secret-tree.sh`: the repository's `scripts/secret-scan.sh --tree` on the export | no secret enters the tree |
| `fmt` | `cargo fmt --all -- --check` in the container | formatting |
| `clippy` | `cargo clippy --workspace --all-targets --locked --offline -- -D warnings` | lints (workspace denies `unwrap`, `expect`, indexing; `unsafe` is forbidden) |
| `architecture` | `cargo test --locked --offline -p fleet-cli --test architecture` | layering, no I/O in the domain, no new kind of dependency |
| `layers` | `sh scripts/test-layers.sh`: the domain, application, adapters (with the consistency tests of `dogfood/`), soak and end-to-end layers; the `remote` layer is skipped in the development image (see "The `remote` layer is not part of the contract") | the tests, including the new ones |

then the task's own `scope` check: a `diff` with `allow` (the table above).

`protected-diff` is the authority: it runs inside fleet, on the real repository's objects. The two scripts add
what a glob cannot (case folding, links, odd names) and run from copies installed outside the repository.
`secret-scan` covers the tree of the commit; run `sh scripts/secret-scan.sh --history` yourself before you
merge, it needs the whole history.

## The `remote` layer is not part of the contract

`scripts/test-layers.sh` has a sixth layer, `remote`: the tests of the SSH remote worktrees against a **real sshd**
(`crates/fleet-infra/tests/ssh_remote.rs`, and the remote tests of the end-to-end layer, `crates/fleet-cli/tests/e2e_remote.rs`).
It runs only when the image has `/usr/sbin/sshd` (`docker/Dockerfile.ssh`, the `fleet-ssh` image, started through
`scripts/ssh-env.sh`). The development image `fleet-dev` that the container wrapper runs has none, so in every contract the
`layers` check prints `skipped: no OpenSSH server in this image` for that layer and still passes or fails on the other five. That is
deliberate and stays so:

- the tests start an sshd of their own and need a passwd entry for the container's uid, which `scripts/ssh-env.sh` appends to the image's `/etc/passwd`; the wrapper's container is a different thing (its tree is read-only, no capabilities, no network), and a check should not depend on such a server;
- it is no judge of a task's change: `crates/fleet-infra/src/ssh/**`, the remote host code, the host key pinning, `docker/Dockerfile.ssh`,
  `scripts/ssh-env.sh` and the sshd tests are protected paths that no task may change ([PROTECTED_PATHS.md](../PROTECTED_PATHS.md)), so no
  task of the roadmap can alter what the remote layer proves.

Do not point the wrapper at the `fleet-ssh` image for these contracts: the layer would then start servers inside a check. The remote layer
is run by a person, and by CI (the `ssh-tests` job): after a person changes one of those protected paths, run it yourself as the README
shows (`IMAGE=fleet-ssh`, section Development). Through `scripts/ssh-env.sh` (`FLEET_SSH_TESTS=1`) a missing server is a failure, not a skip.

## Reviewed exceptions: `flaky-test.json`

A flaky test fails once in a while and passes when run alone, so a task that fixes one needs a check that runs it
many times under load, and the tests that are worth fixing often live in a protected file (`crates/fleet-infra/src/server.rs`,
`jobs.rs`). The template is the one case where the protected list is **not** forbidden wholesale, on purpose and for one file:

| What changes against the fragment | Why |
| --- | --- |
| no `protected-diff` (the glob deny list cannot say "all of the list but this file") | the file would be denied by its directory |
| `protected-script` gets `--except <file>`: that exact path, a Rust source or test file of a crate, never a build script, a manifest or the architecture tests; printed as a REVIEWED EXCEPTION; symlinks, submodules and odd paths are still refused | the one allowed protected path |
| `scope`: allow only `<file>` | nothing else may change |
| `kept`: the file still contains `fn <test name>` | the test is not deleted |
| `test-only`: `check-test-only.sh`: everything above the file's first test module (`#[cfg(test)]` + `mod ... {`) is byte-identical to the base | the production code of the file cannot have changed behind the diff |
| `stress`: build the test once, start 8 busy loops, run exactly that test 25 times (`cargo test -p <package> --lib <filter>`), one failure fails the check | the proof that the flakiness is gone (the pattern of the trial runs, `../trials/`) |

Limits, stated plainly: `test-only` proves nothing about what is *after* the first test module (tests are by convention the last
items; a person reading the diff is the guard), a file whose first `#[cfg(test)]` is on an import is refused as "cannot judge", and
the principle of [PROTECTED_PATHS.md](../PROTECTED_PATHS.md) is met by option 2: **a human approves the exact diff** (the merge
approval) **plus** the independent security review you owe to any change in `crates/fleet-infra`. The placeholders `PACKAGE`, `TEST_NAME`
and `TEST_FILTER` (`server::tests::a_reply...`) are checked by `render-contract.sh`; there is no goal for this template in the roadmap
because the two flaky tests of the trials are fixed on `main`: render it by hand for the next one (RUNBOOK.md, section 9).

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
| `@ALSO@` | optional: `, "a", "b"` built from `ALSO=a,b` (paths or globs) | the scopes of the tasks this one depends on, added to the `scope` allow list of `single-file`, `docs-only` and `crate-src`; `apply-goals.sh --plan` works it out (RUNBOOK.md, section 9) |
| `@PACKAGE@`, `@TEST_NAME@`, `@TEST_FILTER@` | a cargo package; a test function name; the filter that selects it (`module::tests::name`) | `flaky-test.json` only; its `@FILE_PATH@` must be `crates/<crate>/src/**.rs` or `crates/<crate>/tests/**.rs` |

Values may contain only letters, digits and `. _ / -` (a base may also contain `^ ~`, a test filter `:`): nothing that could add a
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
(`../container-run.example.sh` is the template, modelled on the wrapper of the trial runs). You set two things in it: the
absolute path of your docker client and the image (`fleet-dev`, built from `docker/Dockerfile.dev`); the cargo registry and target
directory are two docker volumes it names (`fleet-dogfood-cargo`, `fleet-dogfood-target`), the registry filled once by
`DOGFOOD_MODE=online container-run 'cargo fetch --locked'`. Every check then runs in a throwaway container with **no network,
no capabilities, your uid, the export mounted read-only at `/work`, the registry read-only and nothing of yours mounted**; only the
target volume is writable, so every export builds on a warm cache. Checks start with an environment of `PATH HOME USER LANG TERM`
only (`fleet.dogfood.example.toml`), so `DOGFOOD_MODE` can never reach one. Use absolute paths in the wrapper and never rely on your
shell setup: checks run with a `PATH` of absolute directories and a home of their own.

To change the fragment or the protected list: edit `protected-paths.json` and/or `gen-contracts.sh`, run
`sh dogfood/gen-contracts.sh`, commit; a human does this, these files are protected.

## What these contracts do not prove

A weak check passes a weak result: the tests the task writes are judged only by compiling and passing. Checks run
as you, on the committed build scripts and tests, with the container as the only isolation; the container has no
network but is not a security boundary against a hostile test either (README, Limitations). Evidence says what
fleet saw on one commit; a person still reads the diff before approving the merge.
