//! Optional undo support: a thin wrapper around the external `agent-undo` binary.
//!
//! `agent-undo` is a separate project and never a dependency. When it is not installed every
//! method fails with an "unavailable" error and the rest of fleet works unchanged. It runs
//! through [`run_bounded`]: no shell, an absolute canonical program path resolved once at
//! detection, `/` as working directory, an empty environment plus a small allowlist, a time
//! limit and a cap on captured output.

use crate::jobs::{resolve_program, run_bounded, trusted_program, RunResult, RunSpec};
use fleet_app::ports::UndoTool;
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::show;
use fleet_domain::quality::ExitKind;
use fleet_domain::sanitize::{head_str, sanitize_text};
use std::fs;
use std::path::{Path, PathBuf};

const PROGRAM: &str = "agent-undo";
const TIMEOUT_MS: u64 = 60_000;
const MAX_OUTPUT: usize = 256 * 1024;
/// Longest text returned to the caller.
const MAX_TEXT_CHARS: usize = 400;
/// Bytes of stderr looked at when building an error message.
const STDERR_EXCERPT_BYTES: usize = 2048;
/// Variables of the daemon that the tool may see; everything else is dropped.
const ENV_ALLOWLIST: [&str; 5] = ["PATH", "HOME", "LANG", "TMPDIR", "USER"];
/// Exit code of `agent-undo` when the worktree has no history yet.
const EXIT_NOT_INITIALIZED: i32 = 6;
/// Exit code of an undo that was refused because of conflicts.
const EXIT_REFUSED: i32 = 3;

pub struct AgentUndo {
    program: Option<PathBuf>,
    env: Vec<(String, String)>,
}

impl AgentUndo {
    /// Looks for `agent-undo` in the `PATH`-style string; the tool runs with that same `PATH`.
    pub fn detect(path_env: Option<&str>) -> AgentUndo {
        AgentUndo::detect_with_env(path_env, Vec::new())
    }

    /// Like [`AgentUndo::detect`]; `env` is the daemon's environment, of which only the allowlisted
    /// variables (`HOME`, `LANG`, `TMPDIR`, `USER`) are passed on. `PATH` is always `path_env`.
    pub fn detect_with_env(path_env: Option<&str>, env: Vec<(String, String)>) -> AgentUndo {
        let mut child_env: Vec<(String, String)> = env
            .into_iter()
            .filter(|(k, _)| k != "PATH" && ENV_ALLOWLIST.contains(&k.as_str()))
            .collect();
        if let Some(path) = path_env {
            child_env.push(("PATH".to_string(), path.to_string()));
        }
        AgentUndo {
            program: path_env.and_then(find_in_path),
            env: child_env,
        }
    }

    /// Uses the given executable; for tests. It is canonicalized like a detected one.
    pub fn with_program(program: PathBuf) -> AgentUndo {
        AgentUndo {
            program: Some(fs::canonicalize(&program).unwrap_or(program)),
            env: Vec::new(),
        }
    }

    fn program(&self) -> Result<&Path> {
        self.program.as_deref().ok_or_else(|| {
            FleetError::unavailable("agent-undo is not installed (not found on PATH)")
        })
    }

    /// Runs the tool on `worktree`. The binary was resolved once at detection; it is checked
    /// again on every call, because it could have been replaced or linked since, and is never
    /// run when it lies inside the worktree or its repository, where the worker controls the
    /// contents. The tool runs in `/` and is pointed at the worktree with `-C`, so nothing in
    /// the worktree is looked up as a relative path.
    fn exec(&self, program: &Path, worktree: &str, tail: &[&str]) -> Result<RunResult> {
        let wt = Path::new(worktree);
        if !wt.is_dir() {
            return Err(FleetError::not_found(format!(
                "worktree {} does not exist",
                show(worktree)
            )));
        }
        let program = trusted_program(program, wt)
            .map_err(|why| FleetError::forbidden(format!("agent-undo was not run: {why}")))?;
        let mut args = vec!["-C".to_string(), worktree.to_string(), "--json".to_string()];
        args.extend(tail.iter().map(|s| s.to_string()));
        Ok(run_bounded(&RunSpec {
            program: program.display().to_string(),
            args,
            cwd: PathBuf::from("/"),
            env: self.env.clone(),
            timeout_ms: TIMEOUT_MS,
            max_output: MAX_OUTPUT,
        }))
    }
}

