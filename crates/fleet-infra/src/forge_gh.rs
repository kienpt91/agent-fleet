//! The GitHub adapter: `IssueTracker` and `CodeHost` over the system `gh` program, which is the
//! only transport. fleet never sees, stores or passes a token: signing in is `gh`'s own business
//! (its credential store), and the child gets a minimal environment in which no token variable
//! can exist.
//!
//! How `gh` is run, on every call:
//! * the program is an absolute path resolved once when the daemon starts, never searched for
//!   again, never a relative `PATH` entry, and refused when it lies inside a repository or a
//!   worktree (a worker could have planted it there);
//! * fixed argv, no shell: `gh api --hostname <host> --include --method <GET|POST> ... <endpoint>`
//!   where the endpoint is built from validated names and numbers only;
//! * the working directory is `/`, so `gh` finds no repository (and no repository configuration)
//!   to consult; the repository is always named in the endpoint;
//! * the environment holds `HOME`, `PATH` (absolute entries only), `GH_HOST` when the host is not
//!   `github.com`, two constants that keep `gh` from prompting or checking for updates, and the
//!   variables the operator named in `env_pass` (never a secret-looking one);
//! * a hard timeout, output capped at 2 MiB, its process group listed in the job tracker so that
//!   it and its descendants are refused on the control socket and the dashboard;
//! * the answer is split into status, headers and body (`response`), the body is parsed as JSON
//!   under a size cap and read through the strict schemas of `fleet-forge`.

use crate::jobs::{redact_block, run_bounded_tracked, trusted_program, JobTracker, RunSpec};
use fleet_app::forge::ports::{CodeHost, IssueTracker, ListQuery};
use fleet_domain::error::{ErrorCode, FleetError, Result};
use fleet_domain::ids::show;
use fleet_domain::quality::ExitKind;
use fleet_domain::redact::Redactor;
use fleet_domain::sanitize::{flatten_line, sanitize_text};
use fleet_domain::value::Value;
use fleet_forge::config::{ForgeConfig, SCRUBBED};
use fleet_forge::limits::MAX_RESPONSE_BYTES;
use fleet_forge::model::{
    CreatedPull, Issue, IssueSummary, Page, Pull, PullStatus, PullSummary, StateFilter,
};
use fleet_forge::publish::PullRequestDraft;
use fleet_forge::rate::{Admit, BlockWhy, RateConfig, RateGate, RateStatus};
use fleet_forge::repo::RepoRef;
use fleet_forge::response::{classify, parse_response, Failure};
use fleet_forge::schema::Reader;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

const API_VERSION: &str = "2022-11-28";
/// Pull requests and issues of one call: the page size of the commit endpoints.
const CHECKS_PER_PAGE: usize = 100;

/// Where `gh` was found, and what was passed over on the way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub program: PathBuf,
    /// `PATH` entries that were skipped because they were empty or relative.
    pub skipped_relative: usize,
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Finds `gh` once: the configured absolute path, or the first executable `gh` in an absolute
/// entry of `PATH`. The result is canonical (symlinks resolved), so what is checked later is what
/// runs.
pub fn resolve_gh(cfg: &ForgeConfig, parent_env: &[(String, String)]) -> Result<Resolved> {
    let mut skipped = 0usize;
    let candidate = match &cfg.gh {
        Some(path) => {
            let p = PathBuf::from(path);
            if !p.is_absolute() {
                return Err(FleetError::invalid("[github] gh must be an absolute path"));
            }
            Some(p)
        }
        None => {
            let path = parent_env
                .iter()
                .rev()
                .find(|(k, _)| k == "PATH")
                .map(|(_, v)| v.as_str())
                .unwrap_or("");
            let mut found = None;
            for dir in path.split(':') {
                let dir = Path::new(dir);
                if !dir.is_absolute() {
                    skipped += 1;
                    continue;
                }
                let c = dir.join("gh");
                if found.is_none() && is_executable_file(&c) {
                    found = Some(c);
                }
            }
            found
        }
    };
    let Some(candidate) = candidate else {
        return Err(FleetError::unavailable(
            "gh was not found in an absolute entry of PATH (install the GitHub CLI, or set [github] gh to its absolute path)",
        ));
    };
    let program = std::fs::canonicalize(&candidate).map_err(|e| {
        FleetError::unavailable(format!(
            "gh at {} cannot be resolved ({})",
            show(&candidate.to_string_lossy()),
            e.kind()
        ))
    })?;
    if !is_executable_file(&program) {
        return Err(FleetError::unavailable(format!(
            "{} is not an executable file",
            show(&program.to_string_lossy())
        )));
    }
    Ok(Resolved {
        program,
        skipped_relative: skipped,
    })
}

