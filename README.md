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

> **Status: pre-alpha.** Not audited and not hardened (see [Known limitations](#known-limitations)).
> Do not deploy it on devices that matter yet.

## Features

- **Verified streaming install.** The signature is checked before any payload byte is read, and
  files are hashed while they are extracted.
- **A/B app slots.** The running version is never modified. Switching is an atomic symlink swap.
- **Health-checked trial.** The new version must call `app.ready` (or, in `alive` mode, stay up
  for `trial_s`). A crash, exit or timeout during the trial triggers an automatic rollback.
- **Crash and power-loss safe.** State is written with fsync, then rename, then a directory
  fsync. After a restart, an interrupted switch or trial is rolled back and an interrupted
  staging step is cleaned up.
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
| `homingctl` | Admin CLI: `status`, `install <file> [--wait]`, `job <id> [--wait]`. |
| `homing-launch` | Starts one app: resolves its current slot, checks it, builds the environment and execs the embedder. `--dry-run` prints the command line and environment. |
| `homing-fake-embedder` | A stand-in for flutter-pi with scriptable behavior (ok, crash, never ready, exit), for tests and demos. |

## Quick start

Build (Linux, Rust 1.85 or newer):

```sh
cargo build --release --locked
```

Install on the device:

```sh
sudo install -m755 target/release/{homing-agent,homingctl} /usr/bin/
sudo install -Dm755 target/release/homing-launch /usr/libexec/homing/homing-launch
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
The error code is printed in either case.

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

[embedders.flutter-pi]
binary = "/usr/bin/flutter-pi"
# icudtl = "/usr/share/flutter/icudtl.dat"      # used when a package has no icudtl.dat

[apps."com.example.myapp"]
autostart = true                                # start at boot once committed
allowed_args = ["--rotation"]                   # prefix allowlist for the manifest's runtime.args
```

The agent refuses to start if `dev_mode` is off and the trust store has no `prod-*.pub` key.
Runtime arguments from a package are passed to the embedder only if they match
`allowed_args`; everything else is dropped and logged.

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

Admins are root or the agent's own user. An app is identified as the supervised process or one
of its descendants, never by parameters. `homingctl` copies a package into `incoming/`
atomically before calling `install`.

## Testing

```sh
cargo test        # unit tests + end-to-end tests with real agent, launcher and homingctl processes
```

The end-to-end tests use `homing-fake-embedder` in place of flutter-pi. They install, upgrade
and roll back (crash, never ready, clean exit), reject wrong-engine, tampered and
untrusted packages, enforce the floor, and recover from the agent being killed mid-trial.

A separate QEMU testbed (to be published in a homing-examples repository) runs the same
binaries on aarch64 under systemd. It covers reboots, randomized power cuts and a real
Flutter app on flutter-pi.

## Known limitations

- The agent runs as root and supervises apps itself. Per-app systemd units and a dedicated
  user are planned.
- The `homing-admin` group is not implemented yet; admin means root or the agent's own user.
- Packages are installed only with `homingctl`. There is no HTTP, USB or Bluetooth source yet.
- Not yet implemented: `rollback`, `job.cancel`, `job.confirm`, event subscriptions, update
  deferral, `sd_notify`. Queued jobs that have not started are dropped on agent restart.
- The flutter-pi adapter has been tested in an aarch64 QEMU VM, not yet on a Raspberry Pi. The
  ivi-homescreen adapter is not implemented.

## Roadmap

- [x] Verified A/B install, trial, automatic rollback, crash recovery, floors
- [x] flutter-pi launcher and systemd unit
- [ ] HTTP update source (signed index), USB source
- [ ] Per-app systemd units with sandboxing; `homing-admin` group
- [ ] Remaining API: `rollback`, confirm/cancel, events, update deferral
- [ ] ivi-homescreen adapter; Yocto layer (meta-homing)

## Minimum supported Rust version

Rust **1.85** (edition 2024). CI tests stable and 1.85. Linux only.

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md). Report security issues
privately, as described in [SECURITY.md](SECURITY.md). This project follows a
[Code of Conduct](CODE_OF_CONDUCT.md).

## License

Licensed under the [MIT License](LICENSE).
