//! `homingctl`: admin CLI for the agent.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use homing_agent::config::Config;
use homing_agent::layout::Layout;
use homing_agent::rpc::Client;
use serde_json::{Value, json};

#[derive(Parser)]
#[command(name = "homingctl", version, about = "Control the homing agent")]
struct Args {
    /// Config file (default: $HOMING_CONFIG or /etc/homing/agent.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show apps, slots and jobs.
    Status {
        /// Print the full status as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Copy a package into incoming/ and install it.
    Install {
        /// Package file (.homing).
        file: PathBuf,
        /// Wait until the job finishes; exit non-zero unless it committed.
        #[arg(long)]
        wait: bool,
    },
    /// Return an app to its previous version, marking the current one bad.
    Rollback {
        /// App id.
        id: String,
        /// Wait until the rollback finishes; exit non-zero unless it succeeded.
        #[arg(long)]
        wait: bool,
    },
    /// Check the HTTP update source now and queue any update it offers.
    Check {
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show one job.
    Job {
        /// Job id, as printed by `install`.
        job_id: String,
        /// Wait until the job finishes; exit non-zero unless it committed.
        #[arg(long)]
        wait: bool,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("homingctl: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<ExitCode> {
    let args = Args::parse();
    let cfg = Config::load(&Config::path_from(args.config))?;
    let mut c = Client::connect(&cfg.socket)?;
    match args.cmd {
        Cmd::Status { json } => {
            let s = c.call("status", Value::Null)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                print_status(&s);
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Install { file, wait } => {
            let staged = copy_to_incoming(&cfg, &file)?;
            let r = c.call("install", json!({ "path": staged, "source": "homingctl" }));
            let r = match r {
                Ok(r) => r,
                Err(e) => {
                    let _ = fs::remove_file(&staged);
                    return Err(e);
                }
            };
            let id = r["job_id"].as_str().context("no job_id")?.to_owned();
            println!("job {id}");
            if wait {
                wait_job(&mut c, &id, "committed")
            } else {
                Ok(ExitCode::SUCCESS)
            }
        }
        Cmd::Rollback { id, wait } => {
            let r = c.call("rollback", json!({ "id": id }))?;
            let job_id = r["job_id"].as_str().context("no job_id")?.to_owned();
            println!("job {job_id}");
            if wait {
                wait_job(&mut c, &job_id, "rolled_back")
            } else {
                Ok(ExitCode::SUCCESS)
            }
        }
        Cmd::Check { json } => {
            let r = c.call("source.check", Value::Null)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                print_http(&r);
            }
            Ok(if r["last_error"].is_null() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            })
        }
        Cmd::Job { job_id, wait } => {
            if wait {
                return wait_job(&mut c, &job_id, "committed");
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&c.call("job.get", json!({ "job_id": job_id }))?)?
            );
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Copy as `<name>.part`, fsync, then rename so the agent never sees a partial file.
fn copy_to_incoming(cfg: &Config, file: &Path) -> Result<PathBuf> {
    let incoming = Layout::new(&cfg.data_dir).incoming();
    let name = file
        .file_name()
        .context("not a file")?
        .to_string_lossy()
        .into_owned();
    let part = incoming.join(format!("{name}.part"));
    let dest = incoming.join(&name);
    fs::copy(file, &part).with_context(|| format!("cannot copy into {}", incoming.display()))?;
    fs::File::open(&part)?.sync_all()?;
    fs::rename(&part, &dest)?;
    Ok(dest)
}

/// Poll a job until it finishes. Exit 0 if it ended in `success`, else 2.
fn wait_job(c: &mut Client, id: &str, success: &str) -> Result<ExitCode> {
    let mut last = String::new();
    loop {
        let j = c.call("job.get", json!({ "job_id": id }))?;
        let state = j["state"].as_str().unwrap_or("?").to_owned();
        if state != last {
            println!("  {state}");
            let _ = std::io::stdout().flush();
            last = state.clone();
        }
        match state.as_str() {
            s if s == success => return Ok(ExitCode::SUCCESS),
            "committed" | "rolled_back" | "failed" => {
                let e = &j["error"];
                if e.is_object() {
                    println!(
                        "  error: {} {}",
                        e["code"].as_str().unwrap_or("?"),
                        e["message"].as_str().unwrap_or("")
                    );
                }
                return Ok(ExitCode::from(2));
            }
            "?" => bail!("bad job object: {j}"),
            _ => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

/// Print the result of an HTTP source check.
fn print_http(h: &Value) {
    println!(
        "http source {}  last check {}",
        h["index_url"].as_str().unwrap_or("?"),
        h["last_check"]
            .as_u64()
            .map_or_else(|| "never".into(), |t| format!("at {t} (device clock)"))
    );
    if let Some(e) = h["last_error"].as_str() {
        println!("    error: {e}");
    }
    for a in h["apps"].as_array().cloned().unwrap_or_default() {
        let mut line = format!(
            "    {}: {}",
            a["id"].as_str().unwrap_or("?"),
            a["result"].as_str().unwrap_or("?")
        );
        if let Some(vc) = a["version_code"].as_u64() {
            line += &format!(" (version_code {vc})");
        }
        if let Some(j) = a["job_id"].as_str() {
            line += &format!(" job {j}");
        }
        if let Some(m) = a["message"].as_str() {
            line += &format!(": {m}");
        }
        println!("{line}");
    }
}

fn print_status(s: &Value) {
    println!(
        "agent {}  engine {}  dev_mode {}",
        s["agent_version"].as_str().unwrap_or("?"),
        s["engine"]["engine_hash"]
            .as_str()
            .map(|h| &h[..h.len().min(12)])
            .unwrap_or("?"),
        s["dev_mode"]
    );
    let apps = s["apps"].as_array().cloned().unwrap_or_default();
    if apps.is_empty() {
        println!("no apps installed");
    }
    for a in apps {
        let p = &a["process"];
        println!(
            "{}  current={}  running={}  ready={}  exits={}",
            a["id"].as_str().unwrap_or("?"),
            a["current"].as_str().unwrap_or("-"),
            p["pid"]
                .as_u64()
                .map(|p| format!("pid {p}"))
                .unwrap_or_else(|| "no".into()),
            p["ready"],
            p["exits"],
        );
        for slot in ["a", "b"] {
            let i = &a["slots"][slot];
            if i.is_object() {
                println!(
                    "    slot {slot}: {} (code {}) {}",
                    i["version"].as_str().unwrap_or("?"),
                    i["version_code"],
                    i["status"].as_str().unwrap_or("?")
                );
            }
        }
    }
    if s["sources"]["http"].is_object() {
        print_http(&s["sources"]["http"]);
    }
    for j in s["jobs"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .rev()
        .take(5)
    {
        println!(
            "job {}  {} {}  {}{}",
            &j["job_id"].as_str().unwrap_or("?")[..8.min(j["job_id"].as_str().unwrap_or("").len())],
            j["app_id"].as_str().unwrap_or("?"),
            j["version"].as_str().unwrap_or("?"),
            j["state"].as_str().unwrap_or("?"),
            j["error"]["code"]
                .as_str()
                .map(|c| format!(" ({c})"))
                .unwrap_or_default(),
        );
    }
}
