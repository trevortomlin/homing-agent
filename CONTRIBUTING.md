# Contributing to homing-agent

Thanks for your interest in improving homing-agent. Bug reports, documentation fixes, tests and
code are all welcome.

## Before you start

- For anything larger than a small fix, **open an issue first** so we can agree on the
  approach.
- The package format itself lives in
  [homing-format](https://github.com/trevortomlin/homing-format). Propose format changes there.
- To report a security vulnerability, **do not open an issue**. Follow [SECURITY.md](SECURITY.md).

## Development

You need Rust 1.85 or newer on Linux.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo +1.85 test        # minimum supported Rust version
```

CI runs all of these, plus a license and advisory check with
[cargo-deny](https://github.com/EmbarkStudios/cargo-deny) (`cargo deny check`).

### Working on homing-format at the same time

`homing-format` is a git dependency pinned to a commit. To build against a local checkout,
add this to `.cargo/config.toml` in a directory **above** this repository, so that it is never
committed:

```toml
[patch."https://github.com/trevortomlin/homing-format"]
homing-format = { path = "homing-format" }   # relative to that directory
```

Before committing, remove the patch and run `cargo update -p homing-format` so that
`Cargo.lock` points at a published commit again.

## Guidelines

- **Tests:** every change needs them. Security-relevant code (signature checks, extraction,
  path handling, caller authentication) must fail closed and needs negative tests.
- **Interfaces:** The configuration, socket API, app environment and on-device data layout are interfaces that deployed devices and apps rely on. Changes to them need an issue first and a CHANGELOG entry. The install pipeline must stay crash-safe: every new persistent state needs a recovery path and a test (see `tests/e2e.rs`).
- **No `unsafe`:** the crate forbids it.
- **Dependencies:** keep them few. They must use permissive licenses (MIT, Apache-2.0, BSD, ISC,
  Zlib or similar); no GPL, LGPL or AGPL.
- **Documentation:** update README.md when behavior changes, and add an entry under
  "Unreleased" in [CHANGELOG.md](CHANGELOG.md).

## Commits and pull requests

- Use [Conventional Commits](https://www.conventionalcommits.org/) (`feat:`, `fix:`, `docs:`,
  `test:`, ...). Mark breaking changes with `!` (for example `feat!:`).
- Sign off every commit (`git commit -s`) to certify the
  [Developer Certificate of Origin](https://developercertificate.org/).
- Keep pull requests focused, one logical change each. CI must pass before merge.

## Releases

See [RELEASING.md](RELEASING.md).

## License

By contributing, you agree that your contributions are licensed under the MIT license of this
repository.
