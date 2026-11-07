//! Socket server and method dispatch.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use homing_format::ErrorCode;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::Agent;
use crate::layout::{SlotsFile, current_slot, read_json};
use crate::rpc::{self, RpcError};

/// Bind the socket, replacing a stale one.
pub fn bind(path: &Path) -> Result<UnixListener> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            anyhow::bail!("another agent is already listening on {}", path.display());
        }
        fs::remove_file(path)?;
    }
    let l = UnixListener::bind(path).with_context(|| format!("cannot bind {}", path.display()))?;
    // Anyone may connect (apps run as other users); each method checks the caller itself.
    fs::set_permissions(path, fs::Permissions::from_mode(0o666))?;
    Ok(l)
}

pub async fn serve(agent: Arc<Agent>, listener: UnixListener) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let a = agent.clone();
        tokio::spawn(async move {
            if let Err(e) = connection(a, stream).await {
                eprintln!("homing-agent: connection error: {e:#}");
            }
        });
    }
}

/// Who is calling.
#[derive(Debug, Clone, Copy)]
struct Caller {
    uid: u32,
    pid: Option<u32>,
}

impl Caller {
    /// Admin = root or the agent's own user. Not yet supported: membership in a `homing-admin` group.
    fn is_admin(&self) -> bool {
        self.uid == 0 || self.uid == nix::unistd::geteuid().as_raw()
    }
}

async fn connection(agent: Arc<Agent>, stream: UnixStream) -> Result<()> {
    let cred = stream.peer_cred()?;
    let caller = Caller {
        uid: cred.uid(),
        pid: cred.pid().and_then(|p| u32::try_from(p).ok()),
    };
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r);
    let mut line = String::new();
    loop {
        line.clear();
        let n = (&mut lines)
            .take(rpc::MAX_LINE as u64 + 1)
            .read_line(&mut line)
            .await?;
        if n == 0 {
            return Ok(());
        }
        if n > rpc::MAX_LINE {
            let resp = rpc::response(
                Value::Null,
                Err(RpcError::plain(rpc::INVALID_REQUEST, "request too large")),
            );
            w.write_all(format!("{resp}\n").as_bytes()).await?;
            return Ok(());
        }
        let resp = match serde_json::from_str::<rpc::Request>(&line) {
            Err(e) => rpc::response(
                Value::Null,
                Err(RpcError::plain(rpc::PARSE_ERROR, e.to_string())),
            ),
            Ok(req) if req.jsonrpc != "2.0" => rpc::response(
                req.id,
                Err(RpcError::plain(
                    rpc::INVALID_REQUEST,
                    "jsonrpc must be \"2.0\"",
                )),
            ),
            Ok(req) => {
                let res = dispatch(&agent, caller, &req.method, req.params).await;
                rpc::response(req.id, res)
            }
        };
        w.write_all(format!("{resp}\n").as_bytes()).await?;
    }
}

fn perm(msg: &str) -> RpcError {
    RpcError::spec(ErrorCode::Perm, msg)
}

fn param_str<'a>(p: &'a Value, k: &str) -> Result<&'a str, RpcError> {
    p.get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::plain(rpc::INVALID_PARAMS, format!("missing string param {k:?}")))
}

async fn dispatch(
    agent: &Arc<Agent>,
    caller: Caller,
    method: &str,
    params: Value,
) -> Result<Value, RpcError> {
    match method {
        "status" => Ok(status(agent)),
        "rollback" => {
            if !caller.is_admin() {
                return Err(perm("rollback requires admin"));
            }
            let id = param_str(&params, "id")?;
            let job_id = agent.request_rollback(id)?;
            Ok(json!({ "job_id": job_id }))
        }
        "source.check" => {
            if !caller.is_admin() {
                return Err(perm("source.check requires admin"));
            }
            match super::http::check_now(agent).await {
                Ok(s) => Ok(json!(s)),
                Err(e) => Err(RpcError::plain(rpc::INVALID_REQUEST, format!("{e:#}"))),
            }
        }
        "job.get" => {
            let id = param_str(&params, "job_id")?;
            agent
                .job(id)
                .map(|j| json!(j))
                .ok_or_else(|| RpcError::plain(rpc::INVALID_PARAMS, format!("unknown job {id}")))
        }
        "install" => {
            if !caller.is_admin() {
                return Err(perm("install requires admin"));
            }
            let path = param_str(&params, "path")?;
            let source = params
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or("local");
            install(agent, Path::new(path), source)
        }
        "app.ready" => {
            let pid = caller.pid.ok_or_else(|| perm("cannot identify caller"))?;
            match agent.sup.mark_ready_by_pid(pid) {
                Some(id) => {
                    eprintln!("homing-agent: {id} reported ready");
                    Ok(json!({}))
                }
                None => Err(perm("caller is not a managed app")),
            }
        }
        "app.info" => {
            let pid = caller.pid.ok_or_else(|| perm("cannot identify caller"))?;
            let id = agent
                .sup
                .app_for_pid(pid)
                .ok_or_else(|| perm("caller is not a managed app"))?;
            Ok(app_status(agent, &id))
        }
        _ => Err(RpcError::plain(
            rpc::METHOD_NOT_FOUND,
            format!("unknown method {method}"),
        )),
    }
}

fn install(agent: &Arc<Agent>, path: &Path, source: &str) -> Result<Value, RpcError> {
    let job_id = agent.submit(path, source, None)?;
    Ok(json!({ "job_id": job_id }))
}

fn app_status(agent: &Agent, id: &str) -> Value {
    let slots: SlotsFile = read_json(&agent.layout.slots_file(id)).unwrap_or_default();
    json!({
        "id": id,
        "current": current_slot(&agent.layout, id),
        "slots": slots,
        "process": agent.sup.status(id),
    })
}

fn status(agent: &Arc<Agent>) -> Value {
    let mut apps = vec![];
    if let Ok(rd) = fs::read_dir(agent.layout.apps()) {
        let mut ids: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .collect();
        ids.sort();
        for id in ids {
            if homing_format::manifest::validate_app_id(&id).is_ok() {
                apps.push(app_status(agent, &id));
            }
        }
    }
    let active = agent.lock_state().active.as_ref().map(|a| a.job.clone());
    json!({
        "agent_version": env!("CARGO_PKG_VERSION"),
        "dev_mode": agent.cfg.dev_mode,
        "engine": agent.device,
        "apps": apps,
        "current_job": active,
        "jobs": agent.jobs(),
        "sources": {
            "http": agent.cfg.source.http.as_ref().map(|_| agent.lock_http().clone()),
        },
    })
}
