# Release Versioning

Root `Cargo.toml` `[workspace.package].version` defines the release SemVer.
Workspace crates inherit it, the wiring crate exposes it as `VERSION`, and `mmm-api` exposes it through `/api/v1/version`.

`RELEASE_NOTES.md` is embedded at compile time and rendered in the About dialog.
Keep it concise and user-facing: what site visitors can see, or what data changed.
Operator steps, commands, migrations, table or API field names, commit pins and internal-only changes belong in `CHANGELOG.md` and `docs/operations.md`; a release with no visible change says so in one line.
`CHANGELOG.md` is for engineering history and does not drive the UI.

Release tags use `v<SemVer>`, for example `v0.1.0`.

## Documentation Review

Before a release, check README and About against visible features, including protocol declarations and error-block evidence.
Review runtime dependencies, setup/readiness steps, the dated size example, statistics, screenshots and diagrams. Keep the size note short; remeasure when updating its date or deployment context.
Use the [pre-1.0 schema policy](../migrations/README.md#pre-10-schema-policy) until a migration consolidation and its upgrade procedure are actually released.

Stage unreleased changes in `CHANGELOG.md`, then prepare visitor-facing wording for `RELEASE_NOTES.md` at release time.
The newest served release-note section must match the workspace version; do not add an `Unreleased` section to the served notes or attribute new changes to an earlier release.
Regenerate `fixtures/api/version.json` whenever served notes change, using the release flow below.

## Release Flow

1. Move release-note entries into `## [X.Y.Z] - YYYY-MM-DD`.
2. Set root `Cargo.toml` `[workspace.package].version` to `X.Y.Z` and update the workspace package entries in `Cargo.lock`.
3. Update `CHANGELOG.md` (`www/` uses unversioned relative module specifiers on disk; `mmm-api` serves `/v/<version>/...` asset URLs automatically from `CARGO_PKG_VERSION`).
4. Run `just gen-source-artifacts` to refresh `fixtures/api/version.json` from the compile-time `/api/v1/version` projection.
5. Run the quality gates: `just lint`, `just test`, `just test-integration`.
6. Build from the same commit.
7. Tag the commit as `vX.Y.Z`.
