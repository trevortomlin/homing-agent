//! `/etc/homing/agent.toml`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Default config location.
pub const DEFAULT_CONFIG: &str = "/etc/homing/agent.toml";

/// Agent configuration. Unknown keys are an error (fail closed).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "d_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "d_socket")]
    pub socket: PathBuf,
    #[serde(default = "d_keys_dir")]
    pub keys_dir: PathBuf,
    #[serde(default = "d_engine_file")]
    pub engine_file: PathBuf,
    /// Dev mode: honor dev-*.pub keys and debug builds.
    #[serde(default)]
    pub dev_mode: bool,
    /// Free space to keep beyond the package itself.
    #[serde(default = "d_min_free_mb")]
    pub min_free_mb: u64,
    /// Members of this group may use admin API calls (install, rollback, source.check), in
    /// addition to root and the agent's own user. Ignored if the group does not exist.
    #[serde(default = "d_admin_group")]
    pub admin_group: Option<String>,
    /// MVP process supervisor: path of the `homing-launch` binary.
    #[serde(default = "d_launcher")]
    pub launcher: PathBuf,
    /// Delay before restarting a crashed app.
    #[serde(default = "d_restart_ms")]
    pub restart_delay_ms: u64,
    #[serde(default)]
    pub embedders: Embedders,
    #[serde(default)]
    pub apps: BTreeMap<String, AppPolicy>,
    /// Update sources besides `homingctl install`.
    #[serde(default)]
    pub source: Sources,
}

/// Update sources.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sources {
    /// Poll a signed index over HTTP(S).
    #[serde(default)]
    pub http: Option<HttpSource>,
}

/// `[source.http]`: poll a signed `index.json` and install updates from it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpSource {
    /// URL of `index.json` (`http://` or `https://`); `index.json.sig` is fetched from beside it.
    pub index_url: String,
    /// Seconds between checks.
    #[serde(default = "d_interval")]
    pub interval_s: u64,
    /// Up to this many seconds of random delay added to every check, so a fleet does not poll
    /// at the same moment.
    #[serde(default = "d_jitter")]
    pub jitter_s: u64,
    /// Timeout for each HTTP request, including a whole package download.
    #[serde(default = "d_timeout")]
    pub timeout_s: u64,
}

/// Per-embedder settings.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Embedders {
    #[serde(default, rename = "flutter-pi")]
    pub flutter_pi: FlutterPi,
}

/// flutter-pi adapter settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlutterPi {
    /// flutter-pi binary.
    #[serde(default = "d_flutter_pi")]
    pub binary: PathBuf,
    /// Engine's icudtl.dat, linked into slots whose package has none.
    #[serde(default)]
    pub icudtl: Option<PathBuf>,
}

impl Default for FlutterPi {
    fn default() -> Self {
        Self {
            binary: d_flutter_pi(),
            icudtl: None,
        }
    }
}

/// Per-app device policy.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppPolicy {
    #[serde(default = "d_true")]
    pub autostart: bool,
    /// Prefix allowlist for `runtime.args`; empty = no extra args allowed.
    #[serde(default)]
    pub allowed_args: Vec<String>,
}

impl Default for AppPolicy {
    fn default() -> Self {
        Self {
            autostart: true,
            allowed_args: vec![],
        }
    }
}

fn d_data_dir() -> PathBuf {
    "/data/homing".into()
}
fn d_socket() -> PathBuf {
    "/run/homing/agent.sock".into()
}
fn d_keys_dir() -> PathBuf {
    "/etc/homing/keys".into()
}
fn d_engine_file() -> PathBuf {
    "/etc/homing/engine.json".into()
}
fn d_admin_group() -> Option<String> {
    Some("homing-admin".into())
}
fn d_min_free_mb() -> u64 {
    64
}
fn d_launcher() -> PathBuf {
    "/usr/libexec/homing/homing-launch".into()
}
fn d_restart_ms() -> u64 {
    2000
}
fn d_flutter_pi() -> PathBuf {
    "/usr/bin/flutter-pi".into()
}
fn d_true() -> bool {
    true
}
fn d_interval() -> u64 {
    3600
}
fn d_jitter() -> u64 {
    600
}
fn d_timeout() -> u64 {
    600
}

