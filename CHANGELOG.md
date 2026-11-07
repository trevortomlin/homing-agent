# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0,
minor versions may contain breaking changes.

## [Unreleased]

### Added

- `homing-agent` daemon: verified streaming install into A/B slots, atomic switch,
  health-checked trial (`ready` / `alive`), automatic rollback, anti-rollback floors, and
  crash and power-loss recovery.
- Built-in app supervisor with restart on failure; apps are stopped with the agent.
- `homing-launch`: resolves the current slot, checks it against `slots.json`, sets the
  `HOMING_*` environment and starts flutter-pi. `--dry-run` support.
- `homingctl`: `status`, `install [--wait]`, `job [--wait]`, `rollback [--wait]`, `check`.
- JSON-RPC socket API: `status`, `install`, `job.get`, `app.ready`, `app.info`, `rollback`,
  `source.check`, with kernel-credential-based caller identification.
- `systemd/homing-agent.service` (`Type=notify`: the agent reports readiness and shutdown).
- `homing-fake-embedder` for tests and demos.
- HTTP update source (`[source.http]`): polls a signed index, downloads the newest compatible
  build (size and sha256 checked) and queues its install; `homingctl check` and the
  `source.check` RPC run a check on demand. Versions that failed or were rolled back are not
  retried.
- Manual rollback: `homingctl rollback` / `rollback` RPC, refused below the anti-rollback
  floor.
- Queued jobs are persisted and survive restarts; `incoming/` is cleaned of interrupted
  copies and downloads at startup.
- `admin_group` (default `homing-admin`): its members may use the admin API calls.
- Unknown embedders, build modes and health modes from newer format versions are refused.
