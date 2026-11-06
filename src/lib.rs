//! homing on-device components: agent daemon, launcher and admin CLI.
//!
//! MVP structure: one package with a shared library and four binaries
//! (`homing-agent`, `homing-launch`, `homingctl`, `homing-fake-embedder`).

pub mod config;
pub mod daemon;
pub mod launch;
pub mod layout;
pub mod rpc;