/// The whole environment of a `gh` child. `HOME` is required (`gh` keeps its configuration and
/// its credential store reachable through it); variables named in `SCRUBBED` can never be in it,
/// whatever the configuration says.
pub fn gh_env(cfg: &ForgeConfig, parent_env: &[(String, String)]) -> Result<Vec<(String, String)>> {
    let get = |name: &str| {
        parent_env
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };
    let home = get("HOME").filter(|h| h.starts_with('/')).ok_or_else(|| {
        FleetError::unavailable(
            "HOME is not set to an absolute path in the daemon's environment; gh needs it to find its sign-in",
        )
    })?;
    let path = get("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|d| Path::new(d).is_absolute())
        .collect::<Vec<_>>()
        .join(":");
    let mut env = vec![
        ("HOME".to_string(), home),
        ("PATH".to_string(), path),
        ("GH_PROMPT_DISABLED".to_string(), "1".to_string()),
        ("GH_NO_UPDATE_NOTIFIER".to_string(), "1".to_string()),
    ];
    if cfg.host != fleet_forge::config::DEFAULT_HOST {
        env.push(("GH_HOST".to_string(), cfg.host.clone()));
    }
    for name in &cfg.env_pass {
        if SCRUBBED.contains(&name.as_str()) {
            continue;
        }
        if let Some(v) = get(name) {
            env.push((name.clone(), v));
        }
    }
    env.sort();
    env.dedup_by(|a, b| a.0 == b.0);
    Ok(env)
}

/// Everything the adapter needs, resolved once.
pub struct GhSettings {
    pub program: PathBuf,
    pub host: String,
    pub env: Vec<(String, String)>,
    pub timeout_ms: u64,
    pub redactor: Arc<Redactor>,
    pub tracker: JobTracker,
    pub rate: RateConfig,
}

type Clock = Box<dyn Fn() -> u64 + Send + Sync>;
type Jitter = Box<dyn Fn() -> u32 + Send + Sync>;

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// A random number in 0..=1000 from the standard library's randomly keyed hasher.
fn random_jitter() -> u32 {
    let n = RandomState::new().build_hasher().finish() % 1001;
    u32::try_from(n).unwrap_or(0)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

enum Method {
    Get,
    Post(Vec<Field>),
}

/// A field of a POST: `Text` is sent literally (`-f`), `Flag` as a JSON boolean (`-F`).
enum Field {
    Text(&'static str, String),
    Flag(&'static str, bool),
}

struct Answer {
    body: Value,
    has_next: bool,
}

pub struct GhCli {
    s: GhSettings,
    gate: Arc<Mutex<RateGate>>,
    now: Clock,
    jitter: Jitter,
}

impl GhCli {
    pub fn new(s: GhSettings) -> GhCli {
        let gate = Arc::new(Mutex::new(RateGate::new(s.rate)));
        GhCli {
            s,
            gate,
            now: Box::new(epoch_ms),
            jitter: Box::new(random_jitter),
        }
    }

    pub fn with_clock(mut self, now: impl Fn() -> u64 + Send + Sync + 'static) -> GhCli {
        self.now = Box::new(now);
        self
    }

    pub fn with_jitter(mut self, jitter: impl Fn() -> u32 + Send + Sync + 'static) -> GhCli {
        self.jitter = Box::new(jitter);
        self
    }

    pub fn program(&self) -> &Path {
        &self.s.program
    }

    pub fn rate_status(&self) -> RateStatus {
        lock(&self.gate).status((self.now)())
    }

    /// Refuses a program that lies inside one of `repos` (or a worktree of it).
    pub fn check_trusted(&self, repos: &[String]) -> Result<()> {
        for repo in repos {
            trusted_program(&self.s.program, Path::new(repo)).map_err(|why| {
                FleetError::forbidden(format!(
                    "gh at {} is not trusted: {}",
                    show(&self.s.program.to_string_lossy()),
                    why.trim_start_matches("refused: ")
                ))
            })?;
        }
        Ok(())
    }

    /// `Err` with the reason when a call would be refused by the rate gate right now. Nothing is
    /// counted.
    pub fn check_rate(&self) -> Result<()> {
        let now = (self.now)();
        let st = lock(&self.gate).status(now);
        match st.state {
            "limited" | "budget" => Err(blocked_error(st.state, st.until_ms.unwrap_or(now), now)),
            _ => Ok(()),
        }
    }

    fn reader_clean(&self) -> impl Fn(&str) -> String + '_ {
        move |t: &str| redact_block(&self.s.redactor, &sanitize_text(t.as_bytes()))
    }

    fn argv(&self, method: &Method, endpoint: &str) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "api".into(),
            "--hostname".into(),
            self.s.host.clone(),
            "--include".into(),
            "--method".into(),
            match method {
                Method::Get => "GET".into(),
                Method::Post(_) => "POST".into(),
            },
            "-H".into(),
            "Accept: application/vnd.github+json".into(),
            "-H".into(),
            format!("X-GitHub-Api-Version: {API_VERSION}"),
        ];
        if let Method::Post(fields) = method {
            for f in fields {
                match f {
                    Field::Text(k, v) => {
                        a.push("-f".into());
                        a.push(format!("{k}={v}"));
                    }
                    Field::Flag(k, v) => {
                        a.push("-F".into());
                        a.push(format!("{k}={v}"));
                    }
                }
            }
        }
        a.push(endpoint.to_string());
        a
    }

    /// One `gh api` call, through the rate gate.
    fn call(&self, method: &Method, endpoint: &str) -> Result<Answer> {
        let now = (self.now)();
        if let Admit::Blocked { until_ms, why } = lock(&self.gate).admit(now) {
            let state = match why {
                BlockWhy::Limited => "limited",
                BlockWhy::Budget => "budget",
            };
            return Err(blocked_error(state, until_ms, now));
        }
        let result = run_bounded_tracked(
            &RunSpec {
                program: self.s.program.display().to_string(),
                args: self.argv(method, endpoint),
                cwd: PathBuf::from("/"),
                env: self.s.env.clone(),
                timeout_ms: self.s.timeout_ms,
                max_output: MAX_RESPONSE_BYTES,
            },
            &self.s.tracker,
        );
        let code = match &result.exit {
            ExitKind::Code(c) => Some(*c),
            ExitKind::SpawnFailed(why) => {
                return Err(FleetError::unavailable(format!(
                    "gh could not be started: {}",
                    flatten_line(why, 160)
                )))
            }
            ExitKind::TimedOut => {
                return Err(FleetError::new(
                    ErrorCode::Timeout,
                    format!(
                        "gh did not answer within {} s. Nothing was changed.",
                        self.s.timeout_ms / 1000
                    ),
                ))
            }
            ExitKind::OutputLimit => {
                return Err(FleetError::limit(format!(
                    "gh printed more than {MAX_RESPONSE_BYTES} bytes; the answer was cut off and not used"
                )))
            }
            ExitKind::Signal(n) => {
                return Err(FleetError::unavailable(format!("gh was killed by signal {n}")))
            }
        };
        let response = parse_response(&result.stdout)?;
        let rate = response.rate();
        let ok_status = response.status.is_none_or(|s| (200..300).contains(&s));
        if code == Some(0) && ok_status {
            lock(&self.gate).observe((self.now)(), &rate);
            let body = if response.body.iter().all(u8::is_ascii_whitespace) {
                Value::Null
            } else {
                crate::json::parse_value(response.body, MAX_RESPONSE_BYTES).map_err(|why| {
                    FleetError::invalid(format!(
                        "GitHub: gh did not answer with JSON ({}); the answer was not used",
                        flatten_line(&why, 120)
                    ))
                })?
            };
            return Ok(Answer {
                body,
                has_next: response.has_next_page(),
            });
        }
        // A failure: read the message of the error body (when there is one) and classify.
        let clean = self.reader_clean();
        let reader = Reader::new(&clean);
        let message = crate::json::parse_value(response.body, MAX_RESPONSE_BYTES)
            .map(|v| reader.error_message(&v))
            .unwrap_or_default();
        let stderr = sanitize_text(&result.stderr);
        let failure = classify(code, response.status, rate, &message, &stderr);
        if let Failure::RateLimited(h) = &failure {
            let now = (self.now)();
            let until = lock(&self.gate).limited(now, h, (self.jitter)());
            return Err(blocked_error("limited", until, now));
        }
        lock(&self.gate).observe((self.now)(), &rate);
        Err(failure.into_error())
    }

    fn get(&self, endpoint: &str) -> Result<Answer> {
        self.call(&Method::Get, endpoint)
    }
}

/// The error of a call the gate refused (or that the forge refused with a limit): says until
/// when, in seconds from now, and that nothing was changed.
fn blocked_error(state: &str, until_ms: u64, now_ms: u64) -> FleetError {
    let secs = until_ms.saturating_sub(now_ms).div_ceil(1000);
    let why = if state == "budget" {
        "fleet's own budget of GitHub requests for this hour is used up"
    } else {
        "GitHub said to slow down (rate limit)"
    };
    FleetError::unavailable(format!(
        "GitHub: {why}; no request is made for the next {secs} s. Nothing was changed."
    ))
}

fn state_word(s: StateFilter) -> &'static str {
    s.as_str()
}