/// First executable regular file named `agent-undo` in the absolute entries of `path`, as its
/// canonical path. Relative entries are ignored: they would mean something different in the
/// daemon and in the worktree, and `resolve_program` skips them as well.
fn find_in_path(path: &str) -> Option<PathBuf> {
    let env = [("PATH".to_string(), path.to_string())];
    let found = resolve_program(PROGRAM, Path::new("/"), &env)?;
    fs::canonicalize(found).ok()
}

fn clip_chars(text: &str) -> String {
    text.trim().chars().take(MAX_TEXT_CHARS).collect()
}

/// Compact JSON clipped to 400 characters, or the sanitized start of other output.
fn short_text(stdout: &[u8]) -> String {
    match crate::json::parse_value(stdout, MAX_OUTPUT) {
        Ok(value) => clip_chars(&crate::json::to_line(&value)),
        Err(_) => {
            let (head, _) = stdout.split_at(stdout.len().min(MAX_TEXT_CHARS * 4));
            clip_chars(&sanitize_text(head))
        }
    }
}

fn stderr_excerpt(stderr: &[u8]) -> String {
    let (head, _) = stderr.split_at(stderr.len().min(STDERR_EXCERPT_BYTES));
    clip_chars(head_str(&sanitize_text(head), STDERR_EXCERPT_BYTES))
}

/// Maps a finished run to the text for the caller. `refusal_ok` marks an exit with the
/// "conflicts" code as a normal answer.
fn interpret(what: &str, result: &RunResult, refusal_ok: bool) -> Result<String> {
    match &result.exit {
        ExitKind::Code(0) => Ok(short_text(&result.stdout)),
        ExitKind::Code(EXIT_REFUSED) if refusal_ok => {
            Ok(format!("refused: {}", short_text(&result.stdout)))
        }
        ExitKind::Code(n) => Err(FleetError::internal(format!(
            "agent-undo {what} failed with exit code {n}: {}",
            stderr_excerpt(&result.stderr)
        ))),
        ExitKind::Signal(n) => Err(FleetError::internal(format!(
            "agent-undo {what} was killed by signal {n}"
        ))),
        ExitKind::TimedOut => Err(FleetError::internal(format!("agent-undo {what} timed out"))),
        ExitKind::OutputLimit => Err(FleetError::internal(format!(
            "agent-undo {what} printed more output than allowed"
        ))),
        ExitKind::SpawnFailed(why) => Err(FleetError::internal(format!(
            "agent-undo {what} could not start: {}",
            clip_chars(&sanitize_text(why.as_bytes()))
        ))),
    }
}

impl UndoTool for AgentUndo {
    fn available(&self) -> bool {
        self.program.is_some()
    }

    fn checkpoint(&self, worktree: &str, label: &str) -> Result<String> {
        let program = self.program()?;
        let checkpoint = ["checkpoint", "--label", label];
        let mut result = self.exec(program, worktree, &checkpoint)?;
        if result.exit == ExitKind::Code(EXIT_NOT_INITIALIZED) {
            // First checkpoint of this worktree: set the history up once, then try again.
            let init = self.exec(program, worktree, &["init"])?;
            interpret("init", &init, false)?;
            result = self.exec(program, worktree, &checkpoint)?;
        }
        interpret("checkpoint", &result, false)
    }

