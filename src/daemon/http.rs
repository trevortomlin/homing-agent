//! HTTP update source: poll a signed `index.json`, pick the build for this device, download it
//! into `incoming/` and queue an install.
//!
//! The index and each package are verified with the agent's trust store, so plain `http://`
//! is safe: TLS only adds privacy. No decision depends on the device clock, except that
//! `https://` certificate checks need a roughly correct one.

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Once};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use homing_format::index::MAX_INDEX;
use homing_format::sig::MAX_SIG_DOC;
use homing_format::{Selection, verify_index};
use serde::Serialize;
use url::Url;

use super::{Agent, JobState};
use crate::config::HttpSource;
use crate::layout::{Floors, SlotsFile, read_json};

/// Outcome of the last check, shown by `status`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct HttpStatus {
    /// The configured index URL.
    pub index_url: String,
    /// Device clock (Unix seconds) at the end of the last check; informational only.
    pub last_check: Option<u64>,
    /// Why the last check failed as a whole (index unreachable or invalid), if it did.
    pub last_error: Option<String>,
    /// Per-app outcome of the last check.
    pub apps: Vec<AppResult>,
}

/// What a check did for one app.
#[derive(Debug, Clone, Serialize)]
pub struct AppResult {
    pub id: String,
    /// `up_to_date`, `queued`, `no_compatible_build`, `busy`, `skipped_failed` or `error`.
    pub result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_code: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl AppResult {
    fn new(id: &str, result: &str) -> Self {
        Self {
            id: id.into(),
            result: result.into(),
            version_code: None,
            job_id: None,
            message: None,
        }
    }
}

/// Poll forever (if configured). Checks also run on demand via the `source.check` RPC.
pub async fn run(agent: Arc<Agent>) {
    let Some(cfg) = agent.cfg.source.http.clone() else {
        return;
    };
    // First check after a short random delay, so a fleet that boots together spreads out.
    let mut delay = Duration::from_secs(10 + jitter(cfg.jitter_s));
    loop {
        tokio::time::sleep(delay).await;
        let _ = check_now(&agent).await;
        delay = Duration::from_secs(cfg.interval_s + jitter(cfg.jitter_s));
    }
}

/// Run one check now (serialized with the periodic ones) and return its status.
pub async fn check_now(agent: &Arc<Agent>) -> Result<HttpStatus> {
    let cfg = agent
        .cfg
        .source
        .http
        .clone()
        .ok_or_else(|| anyhow!("no [source.http] configured"))?;
    let _guard = agent.http_check.lock().await;
    let a = agent.clone();
    let status = tokio::task::spawn_blocking(move || check(&a, &cfg))
        .await
        .context("check task failed")?;
    if let Some(e) = &status.last_error {
        eprintln!("homing-agent: http source: {e}");
    }
    for r in &status.apps {
        if r.result != "up_to_date" {
            eprintln!(
                "homing-agent: http source: {} {}{}",
                r.id,
                r.result,
                r.message
                    .as_deref()
                    .map(|m| format!(": {m}"))
                    .unwrap_or_default()
            );
        }
    }
    *agent.lock_http() = status.clone();
    Ok(status)
}

fn check(agent: &Agent, cfg: &HttpSource) -> HttpStatus {
    let mut status = HttpStatus {
        index_url: cfg.index_url.clone(),
        ..Default::default()
    };
    match check_index(agent, cfg) {
        Ok(apps) => status.apps = apps,
        Err(e) => status.last_error = Some(format!("{e:#}")),
    }
    status.last_check = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs());
    status
}

