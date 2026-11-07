//! The `homing-agent` daemon.

mod api;
pub mod http;
mod pipeline;
pub mod supervisor;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use homing_format::{DeviceInfo, ErrorCode, TrustStore};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::layout::{Layout, Slot, read_json, write_json};
use crate::rpc;
use supervisor::Supervisor;

/// How many finished jobs to remember.
const HISTORY: usize = 20;

/// Job states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Verifying,
    Staged,
    Switching,
    Trial,
    Committed,
    RollingBack,
    RolledBack,
    Failed,
}

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Committed | Self::RolledBack | Self::Failed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobError {
    pub code: String,
    pub message: String,
}

/// A job as reported by `job.get` and `status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub job_id: String,
    /// What the job does.
    #[serde(default)]
    pub kind: JobKind,
    pub source: String,
    pub state: JobState,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub version_code: Option<u64>,
    #[serde(default)]
    pub slot: Option<Slot>,
    #[serde(default)]
    pub error: Option<JobError>,
}

/// Persisted position of the active job, used for crash recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Active {
    pub job: Job,
    /// Job file inside incoming/.
    pub file: PathBuf,
    /// Slot that was committed when the job switched (rollback target).
    #[serde(default)]
    pub prev: Option<Slot>,
}

/// Kind of job.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// Install a package.
    #[default]
    Install,
    /// Return an app to its previous version (`rollback` RPC).
    Rollback,
}

/// A job waiting in the queue, with its package file in incoming/.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Queued {
    pub job: Job,
    pub file: PathBuf,
}

/// `agent-state.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentState {
    #[serde(default)]
    pub active: Option<Active>,
    /// Jobs waiting to run, oldest first. Persisted so they survive restarts and power loss.
    #[serde(default)]
    pub queued: VecDeque<Queued>,
    #[serde(default)]
    pub history: VecDeque<Job>,
}

/// Shared daemon state.
pub struct Agent {
    pub cfg: Config,
    pub layout: Layout,
    pub trust: TrustStore,
    pub device: DeviceInfo,
    pub sup: Supervisor,
    state: Mutex<AgentState>,
    tx: mpsc::UnboundedSender<String>,
    /// Result of the last HTTP source check.
    http_status: Mutex<http::HttpStatus>,
    /// Serializes HTTP source checks (periodic and on demand).
    http_check: tokio::sync::Mutex<()>,
}

impl Agent {
    fn lock_http(&self) -> std::sync::MutexGuard<'_, http::HttpStatus> {
        self.http_status.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether an install for `app_id` is queued or running.
    fn has_job_for(&self, app_id: &str) -> bool {
        let s = self.lock_state();
        s.queued
            .iter()
            .any(|q| q.job.app_id.as_deref() == Some(app_id))
            || s.active
                .as_ref()
                .is_some_and(|a| a.job.app_id.as_deref() == Some(app_id))
    }

    /// Whether this version recently failed an HTTP-sourced install, was rolled back, or was
    /// rolled back by an administrator (the job history), so the HTTP source does not install it
    /// again and again.
    fn failed_before(&self, app_id: &str, version_code: u64) -> bool {
        self.lock_state().history.iter().any(|j| {
            (j.source == "http" || j.kind == JobKind::Rollback)
                && j.app_id.as_deref() == Some(app_id)
                && j.version_code == Some(version_code)
                && http::is_failure(j.state)
        })
    }

