# Runbook: letting NEO develop its own repository under human control

**Status of this document.** The scaffold in `dogfood/` (the protected-path list, the contract templates, the checkers,
the plan, the daemon template, this procedure) was built and tested **without running a real agent**: 
shell tests of every script, consistency tests of the data (`crates/fleet-infra/tests/dogfood.rs`), and a dry run of a stage
of the plan with the scripted agent `examples/agents/fake-agent.sh`, whose real output is the
[appendix](#appendix-the-dry-run-as-it-ran) (a run of `dogfood/dryrun.sh`, exit status 0, inside the development container, on the commit it names). **The scaffold itself has never been run with a real agent.**
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
- The development container of the README (`docker/Dockerfile.dev`, image `fleet-dev`) and two docker volumes for it. **Not** the
  `fleet-ssh` image (`docker/Dockerfile.ssh`): the `remote` layer of `scripts/test-layers.sh`, the tests of the SSH remote worktrees against a
  real sshd, runs only where an sshd exists, so inside `fleet-dev` it prints `skipped` and a contract's `layers` check judges the other five
  layers. The `remote` layer is deliberately not part of any self-development contract (the code it tests is protected, so no task can change what
  it proves; [contracts/README.md](contracts/README.md)). Run it yourself after you have changed such code: `IMAGE=fleet-ssh`, README, Development.
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
#    and `image=` (only if you use another image, never the sshd one). It must not be writable by group or others.
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

It loads a stage of the plan (the three starter tasks, a documentation task that waits for them, a human-only task and the agent
task behind it), adds the goals with their rendered contracts, starts the run with one worker at a time and shows what happens,
then does the reviewer's steps. **What you must see** (the appendix is the real output of that script):

1. `starter-docs-glossary` and `starter-error-tests` complete after their contracts passed (`goals list`: `verified`).
2. `starter-value-property` touches `SECURITY.md` on its first attempt: its contract **fails** (`protected-diff`,
   `protected-script`), the failing checks are quoted in the brief, the retry leaves `SECURITY.md` alone and then it passes.
3. `maint-docs-links` waits for the three starters; fleet merges their work into its branch, and its contract's scope check, rendered
   by `apply-goals.sh` from the plan, allows the files they changed as well as `README.md` and `docs/**` (section 9, "A dependent task").
4. `n3-scaffold` (human-only) fails at once and costs nothing (its worker exits 1 twice); `n3-source` behind it is `blocked`;
   `fleet verify` of its goal fails on both checks and the evidence does not count.
5. A goal is `verified` only with evidence; `goals show` lists it with the commit and tree it was taken on.
6. `fleet merge` answers "approval required" with an approval bound to the exact commit; nothing merges before you approve.
7. The commits come out authored `fleet <fleet@localhost>`; a `git cherry-pick -n` into another branch followed by
   `check-conflict-markers.sh --git` and a commit of your own re-authors them.

If any of these differs on your machine, stop: that is a finding, not noise. One thing that **does** vary, and is not a finding about the
scaffold: the `layers` check runs every suite of `scripts/test-layers.sh` except the `remote` layer (no sshd in the image), and the
debug-profile timing tests (the wall-clock bounds of a few domain tests are meant for an idle machine, README) and a few flaky tests of earlier
bases (the ones of [trials/](trials/README.md) and others; several have been fixed since) can fail under load. Earlier dry runs of this scaffold
each lost one task attempt to one of them (`jobs::tests::the_tracker_lists_a_running_group_leader_until_the_job_has_ended` and
`jobs::tests::gates_and_several_jobs_at_once_are_all_listed`). `dryrun.sh` therefore retries a task that ended `failed` with `fleet retry`, up to
three rounds, and says so in its output; do the same by hand in a real run, and treat a task that fails the same check twice as a real
failure. A test that fails under load and passes alone is not a reason to weaken it.

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
sh dogfood/apply-goals.sh --tsv dogfood/goals.neo-roadmap.tsv --plan ~/.fleet-dogfood/stage1.toml --run neo-roadmap \
   --repo ~/work/fleet-dogfood --base "$base" --tools ~/.fleet-dogfood/bin --container-run ~/.fleet-dogfood/bin/container-run --out ~/.fleet-dogfood/contracts \
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

**A dependent task.** fleet merges the finished dependencies of a task into its branch before it starts, and the `diff` check reads
the whole diff since the run's base. A task that `after`-depends on another agent task therefore *carries that task's changes*: with
a scope of one file it would fail on the files of its dependencies. `apply-goals.sh --plan <the plan that was loaded>` works out,
for every task, the scopes of its transitive dependencies (it stops at a human-only task: a person lands what is behind a barrier) and
renders them into the `@ALSO@` slot of the scope check. Consequences: the scope of a dependent is wider than its own file (by exactly
what its dependencies were allowed to change); `protected-diff` is unchanged; and a stage cut with `plan-without.sh` (the
dependencies landed in the base) needs nothing extra. Give `--plan` the stage you loaded, not the full roadmap.

**After a human-only barrier.** See [human-only.md](human-only.md): finish and land the person's work, abandon its goal with a reason,
then `plan-without.sh --repo <clone> --run-id <new id> plan.neo-roadmap.toml <done ids>` and `apply-goals.sh --plan <that stage> --skip-tasks <done ids>`.

**A dependency refresh.** `deps-refresh.sh` (a person runs it; the age check needs the network; the container wrapper's online
mode is used for `cargo update` only). It puts `Cargo.lock` back if any locked version is newer than the cutoff.

## 10. Known gaps

- **No token governor (N4).** Spend is limited only by the provider's console. `max_retries` and the timeouts bound time, not money.
- **The context compiler (N2) exists, and the plan does not use it.** It is an opt-in of a run (`context = "compiled"` in `[run]`, README,
  docs/neo.md); this scaffold was never run with it. The plan's tasks are briefed with the task's spec and the agent does not see the contract;
  it can learn what is checked only from a failed attempt's quoted checks. Whether a compiled pack helps these tasks is for the first real run to find out.
- **The `remote` layer is not run by a contract** (section 1): the tests of the SSH remote worktrees need a real sshd and an image the wrapper does not use.
  The code they test is protected; run them yourself after a person changed it.
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
- **The protected list was written before the later features and was extended when they landed** (ADR 28): the SSH, forge, messaging, fan-out,
  context compiler and web workbench guardrails were added by name, and `crates/fleet-infra/tests/dogfood.rs` checks that every listed path exists. A
  guardrail file that a later feature adds is not on the list until a person adds it and runs `gen-contracts.sh`.

## 11. What was not verified

By the preparation of this runbook: **no run of the scaffold with `omp`**; the isolated profile (it does not exist and was not
tried) and what `omp` prints, retries or stops on when a shell command is refused beyond what the trials showed; whether writes
outside the worktree are refused in write mode; the docker wrapper `container-run.example.sh` itself (it follows the wrapper of the
trials, which was run on macOS, but this file was not run: the dry run used `container-run.inside.sh` inside the development
container, with a soak of 2 seconds instead of 20 and without the network and read-only isolation of the real wrapper, and in an image without
an sshd, so the `remote` layer was skipped); the real
dependency refresh (`deps-refresh.sh` was tested with stand-ins for the container and the age check, never against crates.io);
anything on macOS (the shell tests and the dry run ran on Linux with GNU tools; the scripts use `sha256sum` or `shasum`, `mktemp`,
`sed`, `awk`, `find`, `xargs -0` as found on macOS but were not run there); `flock` is used only by `container-run.inside.sh`, which is not
for macOS; very long runs, several daemons, a plan with all 32 tasks running to the end.

## Appendix: the dry run, as it ran

The real, unedited output of `sh dogfood/dryrun.sh --fleet /target/release/fleet --source /work`, run inside the development container
(`fleet-dev`, no network, cargo cache in a docker volume) on commit `7d50660`, after `cargo build --release --locked --offline` in the same
container. The cargo checks of the contracts went through `container-run.inside.sh` (a copy of the tree at one fixed path, a soak of 2
seconds instead of 20), not through the docker wrapper. Paths under `/tmp` are those of that run. Things to read in it: the three
starters and `maint-docs-links` completing only after every check passed (`goals list`: `verified`); `starter-value-property` failing
its first attempt on `SECURITY.md` (`scope` in the evidence of the goal, `protected-diff` and `protected-script` in the audit log, which the
section of failed checks and `fleet events` show), then passing at the retry; in this run no task needed a manual `fleet retry`; `n3-scaffold` failing at once and `n3-source` behind it
`blocked`; `fleet verify` of the human-only goal failing both checks with evidence that does not count (exit 6); the merge answering exit 5
with an approval bound to the exact commit; the commits authored `fleet <fleet@localhost>`; the cherry-pick and the commit under another
identity. The run ended with `exit=0`.

<details>
<summary>Full output (click to expand)</summary>

```text
   Compiling fleet-cli v0.1.0 (/work/crates/fleet-cli)
    Finished `release` profile [optimized] target(s) in 3m 35s

$ git clone <source> clone   # a throwaway clone; the worker never sees any other checkout
base commit 7d5066016dfcef611ed320d4d5d1843723620173

$ sh /tmp/dogfood-dryrun.J2isuA/clone/dogfood/install-tools.sh --repo /tmp/dogfood-dryrun.J2isuA/clone --ref HEAD --dest /tmp/dogfood-dryrun.J2isuA/tools
install-tools: reading dogfood/ at 7d5066016dfcef611ed320d4d5d1843723620173
  f414a8f567dfb41ed462a917a8e383760eddc338d4c4809857b3321ca36745d3  check-protected.sh
  4ecf5b37c93bc91e09aa05a8119883ac0e85fb90d71f4196b75064a134eb1b2b  check-docs-claims.sh
  268d6b286238209d998fe06b980147fc2777f3255003822a7f19f832bb21b1e7  check-secret-tree.sh
  ca0093d07b5cb43288cc178b236a48e8542066bf3fcd5e2470fdc02739e4d808  check-conflict-markers.sh
  d470021f85e161b5c81ff4f1febba937bfc5c68a2b74d6f43c4ab9632e36b75a  check-test-only.sh
  f5cc56c277653b6deefa2503aa3b9e9d60f1c0067f9e4aecc4777694f4cfb44e  protected-paths.json
install-tools: installed into /tmp/dogfood-dryrun.J2isuA/tools

$ container-run = a copy of dogfood/container-run.inside.sh outside the clone (/tmp/dogfood-dryrun.J2isuA/container-run)

$ dryrun config: dogfood/fleet.dogfood.example.toml with the worker command replaced (diff)
< command = "/Users/kienpt/.local/bin/omp"
< args = [
<   "-p",
<   "--no-session",
<   "--no-extensions",
<   "--no-skills",
<   "--no-rules",
<   "--max-time", "25m",
<   "--cwd", "{worktree}",
<   "--approval-mode", "write",
<   "@{brief_file}",
> command = "/tmp/dogfood-dryrun.J2isuA/clone/examples/agents/fake-agent.sh"
> args = []
< [[gates]]
< name = "tests"
< command = "/Users/kienpt/.fleet-dogfood/bin/container-run"
< args = ["sh scripts/test-layers.sh"]
< timeout_secs = 1800

$ sh dogfood/plan-without.sh --repo <clone> dogfood/plan.neo-roadmap.toml <all but 6 tasks> > stage.toml
tasks in the stage: 6

$ /target/release/fleet plan /tmp/dogfood-dryrun.J2isuA/stage.toml
plan loaded into run neo-roadmap (6 tasks)

$ sh dogfood/apply-goals.sh --plan stage.toml --skip-tasks <the others> --apply ...   # renders one contract per goal (a dependent's scope also allows its dependencies') and adds the goals
goal m-starters added (open)
goal g-starter-error-tests added (open), contract 5dacdcf94661
goal g-starter-value-property added (open), contract dbd272c5434e
goal g-starter-docs-glossary added (open), contract c5cf70bbf96d
goal m-maint added (open)
goal g-maint-docs-links added (open), contract a3221cb32d90
goal m-n3 added (open)
goal g-n3-scaffold added (open), contract 46f26596d621
goal g-n3-source added (open), contract 7b24aab1cb5c
goal m-n4 added (open)
goal m-n5 added (open)
apply-goals: 11 goal(s) added

$ the scope check of maint-docs-links: it runs after the three starters, whose work fleet merges into its branch
      "id": "scope",
      "kind": "diff",
      "allow": ["README.md", "docs/**", "crates/fleet-domain/src/error.rs", "crates/fleet-domain/src/value.rs", "docs/domain.md"]
    }
  ]

$ /target/release/fleet goals list
11 goals, 0 verified, 0 abandoned
GOAL                        STATUS  CONTRACT  TASKS                                             TITLE
m-starters                  open    none      -                                                 The loop works end to end on small safe tasks
  g-starter-error-tests     open    pending   neo-roadmap/starter-error-tests (ready, pending)  fleet-domain error.rs has unit tests
  g-starter-value-property  open    pending   neo-roadmap/starter-value-property (ready, pend…  Value canonical form has a property test
  g-starter-docs-glossary   open    pending   neo-roadmap/starter-docs-glossary (ready, pendi…  The glossary names the self-development terms
m-maint                     open    none      -                                                 Maintenance stays small and under control
  g-maint-docs-links        open    pending   neo-roadmap/maint-docs-links (pending, pending)   Relative links and anchors in the docs resolve
m-n3                        open    none      -                                                 N3: the knowledge store
  g-n3-scaffold             open    pending   neo-roadmap/n3-scaffold (pending, pending)        The neo-knowledge crate exists in the workspace
  g-n3-source               open    pending   neo-roadmap/n3-source (pending, pending)          Fact sources and their confidence
m-n4                        open    none      -                                                 N4: the token governor
m-n5                        open    none      -                                                 N5: the credential broker

$ /target/release/fleet goals show g-starter-docs-glossary
goal g-starter-docs-glossary [open]
title:    The glossary names the self-development terms
parent:   m-starters
reason:   added
intent:
  > docs/domain.md has rows for protected path, human-only task and self-development; only that file changed
contract: pending (id c5cf70bbf96d)
tasks:
  neo-roadmap/starter-docs-glossary  ready  verification: pending
checks (newest evidence of each):
  protected-diff [diff, mandatory] not run
    changes nothing matching .github/**, SECURITY.md, scripts/**, docker/**, docs/adr/**, examples/flee…
  protected-script [command, mandatory] not run
    /tmp/dogfood-dryrun.J2isuA/tools/check-protected.sh --repo /tmp/dogfood-dryrun.J2isuA/clone 7d50660…
  lock-unchanged [file, mandatory] not run
    Cargo.lock has sha256 56faee101362dfa3c62dff6acc44eff1d45651ee956f365632096117202954db
  docs-claims [command, mandatory] not run
    /tmp/dogfood-dryrun.J2isuA/tools/check-docs-claims.sh --repo /tmp/dogfood-dryrun.J2isuA/clone 7d506…
  conflict-markers [command, mandatory] not run
    /tmp/dogfood-dryrun.J2isuA/tools/check-conflict-markers.sh --tree (expect exit 0)
  secret-scan [command, mandatory] not run
    /tmp/dogfood-dryrun.J2isuA/tools/check-secret-tree.sh (expect exit 0)
  fmt [command, mandatory] not run
    /tmp/dogfood-dryrun.J2isuA/container-run cargo fmt --all -- --check (expect exit 0)
  clippy [command, mandatory] not run
    /tmp/dogfood-dryrun.J2isuA/container-run cargo clippy --workspace --all-targets --locked --offline …
  architecture [command, mandatory] not run
    /tmp/dogfood-dryrun.J2isuA/container-run cargo test --locked --offline -p fleet-cli --test architec…
  layers [command, mandatory] not run
    /tmp/dogfood-dryrun.J2isuA/container-run sh scripts/test-layers.sh (expect exit 0)
  scope [diff, mandatory] not run
    changes only docs/domain.md

$ /target/release/fleet goals remaining
11 goals, 0 verified, 0 abandoned
open leaves:
  GOAL                      STATUS  CONTRACT  TASKS                                             TITLE
  g-starter-error-tests     open    pending   neo-roadmap/starter-error-tests (ready, pending)  fleet-domain error.rs has unit tests
  g-starter-value-property  open    pending   neo-roadmap/starter-value-property (ready, pend…  Value canonical form has a property test
  g-starter-docs-glossary   open    pending   neo-roadmap/starter-docs-glossary (ready, pendi…  The glossary names the self-development terms
  g-maint-docs-links        open    pending   neo-roadmap/maint-docs-links (pending, pending)   Relative links and anchors in the docs resolve
  g-n3-scaffold             open    pending   neo-roadmap/n3-scaffold (pending, pending)        The neo-knowledge crate exists in the workspace
  g-n3-source               open    pending   neo-roadmap/n3-source (pending, pending)          Fact sources and their confidence
  m-n4                      open    none      -                                                 N4: the token governor
  m-n5                      open    none      -                                                 N5: the credential broker
blockers:
  none
next ready tasks:
  neo-roadmap/starter-error-tests  (goal g-starter-error-tests)  Starter: unit tests for fleet-domain error.rs
  neo-roadmap/starter-value-property  (goal g-starter-value-property)  Starter: a property test for the canonical form of fleet-do…
  neo-roadmap/starter-docs-glossary  (goal g-starter-docs-glossary)  Starter: glossary entries for self-development in docs/doma…

$ /target/release/fleet start --run neo-roadmap --max-parallel 1
run neo-roadmap started

$ /target/release/fleet status
daemon pid 398 | v0.1.0 | up 10m37s | sessions 0/1 | undo unavailable | rss 5956 kB

run neo-roadmap [active] max-parallel 1
  objective  Build NEO milestones N3, N4 and N5 under human control: agents write pure domain code and docs in n…
  repo       /tmp/dogfood-dryrun.J2isuA/clone (base HEAD @ 7d506601)
  branch     fleet/neo-roadmap/integration
  tasks      pending 0, ready 0, running 0, review 0, completed 4, failed 1, blocked 1
  ID                    STATUS     DEPS                      TRY  WORKER      NOTE
  starter-error-tests   completed  -                         1    agent       added the unit tests of error.rs
  starter-value-prope…  completed  -                         2    agent       added a property test of the canonical form
  starter-docs-glossa…  completed  -                         1    agent       added three glossary rows
  maint-docs-links      completed  starter-error-tests,sta…  1    agent       checked the links
  n3-scaffold           failed     starter-error-tests,sta…  2    human-only  failed: worker exited with code 1
  n3-source             blocked    n3-scaffold               0    agent       blocked: dependency n3-scaffold - dependency n3-scaffold di…

recent
  #128 evidence e49: goal g-maint-docs-links check "conflict-markers" passed
  #129 evidence e50: goal g-maint-docs-links check "secret-scan" passed
  #130 evidence e51: goal g-maint-docs-links check "fmt" passed
  #131 evidence e52: goal g-maint-docs-links check "clippy" passed
  #132 evidence e53: goal g-maint-docs-links check "architecture" passed
  #133 evidence e54: goal g-maint-docs-links check "layers" passed
  #134 evidence e55: goal g-maint-docs-links check "scope" passed
  #135 goal g-maint-docs-links is verified
  #136 goal m-maint is verified
  #137 neo-roadmap/maint-docs-links completed

$ /target/release/fleet goals list
11 goals, 6 verified, 0 abandoned
GOAL                        STATUS    CONTRACT  TASKS                                             TITLE
m-starters                  verified  none      -                                                 The loop works end to end on small safe tasks
  g-starter-error-tests     verified  passed    neo-roadmap/starter-error-tests (completed, ver…  fleet-domain error.rs has unit tests
  g-starter-value-property  verified  passed    neo-roadmap/starter-value-property (completed, …  Value canonical form has a property test
  g-starter-docs-glossary   verified  passed    neo-roadmap/starter-docs-glossary (completed, v…  The glossary names the self-development terms
m-maint                     verified  none      -                                                 Maintenance stays small and under control
  g-maint-docs-links        verified  passed    neo-roadmap/maint-docs-links (completed, verifi…  Relative links and anchors in the docs resolve
m-n3                        open      none      -                                                 N3: the knowledge store
  g-n3-scaffold             open      pending   neo-roadmap/n3-scaffold (failed, pending)         The neo-knowledge crate exists in the workspace
  g-n3-source               open      pending   neo-roadmap/n3-source (blocked, pending)          Fact sources and their confidence
m-n4                        open      none      -                                                 N4: the token governor
m-n5                        open      none      -                                                 N5: the credential broker

$ /target/release/fleet goals show g-starter-value-property
goal g-starter-value-property [verified]
title:    Value canonical form has a property test
parent:   m-starters
reason:   every mandatory check of the contract passed
intent:
  > value.rs has a property-style test of key-order independence; only that file changed
contract: passed (id dbd272c5434e)
tasks:
  neo-roadmap/starter-value-property  completed  verification: verified
checks (newest evidence of each):
  protected-diff [diff, mandatory] passed: 1 changed path(s), all allowed
    changes nothing matching .github/**, SECURITY.md, scripts/**, docker/**, docs/adr/**, examples/flee…
  protected-script [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/tools/check-protected.sh --repo /tmp/dogfood-dryrun.J2isuA/clone 7d50660…
    > check-protected: 1 changed path(s) in 7d5066016dfcef611ed320d4d5d1843723620173..fleet/neo-roadmap/starter-value-property, none protected (50 entries)
  lock-unchanged [file, mandatory] passed: the file's sha256 matches
    Cargo.lock has sha256 56faee101362dfa3c62dff6acc44eff1d45651ee956f365632096117202954db
  docs-claims [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/tools/check-docs-claims.sh --repo /tmp/dogfood-dryrun.J2isuA/clone 7d506…
    > check-docs-claims: 0 changed documentation line(s) in 7d5066016dfcef611ed320d4d5d1843723620173..fleet/neo-roadmap/starter-value-property, no measured number among them
  conflict-markers [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/tools/check-conflict-markers.sh --tree (expect exit 0)
    > check-conflict-markers: clean (--tree)
  secret-scan [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/tools/check-secret-tree.sh (expect exit 0)
    > secret-scan: clean (--tree)
  fmt [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/container-run cargo fmt --all -- --check (expect exit 0)
  clippy [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/container-run cargo clippy --workspace --all-targets --locked --offline …
    >
    > --- stderr ---
    >     Checking fleet-domain v0.1.0 (/tmp/work/crates/fleet-domain)
    >     Checking neo-verify v0.1.0 (/tmp/work/crates/neo-verify)
    >     Checking neo-goals v0.1.0 (/tmp/work/crates/neo-goals)
    >     Checking fleet-app v0.1.0 (/tmp/work/crates/fleet-app)
    >     Checking fleet-infra v0.1.0 (/tmp/work/crates/fleet-infra)
    >     Checking fleet-cli v0.1.0 (/tmp/work/crates/fleet-cli)
    >     Finished `dev` profile [unoptimized + debuginfo] target(s) in 4.06s
  architecture [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/container-run cargo test --locked --offline -p fleet-cli --test architec…
    > ... (23 earlier lines not shown)
    > test the_pure_layers_do_not_unwrap_or_expect_outside_their_tests ... ok
    > test no_source_file_contains_the_unsafe_keyword ... ok
    > test terminal_libraries_are_used_by_the_cli_only ... ok
    > test the_pure_layers_do_not_touch_the_outside_world ... ok
    > test the_inner_layers_know_nothing_of_networking_or_the_web_adapter ... ok
    >
    > test result: ok. 26 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.25s
    >
    >
    > --- stderr ---
    >     Finished `test` profile [unoptimized + debuginfo] target(s) in 1.03s
    >      Running tests/architecture.rs (/target/debug/deps/architecture-8a11d801b1f90613)
  layers [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/container-run sh scripts/test-layers.sh (expect exit 0)
    > ... (3 earlier lines not shown)
    >
    > == application ==
    > application: 299 passed, 0 failed
    >
    > == adapters ==
    > adapters: 524 passed, 0 failed
    >
    > == soak ==
    > soak: 1 passed, 0 failed
    >
    > == end-to-end ==
    > end-to-end: 261 passed, 0 failed
  scope [diff, mandatory] passed: 1 changed path(s), all allowed
    changes only crates/fleet-domain/src/value.rs
evidence history (newest first):
  EVIDENCE  CHECK             VERDICT  COMMIT        TASK                    SUMMARY
  e44       scope             passed   39aa5124edd9  starter-value-property  1 changed path(s), all allowed
  e43       layers            passed   39aa5124edd9  starter-value-property  exit code 0
  e42       architecture      passed   39aa5124edd9  starter-value-property  exit code 0
  e41       clippy            passed   39aa5124edd9  starter-value-property  exit code 0
  e40       fmt               passed   39aa5124edd9  starter-value-property  exit code 0
  e39       secret-scan       passed   39aa5124edd9  starter-value-property  exit code 0
  e38       conflict-markers  passed   39aa5124edd9  starter-value-property  exit code 0
  e37       docs-claims       passed   39aa5124edd9  starter-value-property  exit code 0
  e36       lock-unchanged    passed   39aa5124edd9  starter-value-property  the file's sha256 matches
  e35       protected-script  passed   39aa5124edd9  starter-value-property  exit code 0
  e34       protected-diff    passed   39aa5124edd9  starter-value-property  1 changed path(s), all allowed
  e22       scope             failed   34d11f2f521d  starter-value-property  1 changed path(s) outside the allowed globs
  e21       layers            passed   34d11f2f521d  starter-value-property  exit code 0
  e20       architecture      passed   34d11f2f521d  starter-value-property  exit code 0
  e19       clippy            passed   34d11f2f521d  starter-value-property  exit code 0
  e18       fmt               passed   34d11f2f521d  starter-value-property  exit code 0
  e17       secret-scan       passed   34d11f2f521d  starter-value-property  exit code 0
  e16       conflict-markers  passed   34d11f2f521d  starter-value-property  exit code 0
  e15       docs-claims       passed   34d11f2f521d  starter-value-property  exit code 0
  e14       lock-unchanged    passed   34d11f2f521d  starter-value-property  the file's sha256 matches

$ /target/release/fleet goals show g-starter-docs-glossary
goal g-starter-docs-glossary [verified]
title:    The glossary names the self-development terms
parent:   m-starters
reason:   every mandatory check of the contract passed
intent:
  > docs/domain.md has rows for protected path, human-only task and self-development; only that file changed
contract: passed (id c5cf70bbf96d)
tasks:
  neo-roadmap/starter-docs-glossary  completed  verification: verified
checks (newest evidence of each):
  protected-diff [diff, mandatory] passed: 1 changed path(s), all allowed
    changes nothing matching .github/**, SECURITY.md, scripts/**, docker/**, docs/adr/**, examples/flee…
  protected-script [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/tools/check-protected.sh --repo /tmp/dogfood-dryrun.J2isuA/clone 7d50660…
    > check-protected: 1 changed path(s) in 7d5066016dfcef611ed320d4d5d1843723620173..fleet/neo-roadmap/starter-docs-glossary, none protected (50 entries)
  lock-unchanged [file, mandatory] passed: the file's sha256 matches
    Cargo.lock has sha256 56faee101362dfa3c62dff6acc44eff1d45651ee956f365632096117202954db
  docs-claims [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/tools/check-docs-claims.sh --repo /tmp/dogfood-dryrun.J2isuA/clone 7d506…
    > check-docs-claims: 3 changed documentation line(s) in 7d5066016dfcef611ed320d4d5d1843723620173..fleet/neo-roadmap/starter-docs-glossary, no measured number among them
  conflict-markers [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/tools/check-conflict-markers.sh --tree (expect exit 0)
    > check-conflict-markers: clean (--tree)
  secret-scan [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/tools/check-secret-tree.sh (expect exit 0)
    > secret-scan: clean (--tree)
  fmt [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/container-run cargo fmt --all -- --check (expect exit 0)
  clippy [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/container-run cargo clippy --workspace --all-targets --locked --offline …
    >
    > --- stderr ---
    >     Checking fleet-domain v0.1.0 (/tmp/work/crates/fleet-domain)
    >     Checking neo-verify v0.1.0 (/tmp/work/crates/neo-verify)
    >     Checking neo-goals v0.1.0 (/tmp/work/crates/neo-goals)
    >     Checking fleet-app v0.1.0 (/tmp/work/crates/fleet-app)
    >     Checking fleet-infra v0.1.0 (/tmp/work/crates/fleet-infra)
    >     Checking fleet-cli v0.1.0 (/tmp/work/crates/fleet-cli)
    >     Finished `dev` profile [unoptimized + debuginfo] target(s) in 4.79s
  architecture [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/container-run cargo test --locked --offline -p fleet-cli --test architec…
    > ... (22 earlier lines not shown)
    > test result: ok. 26 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.16s
    >
    >
    > --- stderr ---
    >    Compiling fleet-domain v0.1.0 (/tmp/work/crates/fleet-domain)
    >    Compiling neo-verify v0.1.0 (/tmp/work/crates/neo-verify)
    >    Compiling neo-goals v0.1.0 (/tmp/work/crates/neo-goals)
    >    Compiling fleet-app v0.1.0 (/tmp/work/crates/fleet-app)
    >    Compiling fleet-infra v0.1.0 (/tmp/work/crates/fleet-infra)
    >    Compiling fleet-cli v0.1.0 (/tmp/work/crates/fleet-cli)
    >     Finished `test` profile [unoptimized + debuginfo] target(s) in 6.27s
    >      Running tests/architecture.rs (/target/debug/deps/architecture-8a11d801b1f90613)
  layers [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/container-run sh scripts/test-layers.sh (expect exit 0)
    > ... (3 earlier lines not shown)
    >
    > == application ==
    > application: 299 passed, 0 failed
    >
    > == adapters ==
    > adapters: 524 passed, 0 failed
    >
    > == soak ==
    > soak: 1 passed, 0 failed
    >
    > == end-to-end ==
    > end-to-end: 261 passed, 0 failed
  scope [diff, mandatory] passed: 1 changed path(s), all allowed
    changes only docs/domain.md
evidence history (newest first):
  EVIDENCE  CHECK             VERDICT  COMMIT        TASK                   SUMMARY
  e33       scope             passed   2a69749b3382  starter-docs-glossary  1 changed path(s), all allowed
  e32       layers            passed   2a69749b3382  starter-docs-glossary  exit code 0
  e31       architecture      passed   2a69749b3382  starter-docs-glossary  exit code 0
  e30       clippy            passed   2a69749b3382  starter-docs-glossary  exit code 0
  e29       fmt               passed   2a69749b3382  starter-docs-glossary  exit code 0
  e28       secret-scan       passed   2a69749b3382  starter-docs-glossary  exit code 0
  e27       conflict-markers  passed   2a69749b3382  starter-docs-glossary  exit code 0
  e26       docs-claims       passed   2a69749b3382  starter-docs-glossary  exit code 0
  e25       lock-unchanged    passed   2a69749b3382  starter-docs-glossary  the file's sha256 matches
  e24       protected-script  passed   2a69749b3382  starter-docs-glossary  exit code 0
  e23       protected-diff    passed   2a69749b3382  starter-docs-glossary  1 changed path(s), all allowed

$ /target/release/fleet goals show g-maint-docs-links
goal g-maint-docs-links [verified]
title:    Relative links and anchors in the docs resolve
parent:   m-maint
reason:   every mandatory check of the contract passed
intent:
  > Broken links and anchors in README.md and docs/ are fixed without touching any measured number
contract: passed (id a3221cb32d90)
tasks:
  neo-roadmap/maint-docs-links  completed  verification: verified
checks (newest evidence of each):
  protected-diff [diff, mandatory] passed: 4 changed path(s), all allowed
    changes nothing matching .github/**, SECURITY.md, scripts/**, docker/**, docs/adr/**, examples/flee…
  protected-script [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/tools/check-protected.sh --repo /tmp/dogfood-dryrun.J2isuA/clone 7d50660…
    > check-protected: 4 changed path(s) in 7d5066016dfcef611ed320d4d5d1843723620173..fleet/neo-roadmap/maint-docs-links, none protected (50 entries)
  lock-unchanged [file, mandatory] passed: the file's sha256 matches
    Cargo.lock has sha256 56faee101362dfa3c62dff6acc44eff1d45651ee956f365632096117202954db
  docs-claims [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/tools/check-docs-claims.sh --repo /tmp/dogfood-dryrun.J2isuA/clone 7d506…
    > check-docs-claims: 4 changed documentation line(s) in 7d5066016dfcef611ed320d4d5d1843723620173..fleet/neo-roadmap/maint-docs-links, no measured number among them
  conflict-markers [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/tools/check-conflict-markers.sh --tree (expect exit 0)
    > check-conflict-markers: clean (--tree)
  secret-scan [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/tools/check-secret-tree.sh (expect exit 0)
    > secret-scan: clean (--tree)
  fmt [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/container-run cargo fmt --all -- --check (expect exit 0)
  clippy [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/container-run cargo clippy --workspace --all-targets --locked --offline …
    >
    > --- stderr ---
    >     Checking fleet-domain v0.1.0 (/tmp/work/crates/fleet-domain)
    >     Checking neo-verify v0.1.0 (/tmp/work/crates/neo-verify)
    >     Checking neo-goals v0.1.0 (/tmp/work/crates/neo-goals)
    >     Checking fleet-app v0.1.0 (/tmp/work/crates/fleet-app)
    >     Checking fleet-infra v0.1.0 (/tmp/work/crates/fleet-infra)
    >     Checking fleet-cli v0.1.0 (/tmp/work/crates/fleet-cli)
    >     Finished `dev` profile [unoptimized + debuginfo] target(s) in 4.98s
  architecture [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/container-run cargo test --locked --offline -p fleet-cli --test architec…
    > ... (22 earlier lines not shown)
    > test result: ok. 26 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.07s
    >
    >
    > --- stderr ---
    >    Compiling fleet-domain v0.1.0 (/tmp/work/crates/fleet-domain)
    >    Compiling neo-verify v0.1.0 (/tmp/work/crates/neo-verify)
    >    Compiling neo-goals v0.1.0 (/tmp/work/crates/neo-goals)
    >    Compiling fleet-app v0.1.0 (/tmp/work/crates/fleet-app)
    >    Compiling fleet-infra v0.1.0 (/tmp/work/crates/fleet-infra)
    >    Compiling fleet-cli v0.1.0 (/tmp/work/crates/fleet-cli)
    >     Finished `test` profile [unoptimized + debuginfo] target(s) in 4.07s
    >      Running tests/architecture.rs (/target/debug/deps/architecture-8a11d801b1f90613)
  layers [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/container-run sh scripts/test-layers.sh (expect exit 0)
    > ... (3 earlier lines not shown)
    >
    > == application ==
    > application: 299 passed, 0 failed
    >
    > == adapters ==
    > adapters: 524 passed, 0 failed
    >
    > == soak ==
    > soak: 1 passed, 0 failed
    >
    > == end-to-end ==
    > end-to-end: 261 passed, 0 failed
  scope [diff, mandatory] passed: 4 changed path(s), all allowed
    changes only README.md, docs/**, crates/fleet-domain/src/error.rs, crates/fleet-domain/src/value.rs…
evidence history (newest first):
  EVIDENCE  CHECK             VERDICT  COMMIT        TASK              SUMMARY
  e55       scope             passed   59a2f86c02dd  maint-docs-links  4 changed path(s), all allowed
  e54       layers            passed   59a2f86c02dd  maint-docs-links  exit code 0
  e53       architecture      passed   59a2f86c02dd  maint-docs-links  exit code 0
  e52       clippy            passed   59a2f86c02dd  maint-docs-links  exit code 0
  e51       fmt               passed   59a2f86c02dd  maint-docs-links  exit code 0
  e50       secret-scan       passed   59a2f86c02dd  maint-docs-links  exit code 0
  e49       conflict-markers  passed   59a2f86c02dd  maint-docs-links  exit code 0
  e48       docs-claims       passed   59a2f86c02dd  maint-docs-links  exit code 0
  e47       lock-unchanged    passed   59a2f86c02dd  maint-docs-links  the file's sha256 matches
  e46       protected-script  passed   59a2f86c02dd  maint-docs-links  exit code 0
  e45       protected-diff    passed   59a2f86c02dd  maint-docs-links  4 changed path(s), all allowed

$ /target/release/fleet goals remaining
11 goals, 6 verified, 0 abandoned
open leaves:
  GOAL           STATUS  CONTRACT  TASKS                                      TITLE
  g-n3-scaffold  open    pending   neo-roadmap/n3-scaffold (failed, pending)  The neo-knowledge crate exists in the workspace
  g-n3-source    open    pending   neo-roadmap/n3-source (blocked, pending)   Fact sources and their confidence
  m-n4           open    none      -                                          N4: the token governor
  m-n5           open    none      -                                          N5: the credential broker
blockers:
  task neo-roadmap/n3-scaffold (failed) for goal g-n3-scaffold: worker exited with code 1
  task neo-roadmap/n3-source (blocked) for goal g-n3-source: dependency: dependency n3-scaffold did not complete
next ready tasks:
  none

$ the checks that failed for g-starter-error-tests (fleet goals show g-starter-error-tests --json)

$ the checks that failed for g-starter-value-property (fleet goals show g-starter-value-property --json)
  [scope @ 34d11f2f521d] FAILED: 1 changed path(s) outside the allowed globs
      "SECURITY.md"

$ fleet events --limit 200 | grep ... # every failed protected-path check of the run (the daemon keeps the last 200 events in memory: enough here)
#55 evidence e12: goal g-starter-value-property check "protected-diff" failed
#56 evidence e13: goal g-starter-value-property check "protected-script" failed

$ the worker that costs nothing: fleet tail n3-scaffold
task n3-scaffold is human-only: a person does this one, see dogfood/human-only.md

$ fleet verify r/task: run the contract again, by hand, on the task's current commit
verification of neo-roadmap/starter-docs-glossary: PASSED
task neo-roadmap/starter-docs-glossary: verified
goal g-starter-docs-glossary [verified] contract passed
  protected-diff [diff, mandatory] passed: 1 changed path(s), all allowed
    changes nothing matching .github/**, SECURITY.md, scripts/**, docker/**, docs/adr/**, examples/flee…
  protected-script [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/tools/check-protected.sh --repo /tmp/dogfood-dryrun.J2isuA/clone 7d50660…
    > check-protected: 1 changed path(s) in 7d5066016dfcef611ed320d4d5d1843723620173..fleet/neo-roadmap/starter-docs-glossary, none protected (50 entries)
  lock-unchanged [file, mandatory] passed: the file's sha256 matches
    Cargo.lock has sha256 56faee101362dfa3c62dff6acc44eff1d45651ee956f365632096117202954db
  docs-claims [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/tools/check-docs-claims.sh --repo /tmp/dogfood-dryrun.J2isuA/clone 7d506…
    > check-docs-claims: 3 changed documentation line(s) in 7d5066016dfcef611ed320d4d5d1843723620173..fleet/neo-roadmap/starter-docs-glossary, no measured number among them
  conflict-markers [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/tools/check-conflict-markers.sh --tree (expect exit 0)
    > check-conflict-markers: clean (--tree)
  secret-scan [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/tools/check-secret-tree.sh (expect exit 0)
    > secret-scan: clean (--tree)
  fmt [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/container-run cargo fmt --all -- --check (expect exit 0)
  clippy [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/container-run cargo clippy --workspace --all-targets --locked --offline …
    >
    > --- stderr ---
    >     Checking fleet-domain v0.1.0 (/tmp/work/crates/fleet-domain)
    >     Checking neo-verify v0.1.0 (/tmp/work/crates/neo-verify)
    >     Checking neo-goals v0.1.0 (/tmp/work/crates/neo-goals)
    >     Checking fleet-app v0.1.0 (/tmp/work/crates/fleet-app)
    >     Checking fleet-infra v0.1.0 (/tmp/work/crates/fleet-infra)
    >     Checking fleet-cli v0.1.0 (/tmp/work/crates/fleet-cli)
    >     Finished `dev` profile [unoptimized + debuginfo] target(s) in 5.99s
  architecture [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/container-run cargo test --locked --offline -p fleet-cli --test architec…
    > ... (23 earlier lines not shown)
    > test result: ok. 26 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.08s
    >
    >
    > --- stderr ---
    >    Compiling fleet-domain v0.1.0 (/tmp/work/crates/fleet-domain)
    >    Compiling neo-verify v0.1.0 (/tmp/work/crates/neo-verify)
    >    Compiling neo-goals v0.1.0 (/tmp/work/crates/neo-goals)
    >    Compiling fleet-app v0.1.0 (/tmp/work/crates/fleet-app)
    >    Compiling fleet-infra v0.1.0 (/tmp/work/crates/fleet-infra)
    >    Compiling fleet-cli v0.1.0 (/tmp/work/crates/fleet-cli)
    >     Finished `test` profile [unoptimized + debuginfo] target(s) in 3.77s
    >      Running tests/architecture.rs (/target/debug/deps/architecture-8a11d801b1f90613)
  layers [command, mandatory] passed: exit code 0
    /tmp/dogfood-dryrun.J2isuA/container-run sh scripts/test-layers.sh (expect exit 0)
    > ... (3 earlier lines not shown)
    >
    > == application ==
    > application: 299 passed, 0 failed
    >
    > == adapters ==
    > adapters: 524 passed, 0 failed
    >
    > == soak ==
    > soak: 1 passed, 0 failed
    >
    > == end-to-end ==
    > end-to-end: 261 passed, 0 failed
  scope [diff, mandatory] passed: 1 changed path(s), all allowed
    changes only docs/domain.md

$ fleet verify g-n3-scaffold   # the human-only contract; its task's worker failed, so there is no reviewed commit and no evidence can count
verification of g-n3-scaffold: NOT PASSED
task neo-roadmap/n3-scaffold: pending
note: the commit that was checked is not the one the last review recorded for the task: this evidence does not count
goal g-n3-scaffold [open] contract pending
  no-change-at-all [diff, mandatory] failed (not the reviewed commit: does not count): 3 changed path(s) match a denied glob
    changes nothing matching **
    > "crates/fleet-domain/src/error.rs" (denied)
    > "crates/fleet-domain/src/value.rs" (denied)
    > "docs/domain.md" (denied)
  never-exists [file, mandatory] failed (not the reviewed commit: does not count): the file does not exist
    dogfood/.human-only-this-file-never-exists exists
exit 6

$ git log --oneline 7d5066016dfcef611ed320d4d5d1843723620173..fleet/neo-roadmap/starter-value-property   # what the retry committed
39aa512 fleet: starter-value-property - Starter: a property test for the canonical form of fleet-domain Value
34d11f2 fleet: starter-value-property - Starter: a property test for the canonical form of fleet-domain Value

$ git diff --stat 7d5066016dfcef611ed320d4d5d1843723620173 fleet/neo-roadmap/starter-value-property
 crates/fleet-domain/src/value.rs | 86 ++++++++++++++++++++++++++++++++++++++++
 1 file changed, 86 insertions(+)

$ sh /tmp/dogfood-dryrun.J2isuA/tools/check-protected.sh --repo /tmp/dogfood-dryrun.J2isuA/clone 7d5066016dfcef611ed320d4d5d1843723620173..fleet/neo-roadmap/starter-value-property
check-protected: 1 changed path(s) in 7d5066016dfcef611ed320d4d5d1843723620173..fleet/neo-roadmap/starter-value-property, none protected (50 entries)

$ sh /tmp/dogfood-dryrun.J2isuA/tools/check-docs-claims.sh --repo /tmp/dogfood-dryrun.J2isuA/clone 7d5066016dfcef611ed320d4d5d1843723620173..fleet/neo-roadmap/starter-value-property
check-docs-claims: 0 changed documentation line(s) in 7d5066016dfcef611ed320d4d5d1843723620173..fleet/neo-roadmap/starter-value-property, no measured number among them

$ fleet merge starter-docs-glossary   # needs an approval bound to the exact commit
fleet: merging neo-roadmap/starter-docs-glossary needs approval a1 (digest 315d7f4cea51); approve it with `fleet approve a1` and run the merge again
run: fleet approve a1
exit 5

$ /target/release/fleet approval list
approval a1 [merge] neo-roadmap/starter-docs-glossary pending digest 315d7f4cea51
  git: "merge" "fleet/neo-roadmap/integration" "fleet/neo-roadmap/starter-docs-glossary" "2a69749b338259a54bd9e76ba6dd4b44af98cef2"
  decide: fleet approve a1 | fleet deny a1

$ /target/release/fleet approve a1
approval a1 approved

$ /target/release/fleet merge starter-docs-glossary
merged neo-roadmap/starter-docs-glossary (fleet/neo-roadmap/starter-docs-glossary) into fleet/neo-roadmap/integration at 84133fc3b8d7

$ /target/release/fleet verify-log
event log ok: 155 records, head ff1ac5ab1549bc48cf08e37705d5690f3d10a103e0bffe558df1ae9683a127f9

$ git status --short   # the clone's own branch and working tree were never touched
neo/dogfood

$ git log --format='%h %an <%ae> %s' 7d5066016dfcef611ed320d4d5d1843723620173..fleet/neo-roadmap/starter-docs-glossary   # who fleet says made the commits
2a69749 fleet <fleet@localhost> fleet: starter-docs-glossary - Starter: glossary entries for self-development in docs/domain.md

$ git -C /tmp/dogfood-dryrun.J2isuA/clone switch -q -c owner-branch 7d5066016dfcef611ed320d4d5d1843723620173

$ git cherry-pick -n 7d5066016dfcef611ed320d4d5d1843723620173..fleet/neo-roadmap/starter-docs-glossary   # applied to the index and the files, not committed

$ sh /tmp/dogfood-dryrun.J2isuA/tools/check-conflict-markers.sh --git /tmp/dogfood-dryrun.J2isuA/clone
check-conflict-markers: clean (--git)

$ git commit as the owner (the commit message is the owner's own)
7429b38 Owner Name <owner@example.invalid> Add the self-development terms to the glossary
7d50660 kienpt <kienphan.dev@gmail.com> Refresh the test counts and coverage in the README from a fresh run and name the new checks
exit=0
```

</details>
