# homing-agent

[![CI](https://github.com/trevortomlin/homing-agent/actions/workflows/ci.yml/badge.svg)](https://github.com/trevortomlin/homing-agent/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.85-orange.svg)](#minimum-supported-rust-version)

The on-device side of **homing**: signed, versioned over-the-air app packages for embedded
Linux devices that run Flutter apps (flutter-pi, ivi-homescreen). `homing-agent` verifies a
package, installs it into the inactive of two A/B slots, starts the new version on trial and
commits it only if it passes its health check. Otherwise it rolls back to the previous version
automatically, including after a crash or power loss mid-update.

Packages are built and signed with [homing-cli](https://github.com/trevortomlin/homing-cli).
The format is implemented in
[homing-format](https://github.com/trevortomlin/homing-format).

homing is an independent project, not affiliated with Google or the Flutter project.

> **Status: early release (0.1).** Not audited, and not yet tested on real hardware (see
> [Known limitations](#known-limitations)). Do not deploy it on devices that matter yet.

## Features

- **Verified streaming install.** The signature is checked before any payload byte is read, and
  files are hashed while they are extracted.
- **A/B app slots.** The running version is never modified. Switching is an atomic symlink swap.
- **Health-checked trial.** The new version must call `app.ready` (or, in `alive` mode, stay up
  for `trial_s`). A crash, exit or timeout during the trial triggers an automatic rollback.
- **Crash and power-loss safe.** State is written with fsync, then rename, then a directory
  fsync. After a restart, an interrupted switch or trial is rolled back and an interrupted
  staging step is cleaned up.
- **HTTP update source.** Polls a signed update index and installs the newest build that fits
  the device. Plain `http://` is safe: the index and every package are signature-checked.
- **Manual rollback** to the previous version, still respecting the anti-rollback floor.
- **Anti-rollback floors** stop older versions from being reinstalled.
- **Device compatibility checks** (engine commit, architecture, machine, graphics backend).
- **Stable app environment.** Apps get resolved paths for their bundle, config and persistent
  state directory, which survives updates and rollbacks.
- **Local JSON-RPC API** over a Unix socket. Callers are identified by kernel credentials, not by
  parameters.

## Binaries

| Binary | Purpose |
|---|---|
| `homing-agent` | The daemon: API socket, install pipeline, trial and rollback, crash recovery, app supervision. |
| `homingctl` | Admin CLI: `status`, `install <file> [--wait]`, `job <id> [--wait]`, `rollback <app> [--wait]`, `check`. |
| `homing-launch` | Starts one app: resolves its current slot, checks it, builds the environment and execs the embedder. `--dry-run` prints the command line and environment. |
| `homing-fake-embedder` | A stand-in for flutter-pi with scriptable behavior (ok, crash, never ready, exit), for tests and demos. |

## Quick start

Download the static Linux binaries from the
[releases](https://github.com/trevortomlin/homing-agent/releases) (x86_64, or aarch64 for
e.g. a Raspberry Pi with a 64-bit OS), or build from source with Rust 1.85 or newer
(`cargo build --release --locked`). Then install on the device:

```sh
v=0.1.0 target=aarch64-unknown-linux-musl
curl -fsSLO https://github.com/trevortomlin/homing-agent/releases/download/v$v/homing-agent-$v-$target.tar.gz
curl -fsSLO https://github.com/trevortomlin/homing-agent/releases/download/v$v/SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf homing-agent-$v-$target.tar.gz && cd homing-agent-$v-$target

sudo install -m755 bin/homing-agent bin/homingctl /usr/bin/
sudo install -Dm755 bin/homing-launch /usr/libexec/homing/homing-launch
sudo install -Dm644 systemd/homing-agent.service /etc/systemd/system/homing-agent.service

sudo mkdir -p /etc/homing/keys
sudo cp prod-release.pub /etc/homing/keys/          # from `homing keygen` on your build host
sudo tee /etc/homing/engine.json >/dev/null <<'EOF'
{"schema":1,"engine_hash":"<flutter engine commit>","flutter_version":"3.44.9",
 "arch":"aarch64","machine":"raspberrypi3-64","graphics":["drm-gles"],"base_image_version":"1.0.0"}
EOF
sudo tee /etc/homing/agent.toml >/dev/null <<'EOF'
data_dir = "/var/lib/homing"
EOF
sudo systemctl daemon-reload && sudo systemctl enable --now homing-agent
```

Install a package and watch it go through its trial:

```console
$ sudo homingctl install com.example.myapp-1.0.0-aarch64-5a2a6a42.homing --wait
job 2f6c...
  verifying
  trial
  committed
$ sudo homingctl status
```

`--wait` exits with 0 only if the version committed, and with 2 if it failed or was rolled back.
The error code is printed in either case. `sudo homingctl rollback com.example.myapp --wait`
returns the app to its previous version.

## Configuration: `/etc/homing/agent.toml`

Every key is optional. Unknown keys are an error, so a typo cannot silently weaken the
configuration. Relative paths are resolved against the config file's directory.

```toml
data_dir = "/data/homing"                       # slots, state, job state
socket = "/run/homing/agent.sock"
keys_dir = "/etc/homing/keys"                   # trust store: prod-*.pub, dev-*.pub
engine_file = "/etc/homing/engine.json"         # describes this device image
dev_mode = false                                # honor dev-*.pub keys and debug builds
min_free_mb = 64                                # free space to keep beyond the package
launcher = "/usr/libexec/homing/homing-launch"
restart_delay_ms = 2000                         # delay before restarting a crashed app
admin_group = "homing-admin"                    # members may install, roll back and check

[embedders.flutter-pi]
binary = "/usr/bin/flutter-pi"
# icudtl = "/usr/share/flutter/icudtl.dat"      # used when a package has no icudtl.dat

[apps."com.example.myapp"]
autostart = true                                # start at boot once committed
allowed_args = ["--rotation"]                   # prefix allowlist for the manifest's runtime.args

[source.http]                                   # optional: poll an update index
index_url = "http://updates.example.com/stable/index.json"
interval_s = 3600                               # time between checks
jitter_s = 600                                  # up to this much random extra delay
timeout_s = 600                                 # per request, including a whole download
```

The agent refuses to start if `dev_mode` is off and the trust store has no `prod-*.pub` key.
Runtime arguments from a package are passed to the embedder only if they match
`allowed_args`; everything else is dropped and logged.

## HTTP update source

With `[source.http]`, the agent fetches `index.json` and `index.json.sig` (published with
`homing index add` from [homing-cli](https://github.com/trevortomlin/homing-cli)) every
`interval_s` seconds plus up to `jitter_s` of random delay, and on demand with
`homingctl check`. For each installed app, and each app listed under `[apps."<id>"]` (so new
apps can be installed this way), it:

1. picks the newest version above the installed one and the anti-rollback floor that has a
   build for this device (engine, architecture, machine, graphics, base image);
2. downloads it into `incoming/`, checking free space first and the size and sha256 from the
   index while it streams;
3. queues a normal install, which verifies the package again and runs the trial.

A version whose install failed, was rolled back, or was rolled back by `homingctl rollback`
is not downloaded again; publish a newer version instead. `homingctl status` and
`homingctl check` show the last result per app (`up_to_date`, `queued`,
`no_compatible_build`, `busy`, `skipped_failed`, `error`).

**http or https?** Integrity never depends on TLS: the index and every package are
signature-checked against the device's trust store, so `http://` is safe and also works
without a correct clock. A Raspberry Pi has no real-time clock, and until NTP has synced,
`https://` certificate checks can fail. Those checks simply fail and retry at the next
interval. `https://` uses the operating system's CA store (`/etc/ssl/certs`), so you can add
your own CA.

## Install lifecycle

```text
queued → verifying → staged → switching → trial → committed
                                            └──→ rolling_back → rolled_back (E_HEALTH)
any failure before switching → failed (error code); the staging slot is deleted
```

- **verifying / staged:** the package is checked and streamed into the inactive slot, then
  fsynced. Debug builds are rejected unless `dev_mode` is on.
- **switching:** the job state is persisted, `current` is pointed at the new slot, and the app
  is restarted.
- **trial:** in `ready` mode the app must call `app.ready` within `ready_timeout_s`. In `alive`
  mode it must stay up for `trial_s`. Any exit or crash fails the trial.
- **committed:** the old slot is kept as the rollback target, and the floor is raised to the
  package's `min_version_code`.
- **recovery:** if the agent restarts during switching or trial, the update is rolled back. If
  it restarts before switching, the job fails with `E_IO` and the staging slot is removed.
  Jobs still waiting in the queue are kept and run after the restart.
- **manual rollback** (`homingctl rollback`) is a queued job too: it marks the current version
  bad and switches back to the previous one, unless that is below the anti-rollback floor
  (`E_ROLLBACK`).

## Device layout

```text
/etc/homing/agent.toml, engine.json, keys/*.pub
<data_dir>/
  agent-state.json        current job and its position (crash recovery)
  floors.json             anti-rollback floor per app id
  incoming/               spool: packages waiting for installation
  apps/<id>/
    a/  b/                slots: extracted package + .homing-meta/{manifest.json,manifest.sig}
    current -> a          relative symlink, swapped atomically
    slots.json            version, status and manifest hash of each slot
    state/                app-owned data that survives updates and rollbacks
```

## App environment

`homing-launch` resolves `current` once and passes real paths, so a running app never mixes
files from two versions:

| Variable | Value |
|---|---|
| `HOMING_APP_ID` | App id |
| `HOMING_APP_VERSION`, `HOMING_APP_VERSION_CODE` | From the manifest |
| `HOMING_SLOT` | `a` or `b` |
| `HOMING_APP_DIR` | Resolved slot directory |
| `HOMING_BUNDLE_DIR` | `$HOMING_APP_DIR/bundle` |
| `HOMING_CONFIG_DIR` | `$HOMING_APP_DIR/config` (the package's signed config) |
| `HOMING_STATE_DIR` | `<data_dir>/apps/<id>/state` (persistent, writable) |
| `HOMING_AGENT_SOCKET` | Agent socket path |

The package's `runtime.env` entries are added as well. They may not start with `HOMING_`.

## Socket API

JSON-RPC 2.0 over a Unix stream socket, one JSON object per line. Application errors carry the
error code in `error.data.code` (`E_SIG`, `E_PERM`, ...).

| Method | Caller | Params | Result |
|---|---|---|---|
| `status` | anyone | none | Agent version, engine info, apps with slots and processes, jobs |
| `install` | admin | `{path, source}` | `{job_id}`. `path` must be a file directly inside `incoming/` |
| `job.get` | anyone | `{job_id}` | Job object: state, app, version, slot, error |
| `app.ready` | app | none | `{}`. Ends the trial in `ready` mode (idempotent) |
| `app.info` | app | none | The calling app's slots and process status |
| `rollback` | admin | `{id}` | `{job_id}`. Return the app to its previous version |
| `source.check` | admin | none | Run an HTTP source check now; returns the per-app results |

Admins are root, the agent's own user, and members of `admin_group`. An app is identified as the supervised process or one
of its descendants, never by parameters. `homingctl` copies a package into `incoming/`
atomically before calling `install`.

## Testing

```sh
cargo test        # unit tests + end-to-end tests with real agent, launcher and homingctl processes
```

The end-to-end tests use `homing-fake-embedder` in place of flutter-pi. They install, upgrade
and roll back (crash, never ready, clean exit), reject wrong-engine, tampered and
untrusted packages, enforce the floor, and recover from the agent being killed mid-trial
with jobs still queued. The HTTP source is tested against a local web server, including
tampered and untrusted indexes, hash mismatches and unreachable servers.

A separate QEMU testbed (to be published in a homing-examples repository) runs the same
binaries on aarch64 under systemd. It covers reboots, randomized power cuts and a real
Flutter app on flutter-pi.

## Known limitations

- **Not yet tested on a Raspberry Pi.** Everything runs in an aarch64 QEMU VM with real
  flutter-pi, but the Pi's GPU, memory and SD-card behavior are untested.
- The agent runs as root and supervises apps itself. Per-app systemd units and a dedicated
  user are planned.
- No USB or Bluetooth source yet.
- Not yet implemented: `job.cancel`, `job.confirm`, event subscriptions, update deferral, a
  systemd watchdog.
- The memory of failed versions (so the HTTP source does not retry them) covers the last 20
  jobs.
- The ivi-homescreen adapter is not implemented.

## Roadmap

- [x] Verified A/B install, trial, automatic rollback, crash recovery, floors
- [x] flutter-pi launcher and systemd unit
- [x] HTTP update source (signed index), manual rollback, `homing-admin` group, sd_notify
- [ ] Testing on a Raspberry Pi 3B
- [ ] USB source
- [ ] Per-app systemd units with sandboxing
- [ ] Remaining API: confirm/cancel, events, update deferral
- [ ] ivi-homescreen adapter; Yocto layer (meta-homing)

## Minimum supported Rust version

Rust **1.85** (edition 2024). CI tests stable and 1.85. Linux only.

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md). Report security issues
privately, as described in [SECURITY.md](SECURITY.md). This project follows a
[Code of Conduct](CODE_OF_CONDUCT.md).

## License

Licensed under the [MIT License](LICENSE).