    /// Queue an install of `file`, which must be a regular file directly inside incoming/.
    /// It is renamed to `job-<id>.homing` first, so its source can no longer change it.
    /// `hint` (app id, version code) is shown until the package's own manifest is read.
    fn submit(
        &self,
        file: &std::path::Path,
        source: &str,
        hint: Option<(&str, u64)>,
    ) -> Result<String, rpc::RpcError> {
        let io = |e: std::io::Error| rpc::RpcError::spec(ErrorCode::Io, e.to_string());
        let perm = |m: &str| rpc::RpcError::spec(ErrorCode::Perm, m);
        let incoming = std::fs::canonicalize(self.layout.incoming()).map_err(io)?;
        let real = std::fs::canonicalize(file).map_err(io)?;
        if real.parent() != Some(incoming.as_path()) {
            return Err(perm(
                "path must be a file directly inside the agent's incoming/ directory",
            ));
        }
        if !std::fs::symlink_metadata(&real).map_err(io)?.is_file() {
            return Err(perm("path is not a regular file"));
        }
        let job_id = new_job_id();
        let dest = incoming.join(format!("job-{job_id}.homing"));
        std::fs::rename(&real, &dest).map_err(io)?;
        let job = Job {
            job_id: job_id.clone(),
            kind: JobKind::Install,
            source: source.chars().take(32).collect(),
            state: JobState::Queued,
            app_id: hint.map(|(id, _)| id.to_owned()),
            version: None,
            version_code: hint.map(|(_, vc)| vc),
            slot: None,
            error: None,
        };
        self.enqueue(job, dest)
            .map_err(|e| rpc::RpcError::spec(ErrorCode::Io, e.to_string()))?;
        eprintln!("homing-agent: queued job {job_id} from {source}");
        Ok(job_id)
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, AgentState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Persist the current state.
    fn save(&self) -> Result<()> {
        let s = self.lock_state().clone();
        write_json(&self.layout.agent_state(), &s)
    }

    /// Find a job by id (queued, active or history).
    pub fn job(&self, id: &str) -> Option<Job> {
        let s = self.lock_state();
        if let Some(q) = s.queued.iter().find(|q| q.job.job_id == id) {
            return Some(q.job.clone());
        }
        if let Some(a) = s.active.as_ref().filter(|a| a.job.job_id == id) {
            return Some(a.job.clone());
        }
        s.history.iter().find(|j| j.job_id == id).cloned()
    }

    /// All known jobs, newest last.
    pub fn jobs(&self) -> Vec<Job> {
        let s = self.lock_state();
        let mut v: Vec<Job> = s.history.iter().cloned().collect();
        v.extend(s.active.iter().map(|a| a.job.clone()));
        v.extend(s.queued.iter().map(|q| q.job.clone()));
        v
    }

    /// Update the active job (and persist).
    fn update_active(&self, f: impl FnOnce(&mut Active)) -> Result<()> {
        {
            let mut s = self.lock_state();
            if let Some(a) = s.active.as_mut() {
                f(a);
            }
        }
        self.save()
    }

    /// Move the active job to history with its final state (and persist).
    fn finish_active(&self, state: JobState, error: Option<(ErrorCode, String)>) -> Result<()> {
        {
            let mut s = self.lock_state();
            if let Some(mut a) = s.active.take() {
                a.job.state = state;
                a.job.error = error.map(|(c, m)| JobError {
                    code: c.as_str().into(),
                    message: m,
                });
                let _ = std::fs::remove_file(&a.file);
                match &a.job.error {
                    Some(e) => eprintln!(
                        "homing-agent: job {} {:?}: {} {}",
                        a.job.job_id, state, e.code, e.message
                    ),
                    None => eprintln!("homing-agent: job {} {:?}", a.job.job_id, state),
                }
                s.history.push_back(a.job);
                while s.history.len() > HISTORY {
                    s.history.pop_front();
                }
            }
        }
        self.save()
    }

    /// Enqueue an install job for a file already moved into incoming/.
    fn enqueue(&self, job: Job, file: PathBuf) -> Result<()> {
        let id = job.job_id.clone();
        self.lock_state().queued.push_back(Queued { job, file });
        self.save()?;
        self.tx.send(id).context("job worker stopped")?;
        Ok(())
    }

    /// Queue a rollback of `app_id` to its previous version. Checked again when it runs.
    fn request_rollback(&self, app_id: &str) -> Result<String, rpc::RpcError> {
        let (current, prev) = pipeline::rollback_target(self, app_id)?;
        if self.has_job_for(app_id) {
            return Err(rpc::RpcError::spec(
                ErrorCode::Busy,
                "an install or rollback for this app is queued or running",
            ));
        }
        let job_id = new_job_id();
        let job = Job {
            job_id: job_id.clone(),
            kind: JobKind::Rollback,
            source: "rollback".into(),
            state: JobState::Queued,
            app_id: Some(app_id.to_owned()),
            version: Some(current.1.clone()),
            version_code: Some(current.2),
            slot: Some(current.0),
            error: None,
        };
        self.enqueue(job, PathBuf::new())
            .map_err(|e| rpc::RpcError::spec(ErrorCode::Io, e.to_string()))?;
        eprintln!(
            "homing-agent: queued rollback {job_id}: {app_id} {} -> slot {}",
            current.1,
            prev.name()
        );
        Ok(job_id)
    }

    /// Take the next queued job and make it the active one (state `verifying`), persisting
    /// both in one write so a crash cannot lose the job in between.
    fn start_next(&self) -> Option<PathBuf> {
        let file = {
            let mut s = self.lock_state();
            if s.active.is_some() {
                return None;
            }
            let mut q = s.queued.pop_front()?;
            q.job.state = JobState::Verifying;
            let file = q.file.clone();
            s.active = Some(Active {
                job: q.job,
                file: q.file,
                prev: None,
            });
            file
        };
        if let Err(e) = self.save() {
            eprintln!("homing-agent: cannot persist state: {e:#}");
        }
        Some(file)
    }
}

/// Random job id.
pub fn new_job_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/uuid")
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|_| format!("job-{}", std::process::id()))
}

