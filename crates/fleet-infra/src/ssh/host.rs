//! The [`RemoteHost`] adapter that talks to real machines with OpenSSH.
//!
//! Prepare, collect, signal and clean-up each run the ssh client without a terminal through
//! [`SshClient`]; the worker itself runs in a terminal through the ordinary session host, with the
//! command this adapter builds. Local git work (the export bundle, the verification of what comes
//! back) is done by [`GitVcs`]. Collecting and signalling run on helper threads so the
//! orchestrator never waits for a remote host.

use super::client::{Failure, SshClient, Timing};
use super::exec::{Ending, Input, Output};
use crate::fsx;
use crate::git::{GitVcs, ImportRequest};
use fleet_app::hosts::HostDef;
use fleet_app::ports::{Identity, Signal};
use fleet_app::remote::{
    CollectId, CollectRequest, Collected, LaunchRequest, LocalCommand, PrepareRequest, Prepared,
    RemoteHost, RemoteTask,
};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::{show, HostName, RunId, TaskId};
use fleet_domain::limits::MAX_BRIEF_FILE_BYTES;
use fleet_domain::remote::{ContractFile, RemoteCall, RemoteSignal, REMOTE_CONTRACT_READ};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

/// Background threads (collects, signals, clean-ups) that may run at once.
const MAX_HELPERS: usize = 64;
/// The most time start-up clean-up of leftovers may take in total.
const RECONCILE_BUDGET: Duration = Duration::from_secs(20);
/// Exit code the prepare script gives when the remote worktree exists already.
const EXISTS: i32 = 72;

enum Slot {
    Running,
    Done(Result<Collected>),
}

struct Collects {
    next: u64,
    jobs: BTreeMap<u64, Slot>,
}

struct Shared {
    client: SshClient,
    vcs: GitVcs,
    author: Identity,
    collects: Mutex<Collects>,
    /// The local worktree of each task with remote work in flight (for the quarantine refs).
    worktrees: Mutex<BTreeMap<(HostName, RunId, TaskId), String>>,
    helpers: AtomicUsize,
}

pub struct SshRemoteHost {
    shared: Arc<Shared>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Counts a helper thread while it runs.
struct HelperSlot(Arc<Shared>);

impl Drop for HelperSlot {
    fn drop(&mut self) {
        self.0.helpers.fetch_sub(1, Ordering::SeqCst);
    }
}

impl SshRemoteHost {
    pub fn new(client: SshClient, vcs: GitVcs, author: Identity) -> SshRemoteHost {
        SshRemoteHost {
            shared: Arc::new(Shared {
                client,
                vcs,
                author,
                collects: Mutex::new(Collects {
                    next: 0,
                    jobs: BTreeMap::new(),
                }),
                worktrees: Mutex::new(BTreeMap::new()),
                helpers: AtomicUsize::new(0),
            }),
        }
    }

    pub fn client(&self) -> &SshClient {
        &self.shared.client
    }
}

/// What remains of `budget` since `started`; an error when nothing does.
fn budget_left(started: std::time::Instant, budget: Timing) -> Result<Timing> {
    let left = budget.total.saturating_sub(started.elapsed());
    if left < Duration::from_secs(1) {
        return Err(FleetError::new(
            fleet_domain::error::ErrorCode::Timeout,
            "sending the code to the host took longer than its time budget",
        ));
    }
    Ok(Timing {
        total: left,
        stall: budget.stall.min(left),
    })
}

fn markers_dir(state: &Path) -> PathBuf {
    state.join("remote").join("live")
}

fn work_dir(state: &Path) -> PathBuf {
    state.join("remote").join("work")
}

fn marker_name(host: &HostName, run: &RunId, task: &TaskId) -> String {
    format!("{}.{}.{}", host.as_str(), run.as_str(), task.as_str())
}

fn parse_marker(name: &str) -> Option<(HostName, RunId, TaskId)> {
    let mut parts = name.split('.');
    let (h, r, t) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    Some((
        HostName::parse(h).ok()?,
        RunId::parse(r).ok()?,
        TaskId::parse(t).ok()?,
    ))
}

fn remote_signal(signal: Signal) -> RemoteSignal {
    match signal {
        Signal::Term => RemoteSignal::Term,
        Signal::Kill => RemoteSignal::Kill,
    }
}

impl Shared {
    fn state(&self) -> &Path {
        self.client.state_dir()
    }

