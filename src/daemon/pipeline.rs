//! Install pipeline, trial, commit/rollback and crash recovery.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use homing_format::manifest::{BuildMode, HealthMode};
use homing_format::{ErrorCode, Manifest, VerifyOptions};
use tokio::sync::mpsc::UnboundedReceiver;

use super::{Agent, JobKind, JobState};
use crate::launch::{META_DIR, prepare_slot};
use crate::layout::{
    Floors, Slot, SlotInfo, SlotStatus, SlotsFile, current_slot, fsync_dir, fsync_tree, read_json,
    swap_symlink, write_json,
};

/// A pre-switch failure: spec code + message.
type Fail = (ErrorCode, String);

fn io_fail(e: impl std::fmt::Display) -> Fail {
    (ErrorCode::Io, format!("{e:#}"))
}

fn fmt_fail(e: homing_format::Error) -> Fail {
    let mut msg = e.message().to_owned();
    if let Some(src) = std::error::Error::source(&e) {
        msg.push_str(&format!(" ({src})"));
    }
    (e.code(), msg)
}

/// Job worker: runs queued jobs one at a time.
pub async fn worker(agent: Arc<Agent>, mut rx: UnboundedReceiver<String>) {
    while rx.recv().await.is_some() {
        while let Some(file) = agent.start_next() {
            run_job(&agent, file).await;
        }
    }
}

/// Run the active job (already persisted by `Agent::start_next`).
async fn run_job(agent: &Arc<Agent>, file: PathBuf) {
    let rollback = agent
        .lock_state()
        .active
        .as_ref()
        .filter(|a| a.job.kind == JobKind::Rollback)
        .and_then(|a| a.job.app_id.clone());
    if let Some(id) = rollback {
        manual_rollback(agent, &id).await;
        return;
    }
    // Verify + extract + finalize on a blocking thread.
    let a2 = agent.clone();
    let staged = tokio::task::spawn_blocking(move || stage(&a2, &file))
        .await
        .unwrap_or_else(|e| Err(io_fail(e)));
    let (manifest, slot) = match staged {
        Ok(v) => v,
        Err((code, msg)) => {
            let _ = agent.finish_active(JobState::Failed, Some((code, msg)));
            return;
        }
    };

    if let Err(f) = switch_and_trial(agent, &manifest, slot).await {
        let _ = agent.finish_active(JobState::Failed, Some(f));
    }
}

