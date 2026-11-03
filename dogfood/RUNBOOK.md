# Runbook: letting NEO develop its own repository under human control

**Status of this document.** The scaffold in `dogfood/` (the protected-path list, the contract templates, the checkers,
the plan, the daemon template, this procedure) was built and tested **without running a real agent**: 
shell tests of every script, consistency tests of the data (`crates/fleet-infra/tests/dogfood.rs`), and a dry run of a stage
of the plan with the scripted agent `examples/agents/fake-agent.sh`, whose real output is the
[appendix](#appendix-the-dry-run-as-it-ran). **The scaffold itself has never been run with a real agent.**
What a real agent did is known only from trial runs that the owner made earlier on his macOS, with hand-written contracts and a
`fleet` binary built from a clean export of `main`, and reported to the author of this scaffold: the agent was `omp` run as in
`fleet.dogfood.example.toml`, and **two real tasks completed with every check of their contract passing**. The files of those
trials are kept in [trials/](trials/README.md) as models. Nothing here claims more than that; the point of the runbook is that
you find out the rest in small, reversible steps.

Read first: [PROTECTED_PATHS.md](PROTECTED_PATHS.md) (what no task may touch and why), [human-only.md](human-only.md) (the tasks
only you do), [contracts/README.md](contracts/README.md) (what fleet checks on every task), and SECURITY.md.

The files of this directory:

| File | What it is |
| --- | --- |
| `protected-paths.json`, `PROTECTED_PATHS.md` | the list of paths no task may change, machine-readable and explained |
| `contracts/*.json`, `contracts/README.md` | contract templates (`single-file`, `docs-only`, `crate-src`, `flaky-test`, `human-only`) and the shared fragment |
| `gen-contracts.sh` | regenerates the templates from the list (a person runs it when the list changes) |
| `render-contract.sh` | fills a template's placeholders (accepts only values of a safe shape) |
| `install-tools.sh` | copies the check scripts from a reviewed commit to a directory outside the repository |
| `check-protected.sh`, `check-docs-claims.sh`, `check-secret-tree.sh`, `check-conflict-markers.sh`, `check-test-only.sh` | the checks of a contract that are scripts; `check-protected.sh` and `check-conflict-markers.sh` are also your review tools |
| `container-run.example.sh` | template of the container wrapper the cargo checks go through (yours, outside the repository): offline, read-only tree, fixed cache volumes |
| `container-run.inside.sh` | the same job for when everything already runs inside the development container (the dry run) |
| `plan.neo-roadmap.toml`, `goals.neo-roadmap.tsv` | the roadmap: tasks (the DAG) and the goal and contract of each |
| `apply-goals.sh`, `plan-without.sh` | add the goals to the ledger; cut a plan to the tasks that are still to do and give it its repository |
| `fleet.dogfood.example.toml` | the daemon configuration template |
| `deps-refresh.sh` | the script that refreshes dependencies under the 7 day policy (a person runs it) |
| `dryrun.sh` | the dry run with the fake agent (`examples/agents/fake-agent.sh`) |
| `trials/` | the plans and contracts of the real trial runs, and what they taught |
| `tests/` | shell tests of the scripts; `crates/fleet-infra/tests/dogfood.rs` keeps the data consistent |

## 0. The shape of the arrangement

```text
 you ──review/approve/merge──> fleet daemon (config outside the repo, one worker at a time)
                                  │ one worktree + branch per task, in a THROWAWAY CLONE
                                  ▼
                           the agent (omp, write mode: edits ONE named file, cannot run anything)
                                  │ fleet commits its work
                                  ▼
        fleet runs the task's contract on an EXPORT of the commit, with scripts installed OUTSIDE the repo:
        protected paths untouched? Cargo.lock unchanged? no measured number edited? no conflict markers? no secret?
        fmt, clippy -D warnings, architecture tests, layered tests (in a container: offline, tree read-only)
                                  │ every mandatory check passed, recorded as evidence
                                  ▼
                   task completes ──> `fleet merge` needs YOUR approval of that exact commit
                                  ▼
              fleet/<run>/integration  ──> you read the diff again and take the commits into your branch yourself, as yourself
```

Five rules: (1) one task, one file; (2) protected paths are human-only; (3) the checkers live outside the repository;
(4) `merge` stays an approval; (5) everything runs on a clone you can delete.

## 1. Prerequisites

- git 2.39 or newer, Docker, the `fleet` binary (`cargo build --release --locked` in a container, or a host build from a
  clean export of the commit you reviewed) and the agent CLI (`/Users/kienpt/.local/bin/omp`; version 18.6.1 was inspected and
  is the one of the trials).
- **A provider key with a hard spending limit.** Before the first run, in the provider's console, create a key for this work
  only and set a hard monthly limit you can afford to lose. NEO's token governor (N4) does not exist yet: nothing in fleet
  limits spend, and the 25 minute `--max-time` of one attempt is a time limit, not a money limit. Know where the provider's
  usage page is and look at it (section 7).
- A throwaway clone of the repository (section 2) and a directory for what must live outside it (`~/.fleet-dogfood`).
- The development container of the README (`docker/Dockerfile.dev`, image `fleet-dev`) and two docker volumes for it.
- Disk and time: the first run of the contract's `layers` check builds the whole workspace in the container (minutes, and a
  few GB in the volumes); later runs reuse the target volume. Two verification jobs may run at once, one worker at a time.