    fn prepare_dirs(&self) -> Result<()> {
        fsx::ensure_private_dir(&self.state().join("remote"))?;
        fsx::ensure_private_dir(&markers_dir(self.state()))?;
        fsx::ensure_private_dir(&work_dir(self.state()))
    }

    fn mark_live(&self, t: &RemoteTask) -> Result<()> {
        self.prepare_dirs()?;
        let path = markers_dir(self.state()).join(marker_name(&t.host, &t.run, &t.task));
        fsx::write_private_atomic(&path, b"")
    }

    fn unmark(&self, t: &RemoteTask) {
        let path = markers_dir(self.state()).join(marker_name(&t.host, &t.run, &t.task));
        let _ = fsx::remove_nofollow(&path);
    }

    fn temp(&self, t: &RemoteTask, leaf: &str) -> Result<PathBuf> {
        self.prepare_dirs()?;
        let path = work_dir(self.state()).join(format!(
            "{}-{}-{}-{leaf}",
            t.host.as_str(),
            t.run.as_str(),
            t.task.as_str()
        ));
        let _ = fsx::remove_nofollow(&path);
        Ok(path)
    }

    fn timing(&self, pick: impl Fn(&super::client::Timeouts) -> Timing) -> Timing {
        pick(&self.client.timeouts)
    }

    fn signal_remote(&self, host: &HostDef, t: &RemoteTask, signal: RemoteSignal) -> bool {
        let layout = match host.layout() {
            Ok(l) => l,
            Err(_) => return false,
        };
        let call = RemoteCall::Signal {
            layout: &layout,
            run: &t.run,
            task: &t.task,
            signal,
        };
        matches!(
            self.client.run(
                host,
                &call,
                Input::None,
                Output::Capture { max: 4096 },
                self.timing(|t| t.cleanup),
            ),
            Ok(d) if d.success()
        )
    }

    /// Removes the remote worktree and the marker. Returns a note for the report.
    fn remove_remote(&self, host: &HostDef, t: &RemoteTask) -> String {
        let layout = match host.layout() {
            Ok(l) => l,
            Err(e) => return format!("fleet: remote: cannot clean up: {}", e.message),
        };
        let call = RemoteCall::Remove {
            layout: &layout,
            run: &t.run,
            task: &t.task,
        };
        match self.client.run(
            host,
            &call,
            Input::None,
            Output::Capture { max: 4096 },
            self.timing(|t| t.cleanup),
        ) {
            Ok(d) if d.success() => {
                self.unmark(t);
                format!("fleet: remote: the worktree on host {} was removed", t.host)
            }
            Ok(d) => format!(
                "fleet: remote: the worktree on host {} could not be removed ({}); it is listed and the next start tries again",
                t.host,
                self.client.explain(host, "removing the remote worktree", &d).message
            ),
            Err(e) => format!(
                "fleet: remote: the worktree on host {} could not be removed ({}); it is listed and the next start tries again",
                t.host, e.message
            ),
        }
    }

    fn forget_local(&self, t: &RemoteTask) {
        let key = (t.host.clone(), t.run.clone(), t.task.clone());
        if let Some(worktree) = lock(&self.worktrees).remove(&key) {
            let _ = self.vcs.drop_remote_refs(&worktree, &t.run, &t.task);
        }
    }

