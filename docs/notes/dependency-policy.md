# Dependency pinning and audit policy

Restates the dependency rule from
[the plan's release-engineering work package](../plan/plan.md#wp7-release-engineering)
(§16 WP7) for anyone changing `Cargo.toml` without reading the whole plan.

## Every dependency is exact-pinned

`Cargo.toml` pins every direct dependency (including the Unix-only `nix`
target dependency and both `dev-dependencies`) to an exact version with `=`,
not a caret range. A clean clone with only `Cargo.lock` deleted resolves back
to the same versions; nothing floats to a newer minor or patch release on an
unrelated `cargo update`.

Bumping a dependency is a deliberate two-step change: edit the `=version` in
`Cargo.toml`, then run `cargo update -p <crate>` (or `cargo update`) so
`Cargo.lock` matches, and re-run the audit below before committing both files
together.

## Security-advisory audit

`scripts/audit.sh` runs `cargo audit` against `Cargo.lock` (RustSec advisory
database) and fails the build on any known vulnerability. Run it after any
dependency change and before a release tag:

```console
./scripts/audit.sh
```

This is a local, repo-level check today; wiring it into the Argo `iad-ci`
workflow is the remaining part of §16 WP7 ("verify ... in the configured Argo
workflow"), tracked separately from dependency pinning itself.

`cargo-audit` checks published RustSec security advisories; it does not check
license compatibility. `serde_yaml` 0.9.34 is the crate's final release
(tagged `+deprecated` by its author, archived upstream) but currently carries
no open RustSec advisory -- its continued use is a maintenance risk to
revisit, not a known vulnerability.
