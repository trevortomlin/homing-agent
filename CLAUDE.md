# CLAUDE.md
- README.md documents the user-facing behavior. Keep it in sync with the code in the same change,
  and add an entry to CHANGELOG.md.
- The package format comes from the homing-format crate (git dependency pinned to a commit);
  format changes are made there, not here. If a sibling `../homing-spec/` folder exists (a
  private maintainer checkout), its `spec/` chapters are the detailed normative text.
- Items marked **VERIFY** depend on upstream projects (flutter-pi, ivi-homescreen, meta-flutter,
  Yocto). Check the current upstream source or docs before relying on them.
- Every change needs tests. Security-relevant code (signature checks, extraction, path handling,
  caller authentication) must fail closed and have negative tests.
- Linux only. The install pipeline must stay crash-safe: new persistent state needs a recovery
  path and a test.
- Keep dependencies minimal and permissively licensed (no GPL); `deny.toml` enforces this.
- All code and docs are MIT-licensed; never add files under another license.
- MSRV is Rust 1.85 (`rust-version`); CI tests stable and 1.85.
- Reference hardware is a Raspberry Pi 3B (`raspberrypi3-64`, 1 GB RAM, GLES 2.0, no RTC).
- Conventional commits; DCO sign-off (`git commit -s`).
