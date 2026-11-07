//! End-to-end on the host: real agent, launcher and homingctl processes, with the fake embedder
//! standing in for flutter-pi. Scenario:
//! install v1 -> v2 -> crashing v3 rolls back -> never-ready v4 rolls back, plus negative cases
//! and agent crash recovery.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use homing_agent::rpc::Client;
use homing_format::manifest::{Arch, Compat, HealthMode, Manifest};
use homing_format::{PackageWriter, SecretKey};
use serde_json::{Value, json};

const ENGINE: &str = "0123456789abcdef0123456789abcdef01234567";
const ID: &str = "com.example.hello";

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    config: PathBuf,
    agent: Option<Child>,
}

impl Env {
    fn new() -> Self {
        Self::with_config("")
    }

    /// An agent whose agent.toml has `extra` appended.
    fn with_config(extra: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir(root.join("keys")).unwrap();
        let sk = SecretKey::from_seed(&[5; 32]);
        std::fs::write(
            root.join("keys/prod-test.pub"),
            sk.public_key("test").to_file_string(),
        )
        .unwrap();
        std::fs::write(
            root.join("engine.json"),
            json!({"schema":1,"engine_hash":ENGINE,"arch":"aarch64","machine":"raspberrypi3-64",
                   "graphics":["drm-gles"],"base_image_version":"1.0.0"})
            .to_string(),
        )
        .unwrap();
        let config = root.join("agent.toml");
        std::fs::write(
            &config,
            format!(
                r#"data_dir = "data"
socket = "agent.sock"
keys_dir = "keys"
engine_file = "engine.json"
min_free_mb = 1
launcher = "{}"
restart_delay_ms = 200

[embedders.flutter-pi]
binary = "{}"

{extra}
"#,
                env!("CARGO_BIN_EXE_homing-launch"),
                env!("CARGO_BIN_EXE_homing-fake-embedder")
            ),
        )
        .unwrap();
        let mut e = Env {
            _tmp: tmp,
            root,
            config,
            agent: None,
        };
        e.start_agent();
        e
    }