/// Steps 1-4 of the agent design: intake checks, verify+extract into the inactive slot,
/// finalize the slot. Never touches the committed slot or `current`.
fn stage(agent: &Agent, file: &Path) -> Result<(Manifest, Slot), Fail> {
    let lay = &agent.layout;

    // Verify the signed manifest first (no payload read yet) to learn the app id and payload
    // size; nothing from an unsigned manifest is ever acted on.
    let f = fs::File::open(file).map_err(io_fail)?;
    let peek = homing_format::verify_head(f, &agent.trust)
        .map_err(fmt_fail)?
        .manifest;
    let id = peek.id.clone();
    let payload: u64 = peek.files.iter().map(|f| f.size).sum();
    let free = free_bytes(&lay.root).map_err(io_fail)?;
    let need = payload.saturating_add(agent.cfg.min_free_mb * 1024 * 1024);
    if free < need {
        return Err((ErrorCode::Space, format!("need {need} bytes, {free} free")));
    }

    let app_dir = lay.app(&id);
    fs::create_dir_all(&app_dir).map_err(io_fail)?;
    let slots: SlotsFile = read_json(&lay.slots_file(&id)).map_err(io_fail)?;
    let floors: Floors = read_json(&lay.floors()).map_err(io_fail)?;
    let floor = floors.get(&id).copied().unwrap_or(0);
    let slot = slots.inactive();

    agent
        .update_active(|a| {
            a.job.app_id = Some(id.clone());
            a.job.version = Some(peek.version.clone());
            a.job.version_code = Some(peek.version_code);
            a.job.slot = Some(slot);
        })
        .map_err(io_fail)?;

    let tmp = app_dir.join(format!("{}.tmp", slot.name()));
    if tmp.exists() {
        fs::remove_dir_all(&tmp).map_err(io_fail)?;
    }
    let f = fs::File::open(file).map_err(io_fail)?;
    let opts = VerifyOptions::new(&agent.trust)
        .device(&agent.device)
        .floor(floor);
    let v = homing_format::verify_and_extract(f, opts, &tmp).map_err(fmt_fail)?;
    let m = v.manifest;

    let cleanup = |f: Fail| {
        let _ = fs::remove_dir_all(&tmp);
        f
    };
    if m.id != id {
        return Err(cleanup((
            ErrorCode::Schema,
            "manifest changed during install".into(),
        )));
    }
    if m.build_mode == BuildMode::Debug && !agent.cfg.dev_mode {
        return Err(cleanup((
            ErrorCode::Perm,
            "debug builds need dev_mode".into(),
        )));
    }

    // Finalize: adapter prep, verified manifest + signature, fsync everything.
    prepare_slot(&agent.cfg, &tmp, &m).map_err(|e| cleanup(io_fail(e)))?;
    let meta = tmp.join(META_DIR);
    fs::create_dir(&meta).map_err(|e| cleanup(io_fail(e)))?;
    fs::write(meta.join("manifest.json"), &v.manifest_bytes).map_err(|e| cleanup(io_fail(e)))?;
    fs::write(meta.join("manifest.sig"), &v.sig_bytes).map_err(|e| cleanup(io_fail(e)))?;
    fsync_tree(&tmp).map_err(|e| cleanup(io_fail(e)))?;

    // Replace the inactive slot. The old content is a rollback backup or a bad version; it is
    // only overwritten now that the new one is complete.
    let target = lay.slot(&id, slot);
    let old = app_dir.join(format!("{}.old", slot.name()));
    if old.exists() {
        fs::remove_dir_all(&old).map_err(io_fail)?;
    }
    if target.exists() {
        fs::rename(&target, &old).map_err(io_fail)?;
    }
    fs::rename(&tmp, &target).map_err(io_fail)?;
    fsync_dir(&app_dir).map_err(io_fail)?;
    if old.exists() {
        fs::remove_dir_all(&old).map_err(io_fail)?;
    }

    let mut slots = slots;
    let digest = homing_format::package::sha256_reader(&v.manifest_bytes[..])
        .map_err(io_fail)?
        .0;
    *slots.get_mut(slot) = Some(SlotInfo {
        version: m.version.clone(),
        version_code: m.version_code,
        status: SlotStatus::Staged,
        manifest_sha256: digest,
    });
    write_json(&lay.slots_file(&id), &slots).map_err(io_fail)?;
    fs::create_dir_all(lay.state_dir(&id)).map_err(io_fail)?;
    agent
        .update_active(|a| a.job.state = JobState::Staged)
        .map_err(io_fail)?;
    Ok((m, slot))
}

async fn switch_and_trial(agent: &Arc<Agent>, m: &Manifest, slot: Slot) -> Result<(), Fail> {
    let lay = &agent.layout;
    let id = m.id.as_str();
    let mut slots: SlotsFile = read_json(&lay.slots_file(id)).map_err(io_fail)?;
    let prev = slots.committed;

    // Switch: persist intent first so a crash from here on rolls back.
    agent
        .update_active(|a| {
            a.job.state = JobState::Switching;
            a.prev = prev;
        })
        .map_err(io_fail)?;
    swap_symlink(&lay.current(id), slot.name()).map_err(io_fail)?;
    slots.set_status(slot, SlotStatus::Trial);
    write_json(&lay.slots_file(id), &slots).map_err(io_fail)?;
    let baseline = agent.sup.status(id).exits;
    agent.sup.restart(id).await;
    agent
        .update_active(|a| a.job.state = JobState::Trial)
        .map_err(io_fail)?;

    match trial(agent, m, baseline).await {
        Ok(()) => commit(agent, m, slot, prev).map_err(io_fail),
        Err(why) => {
            rollback(agent, id, slot, prev, &why).await;
            Ok(())
        }
    }
}

