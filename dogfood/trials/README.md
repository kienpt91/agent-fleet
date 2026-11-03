# Trial runs: the models for the templates

Before this scaffold existed, NEO was run on its own repository by hand, with the real agent (`omp`, the
arguments of `../fleet.dogfood.example.toml`), on the owner's macOS, with a `fleet` binary built from a clean export of
`main`. Contracts were written per task. The author of the scaffold was told the result and was given the
files below; he did not run them. What was reported: **two real tasks completed with every contract check passing**, and
that in `--approval-mode write` the agent **could not run shell commands** (the approval it needs is not available with
nobody there), so it edited files blind and the contract, run by fleet in a container, did all the proving.

These files are kept as worked examples of what a task spec and a contract look like when they have worked. They are
not part of the roadmap: the two flaky tests they fix were already fixed on `main` after the trials, so loading the
plans there changes nothing. At the base this branch was cut from (`a5cb757`) both tests still exist unfixed.

| File | The task | What the contract checked |
| --- | --- | --- |
| `plan.flaky-server-test.toml`, `contract.flaky-server-test.json` | remove a timing race from `a_reply_that_is_being_produced_when_the_server_stops_is_still_delivered` in `fleet-infra/src/server.rs`: make the handler signal that it started and have the test wait for the signal, do not weaken the assertion | the diff touches only that file; the test is still there (`kept`); `cargo fmt --check`; clippy `-D warnings` for the crate; **a stress check**; the crate's unit tests |
| `plan.flaky-jobs-test.toml`, `contract.flaky-jobs-test.json` | make `the_children_of_running_jobs_are_listed_until_their_group_is_gone` in `fleet-infra/src/jobs.rs` reliable: find which assertion fails and why (a zombie that still counts as a group, tight time limits), fix the test, do not weaken the rule | the same, for that file and test |
| (not kept: a feature task) | make the workbench image limits configurable in `fleet.toml` (`[web] image_max_side`, `image_max_megapixels`): parse and validate, carry through the application config, apply in the daemon, document in the example, test | a diff allow list of eight files, a `file` check that the example holds the new key, fmt, clippy for three crates, the tests of those crates |

The contracts are the hand-written originals with the path of the owner's wrapper replaced by
`/ABSOLUTE/PATH/TO/container-run`; their programs are `/bin/sh -c '<wrapper> ...'`, which the contract schema accepts
and the templates now replace by calling the wrapper directly.

## What these trials teach, and where it went

- **The stress check.** A flaky test passes alone on an idle machine, so "the tests passed" proves little. The check
  builds the test binary once, starts eight busy loops (`timeout 280 sh -c "while :; do :; done"`), and runs the
  one test 25 times in a row; one failure fails the check. It is the template `../contracts/flaky-test.json`.
- **A spec that works** says what the product rule is, names the file that may change, lists hypotheses without
  asserting them ("verify it by reading the code before you change anything"), forbids the lazy fixes (no sleep, no
  `#[ignore]`, do not weaken the assertion, do not delete the test) and says "Commit nothing yourself: fleet commits
  your work". The roadmap's specs follow it.
- **The two flaky tests live in protected files.** `crates/fleet-infra/**` is on the protected list
  ([PROTECTED_PATHS.md](../PROTECTED_PATHS.md)), so with the list in force these tasks are not ordinary tasks. They are
  the one case the scaffold allows as a **reviewed exception**: the template `flaky-test.json` swaps the glob deny
  check for `check-protected.sh --except <that one file>` and adds `check-test-only.sh`, which proves that everything
  above the file's test module is byte-identical, so the production code of the file cannot have changed; the person
  who approves the merge reads the diff of the test module. The feature task above would be a human-only task.
- **fleet's commits are authored `fleet <fleet@localhost>`**, and a cherry-pick of them can leave conflict
  markers: see RUNBOOK.md, section 5, and `../check-conflict-markers.sh`.
- **The container wrapper** mounts the export read-only, offline, with fixed cache volumes so a verification export does not
  start from a cold cache: `../container-run.example.sh`.