    fn prepare(&self, req: &PrepareRequest) -> Result<Prepared> {
        let t = &req.task;
        let host = self.client.host(&t.host)?;
        self.client.require_pins(host)?;
        let layout = host.layout()?;
        let bundle = self.temp(t, "in.bundle")?;
        let sent = self.vcs.export_for_remote(
            &req.worktree,
            &t.run,
            &t.task,
            &self.author,
            &bundle,
            host.max_bundle_bytes,
        )?;
        lock(&self.worktrees).insert(
            (t.host.clone(), t.run.clone(), t.task.clone()),
            req.worktree.clone(),
        );
        let result = self.send(host, t, &layout, &bundle, &req.worktree);
        let _ = fsx::remove_nofollow(&bundle);
        match result {
            Ok(()) => Ok(Prepared {
                dir: layout.worktree(&t.run, &t.task),
                sent,
            }),
            Err(e) => {
                let _ = self.remove_remote(host, t);
                self.forget_local(t);
                Err(e)
            }
        }
    }

    fn send(
        &self,
        host: &HostDef,
        t: &RemoteTask,
        layout: &fleet_domain::remote::RemoteLayout,
        bundle: &Path,
        worktree: &str,
    ) -> Result<()> {
        self.mark_live(t)?;
        // One budget for the whole of sending (code, replacing a stale worktree, brief): this runs
        // on the engine thread, so it is bounded as a whole and not call by call.
        let started = std::time::Instant::now();
        let budget = self.timing(|t| t.prepare);
        let prepare = RemoteCall::Prepare {
            layout,
            run: &t.run,
            task: &t.task,
        };
        let mut attempt = 0;
        loop {
            let done = self.client.run(
                host,
                &prepare,
                Input::File(bundle.to_path_buf()),
                Output::Capture { max: 16 * 1024 },
                budget_left(started, budget)?,
            )?;
            if done.success() {
                break;
            }
            if done.code() == Some(EXISTS) && attempt == 0 {
                // A worktree of an earlier attempt that was not cleaned up: replace it.
                attempt += 1;
                let note = self.remove_remote(host, t);
                if !note.contains("was removed") {
                    return Err(FleetError::conflict(format!(
                        "the remote worktree of an earlier attempt could not be replaced: {note}"
                    )));
                }
                self.mark_live(t)?;
                continue;
            }
            return Err(self.client.explain(host, "sending the code", &done));
        }
        // The brief, from the local worktree (a plain file below `.fleet`).
        let brief_path = Path::new(worktree).join(".fleet").join("brief.md");
        let brief = fsx::read_regular(&brief_path, MAX_BRIEF_FILE_BYTES as u64)
            .map_err(|e| FleetError::invalid(format!("cannot read the brief: {e}")))?;
        let put = RemoteCall::PutBrief {
            layout,
            run: &t.run,
            task: &t.task,
        };
        let done = self.client.run(
            host,
            &put,
            Input::Bytes(brief),
            Output::Capture { max: 4096 },
            budget_left(started, budget)?,
        )?;
        if !done.success() {
            return Err(self.client.explain(host, "sending the brief", &done));
        }
        Ok(())
    }

