# Releasing homing-agent

Releases follow [Semantic Versioning](https://semver.org/). Before 1.0, a minor version may
change the configuration, socket API, app environment or data layout; a patch version must not.
Devices keep their data across upgrades, so a change to `agent-state.json`, `slots.json` or
the data layout must read the old format too.

1. Make sure `main` is green and the Dependabot PRs are handled. If homing-format has a new
   release, update the tag in `Cargo.toml` and run `cargo update -p homing-format`.
2. Update `version` in `Cargo.toml` (and `Cargo.lock` with `cargo check`).
3. In `CHANGELOG.md`, rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD`, add a new empty
   `## [Unreleased]`, and update the links at the bottom.
4. Update the version in the README's install instructions.
5. Commit (`chore: release vX.Y.Z`, signed off) and push. Wait for CI.
6. Tag and push the tag:

   ```sh
   git tag -a vX.Y.Z -m "homing-agent vX.Y.Z"
   git push origin vX.Y.Z
   ```

   The Release workflow builds the binaries and publishes the GitHub release, with the
   CHANGELOG section as notes. To test the builds without publishing, run the workflow
   manually ("Run workflow" on the Actions tab).