- macOS note: the trial runs were made on macOS and worked there (as reported). The README still says that fleet's macOS code
  paths were type-checked and not run by its author; that statement predates the trials and was not re-verified here.
  The scripts of this directory are POSIX `sh` and use only tools found on macOS and Linux; the shell tests were run on Linux only.

## 2. Set up (once)

```sh
# 1. a throwaway clone: the worker runs as you and can reach any checkout; give it one you can delete
git clone --no-hardlinks <your repository> ~/work/fleet-dogfood
cd ~/work/fleet-dogfood && git switch <the reviewed branch, for example neo/dogfood>
# read .git/config and .git/hooks of this clone before you run git in it after a run (section 6)

# 2. what must live outside the repository
install -d -m 700 ~/.fleet-dogfood
install -d -m 755 ~/.fleet-dogfood/bin
sh dogfood/install-tools.sh --repo ~/work/fleet-dogfood --ref <the commit you reviewed> --dest ~/.fleet-dogfood/bin
#    prints the SHA-256 of each installed file: write them down; run it again after every merged change to dogfood/

# 3. the container wrapper (yours; checks run programs, never a shell, so cargo goes through one program)
install -m 755 dogfood/container-run.example.sh ~/.fleet-dogfood/bin/container-run
#    edit THE TWO THINGS the file says: `docker=` (the absolute path of your docker client, `command -v docker`)
#    and `image=` (only if you use another image). It must not be writable by group or others.
#    It starts the container with no network, no capabilities, your uid, the checked tree read-only at /work,
#    the cargo registry read-only and nothing of yours mounted.

# 4. the image and the two volumes (README, Development), then the cargo cache, filled online ONCE by you
docker build -f docker/Dockerfile.dev -t fleet-dev docker/
docker volume create fleet-dogfood-cargo; docker volume create fleet-dogfood-target
docker run --rm -u 0 -v fleet-dogfood-cargo:/cargo -v fleet-dogfood-target:/target fleet-dev chown "$(id -u):$(id -g)" /cargo /target
cd ~/work/fleet-dogfood && DOGFOOD_MODE=online ~/.fleet-dogfood/bin/container-run 'cargo fetch --locked'
#    online mode: network on, cargo registry and the directory writable. Only you run it; a check starts with PATH, HOME,
#    USER, LANG and TERM only, so DOGFOOD_MODE never reaches one. Repeat it after every change of Cargo.lock.

# 5. the base must be green BEFORE any task: every check of every contract on the untouched commit
cd ~/work/fleet-dogfood
~/.fleet-dogfood/bin/container-run 'cargo fmt --all -- --check'
~/.fleet-dogfood/bin/container-run 'cargo clippy --workspace --all-targets --locked --offline -- -D warnings'
~/.fleet-dogfood/bin/container-run 'cargo test --locked --offline -p fleet-cli --test architecture'
~/.fleet-dogfood/bin/container-run 'sh scripts/test-layers.sh'
~/.fleet-dogfood/bin/container-run 'sh dogfood/tests/run.sh'                      # the scripts' own tests
sh ~/.fleet-dogfood/bin/check-conflict-markers.sh --git .
sh scripts/secret-scan.sh --tree && sh scripts/secret-scan.sh --history
#    if one of these fails on the base, fix it as a person first: otherwise every task fails for a reason that is not its own

# 6. the daemon configuration (outside the repository, read once at daemon start)
cp dogfood/fleet.dogfood.example.toml ~/.fleet-dogfood/fleet.toml
chmod 600 ~/.fleet-dogfood/fleet.toml
#    read it. It names omp's absolute path and the verified arguments, one worker at a time, max_retries 1, merge and
#    external-command as approvals, an env allowlist without any secret variable, the worker `human-only`, and one gate.
#    Replace the paths under /Users/kienpt/.fleet-dogfood if yours differ.
export XDG_STATE_HOME=$HOME/.fleet-dogfood/state    # in every terminal you use for this: the daemon and the commands agree,
                                                    # and this daemon never mixes with one of yours
fleet doctor --config ~/.fleet-dogfood/fleet.toml
```