    fn collect(&self, req: &CollectRequest) -> Result<Collected> {
        let t = &req.task;
        let host = self.client.host(&t.host)?;
        let layout = host.layout()?;
        let mut notes: Vec<String> = Vec::new();
        let mut failure: Option<FleetError> = None;
        let mut reachable = true;
        let out = self.temp(t, "out.bundle")?;

        // 0. The session is over: nothing of the worker's process group may run on while its
        // work is committed and its directory removed (a worker can ignore the hang-up that the
        // end of the terminal causes). Best effort; a host that cannot be reached is found out
        // by the next call.
        let _ = self.signal_remote(host, t, RemoteSignal::Kill);

        // 1. Commit what the worker left on the host and bring the commits back.
        let finalize = RemoteCall::Finalize {
            layout: &layout,
            run: &t.run,
            task: &t.task,
            sent: &req.sent,
        };
        let mut bundled = false;
        match self.client.run(
            host,
            &finalize,
            Input::None,
            Output::File {
                path: out.clone(),
                max: host.max_bundle_bytes,
            },
            self.timing(|t| t.collect),
        ) {
            Ok(d) if d.success() => bundled = d.stdout_len > 0,
            Ok(d) => {
                reachable = !matches!(
                    self.client.classify(&d),
                    Some(
                        Failure::Unreachable
                            | Failure::Timeout
                            | Failure::HostKeyChanged
                            | Failure::NotTrusted
                    )
                );
                failure = Some(self.client.explain(host, "collecting the remote work", &d));
            }
            Err(e) => {
                reachable = false;
                failure = Some(e);
            }
        }

        // 2. The worker's contract files.
        if reachable {
            for file in [ContractFile::Result, ContractFile::Question] {
                match self.read_contract(host, t, &layout, file) {
                    Ok(Some(bytes)) => {
                        if let Err(e) = self.store_contract(&req.worktree, file, &bytes) {
                            notes.push(format!(
                                "fleet: remote: {} could not be stored: {}",
                                file.file_name(),
                                e.message
                            ));
                        }
                    }
                    Ok(None) => {}
                    Err(e) => notes.push(format!(
                        "fleet: remote: {} could not be read: {}",
                        file.file_name(),
                        e.message
                    )),
                }
            }
        }

        // 3. Verify the work and make it the task branch.
        let mut head = None;
        if bundled {
            match self.vcs.import_from_remote(
                &ImportRequest {
                    worktree: &req.worktree,
                    run: &t.run,
                    task: &t.task,
                    sent: &req.sent,
                    bundle: &out,
                    host: t.host.as_str(),
                },
                &self.author,
            ) {
                Ok(imported) => {
                    head = imported.head.clone();
                    notes.push(match &imported.head {
                        Some(h) => format!(
                            "fleet: remote: host {} returned {} commit(s) changing {} path(s); verified and applied as {}",
                            t.host,
                            imported.commits,
                            imported.changed_paths,
                            h.as_str().get(..12).unwrap_or(h.as_str())
                        ),
                        None => format!(
                            "fleet: remote: host {} returned {} commit(s) that left the files as they were",
                            t.host, imported.commits
                        ),
                    });
                }
                Err(e) => {
                    failure = Some(FleetError::forbidden(format!(
                        "the work returned by host {} was not applied: {}",
                        t.host, e.message
                    )));
                }
            }
        } else if failure.is_none() {
            notes.push(format!(
                "fleet: remote: host {} reported no change to the files",
                t.host
            ));
        }

        // 4. Clean up, whatever happened.
        let _ = fsx::remove_nofollow(&out);
        if reachable {
            notes.push(self.remove_remote(host, t));
        } else {
            notes.push(format!(
                "fleet: remote: host {} was not reachable; its worktree is listed and the next start tries to remove it",
                t.host
            ));
        }
        self.forget_local(t);
        match failure {
            Some(e) => Err(e),
            None => Ok(Collected { head, notes }),
        }
    }

    fn read_contract(
        &self,
        host: &HostDef,
        t: &RemoteTask,
        layout: &fleet_domain::remote::RemoteLayout,
        file: ContractFile,
    ) -> Result<Option<Vec<u8>>> {
        let call = RemoteCall::ReadContract {
            layout,
            run: &t.run,
            task: &t.task,
            file,
        };
        let done = self.client.run(
            host,
            &call,
            Input::None,
            Output::Capture {
                max: REMOTE_CONTRACT_READ,
            },
            self.timing(|t| t.quick),
        )?;
        match done.ending {
            Ending::Exited { code: Some(0), .. } => Ok(Some(done.stdout)),
            Ending::Exited { code: Some(3), .. } => Ok(None),
            // The output cap is how much of an oversized file is kept: the local reader then
            // reports the file as too large.
            Ending::OutputLimit => Ok(Some(done.stdout)),
            _ => Err(self.client.explain(host, "reading a contract file", &done)),
        }
    }

