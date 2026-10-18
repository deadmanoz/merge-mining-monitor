# Release Versioning

Root `Cargo.toml` `[workspace.package].version` defines the release SemVer.
Workspace crates inherit it, the wiring crate exposes it as `VERSION`, and `mmm-api` exposes it through `/api/v1/version`.

`RELEASE_NOTES.md` is embedded at compile time and rendered in the About dialog.
Keep it concise and user/operator-facing.
`CHANGELOG.md` is for engineering history and does not drive the UI.

Release tags use `v<SemVer>`, for example `v0.1.0`.

## Release Flow

1. Move release-note entries into `## [X.Y.Z] - YYYY-MM-DD`.
2. Set root `Cargo.toml` `[workspace.package].version` to `X.Y.Z` and update the workspace package entries in `Cargo.lock`.
3. Update `CHANGELOG.md` (`www/` uses unversioned relative module specifiers on disk; `mmm-api` serves `/v/<version>/...` asset URLs automatically from `CARGO_PKG_VERSION`).
4. Run `just gen-source-artifacts` to refresh `fixtures/api/version.json` from the compile-time `/api/v1/version` projection.
5. Run the quality gates: `just lint`, `just test`, `just test-integration`.
6. Build from the same commit.
7. Tag the commit as `vX.Y.Z`.
