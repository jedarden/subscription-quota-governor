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

The same script is a required gate in the `iad-ci` Argo WorkflowTemplate
`subscription-quota-governor-ci` (declarative-config,
`k8s/iad-ci/argo-workflows/subscription-quota-governor-ci-workflowtemplate.yml`).
The workflow installs `cargo-audit` 0.22.1 (pinned, `--locked`) and runs
`./scripts/audit.sh` after `cargo doc` in the single fail-fast verify script,
so any advisory against `Cargo.lock` fails the workflow before the schema and
secret-scan steps. Bump the pinned `cargo-audit` version in that template
deliberately, like any other dependency.

`cargo-audit` checks published RustSec security advisories; it does not check
license compatibility. `serde_yaml` 0.9.34 is the crate's final release
(tagged `+deprecated` by its author, archived upstream) but currently carries
no open RustSec advisory -- its continued use is a maintenance risk to
revisit, not a known vulnerability.