    fn store_contract(&self, worktree: &str, file: ContractFile, bytes: &[u8]) -> Result<()> {
        let dir = Path::new(worktree).join(".fleet");
        if !fsx::is_real_dir(&dir) {
            return Err(FleetError::forbidden(".fleet is not a plain directory"));
        }
        fsx::write_private_atomic(&dir.join(file.file_name()), bytes)
    }

    fn spawn_helper(
        self: &Arc<Self>,
        name: &str,
        job: impl FnOnce(&Shared) + Send + 'static,
    ) -> bool {
        if self.helpers.fetch_add(1, Ordering::SeqCst) >= MAX_HELPERS {
            self.helpers.fetch_sub(1, Ordering::SeqCst);
            return false;
        }
        let shared = Arc::clone(self);
        let slot = HelperSlot(Arc::clone(self));
        let started = thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                let _slot = slot;
                job(&shared);
            });
        if started.is_err() {
            self.helpers.fetch_sub(1, Ordering::SeqCst);
            return false;
        }
        true
    }
}

impl RemoteHost for SshRemoteHost {
    fn remote_dir(&self, task: &RemoteTask) -> Result<String> {
        let host = self.shared.client.host(&task.host)?;
        Ok(host.layout()?.worktree(&task.run, &task.task))
    }

    fn prepare(&self, req: &PrepareRequest) -> Result<Prepared> {
        self.shared.prepare(req)
    }

    fn launch_command(&self, req: &LaunchRequest) -> Result<LocalCommand> {
        let t = &req.task;
        let client = &self.shared.client;
        let host = client.host(&t.host)?;
        let layout = host.layout()?;
        // The variables go to the host in a file (stdin of a small call), never on a command line,
        // where every local user could read them in the process table.
        let put = RemoteCall::PutEnv {
            layout: &layout,
            run: &t.run,
            task: &t.task,
        };
        let text = fleet_domain::remote::env_file_text(&req.env)?;
        let done = client.run(
            host,
            &put,
            Input::Bytes(text.into_bytes()),
            Output::Capture { max: 4096 },
            self.shared.timing(|t| t.quick),
        )?;
        if !done.success() {
            return Err(client.explain(host, "sending the environment", &done));
        }
        let call = RemoteCall::Launch {
            layout: &layout,
            run: &t.run,
            task: &t.task,
            program: &req.program,
            allowed: &host.allowed_workers,
            args: &req.args,
        };
        let invocation = client.command_for(host, &call, true)?;
        let program = client
            .ssh
            .to_str()
            .ok_or_else(|| FleetError::invalid("the ssh path is not valid UTF-8"))?
            .to_string();
        Ok(LocalCommand {
            program,
            args: invocation.args,
            env: invocation.env,
        })
    }

    fn begin_collect(&self, req: &CollectRequest) -> Result<CollectId> {
        let id = {
            let mut c = lock(&self.shared.collects);
            c.next += 1;
            let id = c.next;
            c.jobs.insert(id, Slot::Running);
            id
        };
        let request = req.clone();
        let started = self
            .shared
            .spawn_helper("fleet-ssh-collect", move |shared| {
                let result = shared.collect(&request);
                lock(&shared.collects).jobs.insert(id, Slot::Done(result));
            });
        if !started {
            lock(&self.shared.collects).jobs.remove(&id);
            return Err(FleetError::limit("too many remote operations are running"));
        }
        Ok(CollectId(id))
    }

    fn poll_collect(&self, id: CollectId) -> Option<Result<Collected>> {
        let mut c = lock(&self.shared.collects);
        match c.jobs.get(&id.0) {
            None => Some(Err(FleetError::internal("unknown collect job"))),
            Some(Slot::Running) => None,
            Some(Slot::Done(_)) => match c.jobs.remove(&id.0) {
                Some(Slot::Done(r)) => Some(r),
                _ => None,
            },
        }
    }

