# Human-only tasks

A task is **human-only** when doing it means changing a [protected path](PROTECTED_PATHS.md). The plan
format (`plan.neo-roadmap.toml`) has no such flag, so the roadmap marks them in four places that a test keeps
in step (`crates/fleet-infra/tests/dogfood.rs`):

- the title and the spec start with `HUMAN-ONLY`;
- the task uses the worker `human-only` (in `fleet.dogfood.example.toml`): a command that prints why and exits
  1, so the task fails at once and costs nothing, and its dependents wait behind it;
- its goal in `goals.neo-roadmap.tsv` has the contract `contracts/human-only.json`, which **fails by
  construction**: one check forbids every change (`diff` with `deny: ["**"]`), the other asks for a file that
  nothing may create. No change satisfies both, so even an agent that were somehow started on the task could
  never complete it;
- it is listed here.

A human-only task is a **barrier**. fleet cannot mark a task completed by hand, so the run stays blocked
behind it. The procedure at a barrier:

1. Let the run finish everything that does not depend on the barrier (`fleet status`).
2. Review and merge what the run produced (RUNBOOK.md, "Review and merge"). Land it on the branch the next run
   starts from.
3. Do the human-only work **yourself, in your own clone**, as commits you author, and have it reviewed as the task
   says. Land it on the same branch.
4. Mark the goal of the barrier done: `fleet goals status <goal> abandoned --reason "done by a person in <commit>"`
   (a verified goal cannot be set by hand, and a human-only goal can never be verified; an abandoned child no longer
   holds its milestone back).
5. Start the next stage from a plan without the finished tasks:
   `dogfood/plan-without.sh dogfood/plan.neo-roadmap.toml <done task ids> > stage.toml`, then `fleet plan stage.toml`
   (give it a new run id in the `[run]` table) and `apply-goals.sh --skip-tasks <done task ids>` for its goals.

The tasks, in the order of the roadmap (the goal of each is `g-<task>`):

| Task | What the person does | Needs a reviewer other than the author |
| --- | --- | --- |
| `maint-deps-refresh` | Run `deps-refresh.sh` (it updates `Cargo.lock` in the container, checks every locked version against a cutoff of now minus 7 days and puts the lock back if any is too new). Read the `Cargo.lock` diff: new crates, new `build.rs` and procedural-macro crates against SECURITY.md's lists, sources other than crates.io. Run the layered tests. Commit. | yes, for any new crate |
| `maint-numbers` | From a fresh run (fresh docker volumes) of `scripts/test-layers.sh`, `scripts/coverage.sh`, the soak test and `check-crates-age`, refresh the test counts, the coverage table, the check counts and the crate count in README.md, SECURITY.md and docs/. A task never edits these numbers: `check-docs-claims.sh` fails a task that does. | no |
| `n3-scaffold` | Create `crates/neo-knowledge` (manifest with std and `fleet-domain` only, `[lints] workspace = true`, `lib.rs` with `#![forbid(unsafe_code)]` and one empty module file per name in the spec), add it to the workspace members and the lock file, to `CRATES`, `DOMAIN`, `INNER` and `PURE` and the dependency table in `crates/fleet-cli/tests/architecture.rs`, and to the layer scripts. | yes (it edits the architecture tests) |
| `n3-integrate` | Events in `fleet-domain`, the knowledge use cases and port in `fleet-app`, the JSONL store (0600, bounded, redacted before writing) and the safe read of `.fleet/facts.json` in `fleet-infra`, the contradiction decision gate, the `fleet facts` commands, port contract suites, end-to-end tests. | yes |
| `n3-review` | The independent review of the whole N3 diff against the milestone criteria; the numbers refreshed as in `maint-numbers`; README, SECURITY.md and docs/neo.md updated to say what exists. | **is** the review |
| `n4-scaffold` | As `n3-scaffold` for `crates/neo-governor`. | yes |
| `n4-integrate` | Events, the hard-stop enforcement in the orchestrator's supervision (kill or pause the worker), the adapter for the usage the agent reports (if the agent CLI reports no usage, estimates only, and say so), `fleet budget`, the soak test of the hard stop. | yes |
| `n4-review` | As `n3-review` for N4. | **is** the review |
| `n5-scaffold` | As `n3-scaffold` for `crates/neo-broker`. | yes |
| `n5-adapter` | The secret-touching adapter and the local endpoint with per-lease capabilities, credential references (keychain, daemon environment at start, a 0600 file outside any repository), events, use cases, CLI, and the tests that no credential appears in any worker environment, argument, file in its view, log, trace or event. | yes, line by line |
| `n5-review` | The independent review of every line that touches a credential; the secret scanner on tree and history; the provider-terms document read against what the code can do. | **is** the review |

Everything in this list is also something a person may decide **not** to do yet: the plan simply stays
blocked there. Nothing proceeds around a barrier.