impl Config {
    /// Load and validate a config file. Relative paths are resolved against its directory.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let mut c: Config =
            toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new("/"));
        for p in [
            &mut c.data_dir,
            &mut c.socket,
            &mut c.keys_dir,
            &mut c.engine_file,
            &mut c.launcher,
            &mut c.embedders.flutter_pi.binary,
        ] {
            if p.is_relative() {
                *p = base.join(&*p);
            }
        }
        for id in c.apps.keys() {
            homing_format::manifest::validate_app_id(id)
                .map_err(|e| anyhow::anyhow!("[apps.{id:?}]: {e}"))?;
        }
        if c.restart_delay_ms > 600_000 {
            bail!("restart_delay_ms too large");
        }
        if let Some(h) = &c.source.http {
            let u = url::Url::parse(&h.index_url)
                .map_err(|e| anyhow::anyhow!("[source.http] index_url: {e}"))?;
            if !matches!(u.scheme(), "http" | "https") || u.host().is_none() {
                bail!("[source.http] index_url must be an http:// or https:// URL");
            }
            if h.interval_s < 10 || h.timeout_s == 0 {
                bail!("[source.http] interval_s must be at least 10 and timeout_s positive");
            }
        }
        Ok(c)
    }

    /// Path to load config from: `--config`, else `$HOMING_CONFIG`, else the default.
    pub fn path_from(arg: Option<PathBuf>) -> PathBuf {
        arg.or_else(|| std::env::var_os("HOMING_CONFIG").map(PathBuf::from))
            .unwrap_or_else(|| DEFAULT_CONFIG.into())
    }

    /// Policy for an app (default if not configured).
    pub fn policy(&self, id: &str) -> AppPolicy {
        self.apps.get(id).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_relative_paths() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("agent.toml");
        std::fs::write(
            &p,
            "data_dir = \"data\"\n[apps.\"com.example.a\"]\nallowed_args = [\"--rotation\"]\n",
        )
        .unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!(c.data_dir, d.path().join("data"));
        assert_eq!(c.socket, PathBuf::from("/run/homing/agent.sock"));
        assert!(c.policy("com.example.a").autostart);
        assert_eq!(c.policy("com.example.a").allowed_args, ["--rotation"]);
        assert!(c.policy("com.example.b").allowed_args.is_empty());
    }

    #[test]
    fn http_source() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("agent.toml");
        std::fs::write(
            &p,
            "[source.http]\nindex_url = \"http://updates.example.com/a/index.json\"\n",
        )
        .unwrap();
        let h = Config::load(&p).unwrap().source.http.unwrap();
        assert_eq!((h.interval_s, h.jitter_s, h.timeout_s), (3600, 600, 600));
        for bad in [
            "index_url = \"ftp://x/index.json\"",
            "index_url = \"file:///etc/index.json\"",
            "index_url = \"not a url\"",
            "index_url = \"http://x/i.json\"\ninterval_s = 1",
            "index_url = \"http://x/i.json\"\nextra = 1",
        ] {
            std::fs::write(&p, format!("[source.http]\n{bad}\n")).unwrap();
            assert!(Config::load(&p).is_err(), "{bad}");
        }
        std::fs::write(&p, "").unwrap();
        assert!(Config::load(&p).unwrap().source.http.is_none());
    }

    #[test]
    fn unknown_keys_rejected() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("agent.toml");
        std::fs::write(&p, "datadir = \"/x\"\n").unwrap();
        assert!(Config::load(&p).is_err());
        std::fs::write(&p, "[apps.\"BAD\"]\n").unwrap();
        assert!(Config::load(&p).is_err());
    }
}