    fn undo(&self, worktree: &str, apply: bool) -> Result<String> {
        let program = self.program()?;
        let mut tail = vec!["undo", "--last"];
        if apply {
            tail.push("--apply");
        }
        let result = self.exec(program, worktree, &tail)?;
        interpret("undo", &result, !apply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use fleet_domain::error::ErrorCode;

    /// Installs a fake `agent-undo`. The script logs one invocation per block: a `---` line and
    /// then one argument per line, and then runs `body` (which may use `$4`, the subcommand).
    fn fake(dir: &TempDir, body: &str) -> PathBuf {
        let log = dir.join("argv.log");
        let text = format!(
            "#!/bin/sh\nLOG='{}'\necho '---' >> \"$LOG\"\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> \"$LOG\"; done\n{body}\n",
            log.display()
        );
        dir.script("bin/agent-undo", &text)
    }

    fn worktree(dir: &TempDir) -> String {
        dir.write("wt/.keep", "");
        dir.join("wt").display().to_string()
    }

    /// Invocations recorded by the fake, each as its list of arguments.
    fn calls(dir: &TempDir) -> Vec<Vec<String>> {
        let mut out: Vec<Vec<String>> = Vec::new();
        for line in dir.read("argv.log").lines() {
            if line == "---" {
                out.push(Vec::new());
            } else if let Some(last) = out.last_mut() {
                last.push(line.to_string());
            }
        }
        out
    }

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ---- detection -------------------------------------------------------------------------

    #[test]
    fn detect_finds_the_tool_on_a_path_string() {
        let dir = TempDir::new("undo-detect");
        fake(&dir, "exit 0");
        let bin = dir.join("bin").display().to_string();
        assert!(AgentUndo::detect(Some(&bin)).available());
        assert!(AgentUndo::detect(Some(&format!("/nonexistent-dir:{bin}"))).available());
    }

    #[test]
    fn detect_reports_unavailable_without_the_tool() {
        let dir = TempDir::new("undo-detect-none");
        dir.write("empty/.keep", "");
        let empty = dir.join("empty").display().to_string();
        assert!(!AgentUndo::detect(None).available());
        assert!(!AgentUndo::detect(Some("")).available());
        assert!(!AgentUndo::detect(Some(&empty)).available());
        assert!(!AgentUndo::detect(Some("/nonexistent-dir")).available());
    }

    #[test]
    fn detect_skips_files_that_are_not_executable_and_relative_entries() {
        let dir = TempDir::new("undo-detect-bad");
        dir.write("noexec/agent-undo", "#!/bin/sh\n");
        dir.script("rel/agent-undo", "#!/bin/sh\n");
        std::fs::create_dir_all(dir.join("dirs/agent-undo")).unwrap();
        let noexec = dir.join("noexec").display().to_string();
        let dirs = dir.join("dirs").display().to_string();
        assert!(!AgentUndo::detect(Some(&noexec)).available());
        assert!(!AgentUndo::detect(Some(&dirs)).available());
        assert!(!AgentUndo::detect(Some("rel")).available());
        assert!(!AgentUndo::detect(Some(".")).available());
        // A later usable entry is still found.
        let both = format!("{noexec}:{}", dir.join("rel").display());
        assert!(AgentUndo::detect(Some(&both)).available());
    }

    #[test]
    fn every_method_fails_as_unavailable_when_the_tool_is_missing() {
        let dir = TempDir::new("undo-missing");
        let wt = worktree(&dir);
        let tool = AgentUndo::detect(None);
        let want = "agent-undo is not installed (not found on PATH)";
        for result in [
            tool.checkpoint(&wt, "x"),
            tool.undo(&wt, false),
            tool.undo(&wt, true),
        ] {
            let err = result.unwrap_err();
            assert_eq!(err.code, ErrorCode::Unavailable);
            assert_eq!(err.message, want);
        }
    }

    // ---- commands --------------------------------------------------------------------------

    #[test]
    fn checkpoint_passes_exact_arguments_and_returns_compact_json() {
        let dir = TempDir::new("undo-checkpoint");
        let program = fake(&dir, "echo '{ \"id\": \"c1\",\n  \"files\": 3 }'");
        let wt = worktree(&dir);
        let tool = AgentUndo::with_program(program);
        assert!(tool.available());
        let text = tool.checkpoint(&wt, "x").unwrap();
        assert_eq!(text, r#"{"files":3,"id":"c1"}"#);
        assert_eq!(
            calls(&dir),
            vec![strs(&["-C", &wt, "--json", "checkpoint", "--label", "x"])]
        );
    }

    #[test]
    fn the_worktree_is_passed_with_dash_c() {
        let dir = TempDir::new("undo-dash-c");
        let program = fake(&dir, "echo '{}'");
        let wt = worktree(&dir);
        AgentUndo::with_program(program)
            .checkpoint(&wt, "x")
            .unwrap();
        assert_eq!(calls(&dir)[0][..2], strs(&["-C", &wt])[..]);
    }

    #[test]
    fn not_initialized_triggers_init_and_one_retry() {
        let dir = TempDir::new("undo-init");
        let marker = dir.join("inited");
        let body = format!(
            "case \"$4\" in\n init) : > '{m}'; echo '{{\"initialized\":true}}' ;;\n checkpoint) if [ ! -e '{m}' ]; then echo 'not initialized' >&2; exit 6; fi; echo '{{\"id\":\"c2\"}}' ;;\nesac",
            m = marker.display()
        );
        let program = fake(&dir, &body);
        let wt = worktree(&dir);
        let tool = AgentUndo::with_program(program);
        assert_eq!(tool.checkpoint(&wt, "first").unwrap(), r#"{"id":"c2"}"#);
        let seen: Vec<String> = calls(&dir).iter().map(|c| c[3].clone()).collect();
        assert_eq!(seen, ["checkpoint", "init", "checkpoint"]);
        assert_eq!(calls(&dir)[1], strs(&["-C", &wt, "--json", "init"]));
        // Once initialized, a later checkpoint needs no init.
        tool.checkpoint(&wt, "second").unwrap();
        assert_eq!(calls(&dir).len(), 4);
    }

    #[test]
    fn a_checkpoint_that_still_reports_not_initialized_fails_after_one_retry() {
        let dir = TempDir::new("undo-init-loop");
        let program = fake(
            &dir,
            "case \"$4\" in init) echo '{}' ;; *) echo 'no history' >&2; exit 6 ;; esac",
        );
        let wt = worktree(&dir);
        let err = AgentUndo::with_program(program)
            .checkpoint(&wt, "x")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Internal);
        assert!(err.message.contains("exit code 6"), "{}", err.message);
        assert_eq!(calls(&dir).len(), 3, "no further retries");
    }

    #[test]
    fn a_failing_init_is_reported_and_the_checkpoint_is_not_retried() {
        let dir = TempDir::new("undo-init-fails");
        let program = fake(
            &dir,
            "case \"$4\" in init) echo 'cannot init here' >&2; exit 1 ;; *) exit 6 ;; esac",
        );
        let wt = worktree(&dir);
        let err = AgentUndo::with_program(program)
            .checkpoint(&wt, "x")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Internal);
        assert!(err.message.contains("init") && err.message.contains("cannot init here"));
        assert_eq!(calls(&dir).len(), 2);
    }

    #[test]
    fn undo_preview_and_apply_use_the_right_flags() {
        let dir = TempDir::new("undo-args");
        let program = fake(&dir, "echo '{\"would_restore\":2}'");
        let wt = worktree(&dir);
        let tool = AgentUndo::with_program(program);
        assert_eq!(tool.undo(&wt, false).unwrap(), r#"{"would_restore":2}"#);
        tool.undo(&wt, true).unwrap();
        assert_eq!(
            calls(&dir),
            vec![
                strs(&["-C", &wt, "--json", "undo", "--last"]),
                strs(&["-C", &wt, "--json", "undo", "--last", "--apply"]),
            ]
        );
    }

    #[test]
    fn a_refused_preview_is_an_ok_with_a_prefix_but_a_refused_apply_is_an_error() {
        let dir = TempDir::new("undo-refused");
        let program = fake(&dir, "echo '{\"conflicts\":[\"a.rs\"]}'; exit 3");
        let wt = worktree(&dir);
        let tool = AgentUndo::with_program(program);
        let preview = tool.undo(&wt, false).unwrap();
        assert_eq!(preview, r#"refused: {"conflicts":["a.rs"]}"#);
        let apply = tool.undo(&wt, true).unwrap_err();
        assert_eq!(apply.code, ErrorCode::Internal);
        assert!(apply.message.contains("exit code 3"));
    }

    #[test]
    fn failures_carry_a_sanitized_stderr_excerpt() {
        let dir = TempDir::new("undo-fail");
        let program = fake(
            &dir,
            "printf 'bad \\033]52;c;aGk=\\007thing\\n' >&2; exit 1",
        );
        let wt = worktree(&dir);
        let tool = AgentUndo::with_program(program);
        for err in [
            tool.checkpoint(&wt, "x").unwrap_err(),
            tool.undo(&wt, false).unwrap_err(),
            tool.undo(&wt, true).unwrap_err(),
        ] {
            assert_eq!(err.code, ErrorCode::Internal);
            assert!(err.message.contains("exit code 1"), "{}", err.message);
            assert!(err.message.contains("thing"));
            assert!(!err.message.contains('\u{1b}') && !err.message.contains('\u{7}'));
        }
    }

    #[test]
    fn a_tool_killed_by_a_signal_is_an_internal_error() {
        let dir = TempDir::new("undo-signal");
        let program = fake(&dir, "kill -9 $$");
        let wt = worktree(&dir);
        let err = AgentUndo::with_program(program)
            .undo(&wt, false)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Internal);
        assert!(err.message.contains("signal 9"));
    }

    #[test]
    fn a_missing_worktree_is_an_error_not_a_panic() {
        let dir = TempDir::new("undo-no-wt");
        let program = fake(&dir, "exit 0");
        let wt = dir.join("does-not-exist").display().to_string();
        let err = AgentUndo::with_program(program)
            .checkpoint(&wt, "x")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        assert!(err.message.contains("does not exist"), "{}", err.message);
        assert!(
            !dir.exists("argv.log"),
            "the tool must not run without a worktree"
        );
    }

    // ---- output shaping --------------------------------------------------------------------

    #[test]
    fn long_json_is_clipped_to_400_characters() {
        let dir = TempDir::new("undo-long-json");
        let program = fake(
            &dir,
            "printf '{\"note\":\"'; i=0; while [ $i -lt 300 ]; do printf 'abcd'; i=$((i+1)); done; printf '\"}\\n'",
        );
        let wt = worktree(&dir);
        let text = AgentUndo::with_program(program)
            .checkpoint(&wt, "x")
            .unwrap();
        assert_eq!(text.chars().count(), 400);
        assert!(text.starts_with(r#"{"note":"abcd"#));
    }

    #[test]
    fn non_json_output_is_sanitized_and_clipped() {
        let dir = TempDir::new("undo-plain");
        let program = fake(
            &dir,
            "printf 'plain \\033[31mred\\033[0m text '; i=0; while [ $i -lt 200 ]; do printf 'zzzzzz'; i=$((i+1)); done",
        );
        let wt = worktree(&dir);
        let text = AgentUndo::with_program(program)
            .checkpoint(&wt, "x")
            .unwrap();
        assert!(text.starts_with("plain "), "{text}");
        assert!(!text.contains('\u{1b}'));
        assert!(text.chars().count() <= 400);
        let empty = fake(&dir, "exit 0");
        assert_eq!(
            AgentUndo::with_program(empty).checkpoint(&wt, "x").unwrap(),
            ""
        );
    }

    // ---- hostile input ---------------------------------------------------------------------

    #[test]
    fn hostile_label_and_worktree_stay_single_arguments() {
        let dir = TempDir::new("undo-hostile");
        let program = fake(&dir, "echo '{}'");
        let wt_dir = dir.join("w t 'q' $HOME `x`");
        std::fs::create_dir_all(&wt_dir).unwrap();
        let wt = wt_dir.display().to_string();
        let label = "a b; touch pwned $(touch pwned2) `touch pwned3` 'q' \"d\" --apply";
        AgentUndo::with_program(program)
            .checkpoint(&wt, label)
            .unwrap();
        assert_eq!(
            calls(&dir),
            vec![strs(&["-C", &wt, "--json", "checkpoint", "--label", label])]
        );
        for name in ["pwned", "pwned2", "pwned3"] {
            assert!(!dir.exists(name) && !wt_dir.join(name).exists());
        }
    }

    // ---- environment -----------------------------------------------------------------------

    #[test]
    fn the_tool_sees_only_the_allowlisted_environment() {
        let dir = TempDir::new("undo-env");
        let env_file = dir.join("env.txt");
        fake(
            &dir,
            &format!("env | sort > '{}'; echo '{{}}'", env_file.display()),
        );
        let wt = worktree(&dir);
        let path = format!("{}:/usr/bin:/bin", dir.join("bin").display());
        let daemon_env = vec![
            ("HOME".to_string(), "/home/someone".to_string()),
            ("LANG".to_string(), "C".to_string()),
            ("USER".to_string(), "someone".to_string()),
            ("TMPDIR".to_string(), "/tmp".to_string()),
            ("PATH".to_string(), "/daemon/only".to_string()),
            (
                "FLEET_SENTINEL_TOKEN".to_string(),
                "do-not-leak".to_string(),
            ),
            ("SSH_AUTH_SOCK".to_string(), "/tmp/agent.sock".to_string()),
        ];
        let tool = AgentUndo::detect_with_env(Some(&path), daemon_env);
        assert!(tool.available());
        tool.checkpoint(&wt, "x").unwrap();
        let seen = dir.read("env.txt");
        for want in [
            "HOME=/home/someone",
            "LANG=C",
            "USER=someone",
            "TMPDIR=/tmp",
            &format!("PATH={path}"),
        ] {
            assert!(seen.lines().any(|l| l == want), "missing {want} in {seen}");
        }
        assert!(!seen.contains("FLEET_SENTINEL_TOKEN") && !seen.contains("do-not-leak"));
        assert!(!seen.contains("SSH_AUTH_SOCK") && !seen.contains("/daemon/only"));
    }

    // ---- trust in the binary ---------------------------------------------------------------

    /// A fake that records that it ran and where, placed at `rel` below the temp dir.
    fn marker_fake(dir: &TempDir, rel: &str) -> PathBuf {
        let ran = dir.join("ran.txt");
        dir.script(
            rel,
            &format!("#!/bin/sh\npwd >> '{}'\necho '{{}}'\n", ran.display()),
        )
    }

    /// `<tmp>/repo/.fleet-worktrees/r1/t1` as the task's worktree.
    fn fleet_worktree(dir: &TempDir) -> String {
        dir.write("repo/.fleet-worktrees/r1/t1/.keep", "");
        dir.write("repo/tools/.keep", "");
        dir.join("repo/.fleet-worktrees/r1/t1")
            .display()
            .to_string()
    }

    #[test]
    fn the_tool_runs_in_the_root_directory_with_an_absolute_program() {
        let dir = TempDir::new("undo-cwd");
        let program = marker_fake(&dir, "bin/agent-undo");
        let wt = worktree(&dir);
        AgentUndo::with_program(program)
            .checkpoint(&wt, "x")
            .unwrap();
        assert_eq!(dir.read("ran.txt").trim(), "/");
        assert!(!dir.exists("wt/ran.txt"));
    }

    #[test]
    fn a_binary_inside_the_worktree_is_refused_at_every_call() {
        let dir = TempDir::new("undo-in-wt");
        let wt = fleet_worktree(&dir);
        let planted = marker_fake(&dir, "repo/.fleet-worktrees/r1/t1/agent-undo");
        let tool = AgentUndo::with_program(planted);
        assert!(tool.available());
        for result in [
            tool.checkpoint(&wt, "x"),
            tool.undo(&wt, false),
            tool.undo(&wt, true),
        ] {
            let err = result.unwrap_err();
            assert_eq!(err.code, ErrorCode::Forbidden);
            assert!(err.message.contains("refused"), "{}", err.message);
        }
        assert!(!dir.exists("ran.txt"), "the planted binary must not run");
    }

    #[test]
    fn a_binary_inside_the_repository_is_refused() {
        let dir = TempDir::new("undo-in-repo");
        let wt = fleet_worktree(&dir);
        let in_repo = marker_fake(&dir, "repo/tools/agent-undo");
        let err = AgentUndo::with_program(in_repo)
            .checkpoint(&wt, "x")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        // A sibling task's worktree is part of the repository as well.
        let sibling = marker_fake(&dir, "repo/.fleet-worktrees/r1/t2/agent-undo");
        assert!(AgentUndo::with_program(sibling).undo(&wt, false).is_err());
        assert!(!dir.exists("ran.txt"));
    }

    #[test]
    fn a_link_from_a_trusted_place_into_the_repository_is_refused() {
        let dir = TempDir::new("undo-link");
        let wt = fleet_worktree(&dir);
        let target = marker_fake(&dir, "repo/tools/agent-undo");
        dir.write("trusted/.keep", "");
        std::os::unix::fs::symlink(&target, dir.join("trusted/agent-undo")).unwrap();
        // Detection follows the link to the canonical path, which is what is judged.
        let trusted = dir.join("trusted").display().to_string();
        let tool = AgentUndo::detect(Some(&trusted));
        assert!(tool.available());
        let err = tool.checkpoint(&wt, "x").unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        assert!(!dir.exists("ran.txt"));
    }

    #[test]
    fn a_binary_outside_the_repository_runs() {
        let dir = TempDir::new("undo-legit");
        let wt = fleet_worktree(&dir);
        marker_fake(&dir, "elsewhere/agent-undo");
        let path = dir.join("elsewhere").display().to_string();
        let tool = AgentUndo::detect(Some(&path));
        assert_eq!(tool.checkpoint(&wt, "x").unwrap(), "{}");
        assert_eq!(dir.read("ran.txt").trim(), "/");
    }

    #[test]
    fn relative_path_entries_are_never_searched() {
        let dir = TempDir::new("undo-relative");
        marker_fake(&dir, "bin/agent-undo");
        // The same directory spelled relative to `/`, where the daemon's relative entries would
        // be resolved by a careless lookup.
        let abs = dir.join("bin");
        let rel = abs.strip_prefix("/").unwrap().display().to_string();
        assert!(!AgentUndo::detect(Some(&rel)).available());
        assert!(!AgentUndo::detect(Some(&format!(".:{rel}"))).available());
        assert!(AgentUndo::detect(Some(&format!(".:{}", abs.display()))).available());
    }

    #[test]
    fn the_binary_is_resolved_once_and_not_looked_up_again() {
        let dir = TempDir::new("undo-once");
        let wt = worktree(&dir);
        let late = dir.join("late");
        let path = format!("{}:{}", late.display(), dir.join("bin").display());
        let first = dir.join("first.txt");
        dir.script(
            "bin/agent-undo",
            &format!(
                "#!/bin/sh\necho first >> '{}'\necho '{{}}'\n",
                first.display()
            ),
        );
        let tool = AgentUndo::detect(Some(&path));
        // A binary that appears earlier on the PATH after detection must not be picked up.
        dir.script(
            "late/agent-undo",
            &format!(
                "#!/bin/sh\necho late >> '{}'\necho '{{}}'\n",
                dir.join("late.txt").display()
            ),
        );
        tool.checkpoint(&wt, "x").unwrap();
        assert!(dir.exists("first.txt"));
        assert!(!dir.exists("late.txt"));
    }
}
