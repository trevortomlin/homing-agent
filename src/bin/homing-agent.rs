//! `homing-agent`: the on-device daemon.

use std::path::PathBuf;

use clap::Parser;
use homing_agent::config::Config;

#[derive(Parser)]
#[command(
    name = "homing-agent",
    version,
    about = "homing on-device update agent"
)]
struct Args {
    /// Config file (default: $HOMING_CONFIG or /etc/homing/agent.toml).
    #[arg(long)]
    config: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let path = Config::path_from(args.config);
    let cfg = Config::load(&path)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(homing_agent::daemon::run(cfg, path))
}
