//! `homing-launch`: resolve the current slot, build the env contract and exec the
//! embedder.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use homing_format::Manifest;
use homing_format::manifest::{BuildMode, Embedder};

use crate::config::Config;
use crate::layout::{Layout, Slot, SlotsFile, read_json};

/// Directory inside a slot holding the verified manifest and signature.
pub const META_DIR: &str = ".homing-meta";

/// Everything needed to exec an app.
#[derive(Debug)]
pub struct LaunchPlan {
    pub argv: Vec<OsString>,
    pub env: Vec<(String, String)>,
}

/// Resolve and validate everything for launching `app_id`. Does not exec.
pub fn plan(cfg: &Config, app_id: &str) -> Result<LaunchPlan> {
    homing_format::manifest::validate_app_id(app_id)?;
    let layout = Layout::new(&cfg.data_dir);
    let app_dir = layout.app(app_id);
    // Resolve `current` exactly once; everything below uses the real slot path.
    let slot_dir = std::fs::canonicalize(layout.current(app_id))
        .with_context(|| format!("{app_id} has no current slot"))?;
    let real_app_dir = std::fs::canonicalize(&app_dir)?;
    let slot = slot_dir
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(Slot::parse)
        .filter(|_| slot_dir.parent() == Some(real_app_dir.as_path()))
        .ok_or_else(|| {
            anyhow!(
                "current points outside the app's slots: {}",
                slot_dir.display()
            )
        })?;

    let mbytes = std::fs::read(slot_dir.join(META_DIR).join("manifest.json"))
        .context("slot has no manifest")?;
    let slots: SlotsFile = read_json(&layout.slots_file(app_id))?;
    let info = slots
        .get(slot)
        .ok_or_else(|| anyhow!("slots.json has no entry for slot {}", slot.name()))?;
    let digest = homing_format::package::sha256_reader(&mbytes[..])?.0;
    if digest != info.manifest_sha256 {
        bail!(
            "manifest in slot {} does not match slots.json (corrupt slot)",
            slot.name()
        );
    }
    let m = Manifest::parse(&mbytes)?;
    if m.id != app_id {
        bail!("slot holds {}, not {app_id}", m.id);
    }

    let state_dir =
        std::fs::canonicalize(layout.state_dir(app_id)).context("app state dir missing")?;
    let mut env = vec![
        ("HOMING_APP_ID".into(), m.id.clone()),
        ("HOMING_APP_VERSION".into(), m.version.clone()),
        ("HOMING_APP_VERSION_CODE".into(), m.version_code.to_string()),
        ("HOMING_SLOT".into(), slot.name().into()),
        ("HOMING_APP_DIR".into(), path_str(&slot_dir)?),
        (
            "HOMING_BUNDLE_DIR".into(),
            path_str(&slot_dir.join("bundle"))?,
        ),
        (
            "HOMING_CONFIG_DIR".into(),
            path_str(&slot_dir.join("config"))?,
        ),
        ("HOMING_STATE_DIR".into(), path_str(&state_dir)?),
        ("HOMING_AGENT_SOCKET".into(), path_str(&cfg.socket)?),
    ];
    for (k, v) in &m.runtime.env {
        env.push((k.clone(), v.clone()));
    }

    let policy = cfg.policy(app_id);
    let extra: Vec<String> = m
        .runtime
        .args
        .iter()
        .filter(|a| {
            let ok = policy
                .allowed_args
                .iter()
                .any(|p| a.starts_with(p.as_str()));
            if !ok {
                eprintln!("homing-launch: dropping runtime arg {a:?} (not in allowed_args)");
            }
            ok
        })
        .cloned()
        .collect();

    let argv = adapter_argv(cfg, &slot_dir, &m, &extra)?;
    Ok(LaunchPlan { argv, env })
}

fn path_str(p: &Path) -> Result<String> {
    p.to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("non-UTF-8 path {}", p.display()))
}

/// Build argv for the manifest's embedder.
pub fn adapter_argv(
    cfg: &Config,
    slot: &Path,
    m: &Manifest,
    extra: &[String],
) -> Result<Vec<OsString>> {
    match m.runtime.embedder {
        Embedder::FlutterPi => {
            // flutter-pi [options] <asset bundle path> [engine options]
            // Verified with flutter-pi 8b86c52a (2026-09) in an aarch64 VM; not yet on a Pi.
            let mut argv: Vec<OsString> = vec![cfg.embedders.flutter_pi.binary.clone().into()];
            match m.build_mode {
                BuildMode::Release => argv.push("--release".into()),
                BuildMode::Profile => argv.push("--profile".into()),
                BuildMode::Debug => {}
                other => bail!("unsupported build mode {other:?}"),
            }
            argv.extend(extra.iter().map(OsString::from));
            argv.push(slot.join("bundle/flutter_assets").into());
            Ok(argv)
        }
        Embedder::IviHomescreen => bail!("the ivi-homescreen adapter is not implemented yet"),
        other => bail!("unsupported embedder {other:?}"),
    }
}

/// Install-time preparation inside a freshly extracted slot (before it is renamed into place).
pub fn prepare_slot(cfg: &Config, slot: &Path, m: &Manifest) -> Result<()> {
    match m.runtime.embedder {
        Embedder::FlutterPi => {
            // flutter-pi expects the AOT snapshot inside the asset bundle dir as `app.so`.
            if matches!(m.build_mode, BuildMode::Release | BuildMode::Profile) {
                let link = slot.join("bundle/flutter_assets/app.so");
                if !link.exists() {
                    std::os::unix::fs::symlink("../lib/libapp.so", &link)
                        .context("cannot link app.so")?;
                }
            }
            let icu = slot.join("bundle/icudtl.dat");
            if !icu.exists() {
                if let Some(engine_icu) = &cfg.embedders.flutter_pi.icudtl {
                    std::os::unix::fs::symlink(engine_icu, &icu)
                        .context("cannot link icudtl.dat")?;
                }
            }
            Ok(())
        }
        Embedder::IviHomescreen => bail!("the ivi-homescreen adapter is not implemented yet"),
        other => bail!("unsupported embedder {other:?}"),
    }
}

/// Default config path for the launcher binary.
pub fn default_config() -> PathBuf {
    Config::path_from(None)
}
