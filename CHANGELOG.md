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
- `homingctl`: `status`, `install [--wait]`, `job [--wait]`.
- JSON-RPC socket API: `status`, `install`, `job.get`, `app.ready`, `app.info`, with
  kernel-credential-based caller identification.
- `systemd/homing-agent.service`.
- `homing-fake-embedder` for tests and demos.