/// Run the daemon until SIGINT/SIGTERM.
pub async fn run(cfg: Config, cfg_path: PathBuf) -> Result<()> {
    let layout = Layout::new(&cfg.data_dir);
    layout.ensure()?;
    let trust = TrustStore::load_dir(&cfg.keys_dir, cfg.dev_mode)
        .with_context(|| format!("loading trust store {}", cfg.keys_dir.display()))?;
    if !trust.has_prod_key() && !cfg.dev_mode {
        bail!("trust store has no prod-*.pub key and dev_mode is off");
    }
    if trust.usable_len() == 0 {
        bail!("trust store {} has no usable keys", cfg.keys_dir.display());
    }
    let device = DeviceInfo::read(&cfg.engine_file)?;
    let state: AgentState = read_json(&layout.agent_state())?;
    let sup = Supervisor::new(
        cfg.launcher.clone(),
        std::fs::canonicalize(&cfg_path).unwrap_or(cfg_path),
        Duration::from_millis(cfg.restart_delay_ms),
    );
    let (tx, rx) = mpsc::unbounded_channel();
    let agent = Arc::new(Agent {
        cfg,
        layout,
        trust,
        device,
        sup,
        state: Mutex::new(state),
        tx,
        http_status: Mutex::new(http::HttpStatus::default()),
        http_check: tokio::sync::Mutex::new(()),
    });

    pipeline::recover(&agent).await?;
    // Restart the worker on jobs that were queued when the agent stopped.
    let pending = agent.lock_state().queued.len();
    if pending > 0 {
        eprintln!("homing-agent: {pending} queued job(s) carried over");
        let _ = agent.tx.send("resume".into());
    }
    pipeline::autostart(&agent);

    let listener = api::bind(&agent.cfg.socket)?;
    eprintln!(
        "homing-agent {} listening on {} (data {}, dev_mode {})",
        env!("CARGO_PKG_VERSION"),
        agent.cfg.socket.display(),
        agent.layout.root.display(),
        agent.cfg.dev_mode
    );
    tokio::spawn(pipeline::worker(agent.clone(), rx));
    tokio::spawn(http::run(agent.clone()));
    let server = tokio::spawn(api::serve(agent.clone(), listener));

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
        r = server => { r??; }
    }
    eprintln!("homing-agent: shutting down");
    let _ = std::fs::remove_file(&agent.cfg.socket);
    Ok(())
}