fn check_index(agent: &Agent, cfg: &HttpSource) -> Result<Vec<AppResult>> {
    let http = client(cfg);
    let index_url = Url::parse(&cfg.index_url).context("index_url")?;
    let mut sig_url = index_url.clone();
    sig_url.set_path(&format!("{}.sig", index_url.path()));
    let doc = get_bytes(&http, &index_url, MAX_INDEX as u64)?;
    let sig = get_bytes(&http, &sig_url, MAX_SIG_DOC as u64)?;
    let index = verify_index(&doc, &sig, &agent.trust)
        .map_err(|e| anyhow!("index rejected: {e}"))?
        .index;

    let floors: Floors = read_json(&agent.layout.floors()).unwrap_or_default();
    let mut results = vec![];
    for id in app_ids(agent) {
        let installed = installed_version_code(agent, &id);
        let floor = floors.get(&id).copied().unwrap_or(0);
        let r = match index.select(&id, installed, floor, &agent.device) {
            Selection::Update { app, build } => {
                let mut r = AppResult::new(&id, "queued");
                r.version_code = Some(app.version_code);
                if agent.has_job_for(&id) {
                    r.result = "busy".into();
                    r.message = Some("an install for this app is already queued or running".into());
                } else if agent.failed_before(&id, app.version_code) {
                    r.result = "skipped_failed".into();
                    r.message = Some(format!(
                        "version_code {} failed to install before; publish a newer version",
                        app.version_code
                    ));
                } else {
                    match fetch(agent, cfg, &http, &index_url, &id, app.version_code, build) {
                        Ok(job_id) => r.job_id = Some(job_id),
                        Err(e) => {
                            r.result = "error".into();
                            r.message = Some(format!("{e:#}"));
                        }
                    }
                }
                r
            }
            Selection::NoCompatibleBuild {
                newest_version_code,
            } => {
                let mut r = AppResult::new(&id, "no_compatible_build");
                r.version_code = Some(newest_version_code);
                r.message =
                    Some("newer versions exist, but none has a build for this device".into());
                r
            }
            _ => AppResult::new(&id, "up_to_date"),
        };
        results.push(r);
    }
    Ok(results)
}

/// Apps to check: those installed on the device and those configured in `[apps."<id>"]`.
fn app_ids(agent: &Agent) -> BTreeSet<String> {
    let mut ids: BTreeSet<String> = agent.cfg.apps.keys().cloned().collect();
    if let Ok(rd) = fs::read_dir(agent.layout.apps()) {
        for e in rd.flatten() {
            if let Some(name) = e.file_name().to_str() {
                if homing_format::manifest::validate_app_id(name).is_ok() {
                    ids.insert(name.to_owned());
                }
            }
        }
    }
    ids
}

/// `version_code` of the committed slot, or 0 if the app is not installed.
fn installed_version_code(agent: &Agent, id: &str) -> u64 {
    let slots: SlotsFile = read_json(&agent.layout.slots_file(id)).unwrap_or_default();
    slots
        .committed
        .and_then(|s| slots.get(s))
        .map_or(0, |i| i.version_code)
}

/// Download a build into incoming/, check its size and sha256, and queue the install.
#[allow(clippy::too_many_arguments)]
fn fetch(
    agent: &Agent,
    cfg: &HttpSource,
    http: &ureq::Agent,
    index_url: &Url,
    id: &str,
    version_code: u64,
    build: &homing_format::IndexBuild,
) -> Result<String> {
    let url = index_url
        .join(&build.url)
        .with_context(|| format!("bad package url {:?}", build.url))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("package url {url} is not http(s)");
    }
    let need = build.size + agent.cfg.min_free_mb * 1024 * 1024;
    let free = free_bytes(&agent.layout.root)?;
    if free < need {
        bail!(
            "E_SPACE: need {} MiB free for the download, have {} MiB",
            need / (1024 * 1024),
            free / (1024 * 1024)
        );
    }
    let incoming = agent.layout.incoming();
    let name = format!("http-{id}-{version_code}.homing");
    let part = incoming.join(format!("{name}.part"));
    let dest = incoming.join(&name);
    let res = download(http, &url, &part, build.size, &build.sha256);
    if let Err(e) = res {
        let _ = fs::remove_file(&part);
        return Err(e.context(format!("download {url}")));
    }
    fs::rename(&part, &dest).with_context(|| format!("cannot move into {}", dest.display()))?;
    let _ = crate::layout::fsync_dir(&incoming);
    eprintln!(
        "homing-agent: http source: downloaded {id} version_code {version_code} ({} bytes, timeout {} s)",
        build.size, cfg.timeout_s
    );
    agent
        .submit(&dest, "http", Some((id, version_code)))
        .map_err(|e| anyhow!("cannot queue the install: {}", e.message))
}