/// Wait for the health condition. `Err` carries the reason the trial failed.
async fn trial(agent: &Agent, m: &Manifest, baseline: u64) -> Result<(), String> {
    let start = Instant::now();
    let h = m.health;
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let st = agent.sup.status(&m.id);
        if st.exits > baseline || !st.desired {
            return Err(format!(
                "app exited during trial ({})",
                st.last_exit.unwrap_or_else(|| "unknown".into())
            ));
        }
        let elapsed = start.elapsed();
        match h.mode {
            HealthMode::Ready => {
                if st.ready {
                    return Ok(());
                }
                if elapsed >= Duration::from_secs(h.ready_timeout_s.into()) {
                    return Err(format!(
                        "app did not call app.ready within {} s",
                        h.ready_timeout_s
                    ));
                }
            }
            HealthMode::Alive => {
                if elapsed >= Duration::from_secs(h.trial_s.into()) {
                    return Ok(());
                }
            }
            // A health mode added by a newer format version: fail the trial (fail closed).
            other => return Err(format!("unsupported health mode {other:?}")),
        }
    }
}

fn commit(agent: &Agent, m: &Manifest, slot: Slot, prev: Option<Slot>) -> anyhow::Result<()> {
    let lay = &agent.layout;
    let mut slots: SlotsFile = read_json(&lay.slots_file(&m.id))?;
    if let Some(p) = prev.filter(|p| *p != slot) {
        slots.set_status(p, SlotStatus::Previous);
    }
    slots.set_status(slot, SlotStatus::Committed);
    slots.committed = Some(slot);
    write_json(&lay.slots_file(&m.id), &slots)?;
    raise_floor(agent, &m.id, m.min_version_code)?;
    agent.finish_active(JobState::Committed, None)
}

fn raise_floor(agent: &Agent, id: &str, min: u64) -> anyhow::Result<()> {
    let mut floors: Floors = read_json(&agent.layout.floors())?;
    let f = floors.entry(id.to_owned()).or_insert(0);
    if min > *f {
        *f = min;
        write_json(&agent.layout.floors(), &floors)?;
    }
    Ok(())
}

async fn rollback(agent: &Agent, id: &str, slot: Slot, prev: Option<Slot>, why: &str) {
    let _ = agent.update_active(|a| a.job.state = JobState::RollingBack);
    match restore_previous(agent, id, slot, prev) {
        Ok(true) => {
            agent.sup.restart(id).await;
            let _ =
                agent.finish_active(JobState::RolledBack, Some((ErrorCode::Health, why.into())));
        }
        Ok(false) => {
            agent.sup.stop(id).await;
            let msg = format!("{why}; no previous version to roll back to");
            let _ = agent.finish_active(JobState::Failed, Some((ErrorCode::Health, msg)));
        }
        Err(e) => {
            agent.sup.stop(id).await;
            let _ = agent.finish_active(JobState::Failed, Some(io_fail(e)));
        }
    }
}

/// Mark `slot` bad and point `current` back at `prev`. Returns false if there is no previous
/// version (first install): then `current` is removed so nothing starts the bad version.
fn restore_previous(
    agent: &Agent,
    id: &str,
    slot: Slot,
    prev: Option<Slot>,
) -> anyhow::Result<bool> {
    let lay = &agent.layout;
    let mut slots: SlotsFile = read_json(&lay.slots_file(id))?;
    slots.set_status(slot, SlotStatus::Bad);
    let target = prev.filter(|p| *p != slot && slots.get(*p).is_some());
    match target {
        Some(p) => {
            swap_symlink(&lay.current(id), p.name())?;
            slots.committed = Some(p);
            slots.set_status(p, SlotStatus::Committed);
        }
        None => {
            let _ = fs::remove_file(lay.current(id));
            fsync_dir(&lay.app(id))?;
            slots.committed = None;
        }
    }
    write_json(&lay.slots_file(id), &slots)?;
    Ok(target.is_some())
}