    fn signal(&self, task: &RemoteTask, signal: Signal) {
        let t = task.clone();
        let _ = self.shared.spawn_helper("fleet-ssh-signal", move |shared| {
            if let Ok(host) = shared.client.host(&t.host) {
                let _ = shared.signal_remote(host, &t, remote_signal(signal));
            }
        });
    }

    fn abandon(&self, task: &RemoteTask) {
        let t = task.clone();
        let _ = self
            .shared
            .spawn_helper("fleet-ssh-abandon", move |shared| {
                if let Ok(host) = shared.client.host(&t.host) {
                    let _ = shared.signal_remote(host, &t, RemoteSignal::Kill);
                    let _ = shared.remove_remote(host, &t);
                }
                shared.forget_local(&t);
            });
    }

    fn reconcile(&self) -> Vec<String> {
        let shared = &self.shared;
        let dir = markers_dir(shared.state());
        let mut left: Vec<RemoteTask> = Vec::new();
        let mut lines = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                match parse_marker(&name) {
                    Some((host, run, task)) => left.push(RemoteTask { host, run, task }),
                    None => lines.push(format!(
                        "fleet: remote: ignoring an unknown entry {} in the list of remote worktrees",
                        show(&name)
                    )),
                }
            }
        }
        // Stale transfer files of a daemon that was killed.
        if let Ok(entries) = std::fs::read_dir(work_dir(shared.state())) {
            for entry in entries.flatten() {
                let _ = fsx::remove_nofollow(&entry.path());
            }
        }
        if left.is_empty() {
            return lines;
        }
        left.sort_by(|a, b| (&a.host, &a.run, &a.task).cmp(&(&b.host, &b.run, &b.task)));
        let results: Arc<Mutex<Vec<(RemoteTask, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let mut waiting = 0usize;
        for t in left {
            let sink = Arc::clone(&results);
            let task = t.clone();
            let ok = shared.spawn_helper("fleet-ssh-reconcile", move |s| {
                let note = match s.client.host(&task.host) {
                    Ok(host) => {
                        let _ = s.signal_remote(host, &task, RemoteSignal::Kill);
                        s.remove_remote(host, &task)
                    }
                    Err(_) => format!(
                        "fleet: remote: host {} is not configured any more; its worktree {}/{} was left where it is",
                        task.host, task.run, task.task
                    ),
                };
                lock(&sink).push((task, note));
            });
            if ok {
                waiting += 1;
            }
        }
        let end = std::time::Instant::now() + RECONCILE_BUDGET;
        while std::time::Instant::now() < end && lock(&results).len() < waiting {
            thread::sleep(Duration::from_millis(20));
        }
        let done = lock(&results).clone();
        for (t, note) in &done {
            lines.push(format!(
                "{} (leftover {}/{} of an earlier run)",
                note.trim_start_matches("fleet: remote: "),
                t.run,
                t.task
            ));
        }
        if done.len() < waiting {
            lines.push(format!(
                "remote clean-up: {} leftover worktree(s) are still being removed in the background",
                waiting - done.len()
            ));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_of_sending_is_shared_by_its_calls() {
        let budget = Timing {
            total: Duration::from_secs(120),
            stall: Duration::from_secs(60),
        };
        let now = std::time::Instant::now();
        let first = budget_left(now, budget).unwrap();
        assert!(first.total <= Duration::from_secs(120) && first.stall == Duration::from_secs(60));
        let spent = now.checked_sub(Duration::from_secs(100)).unwrap();
        let later = budget_left(spent, budget).unwrap();
        assert!(later.total <= Duration::from_secs(20) && later.stall <= Duration::from_secs(20));
        let over = now.checked_sub(Duration::from_secs(120)).unwrap();
        assert!(budget_left(over, budget).is_err());
    }
}