fn list_endpoint(repo: &RepoRef, what: &str, q: &ListQuery) -> String {
    format!(
        "{}/{what}?state={}&per_page={}&page={}&sort=updated&direction=desc",
        repo.api_path(),
        state_word(q.state),
        q.per_page,
        q.page
    )
}

impl IssueTracker for GhCli {
    fn list_issues(&self, repo: &RepoRef, q: &ListQuery) -> Result<Page<IssueSummary>> {
        let a = self.get(&list_endpoint(repo, "issues", q))?;
        let clean = self.reader_clean();
        Reader::new(&clean).issue_page(&a.body, q.page, a.has_next)
    }

    fn get_issue(&self, repo: &RepoRef, number: u64) -> Result<Issue> {
        let a = self.get(&format!("{}/issues/{number}", repo.api_path()))?;
        let clean = self.reader_clean();
        Reader::new(&clean).issue(&a.body)
    }
}

impl CodeHost for GhCli {
    fn list_pulls(&self, repo: &RepoRef, q: &ListQuery) -> Result<Page<PullSummary>> {
        let a = self.get(&list_endpoint(repo, "pulls", q))?;
        let clean = self.reader_clean();
        Reader::new(&clean).pull_page(&a.body, q.page, a.has_next)
    }

    fn get_pull(&self, repo: &RepoRef, number: u64) -> Result<Pull> {
        let a = self.get(&format!("{}/pulls/{number}", repo.api_path()))?;
        let clean = self.reader_clean();
        Reader::new(&clean).pull(&a.body)
    }

