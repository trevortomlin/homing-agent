//! MVP service manager: the agent supervises app processes itself.
//!
//! The design's target is systemd (`homing-app@.service` controlled over D-Bus). This module
//! implements the same contract (start / stop / restart / exit counting / restart-on-failure)
//! without systemd so the prototype runs anywhere, including Raspberry Pi OS and a dev desktop.
//! A systemd backend can replace it behind the same methods.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::process::Command;
use tokio::sync::Notify;

/// Snapshot of one app's process state.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ProcStatus {
    /// Supervisor wants it running.
    pub desired: bool,
    /// Current pid, if running.
    pub pid: Option<u32>,
    /// Number of times the process exited while it was supposed to be running.
    pub exits: u64,
    /// The running process has called `app.ready`.
    pub ready: bool,
    /// Description of the last exit.
    pub last_exit: Option<String>,
}

struct AppProc {
    state: Mutex<ProcStatus>,
    notify: Notify,
}

/// Supervises one process per app id.
#[derive(Clone)]
pub struct Supervisor {
    apps: Arc<Mutex<HashMap<String, Arc<AppProc>>>>,
    launcher: PathBuf,
    config_path: PathBuf,
    restart_delay: Duration,
}

impl Supervisor {
    pub fn new(launcher: PathBuf, config_path: PathBuf, restart_delay: Duration) -> Self {
        Self {
            apps: Default::default(),
            launcher,
            config_path,
            restart_delay,
        }
    }

    fn handle(&self, id: &str) -> Arc<AppProc> {
        let mut apps = self.apps.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(h) = apps.get(id) {
            return h.clone();
        }
        let h = Arc::new(AppProc {
            state: Mutex::new(ProcStatus::default()),
            notify: Notify::new(),
        });
        apps.insert(id.to_owned(), h.clone());
        tokio::spawn(run_loop(self.clone(), id.to_owned(), h.clone()));
        h
    }

    /// Status of an app (default if never started).
    pub fn status(&self, id: &str) -> ProcStatus {
        let apps = self.apps.lock().unwrap_or_else(|e| e.into_inner());
        apps.get(id)
            .map(|h| lock(&h.state).clone())
            .unwrap_or_default()
    }

    /// Start (no-op if running).
    pub fn start(&self, id: &str) {
        let h = self.handle(id);
        lock(&h.state).desired = true;
        h.notify.notify_waiters();
        h.notify.notify_one();
    }

    /// Stop and wait for the process to exit (up to ~10 s).
    pub async fn stop(&self, id: &str) {
        let h = self.handle(id);
        lock(&h.state).desired = false;
        h.notify.notify_waiters();
        h.notify.notify_one();
        for _ in 0..200 {
            if lock(&h.state).pid.is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Stop, then start a fresh process.
    pub async fn restart(&self, id: &str) {
        self.stop(id).await;
        self.start(id);
    }

    /// Mark the app owning `pid` (or an ancestor of it) as ready. Returns the app id.
    pub fn mark_ready_by_pid(&self, pid: u32) -> Option<String> {
        let id = self.app_for_pid(pid)?;
        let apps = self.apps.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(h) = apps.get(&id) {
            lock(&h.state).ready = true;
        }
        Some(id)
    }

    /// Find which supervised app a pid belongs to by walking the parent chain.
    pub fn app_for_pid(&self, pid: u32) -> Option<String> {
        let roots: HashMap<u32, String> = {
            let apps = self.apps.lock().unwrap_or_else(|e| e.into_inner());
            apps.iter()
                .filter_map(|(id, h)| lock(&h.state).pid.map(|p| (p, id.clone())))
                .collect()
        };
        let mut cur = pid;
        for _ in 0..32 {
            if let Some(id) = roots.get(&cur) {
                return Some(id.clone());
            }
            cur = parent_pid(cur)?;
            if cur <= 1 {
                return None;
            }
        }
        None
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 2 (comm) may contain spaces/parens; ppid is the 2nd field after the last ')'.
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(1)?.parse().ok()
}

async fn wait_desired(h: &AppProc) {
    loop {
        let n = h.notify.notified();
        if lock(&h.state).desired {
            return;
        }
        n.await;
    }
}

async fn run_loop(sup: Supervisor, id: String, h: Arc<AppProc>) {
    loop {
        wait_desired(&h).await;
        let spawned = Command::new(&sup.launcher)
            .arg("--config")
            .arg(&sup.config_path)
            .arg(&id)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                eprintln!("homing-agent: cannot start {id}: {e}");
                {
                    let mut s = lock(&h.state);
                    s.exits += 1;
                    s.last_exit = Some(format!("spawn failed: {e}"));
                }
                tokio::time::sleep(sup.restart_delay).await;
                continue;
            }
        };
        let pid = child.id();
        {
            let mut s = lock(&h.state);
            s.pid = pid;
            s.ready = false;
        }
        eprintln!("homing-agent: started {id} (pid {})", pid.unwrap_or(0));

        let status = loop {
            let n = h.notify.notified();
            tokio::select! {
                st = child.wait() => break st.ok(),
                _ = n => {
                    if !lock(&h.state).desired {
                        if let Some(p) = pid {
                            let _ = kill(Pid::from_raw(p as i32), Signal::SIGTERM);
                        }
                        let st = match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                            Ok(st) => st.ok(),
                            Err(_) => {
                                let _ = child.kill().await;
                                child.wait().await.ok()
                            }
                        };
                        let mut s = lock(&h.state);
                        s.pid = None;
                        s.ready = false;
                        s.last_exit = Some(format!("stopped ({st:?})"));
                        break None;
                    }
                }
            }
        };
        let Some(status) = status else { continue }; // stopped on request

        let delay = {
            let mut s = lock(&h.state);
            s.pid = None;
            s.ready = false;
            s.last_exit = Some(status.to_string());
            if s.desired {
                s.exits += 1;
                eprintln!("homing-agent: {id} exited unexpectedly: {status}");
                if status.success() {
                    // Like systemd Restart=on-failure: a clean exit is not restarted.
                    s.desired = false;
                    None
                } else {
                    Some(sup.restart_delay)
                }
            } else {
                None
            }
        };
        if let Some(d) = delay {
            let n = h.notify.notified();
            tokio::select! {
                _ = tokio::time::sleep(d) => {}
                _ = n => {}
            }
        }
    }
}
