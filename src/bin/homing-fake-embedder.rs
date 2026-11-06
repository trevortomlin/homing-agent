//! `homing-fake-embedder`: a stand-in for flutter-pi used by tests and the demo.
//!
//! Accepts flutter-pi's command line (`[options] <asset bundle path>`), checks the slot layout
//! the launcher prepared, and behaves according to `<bundle>/fake.json`:
//! `{"mode": "ok" | "crash" | "never_ready" | "exit", "ready_after_ms": 200}`.
//! It also counts launches in `$HOMING_STATE_DIR/launches` to show state survives updates.

use std::path::PathBuf;
use std::time::Duration;

use homing_agent::rpc::Client;
use serde_json::Value;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let release = args.iter().any(|a| a == "--release");
    let bundle = PathBuf::from(
        args.last()
            .expect("usage: homing-fake-embedder [opts] <bundle>"),
    );
    let id = std::env::var("HOMING_APP_ID").unwrap_or_default();
    let tag = format!(
        "fake-embedder[{id} {} slot {}]",
        std::env::var("HOMING_APP_VERSION").unwrap_or_default(),
        std::env::var("HOMING_SLOT").unwrap_or_default()
    );

    if release && !bundle.join("app.so").exists() {
        eprintln!(
            "{tag}: release mode but {}/app.so missing",
            bundle.display()
        );
        std::process::exit(3);
    }
    let behavior: Value = std::fs::read(bundle.join("fake.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let mode = behavior["mode"].as_str().unwrap_or("ok").to_owned();
    let ready_after = behavior["ready_after_ms"].as_u64().unwrap_or(200);

    let greeting = std::env::var("HOMING_CONFIG_DIR")
        .ok()
        .and_then(|d| std::fs::read(PathBuf::from(d).join("app.json")).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["greeting"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "(no config)".into());
    let launches = std::env::var("HOMING_STATE_DIR").ok().map(|d| {
        let p = PathBuf::from(d).join("launches");
        let n: u64 = std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
            + 1;
        let _ = std::fs::write(&p, n.to_string());
        n
    });
    eprintln!("{tag}: started, mode={mode}, greeting={greeting:?}, launches={launches:?}");

    match mode.as_str() {
        "crash" => {
            std::thread::sleep(Duration::from_millis(300));
            eprintln!("{tag}: crashing");
            std::process::exit(101);
        }
        "exit" => std::process::exit(0),
        "never_ready" => {}
        _ => {
            std::thread::sleep(Duration::from_millis(ready_after));
            let sock = std::env::var("HOMING_AGENT_SOCKET").unwrap_or_default();
            match Client::connect(sock.as_ref()).and_then(|mut c| c.call("app.ready", Value::Null))
            {
                Ok(_) => eprintln!("{tag}: reported ready"),
                Err(e) => eprintln!("{tag}: app.ready failed: {e:#}"),
            }
        }
    }
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