    fn pull_status(&self, repo: &RepoRef, number: u64) -> Result<PullStatus> {
        let pull = self.get(&format!("{}/pulls/{number}", repo.api_path()))?;
        let clean = self.reader_clean();
        let reader = Reader::new(&clean);
        let head = reader.pull(&pull.body)?.summary.head_sha;
        let sha = head.as_str();
        let runs = self.get(&format!(
            "{}/commits/{sha}/check-runs?per_page={CHECKS_PER_PAGE}",
            repo.api_path()
        ))?;
        let status = self.get(&format!("{}/commits/{sha}/status", repo.api_path()))?;
        let reviews = self.get(&format!(
            "{}/pulls/{number}/reviews?per_page={CHECKS_PER_PAGE}",
            repo.api_path()
        ))?;
        reader.pull_status(&pull.body, &runs.body, Some(&status.body), &reviews.body)
    }

    fn create_pull(&self, draft: &PullRequestDraft) -> Result<CreatedPull> {
        draft.validate()?;
        let fields = vec![
            Field::Text("title", draft.title.clone()),
            Field::Text("body", draft.body.clone()),
            Field::Text("head", draft.head_branch.clone()),
            Field::Text("base", draft.base.clone()),
            Field::Flag("draft", draft.draft),
        ];
        let a = self.call(
            &Method::Post(fields),
            &format!("{}/pulls", draft.repo.api_path()),
        )?;
        let clean = self.reader_clean();
        Reader::new(&clean).created_pull(&a.body)
    }

    fn check_access(&self, repo: &RepoRef) -> Result<()> {
        self.get(&repo.api_path()).map(|_| ())
    }
}