/// For a manual rollback of `id`: the committed slot `(slot, version, version_code)` and the
/// previous slot to return to. Fails if there is none, or if the previous version is below the
/// anti-rollback floor (`E_ROLLBACK`).
pub(super) fn rollback_target(
    agent: &Agent,
    id: &str,
) -> Result<((Slot, String, u64), Slot), crate::rpc::RpcError> {
    use crate::rpc::RpcError;
    homing_format::manifest::validate_app_id(id)
        .map_err(|e| RpcError::plain(crate::rpc::INVALID_PARAMS, e.to_string()))?;
    let slots: SlotsFile = read_json(&agent.layout.slots_file(id))
        .map_err(|e| RpcError::spec(ErrorCode::Io, format!("{e:#}")))?;
    let not_possible = |m: String| RpcError::plain(crate::rpc::INVALID_PARAMS, m);
    let current = slots
        .committed
        .ok_or_else(|| not_possible(format!("{id} has no committed version")))?;
    let cur = slots
        .get(current)
        .ok_or_else(|| not_possible(format!("{id} has no committed version")))?;
    let prev = current.other();
    let p = slots
        .get(prev)
        .filter(|i| i.status == SlotStatus::Previous)
        .ok_or_else(|| not_possible(format!("{id} has no previous version to roll back to")))?;
    let floors: crate::layout::Floors = read_json(&agent.layout.floors()).unwrap_or_default();
    let floor = floors.get(id).copied().unwrap_or(0);
    if p.version_code < floor {
        return Err(RpcError::spec(
            ErrorCode::Rollback,
            format!(
                "previous version {} (version_code {}) is below the anti-rollback floor {floor}",
                p.version, p.version_code
            ),
        ));
    }
    Ok(((current, cur.version.clone(), cur.version_code), prev))
}

/// Run a queued manual rollback: re-check it, then mark the current version bad and switch back.
async fn manual_rollback(agent: &Arc<Agent>, id: &str) {
    let (current, prev) = match rollback_target(agent, id) {
        Ok(t) => t,
        Err(e) => {
            let code = e
                .spec_code()
                .and_then(ErrorCode::parse)
                .unwrap_or(ErrorCode::Io);
            let _ = agent.finish_active(JobState::Failed, Some((code, e.message)));
            return;
        }
    };
    let _ = agent.update_active(|a| {
        a.job.state = JobState::RollingBack;
        a.job.slot = Some(current.0);
        a.prev = Some(prev);
    });
    match restore_previous(agent, id, current.0, Some(prev)) {
        Ok(_) => {
            agent.sup.restart(id).await;
            let _ = agent.finish_active(JobState::RolledBack, None);
        }
        Err(e) => {
            let _ = agent.finish_active(JobState::Failed, Some(io_fail(e)));
        }
    }
}