`install-tools.sh` reads the scripts from git objects of the commit you name, refuses a destination inside the repository or
writable by others, and prints hashes. fleet's contracts run the scripts from there, not from the repository a worker can edit.

### The agent: what was run for real

The worker is `omp` run non-interactively, with the arguments of the template (the ones the trial runs used):

```text
omp -p --no-session --no-extensions --no-skills --no-rules --max-time 25m --cwd {worktree} --approval-mode write @{brief_file}
```

with the daemon's environment allowlist `PATH HOME USER LANG TERM`, a timeout of 30 minutes per attempt and 10 minutes without
output. It is the first thing the dry run **cannot** test: the fake agent replaces it.

**What `--approval-mode write` did headless, as observed in the trials.** `omp` sorts every tool call into `read`, `write` and
`exec` and has three modes:

| Mode | Approves by itself | Asks about | Headless, with nobody to answer |
| --- | --- | --- | --- |
| `always-ask` | read | write, exec | every edit is refused: useless for a task that must change a file |
| `write` (**the template**) | read, write | exec | **the agent could not run shell commands** (the approval is not available), so it edited the file **blind**: it never built or tested what it wrote. NEO's own verification of the committed result did all the proving |
| `yolo` (`omp`'s default, and probably your global setting) | everything | nothing | the agent could run any command as you. It would test its own work, and nothing would brake it |

This is the **safe default**, and it is why the contract matters: the agent cannot even run the tests it is judged by, so what
it claims proves nothing and the contract is the only proof. Consequences you will see: a first attempt that does not compile
or is not formatted is normal (one automatic retry, with the failing check quoted in its brief), and a task must be small,
named by file, and specified precisely. Do **not** "fix" it with `--auto-approve`, `--yolo` or `--approval-mode yolo`: the
agent could then run anything as you, the contracts would still judge the commit afterwards, but they cannot judge what a
process did to your files.

Risks that remain in `write` mode, plainly:

- `write` means workspace write; **nothing was verified about a write outside the worktree being refused.** The worktree is a
  throwaway branch; a file written elsewhere is not. Contracts judge what was committed, not what was done to your files.
- **The default profile reads your configuration through `HOME`**: your global MCP servers (their tools count as `write` and are
  approved automatically), your rules and your login. In a dry test with the default profile `omp` printed a warning that a
  globally configured MCP server failed to connect, and `--no-extensions` did not prevent it. List your global MCP servers
  before the first run and turn off the ones a worker has no business with, or use the isolated profile below.
- Project files that could configure the agent (`.omp/`, `.mcp.json`, `AGENTS.md`) are guards on the protected list so a task
  cannot create them, and `--no-rules --no-extensions --no-skills` are in the arguments.
- `HOME` is passed (omp needs it), so the worker can read `~/.omp` and the rest of your home. **That is not a security
  boundary**: same user. For the first runs prefer a container, a VM or a separate OS account that owns the clone, the profile
  and the state directory and nothing else.

**Optional, recommended, not exercised: an isolated profile.** `omp --profile fleet-dogfood` gives `omp` its own auth, sessions,
settings and caches (`~/.omp/profiles/fleet-dogfood`) and keeps your global MCP servers and rules out. Log in once, in a terminal,
with the key that has the hard limit: `omp --profile fleet-dogfood login <provider>`, then add `"--profile", "fleet-dogfood",`
to `args` before `"--cwd"`. Check it in an empty directory, with no tools and a trivial prompt (a few tokens):

```sh
d=$(mktemp -d) && cd "$d" && printf 'Reply with the single word OK. Do not use any tool.\n' > brief.md
omp -p --no-session --no-tools --no-extensions --no-skills --no-rules --profile fleet-dogfood \
    --max-time 60 --cwd "$d" --approval-mode write @brief.md
```

Expected: `OK` and no warning about MCP servers or other configuration of yours. The same command without `--profile`, run when this
runbook was prepared, printed `OK` after the MCP warning (exit status 0, about 2 seconds); `@file` as the prompt, `--approval-mode write`,
`--no-rules`, `--cwd` and `--max-time` were accepted. **The profile itself does not exist yet and was not tried.**

### If the key must be a variable

If the provider's key cannot live in omp's own storage and must be an environment variable, name that **one** variable in
`[workers.agent] env_pass`, export it only in the terminal that starts the daemon, never put its value in any file, and remember that
the daemon's `[redact] env_values = true` redacts its value from stored output. A variable passed this way is visible to the worker
and to everything the worker starts.

## 3. The dry run with the fake agent

Before the real agent, run the whole loop with the scripted agent, in a throwaway clone of your clone, with a throwaway HOME and
state directory. It runs the real contract of each task, container and all, so it takes a while; run it **inside the development
container** (no docker client there; it uses `container-run.inside.sh`):

```sh
# from the repository, in the container (README, Development): cargo build --release --locked && ...
sh dogfood/dryrun.sh --fleet /target/release/fleet --source "$PWD"
```

It loads a stage of the plan (the three starter tasks, a human-only task and the agent task behind it), adds the goals with their
rendered contracts, starts the run with one worker at a time and shows what happens, then does the reviewer's steps. **What you
must see** (the appendix is the real output of that script):

1. `starter-docs-glossary` and `starter-error-tests` complete after their contracts passed (`goals list`: `verified`).
2. `starter-value-property` touches `SECURITY.md` on its first attempt: its contract **fails** (`protected-diff`,
   `protected-script`), the failing checks are quoted in the brief, the retry leaves `SECURITY.md` alone and then it passes.
3. `n3-scaffold` (human-only) fails at once and costs nothing; `n3-source` behind it is `blocked`; `fleet verify` of it says there is
   no worktree to verify, so its contract can never pass.
4. A goal is `verified` only with evidence; `goals show` lists it with the commit and tree it was taken on.
5. `fleet merge` answers "approval required" with an approval bound to the exact commit; nothing merges before you approve.
6. The commits come out authored `fleet <fleet@localhost>`; a `git cherry-pick -n` into another branch followed by
   `check-conflict-markers.sh --git` and a commit of your own re-authors them.

If any of these differs on your machine, stop: that is a finding, not noise.

## 4. The first real run: ONE starter task, one worker

Start with the smallest, safest task: `starter-docs-glossary` (three rows in `docs/domain.md`).

```sh
cd ~/work/fleet-dogfood
base=$(git rev-parse HEAD)                                   # a hash: it must not move during the run
# (a) the daemon, in its own terminal (XDG_STATE_HOME exported as in section 2)
fleet daemon --config ~/.fleet-dogfood/fleet.toml --web 127.0.0.1:0       # the dashboard is optional; it prints a login URL once
# (b) a stage of the plan with ONLY that task
ids=$(sed -n '/^\[\[tasks\]\]/,$ s/^id = "\(.*\)"$/\1/p' dogfood/plan.neo-roadmap.toml | grep -v '^starter-docs-glossary$' | tr '\n' ',' | sed 's/,$//')
#     --repo is mandatory in practice: fleet does not make the `repo` of a plan file absolute, a relative one is resolved by the
#     DAEMON against its own working directory (the first dry run of this scaffold used the wrong repository that way)
sh dogfood/plan-without.sh --repo "$HOME/work/fleet-dogfood" dogfood/plan.neo-roadmap.toml "$ids" > ~/.fleet-dogfood/stage1.toml
fleet plan ~/.fleet-dogfood/stage1.toml
# (c) its goal and contract, rendered for this run
sh dogfood/apply-goals.sh --tsv dogfood/goals.neo-roadmap.tsv --run neo-roadmap --repo ~/work/fleet-dogfood --base "$base" \
   --tools ~/.fleet-dogfood/bin --container-run ~/.fleet-dogfood/bin/container-run --out ~/.fleet-dogfood/contracts \
   --skip-tasks "$ids"                      # prints the commands: read them and the rendered contract, then add --apply
fleet goals show g-starter-docs-glossary    # the checks, none with evidence yet
# (d) go: one worker, no parallelism
fleet start --run neo-roadmap --max-parallel 1
fleet tail starter-docs-glossary --follow
```

`fleet status` and `fleet goals show g-starter-docs-glossary` tell you where it is. Expect: the worker runs (up to 25 minutes of
`omp`, usually far less), fleet commits its output, the contract runs in the container (tens of minutes the first time: the build and
the layered tests are most of it), the task completes if every check passed, or it fails and is retried once with the failing check
in the brief. Watch the first `fleet tail` for what `omp` prints when it is refused a shell command: that is the behaviour of
section 2 as it is today, and the first thing to compare with the trials.

## 5. Review, merge, and taking the work into the branch you keep

Do all of this before approving anything. Replace `RUN`, `TASK`, `BASE` by your run, task and the base commit.

```sh
fleet goals list                      # verification: verified | unverified | failed | pending
fleet goals show g-<task>             # every check, its newest evidence, the commit and tree it was taken on
fleet verify neo-roadmap/<task>       # run the contract again now on the task's current commit; exit 0 only if it passed
fleet tail <task>                     # what the agent printed
git -C ~/work/fleet-dogfood log --format='%h %an <%ae> %s' BASE..fleet/RUN/TASK
git -C ~/work/fleet-dogfood diff BASE..fleet/RUN/TASK         # READ IT. The contract proves the checks passed, not that the diff is good
sh ~/.fleet-dogfood/bin/check-protected.sh   --repo ~/work/fleet-dogfood BASE..fleet/RUN/TASK
sh ~/.fleet-dogfood/bin/check-docs-claims.sh --repo ~/work/fleet-dogfood BASE..fleet/RUN/TASK
git -C ~/work/fleet-dogfood worktree list                      # only the worktrees you expect
```

There is no `fleet diff` command; the diff is git's. The web dashboard (`fleet daemon --web`) shows the same goals, evidence,
approvals and the DAG; approving and merging from it needs the same digest as the CLI.

To merge into the run's integration branch:

```sh
fleet merge <task>                    # exits 5 and names an approval bound to the exact commit
fleet approval list                   # the exact git command and the digest
fleet approve <approval id>           # only now; it covers that commit and nothing the branch gains afterwards
fleet merge <task>                    # merged into fleet/RUN/integration
```

fleet refuses the merge unless there is passing evidence for exactly that commit.

### Taking the commits into the repository you publish

**The commits fleet makes are authored `fleet <fleet@localhost>`** (fleet's default; the template does not change it) and their
messages are fleet's (`fleet: <task> - <title>`). The branch you publish carries your identity and your messages, so you take the
work in yourself, from the **task branches**, not from the integration branch's merge commits. In your own clone of the
repository, not the throwaway one:

```sh
git remote add dogfood ~/work/fleet-dogfood && git fetch dogfood 'refs/heads/fleet/*:refs/remotes/dogfood/fleet/*'
git switch -c task/<name> main
git cherry-pick -n BASE..dogfood/fleet/RUN/TASK     # onto the index and the files, committing nothing
```

**A cherry-pick can leave conflict markers** (`<<<<<<<`, `=======`, `>>>>>>>`) in a file whose neighbouring lines changed on
`main` since BASE, and `-n` leaves them in the files and the index without a word in the commit. **Always look before you commit:**

```sh
git grep -nE '^(<<<<<<<|=======|>>>>>>>)'                # every line, including legitimate `=======` heading underlines in markdown
sh ~/.fleet-dogfood/bin/check-conflict-markers.sh --git . # unmerged index entries and any file with a start or end marker (the contract runs `--tree` too)
git diff --cached                                         # read it once more
sh scripts/secret-scan.sh --staged
git commit -m "<your own message>"                       # as yourself, no trailers
git log --format='%h %an <%ae> %s' -1                    # check the identity before you push anything
```

If markers are there, resolve them as a person (the diff on `main` is what you know and the agent did not). The dry run in the
appendix does exactly this with a task that does not conflict. `check-conflict-markers.sh --tree` is also a check of every
contract, so a *task* that commits markers is caught; the cherry-pick case is the one only you can catch, and it is on the checklist.

## 6. Abort, and roll back

| Situation | Do |
| --- | --- |
| a worker is running and you do not like it | `fleet stop <task>` (TERM, then KILL of its whole process group); then `fleet cancel <task>` or leave it failed |
| stop everything | `fleet shutdown`; workers lose their terminals; the next daemon start stops survivors and re-queues |
| a check left a container behind (a killed `docker run` client does not stop the container) | `docker ps`; `docker kill <id>` (the stress check's busy loops end by themselves after 280 s) |
| undo what the worker did in its worktree | with `agent-undo` installed (`fleet status` does not say `undo unavailable`): `fleet undo <task>` previews, `fleet undo <task> --apply` reverts. Otherwise git, below |
| drop a task's work | `fleet cleanup --run RUN --branches`, or by hand: `git -C ~/work/fleet-dogfood worktree remove --force .fleet-worktrees/RUN/TASK && git -C ~/work/fleet-dogfood branch -D fleet/RUN/TASK` |
| drop what was merged into the integration branch | `git -C ~/work/fleet-dogfood branch -D fleet/RUN/integration` (your own branch was never touched) |
| drop what you took into your branch | `git reset --hard <before>` on your branch (before pushing), or `git revert` after |
| drop everything | delete the throwaway clone, `~/.fleet-dogfood/state`, the docker volumes `fleet-dogfood-*` if you want the caches gone, and the omp profile `~/.omp/profiles/fleet-dogfood` if you made one; revoke the key in the provider console |
| you suspect the worker did more than edit its worktree | assume it can: read `.git/config` and `.git/hooks/` of the clone, `git status` and `git worktree list` of it, files in `~/.fleet-dogfood`, recent files in your home (`find ~ -mtime -1 -not -path '*/Library/*'`), the provider's key list; then delete the clone and rotate the key |

## 7. What to monitor

- **Spend, by the provider's own dashboard, during every run.** There is no token governor (N4 does not exist): fleet does not
  count tokens or money. The hard limit set in the console (section 1) is the only brake. Look before and after each task
  and write down the cost of a starter task: it is your first real number.
- `fleet status` (running tasks, retries, `rss`), `fleet goals remaining`, `fleet approval list`, `fleet gate list`
  (a worker may ask questions: answer them yourself, never let a worker's answer decide).
- The daemon's stderr, `fleet events`, and `fleet verify-log` (the audit log's hash chain) after each run.
- The clone: `git -C ~/work/fleet-dogfood status --short` (must be empty; fleet never touches your checkout) and
  `git -C ~/work/fleet-dogfood branch --list 'fleet/*'`.
- Disk (the docker volumes and `.fleet-worktrees`), processes (`ps` for leftover `omp` or cargo processes after a stop, `docker ps`).
- Time: `--max-time 25m` and `timeout_secs = 1800` bound one attempt; `max_retries = 1` means at most two attempts a task.

## 8. Before you enable more than one task at a time

Do not raise `max_sessions` or `--max-parallel`, and do not leave a run unattended, until **every** box is true:

- [ ] Two starter tasks were completed by the real agent and merged by you, and you read both diffs.
- [ ] You know the cost of one starter task from the provider's dashboard, and the hard limit is a multiple of what a whole
      stage costs that you accept losing.
- [ ] A task that was meant to fail did fail: for example (by hand) a branch that touches `SECURITY.md` makes `protected-diff` and
      `protected-script` fail, a changed `Cargo.lock` makes `lock-unchanged` fail, a file with a conflict marker makes
      `conflict-markers` fail, and fleet did not complete or merge it.
- [ ] You ran `check-protected.sh` and `check-docs-claims.sh` yourself on a real task's commit and read their output.
- [ ] You took one task's commits into your branch with `git cherry-pick -n`, ran **`git grep -nE '^(<<<<<<<|=======|>>>>>>>)'` and
      `check-conflict-markers.sh --git .`** before the commit, and checked the identity of the commit afterwards.
- [ ] The base is green (section 2, step 5) and stays green: `sh dogfood/tests/run.sh` and the layered tests pass on your
      branch after every merge.
- [ ] You listed the global MCP servers `omp` would load for the worker and either removed the ones it has no business with, or made
      the isolated profile and checked it, or you run the worker in a container or under another OS user.
- [ ] Nothing the worker did outside its worktree has been found (section 6, last row) after at least two runs.
- [ ] The tools directory still holds exactly the scripts you installed (compare the printed hashes), and no task touched `dogfood/`.
- [ ] You decided what to do about the known gaps below, in writing.

## 9. Recipes

**A flaky test (the stress pattern).** A flaky test passes when it runs alone, so "the suite passed" proves little. The template
`contracts/flaky-test.json` adds a `stress` check: it builds the test binary once, starts eight busy loops, and runs the one test
25 times in a row; one failure fails the check. If the test lives in a protected file (the two trials' tests did: `server.rs`, `jobs.rs`
of `fleet-infra`), the template is a **reviewed exception for that one file**: no glob deny check, but `check-protected.sh --except
<file>` (that exact path only, a Rust source or test file of a crate) and `check-test-only.sh` (everything above the file's test
module is byte-identical to the base). You still read the diff of the test module before you approve the merge, and the daemon must be told the exception on
purpose, by rendering this template by hand:

```sh
sh dogfood/render-contract.sh dogfood/contracts/flaky-test.json ~/.fleet-dogfood/contracts/g-flaky.json \
   TOOLS=$HOME/.fleet-dogfood/bin CONTAINER_RUN=$HOME/.fleet-dogfood/bin/container-run REPO=$HOME/work/fleet-dogfood \
   BASE=$(git rev-parse HEAD) RUN=flaky TASK=flaky-server FILE_PATH=crates/fleet-infra/src/server.rs \
   PACKAGE=fleet-infra TEST_NAME=a_reply_that_is_being_produced_when_the_server_stops_is_still_delivered \
   TEST_FILTER=server::tests::a_reply_that_is_being_produced_when_the_server_stops_is_still_delivered
```

then a plan like [trials/plan.flaky-server-test.toml](trials/plan.flaky-server-test.toml) (its spec is the model: product rule, hypotheses
to check, forbidden lazy fixes, "commit nothing yourself") and `fleet goals add --task RUN/TASK --contract ...`. Its limits: it
proves nothing about the part of the file after the first test module, and the agent in `write` mode cannot run the test it is
fixing; the stress check is what proves it.

**After a human-only barrier.** See [human-only.md](human-only.md): finish and land the person's work, abandon its goal with a reason,
then `plan-without.sh --repo <clone> --run-id <new id> plan.neo-roadmap.toml <done ids>` and `apply-goals.sh --skip-tasks <done ids>`.

**A dependency refresh.** `deps-refresh.sh` (a person runs it; the age check needs the network; the container wrapper's online
mode is used for `cargo update` only). It puts `Cargo.lock` back if any locked version is newer than the cutoff.

## 10. Known gaps

- **No token governor (N4).** Spend is limited only by the provider's console. `max_retries` and the timeouts bound time, not money.
- **No context compiler in this branch** (N2 is being built elsewhere). The agent receives the task's spec as its brief and does not
  see the contract; it can learn what is checked only from a failed attempt's quoted checks. Nothing assembles a context pack or
  hands over verified knowledge.
- **The agent cannot run anything in `write` mode**: it edits blind, so a small task passes and a task that needs a build to get
  right fails twice and ends `failed`. Prefer specs that name the file, the functions and the tests; keep tasks to one file.
- **The worker is the same user as you; that is not a security boundary** (README and SECURITY.md, Limitations). `HOME` is passed,
  so the worker can read `~/.omp` and the rest of your home. Contracts and checks judge committed work; a worker can still write
  anywhere you can. Use a clone you can delete, and prefer a container, a VM or another OS user.
- **Checks run the committed code.** `cargo test` in the container executes the task's tests and build scripts (a `build.rs` is a
  guarded path, a test is not). The container has no network, a read-only tree and nothing of yours mounted, but it is not a boundary
  against a hostile test.
- **A weak check passes a weak result.** A task that adds a poor test passes if the test compiles and passes. You read the diff.
- **The protected list is only as good as its author.** Names that matter and do not exist yet are guards; a new kind of
  guardrail file must be added by a person first. `crates/fleet-infra/**` is protected as a whole, so even a test-only change in it is a
  reviewed exception (section 9) or a human-only task.
- **Re-authoring is manual** and can leave conflict markers (section 5); nothing automates it.
- **Not verified with the real agent** (section 11).
- **fleet cannot mark a task completed by hand**: human-only tasks are barriers, handled by cutting the plan
  (`plan-without.sh`) and starting a new run; goals of finished barriers are abandoned by hand with a reason.
- ADR numbers: this branch's ADR is numbered 20 because `main` already holds 17 to 19 (context compiler, handoffs, workbench);
  README, SECURITY.md and docs/neo.md of this branch will conflict with `main`'s in the lines they both edit.

## 11. What was not verified

By the preparation of this runbook: **no run of the scaffold with `omp`**; the isolated profile (it does not exist and was not
tried) and what `omp` prints, retries or stops on when a shell command is refused beyond what the trials showed; whether writes
outside the worktree are refused in write mode; the docker wrapper `container-run.example.sh` itself (it follows the wrapper of the
trials, which was run on macOS, but this file was not run: the dry run used `container-run.inside.sh` inside the development
container, with a soak of 2 seconds instead of 20 and without the network and read-only isolation of the real wrapper); the real
dependency refresh (`deps-refresh.sh` was tested with stand-ins for the container and the age check, never against crates.io);
anything on macOS (the shell tests and the dry run ran on Linux with GNU tools; the scripts use `sha256sum` or `shasum`, `mktemp`,
`sed`, `awk`, `find`, `xargs -0` as found on macOS but were not run there); `flock` is used only by `container-run.inside.sh`, which is not
for macOS; very long runs, several daemons, a plan with all 32 tasks running to the end.