    fn start_agent(&mut self) {
        let child = Command::new(env!("CARGO_BIN_EXE_homing-agent"))
            .arg("--config")
            .arg(&self.config)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        self.agent = Some(child);
        let sock = self.root.join("agent.sock");
        let t = Instant::now();
        while Client::connect(&sock).is_err() {
            assert!(t.elapsed() < Duration::from_secs(10), "agent did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn kill_agent(&mut self) {
        if let Some(mut c) = self.agent.take() {
            let _ = c.kill(); // SIGKILL: no cleanup, like a power cut for the agent process
            let _ = c.wait();
        }
    }

    fn client(&self) -> Client {
        Client::connect(&self.root.join("agent.sock")).unwrap()
    }

    fn status(&self) -> Value {
        self.client().call("status", Value::Null).unwrap()
    }

    fn app(&self) -> Value {
        self.status()["apps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == ID)
            .cloned()
            .unwrap()
    }

    /// Install via homingctl --wait; returns the final job object.
    fn install(&self, pkg: &Path) -> Value {
        let out = Command::new(env!("CARGO_BIN_EXE_homingctl"))
            .arg("--config")
            .arg(&self.config)
            .arg("install")
            .arg(pkg)
            .arg("--wait")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let id = stdout
            .lines()
            .find_map(|l| l.strip_prefix("job "))
            .expect("job id")
            .trim()
            .to_owned();
        self.client()
            .call("job.get", json!({ "job_id": id }))
            .unwrap()
    }

    fn wait_for<F: Fn(&Env) -> bool>(&self, what: &str, f: F) {
        let t = Instant::now();
        while !f(self) {
            assert!(
                t.elapsed() < Duration::from_secs(15),
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn pkg(&self, name: &str, vc: u64, mode: &str, f: impl FnOnce(&mut Manifest)) -> PathBuf {
        let mut compat = Compat::new(ENGINE, Arch::Aarch64);
        compat.machines = vec!["raspberrypi3-64".into()];
        compat.graphics = Some(vec![homing_format::manifest::Graphics::DrmGles]);
        let mut m = Manifest::new(ID, "Hello", format!("1.0.{vc}"), vc, compat);
        m.health.mode = HealthMode::Ready;
        m.health.ready_timeout_s = 3;
        m.health.trial_s = 2;
        f(&mut m);
        let mut w = PackageWriter::new(0);
        w.add_bytes(
            "bundle/flutter_assets/AssetManifest.json",
            b"{}".to_vec(),
            false,
        )
        .unwrap();
        w.add_bytes(
            "bundle/flutter_assets/fake.json",
            json!({ "mode": mode }).to_string().into_bytes(),
            false,
        )
        .unwrap();
        w.add_bytes("bundle/lib/libapp.so", vec![vc as u8; 1000], false)
            .unwrap();
        w.add_bytes(
            "config/app.json",
            json!({ "greeting": format!("hello v{vc}") })
                .to_string()
                .into_bytes(),
            false,
        )
        .unwrap();
        let path = self.root.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        w.write(m, &SecretKey::from_seed(&[5; 32]), &mut f).unwrap();
        path
    }

    fn launches(&self) -> u64 {
        std::fs::read_to_string(self.root.join("data/apps").join(ID).join("state/launches"))
            .map(|s| s.trim().parse().unwrap())
            .unwrap_or(0)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.kill_agent();
    }
}

fn code(job: &Value) -> &str {
    job["error"]["code"].as_str().unwrap_or("")
}

#[test]
fn update_rollback_and_recovery() {
    let mut e = Env::new();

    // v1: first install -> slot a, committed, running and ready.
    let j = e.install(&e.pkg("v1.homing", 1, "ok", |_| {}));
    assert_eq!(j["state"], "committed", "{j}");
    let a = e.app();
    assert_eq!(a["current"], "a");
    assert_eq!(a["slots"]["a"]["status"], "committed");
    assert_eq!(a["process"]["ready"], true);

    // v2 -> slot b; v1 kept as previous; state dir survives.
    let j = e.install(&e.pkg("v2.homing", 2, "ok", |_| {}));
    assert_eq!(j["state"], "committed", "{j}");
    let a = e.app();
    assert_eq!(a["current"], "b");
    assert_eq!(a["slots"]["a"]["status"], "previous");
    assert_eq!(a["slots"]["b"]["status"], "committed");
    assert!(e.launches() >= 2);

    // v3 crashes on start -> rolled back to v2 (slot b), E_HEALTH.
    let j = e.install(&e.pkg("v3.homing", 3, "crash", |_| {}));
    assert_eq!(j["state"], "rolled_back", "{j}");
    assert_eq!(code(&j), "E_HEALTH");
    let a = e.app();
    assert_eq!(a["current"], "b");
    assert_eq!(a["slots"]["a"]["status"], "bad");
    assert_eq!(a["slots"]["b"]["version_code"], 2);
    e.wait_for("v2 running and ready again", |e| {
        e.app()["process"]["ready"] == true
    });

    // v4 never calls app.ready -> rolled back after ready_timeout_s.
    let j = e.install(&e.pkg("v4.homing", 4, "never_ready", |_| {}));
    assert_eq!(j["state"], "rolled_back", "{j}");
    assert_eq!(code(&j), "E_HEALTH");
    assert_eq!(e.app()["current"], "b");

    // v5 in alive mode with a clean exit counts as a failure too.
    let j = e.install(&e.pkg("v5.homing", 5, "exit", |m| {
        m.health.mode = HealthMode::Alive
    }));
    assert_eq!(j["state"], "rolled_back", "{j}");

    // Negative cases never touch the running version.
    let j = e.install(&e.pkg("eng.homing", 6, "ok", |m| {
        m.compat.engine_hash = "f".repeat(40)
    }));
    assert_eq!(
        (j["state"].as_str().unwrap(), code(&j)),
        ("failed", "E_ENGINE")
    );
    let tampered = e.pkg("t.homing", 7, "ok", |_| {});
    let mut b = std::fs::read(&tampered).unwrap();
    let n = b.len();
    b[n / 2] ^= 0xff;
    std::fs::write(&tampered, b).unwrap();
    let j = e.install(&tampered);
    assert_eq!(j["state"], "failed");
    assert!(["E_SIG", "E_HASH", "E_SCHEMA"].contains(&code(&j)), "{j}");
    let unsigned = e.root.join("unsigned.homing");
    {
        let mut w = PackageWriter::new(0);
        w.add_bytes("bundle/flutter_assets/a", b"x".to_vec(), false)
            .unwrap();
        w.add_bytes("bundle/lib/libapp.so", b"x".to_vec(), false)
            .unwrap();
        let other = SecretKey::from_seed(&[9; 32]);
        let m = homing_format::Manifest::parse(
            &std::fs::read(e.pkg("tmp.homing", 8, "ok", |_| {}))
                .map(|b| homing_format::inspect(&b[..]).unwrap().0.to_bytes())
                .unwrap(),
        )
        .unwrap();
        w.write(m, &other, std::fs::File::create(&unsigned).unwrap())
            .unwrap();
    }
    let j = e.install(&unsigned);
    assert_eq!(code(&j), "E_KEY_UNKNOWN");
    assert_eq!(e.app()["current"], "b");

    // Anti-rollback: committing v10 with min_version_code 10 makes v9 unacceptable.
    let j = e.install(&e.pkg("v10.homing", 10, "ok", |m| m.min_version_code = 10));
    assert_eq!(j["state"], "committed", "{j}");
    let j = e.install(&e.pkg("v9.homing", 9, "ok", |_| {}));
    assert_eq!(code(&j), "E_ROLLBACK");

    // Agent killed during a trial -> on restart it rolls back to the committed version.
    let current_before = e.app()["current"].as_str().unwrap().to_owned();
    let pkg = e.pkg("v11.homing", 11, "never_ready", |m| {
        m.health.ready_timeout_s = 60
    });
    let staged = e.root.join("data/incoming/v11.homing");
    std::fs::copy(&pkg, &staged).unwrap();
    let job = e
        .client()
        .call("install", json!({ "path": staged }))
        .unwrap();
    let job_id = job["job_id"].as_str().unwrap().to_owned();
    e.wait_for("trial", |e| {
        e.client()
            .call("job.get", json!({ "job_id": job_id }))
            .unwrap()["state"]
            == "trial"
    });
    e.kill_agent();
    e.start_agent();
    let j = e
        .client()
        .call("job.get", json!({ "job_id": job_id }))
        .unwrap();
    assert_eq!(j["state"], "rolled_back", "{j}");
    let a = e.app();
    assert_eq!(a["current"], current_before.as_str());
    assert_eq!(a["slots"][current_before.as_str()]["version_code"], 10);
    e.wait_for("v10 running after recovery", |e| {
        e.app()["process"]["ready"] == true
    });
}

#[test]
fn install_rejects_paths_outside_incoming() {
    let e = Env::new();
    let pkg = e.pkg("v1.homing", 1, "ok", |_| {});
    let err = e
        .client()
        .call("install", json!({ "path": pkg }))
        .unwrap_err();
    let rpc = err.downcast_ref::<homing_agent::rpc::RpcError>().unwrap();
    assert_eq!(rpc.spec_code(), Some("E_PERM"));
}

#[test]
fn app_ready_from_unmanaged_process_is_refused() {
    let e = Env::new();
    let err = e.client().call("app.ready", Value::Null).unwrap_err();
    let rpc = err.downcast_ref::<homing_agent::rpc::RpcError>().unwrap();
    assert_eq!(rpc.spec_code(), Some("E_PERM"));
}

// ---- HTTP update source ----

/// Minimal static file server for `dir` on 127.0.0.1 (one thread, HTTP/1.1, Connection: close).
fn serve(dir: PathBuf) -> u16 {
    use std::io::{BufRead as _, BufReader, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut r = BufReader::new(&stream);
            let mut line = String::new();
            let _ = r.read_line(&mut line);
            let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                    break;
                }
            }
            let file = dir.join(path.trim_start_matches('/'));
            let mut w = &stream;
            match std::fs::read(&file) {
                Ok(body) if !path.contains("..") => {
                    let _ = write!(
                        w,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = w.write_all(&body);
                }
                _ => {
                    let _ = w.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
            }
        }
    });
    port
}

fn sha_size(p: &Path) -> (String, u64) {
    homing_format::package::sha256_reader(std::fs::File::open(p).unwrap()).unwrap()
}

/// Sign and publish an index listing `(version_code, engine, url, package file)`.
fn publish(site: &Path, key: &SecretKey, builds: &[(u64, &str, &str, &Path)]) {
    let mut index = homing_format::Index::new();
    for (vc, engine, url, file) in builds {
        let (sha, size) = sha_size(file);
        let mut app = homing_format::IndexApp::new(ID, format!("1.0.{vc}"), *vc);
        let mut b = homing_format::IndexBuild::new(*engine, Arch::Aarch64, *url, sha, size);
        b.machines = vec!["raspberrypi3-64".into()];
        app.builds.push(b);
        index.apps.push(app);
    }
    let (doc, sig) = index.sign(key).unwrap();
    std::fs::write(site.join("index.json"), doc).unwrap();
    std::fs::write(site.join("index.json.sig"), sig).unwrap();
}

impl Env {
    /// `homingctl check --json`: (exit code, result).
    fn check(&self) -> (i32, Value) {
        let out = Command::new(env!("CARGO_BIN_EXE_homingctl"))
            .arg("--config")
            .arg(&self.config)
            .args(["check", "--json"])
            .output()
            .unwrap();
        let v = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
            panic!(
                "homingctl check: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.code().unwrap(), v)
    }

    fn wait_job(&self, id: &str) -> Value {
        let t = Instant::now();
        loop {
            let j = self
                .client()
                .call("job.get", json!({ "job_id": id }))
                .unwrap();
            if matches!(
                j["state"].as_str(),
                Some("committed" | "rolled_back" | "failed")
            ) {
                return j;
            }
            assert!(t.elapsed() < Duration::from_secs(20), "job {id} stuck: {j}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn app_result(r: &Value) -> &Value {
    &r["apps"][0]
}

#[test]
fn http_source_installs_updates_from_a_signed_index() {
    let site = tempfile::tempdir().unwrap();
    std::fs::create_dir(site.path().join("pkgs")).unwrap();
    let port = serve(site.path().to_path_buf());
    let base = format!("http://127.0.0.1:{port}");
    let e = Env::with_config(&format!(
        "[apps.\"{ID}\"]\n\n[source.http]\nindex_url = \"{base}/index.json\"\ninterval_s = 3600\n"
    ));
    let key = SecretKey::from_seed(&[5; 32]);
    let pkg = |vc: u64, mode: &str| {
        let p = e.pkg(&format!("v{vc}.homing"), vc, mode, |_| {});
        let dest = site.path().join(format!("pkgs/v{vc}.homing"));
        std::fs::copy(&p, &dest).unwrap();
        dest
    };

    // 1. First install: the app is configured but not installed yet. Relative URL.
    let v1 = pkg(1, "ok");
    publish(site.path(), &key, &[(1, ENGINE, "pkgs/v1.homing", &v1)]);
    let (code, r) = e.check();
    assert_eq!(
        (code, app_result(&r)["result"].as_str()),
        (0, Some("queued")),
        "{r}"
    );
    let j = e.wait_job(app_result(&r)["job_id"].as_str().unwrap());
    assert_eq!(
        (j["state"].as_str(), j["source"].as_str()),
        (Some("committed"), Some("http"))
    );

    // 2. An update with an absolute URL.
    let v2 = pkg(2, "ok");
    publish(
        site.path(),
        &key,
        &[
            (1, ENGINE, "pkgs/v1.homing", &v1),
            (2, ENGINE, &format!("{base}/pkgs/v2.homing"), &v2),
        ],
    );
    let (_, r) = e.check();
    assert_eq!(app_result(&r)["version_code"], 2, "{r}");
    let j = e.wait_job(app_result(&r)["job_id"].as_str().unwrap());
    assert_eq!(j["state"], "committed");
    assert_eq!(
        e.app()["slots"][e.app()["current"].as_str().unwrap()]["version_code"],
        2
    );

    // 3. Nothing newer: up to date. The status shows the last check.
    let (_, r) = e.check();
    assert_eq!(app_result(&r)["result"], "up_to_date", "{r}");
    assert_eq!(
        e.status()["sources"]["http"]["apps"][0]["result"],
        "up_to_date"
    );

    // 4. A version that fails its trial is rolled back and not downloaded again.
    let v3 = pkg(3, "crash");
    publish(
        site.path(),
        &key,
        &[
            (2, ENGINE, "pkgs/v2.homing", &v2),
            (3, ENGINE, "pkgs/v3.homing", &v3),
        ],
    );
    let (_, r) = e.check();
    let j = e.wait_job(app_result(&r)["job_id"].as_str().unwrap());
    assert_eq!(j["state"], "rolled_back");
    let (_, r) = e.check();
    assert_eq!(app_result(&r)["result"], "skipped_failed", "{r}");

    // 5. Only a build for another engine: reported, nothing installed.
    let other = "fedcba9876543210fedcba9876543210fedcba98";
    publish(site.path(), &key, &[(4, other, "pkgs/v3.homing", &v3)]);
    let (_, r) = e.check();
    assert_eq!(app_result(&r)["result"], "no_compatible_build", "{r}");

    // 6. The index promises different bytes than the server sends: the download is rejected
    //    before anything is queued, and no partial file is left behind.
    let v5 = pkg(5, "ok");
    publish(site.path(), &key, &[(5, ENGINE, "pkgs/v5.homing", &v5)]);
    std::fs::write(&v5, b"not the promised package").unwrap();
    let (_, r) = e.check();
    assert_eq!(app_result(&r)["result"], "error", "{r}");
    assert!(
        app_result(&r)["message"]
            .as_str()
            .unwrap()
            .contains("E_HASH"),
        "{r}"
    );
    let leftovers: Vec<_> = std::fs::read_dir(e.root.join("data/incoming"))
        .unwrap()
        .flatten()
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // 7. A tampered index and an index signed by an untrusted key are rejected as a whole.
    let doc = std::fs::read_to_string(site.path().join("index.json")).unwrap();
    std::fs::write(
        site.path().join("index.json"),
        doc.replace("1.0.5", "1.0.6"),
    )
    .unwrap();
    let (code, r) = e.check();
    assert_eq!(code, 2);
    assert!(r["last_error"].as_str().unwrap().contains("E_SIG"), "{r}");
    publish(
        site.path(),
        &SecretKey::from_seed(&[9; 32]),
        &[(5, ENGINE, "pkgs/v5.homing", &v5)],
    );
    let (code, r) = e.check();
    assert_eq!(code, 2);
    assert!(
        r["last_error"].as_str().unwrap().contains("E_KEY_UNKNOWN"),
        "{r}"
    );

    // 8. An unreachable server is an error, not a crash.
    std::fs::remove_file(site.path().join("index.json")).unwrap();
    let (code, r) = e.check();
    assert_eq!(code, 2);
    assert!(r["last_error"].as_str().unwrap().contains("404"), "{r}");
    // The app kept running version 2 throughout.
    assert_eq!(
        e.app()["slots"][e.app()["current"].as_str().unwrap()]["version_code"],
        2
    );
}

#[test]
fn check_without_http_source_is_an_error() {
    let e = Env::new();
    let out = Command::new(env!("CARGO_BIN_EXE_homingctl"))
        .arg("--config")
        .arg(&e.config)
        .arg("check")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no [source.http]"));
}