/// Finish or undo whatever job was active when the agent last stopped.
pub async fn recover(agent: &Arc<Agent>) -> anyhow::Result<()> {
    let active = agent.lock_state().active.clone();
    if let Some(a) = active {
        let st = a.job.state;
        eprintln!(
            "homing-agent: recovering job {} (was {:?})",
            a.job.job_id, st
        );
        match (st, a.job.app_id.as_deref(), a.job.slot) {
            (
                JobState::Switching | JobState::Trial | JobState::RollingBack,
                Some(id),
                Some(slot),
            ) => {
                let slots: SlotsFile = read_json(&agent.layout.slots_file(id))?;
                if slots.committed == Some(slot) {
                    // Commit was written; only the bookkeeping was interrupted.
                    agent.finish_active(JobState::Committed, None)?;
                } else {
                    let had_prev = restore_previous(agent, id, slot, a.prev)?;
                    let msg = "agent restarted before the new version was committed".to_owned();
                    let state = if had_prev {
                        JobState::RolledBack
                    } else {
                        JobState::Failed
                    };
                    agent.finish_active(state, Some((ErrorCode::Health, msg)))?;
                }
            }
            (s, _, _) if s.is_terminal() => {
                let err = a.job.error.as_ref().map(|e| {
                    (
                        ErrorCode::parse(&e.code).unwrap_or(ErrorCode::Io),
                        e.message.clone(),
                    )
                });
                agent.finish_active(s, err)?;
            }
            _ => {
                // Before switching: the committed slot was never touched.
                agent.finish_active(
                    JobState::Failed,
                    Some((
                        ErrorCode::Io,
                        "agent restarted before the install was switched".into(),
                    )),
                )?;
            }
        }
    }
    // Remove leftovers from interrupted extractions.
    if let Ok(rd) = fs::read_dir(agent.layout.apps()) {
        for app in rd.flatten() {
            for leftover in ["a.tmp", "b.tmp", "a.old", "b.old", "current.tmp"] {
                let p = app.path().join(leftover);
                if p.is_dir() {
                    let _ = fs::remove_dir_all(&p);
                } else if p.exists() || p.is_symlink() {
                    let _ = fs::remove_file(&p);
                }
            }
        }
    }
    // Queued jobs whose package file is gone cannot run.
    let missing: Vec<super::Queued> = {
        let mut s = agent.lock_state();
        let mut gone = vec![];
        for q in std::mem::take(&mut s.queued) {
            if q.job.kind == JobKind::Rollback || q.file.is_file() {
                s.queued.push_back(q);
            } else {
                gone.push(q);
            }
        }
        gone
    };
    for q in missing {
        let mut job = q.job;
        job.state = JobState::Failed;
        job.error = Some(super::JobError {
            code: ErrorCode::Io.as_str().into(),
            message: "package file disappeared while queued".into(),
        });
        eprintln!("homing-agent: job {} lost its package file", job.job_id);
        agent.lock_state().history.push_back(job);
    }
    agent.save()?;
    // Leftovers in incoming/: interrupted copies and downloads (*.part), downloads that were
    // never queued (http-*), and job files no queued job refers to.
    // Compared by file name: queued paths are canonical, read_dir paths may not be.
    let keep: std::collections::HashSet<std::ffi::OsString> = agent
        .lock_state()
        .queued
        .iter()
        .filter_map(|q| q.file.file_name().map(ToOwned::to_owned))
        .collect();
    if let Ok(rd) = fs::read_dir(agent.layout.incoming()) {
        for f in rd.flatten() {
            let n = f.file_name().to_string_lossy().into_owned();
            let stale = n.ends_with(".part")
                || n.starts_with("http-")
                || (n.starts_with("job-") && !keep.contains(&f.file_name()));
            if stale {
                let _ = fs::remove_file(f.path());
            }
        }
    }
    Ok(())
}

/// Start every app that has a committed slot and autostart enabled.
pub fn autostart(agent: &Arc<Agent>) {
    let Ok(rd) = fs::read_dir(agent.layout.apps()) else {
        return;
    };
    for app in rd.flatten() {
        let Some(id) = app.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if homing_format::manifest::validate_app_id(&id).is_err() {
            continue;
        }
        let slots: SlotsFile = match read_json(&agent.layout.slots_file(&id)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("homing-agent: {id}: {e:#}");
                continue;
            }
        };
        let committed = slots.committed;
        if committed.is_some()
            && current_slot(&agent.layout, &id) == committed
            && agent.cfg.policy(&id).autostart
        {
            agent.sup.start(&id);
        }
    }
}

fn free_bytes(dir: &Path) -> std::io::Result<u64> {
    let s = nix::sys::statvfs::statvfs(dir).map_err(std::io::Error::from)?;
    Ok((s.blocks_available() as u64).saturating_mul(s.fragment_size() as u64))
}
