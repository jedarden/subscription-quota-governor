# Linux release artifacts

The packaging command creates one Linux x86_64 `subgov` executable for each
version tag whose version matches `Cargo.toml`. With `--publish`, it uploads the
versioned executable, SHA-256 checksum manifest, and in-toto SLSA provenance
statement to the Forgejo Generic Package Registry. Published package versions
are immutable: repeating a publication succeeds only when every existing asset
has identical bytes. In `declarative-config`, the
`subscription-quota-governor-tag` Sensor starts an Argo release workflow for
version tags pushed to Forgejo. The workflow verifies the tagged commit with
the CI gates, builds the Linux binary, and publishes the three assets.
The publisher credential is a separate, package-write-only Forgejo token read
from OpenBao at `rs-manager/iad-ci/forgejo/subgov-package-publisher`; the
workflow never logs it.

## Rebuild a release locally

Use Rust 1.97.1 (pinned in `rust-toolchain.toml`) and a clean clone at the
release tag:

```sh
git clone https://git.ardenone.com/jedarden/subscription-quota-governor.git
cd subscription-quota-governor
git checkout v1.0.0-rc.1
SUBGOV_BUILDER_ID='https://git.ardenone.com/jedarden/subscription-quota-governor/builders/argo-workflows/v1' \
scripts/package-linux-release.sh v1.0.0-rc.1 /tmp/subgov-release
(cd /tmp/subgov-release && sha256sum --check SHA256SUMS)
```

The output directory contains:

- `subgov-v1.0.0-rc.1-linux-x86_64`, the executable;
- `subgov-v1.0.0-rc.1-linux-x86_64.intoto.jsonl`, SLSA provenance bound to the
  executable's SHA-256 digest, source tag and commit, lockfile, toolchain
  target, and package command;
- `SHA256SUMS`, covering the executable and provenance file. Provenance omits
  wall-clock build times so clean rebuilds reproduce both checksums.

The provenance describes the build inputs and invocation. Argo includes its
unique Workflow UID as the SLSA invocation ID; local rebuilds omit that
run-specific field. Provenance is distributed alongside the release assets and
is not itself signed. Run `--publish` to upload a locally verified release;
the registry rejects it if any asset differs from an existing version.

A clean local rebuild can reproduce its binary, provenance, and `SHA256SUMS`.
The published Argo provenance has a run-specific invocation ID, so a local
rebuild of that same commit produces different provenance and checksum bytes.

Assets are downloaded from:

```text
https://git.ardenone.com/api/packages/jedarden/generic/subgov/1.0.0-rc.1/<filename>
```
