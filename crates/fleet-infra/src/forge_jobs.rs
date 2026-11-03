//! Runs forge requests on plain threads, off the orchestrator's thread, through the real adapters:
//! `GhCli` for issues and pull requests and the git publisher for the push. One request is one
//! thread; the number running at once is capped, and a request that the rate gate would refuse is
//! refused here, before a thread is spawned.

use crate::forge_gh::GhCli;
use fleet_app::forge::exec;
use fleet_app::forge::ports::{BranchPublisher, ForgeAnswer, ForgeJobs, ForgeRequest, ForgeStatus};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::value::Value;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread;

/// Forge requests running at once.
pub const MAX_CONCURRENT_FORGE: usize = 4;

pub struct GhForgeJobs {
    gh: Arc<GhCli>,
    publisher: Arc<dyn BranchPublisher>,
    note: String,
    tx: Sender<(u64, Result<ForgeAnswer>)>,
    rx: Receiver<(u64, Result<ForgeAnswer>)>,
    running: Arc<AtomicUsize>,
}

/// Decrements the running counter when the thread ends, however it ends.
struct Running(Arc<AtomicUsize>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl GhForgeJobs {
    /// `note` is shown with the status (for example the relative `PATH` entries that were
    /// ignored).
    pub fn new(gh: Arc<GhCli>, publisher: Arc<dyn BranchPublisher>, note: String) -> GhForgeJobs {
        let (tx, rx) = channel();
        GhForgeJobs {
            gh,
            publisher,
            note,
            tx,
            rx,
            running: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl ForgeJobs for GhForgeJobs {
    fn status(&self, repos: &[String]) -> ForgeStatus {
        let rate = self.gh.rate_status().to_value();
        match self.gh.check_trusted(repos) {
            Ok(()) => ForgeStatus {
                ready: true,
                detail: format!("gh at {}{}", self.gh.program().display(), self.note),
                rate,
            },
            Err(e) => ForgeStatus {
                ready: false,
                detail: e.message,
                rate,
            },
        }
    }

    fn start(&mut self, ticket: u64, req: ForgeRequest, repos: &[String]) -> Result<()> {
        self.gh.check_trusted(repos)?;
        self.gh.check_rate()?;
        if self.running.load(Ordering::SeqCst) >= MAX_CONCURRENT_FORGE {
            return Err(FleetError::limit(format!(
                "{MAX_CONCURRENT_FORGE} GitHub requests are running; wait for one to finish"
            )));
        }
        self.running.fetch_add(1, Ordering::SeqCst);
        let guard = Running(Arc::clone(&self.running));
        let gh = Arc::clone(&self.gh);
        let publisher = Arc::clone(&self.publisher);
        let tx = self.tx.clone();
        let spawned = thread::Builder::new()
            .name("fleet-forge".to_string())
            .spawn(move || {
                let _running = guard;
                let result = catch_unwind(AssertUnwindSafe(|| {
                    exec::run(&req, gh.as_ref(), gh.as_ref(), publisher.as_ref())
                }))
                .unwrap_or_else(|_| Err(FleetError::internal("the GitHub request panicked")));
                let _ = tx.send((ticket, result));
            });
        if let Err(e) = spawned {
            // the guard moved into the closure that never ran: it was dropped with it
            return Err(FleetError::io(
                "could not start a GitHub request thread",
                &e,
            ));
        }
        Ok(())
    }

    fn poll(&mut self) -> Vec<(u64, Result<ForgeAnswer>)> {
        self.rx.try_iter().collect()
    }

    fn running(&self) -> usize {
        self.running.load(Ordering::SeqCst)
    }

    fn remote_url(&self, repo_path: &str, remote: &str) -> Result<String> {
        self.publisher.remote_url(repo_path, remote)
    }
}

/// The adapter of a daemon where the integration is on but `gh` cannot be used: every request is
/// refused with the reason, and nothing else is affected.
pub struct UnavailableForge {
    reason: String,
}

impl UnavailableForge {
    pub fn new(reason: impl Into<String>) -> UnavailableForge {
        UnavailableForge {
            reason: reason.into(),
        }
    }
}

impl ForgeJobs for UnavailableForge {
    fn status(&self, _repos: &[String]) -> ForgeStatus {
        ForgeStatus {
            ready: false,
            detail: self.reason.clone(),
            rate: Value::Null,
        }
    }

    fn start(&mut self, _ticket: u64, _req: ForgeRequest, _repos: &[String]) -> Result<()> {
        Err(FleetError::unavailable(self.reason.clone()))
    }

    fn poll(&mut self) -> Vec<(u64, Result<ForgeAnswer>)> {
        Vec::new()
    }

    fn running(&self) -> usize {
        0
    }

    fn remote_url(&self, _repo_path: &str, _remote: &str) -> Result<String> {
        Err(FleetError::unavailable(self.reason.clone()))
    }
}

/// The adapter of a daemon whose configuration switched the integration on: `gh` is resolved
/// once, here; when that fails the daemon still starts and every forge request says why it cannot
/// run.
pub fn build(
    cfg: &fleet_forge::config::ForgeConfig,
    parent_env: &[(String, String)],
    redactor: Arc<fleet_domain::redact::Redactor>,
    tracker: crate::jobs::JobTracker,
) -> Box<dyn ForgeJobs> {
    use crate::forge_gh::{gh_env, resolve_gh, GhSettings};
    let resolved = match resolve_gh(cfg, parent_env) {
        Ok(r) => r,
        Err(e) => return Box::new(UnavailableForge::new(e.message)),
    };
    let env = match gh_env(cfg, parent_env) {
        Ok(env) => env,
        Err(e) => return Box::new(UnavailableForge::new(e.message)),
    };
    let gh = Arc::new(GhCli::new(GhSettings {
        program: resolved.program.clone(),
        host: cfg.host.clone(),
        env,
        timeout_ms: cfg.timeout_ms,
        redactor,
        tracker: tracker.clone(),
        rate: cfg.rate,
    }));
    let publisher = Arc::new(crate::git::GitPublisher::new(
        parent_env.to_vec(),
        cfg.clone(),
        Some(resolved.program),
        tracker,
    ));
    let note = if resolved.skipped_relative > 0 {
        format!(
            " ({} empty or relative PATH entries were ignored)",
            resolved.skipped_relative
        )
    } else {
        String::new()
    };
    Box::new(GhForgeJobs::new(gh, publisher, note))
}
