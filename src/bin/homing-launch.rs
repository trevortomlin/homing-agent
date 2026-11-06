//! `homing-launch <app-id>`: exec'd by the service manager for each app.

use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use clap::Parser;
use homing_agent::config::Config;

#[derive(Parser)]
#[command(name = "homing-launch", version, about = "Start a homing-managed app")]
struct Args {
    /// Config file (default: $HOMING_CONFIG or /etc/homing/agent.toml).
    #[arg(long)]
    config: Option<PathBuf>,
    /// App id to launch (its `current` slot).
    app_id: String,
    /// Print the command and environment instead of executing.
    #[arg(long)]
    dry_run: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let run = || -> anyhow::Result<()> {
        let cfg = Config::load(&Config::path_from(args.config.clone()))?;
        let plan = homing_agent::launch::plan(&cfg, &args.app_id)?;
        if !args.dry_run {
            // MVP supervisor: if the agent dies, the app must not linger as an orphan (systemd
            // would kill the unit). The setting survives execve into the embedder.
            nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGTERM)?;
            if nix::unistd::getppid().as_raw() == 1 {
                anyhow::bail!("agent already exited");
            }
        }
        if args.dry_run {
            for (k, v) in &plan.env {
                println!("{k}={v}");
            }
            println!("{:?}", plan.argv);
            return Ok(());
        }
        let mut cmd = Command::new(&plan.argv[0]);
        cmd.args(&plan.argv[1..]);
        for (k, v) in &plan.env {
            cmd.env(k, v);
        }
        // Only returns on failure.
        Err(cmd.exec().into())
    };
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("homing-launch {}: {e:#}", args.app_id);
            ExitCode::from(1)
        }
    }
}