/// Stream `url` into `part`, failing if it is not exactly `size` bytes with this sha256.
fn download(http: &ureq::Agent, url: &Url, part: &Path, size: u64, sha256: &str) -> Result<()> {
    let resp = http.get(url.as_str()).call().map_err(http_err)?;
    let body = resp.into_body().into_with_config().limit(size + 1).reader();
    let file =
        fs::File::create(part).with_context(|| format!("cannot create {}", part.display()))?;
    let mut tee = Tee {
        inner: body,
        out: std::io::BufWriter::new(file),
        err: None,
    };
    let (got_sha, got_size) = homing_format::package::sha256_reader(&mut tee)
        .map_err(|e| anyhow!("download failed: {e}"))?;
    if let Some(e) = tee.err {
        return Err(e).context("cannot write the download");
    }
    let file = tee.out.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()?;
    if got_size != size || got_sha != sha256 {
        bail!(
            "E_HASH: got {got_size} bytes with sha256 {}…, index says {size} bytes with {}…",
            &got_sha[..12],
            &sha256[..sha256.len().min(12)]
        );
    }
    Ok(())
}

/// A reader that copies everything it reads into a writer.
struct Tee<R, W> {
    inner: R,
    out: W,
    err: Option<std::io::Error>,
}

impl<R: Read, W: Write> Read for Tee<R, W> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if let Err(e) = self.out.write_all(&buf[..n]) {
            self.err = Some(e);
            return Err(std::io::Error::other("write failed"));
        }
        Ok(n)
    }
}

fn get_bytes(http: &ureq::Agent, url: &Url, limit: u64) -> Result<Vec<u8>> {
    let resp = http
        .get(url.as_str())
        .call()
        .map_err(http_err)
        .with_context(|| format!("GET {url}"))?;
    resp.into_body()
        .into_with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(http_err)
        .with_context(|| format!("GET {url} (limit {limit} bytes)"))
}

fn http_err(e: ureq::Error) -> anyhow::Error {
    anyhow!("{e}")
}

fn client(cfg: &HttpSource) -> ureq::Agent {
    static PROVIDER: Once = Once::new();
    PROVIDER.call_once(|| {
        // rustls needs a process-wide crypto provider; ring is the one compiled in. Ignore the
        // error if something already installed one.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        // The operating system's CA store (e.g. /etc/ssl/certs), so operators can add their own.
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_global(Some(Duration::from_secs(cfg.timeout_s)))
        .user_agent(concat!("homing-agent/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent()
}

fn free_bytes(dir: &Path) -> Result<u64> {
    let s =
        nix::sys::statvfs::statvfs(dir).with_context(|| format!("statvfs {}", dir.display()))?;
    // The conversions are no-ops on 64-bit targets but needed on 32-bit ones (e.g. armv7).
    #[allow(clippy::useless_conversion)]
    Ok(u64::from(s.blocks_available()) * u64::from(s.fragment_size()))
}

/// Up to `max` seconds of randomness (from the kernel's per-boot random UUIDs).
fn jitter(max: u64) -> u64 {
    if max == 0 {
        return 0;
    }
    let r = fs::read_to_string("/proc/sys/kernel/random/uuid").unwrap_or_default();
    let n = r
        .bytes()
        .filter(u8::is_ascii_hexdigit)
        .take(15)
        .fold(0u64, |acc, c| {
            acc * 16 + u64::from((c as char).to_digit(16).unwrap_or(0))
        });
    n % (max + 1)
}

/// Whether a job state counts as a failed install of that version.
pub(crate) fn is_failure(s: JobState) -> bool {
    matches!(s, JobState::Failed | JobState::RolledBack)
}
