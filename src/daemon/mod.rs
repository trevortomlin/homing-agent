//! The `homing-agent` daemon.

mod api;
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

/// `agent-state.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentState {
    #[serde(default)]
    pub active: Option<Active>,
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
    /// Queued (not yet started) jobs with their files in incoming/.
    queued: Mutex<VecDeque<(Job, PathBuf)>>,
    tx: mpsc::UnboundedSender<String>,
}

impl Agent {
    fn lock_queued(&self) -> std::sync::MutexGuard<'_, VecDeque<(Job, PathBuf)>> {
        self.queued.lock().unwrap_or_else(|e| e.into_inner())
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
        if let Some((j, _)) = self.lock_queued().iter().find(|(j, _)| j.job_id == id) {
            return Some(j.clone());
        }
        let s = self.lock_state();
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
        v.extend(self.lock_queued().iter().map(|(j, _)| j.clone()));
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
        self.lock_queued().push_back((job, file));
        self.tx.send(id).context("job worker stopped")?;
        Ok(())
    }

    /// Take the next queued job.
    fn dequeue(&self) -> Option<(Job, PathBuf)> {
        self.lock_queued().pop_front()
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
        queued: Mutex::new(VecDeque::new()),
        tx,
    });

    pipeline::recover(&agent).await?;
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
