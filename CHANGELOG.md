# Changelog

All notable changes to `subgov` are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
`subscription-governor` binary follows [Semantic Versioning](https://semver.org/).
The YAML `version:` field inside `governor.yaml` is a separate, independently
versioned config-schema contract; see
[docs/notes/compatibility.md](docs/notes/compatibility.md).

## [Unreleased]

### Added

- Plan §26.2 roster takeover: an account's fleet may declare a `takeover`
  list of roster workers (worker_id, agent, optional systemd unit) plus a
  `takeover_helper` command. Listed workers leave the §23 unowned floor, so
  elastic workers launch even while the roster satisfies demand, and after
  each cycle's elastic reconcile the helper runs one idle-only retirement
  step, retiring a listed worker only while a fresh prefixed replacement
  runs in the same pool. `scripts/zai-takeover.py` is the production helper
  for the Z.AI pool (identity, idleness, live-children and unit MainPID
  verification, pidfd/SIGSTOP/SIGTERM, restart-guard drop-ins for unit
  targets, straggler adoption without a unit; never `needle cleanup`).
  See `examples/takeover-claude-zai.yaml` and
  `deploy/codinghome/zai-takeover-manifest.json`.
- Plan §26.2 cutover rehearsal: `scripts/zai-takeover.py --dry-run` prints one
  verdict per takeover target (eligible / busy / identity mismatch / unit
  missing / stale heartbeat / not running) by running every read-only
  retirement gate — identity pinning, idleness, live children, unit MainPID —
  plus the pool replacement budget, without opening a pidfd, taking the state
  lock, or sending any signal.
- Plan §26.5 seed-workspace selection: new workers are launched with
  `-w <ready workspace with fewest running workers>` and never bare. The
  productive pool adapters seed each launch from live supervisor occupancy;
  `needle_run` actuators accept an optional `seed_demand_state` /
  `seed_heartbeats` pair to seed from the WP23 demand state and NEEDLE
  heartbeats, failing closed when the demand evidence is missing or failed.
  Without the pair the configured `repo` remains the permanent single seed.

## [1.0.0-rc.1] - 2026-10-03

First release candidate for observation-only rollout. Actuation remains a
separately controlled step.

### Added

- Provider-neutral governor core with `check`, `snapshot <account>`, and
  `run [--once] [--observe-only]` CLI commands.
- Native Claude Code / Anthropic OAuth usage collector: credential refresh
  within five minutes of expiry, atomic mode-0600 credential rewrites that
  preserve unknown fields, and legacy/generic window normalization.
- Native Codex `app-server` collector using the documented
  `account/rateLimits/read` method, including banked reset-credit detection
  and configurable pacing. Redemption stays a human, interactive action --
  see [ADR-0001](docs/adr/0001-human-controlled-reset-redemption.md).
- Generic `command`, `normalized_file`, and `normalized_http` sources for
  Z.AI and other site-local collectors.
- `linear_to_reset` and `ceiling_only` utilization strategies, per-window
  overrides, staleness gating, and bootstrap-worker probing for an empty
  fleet.
- File- and command-based fleet observers and actuators, plus a `none`
  actuator so observe-only deployment is the default-safe choice.
- Durable, versioned, advisory-locked JSON state with atomic writes, so only
  one process can own a state file at a time.
- Per-account failure isolation: a source or actuation error affects only
  that account and never blocks or misrepresents another account's cycle.
- A hardened systemd service example (`deploy/systemd/`), with restart
  limits and filesystem protections.
- Exact-pinned dependency versions and a `scripts/audit.sh` RustSec
  security-advisory check (`cargo audit`); see
  [docs/notes/dependency-policy.md](docs/notes/dependency-policy.md).
- Versioned Linux x86_64 release artifacts with SHA-256 checksums and an
  in-toto provenance statement; see
  [docs/notes/releasing.md](docs/notes/releasing.md).
- A §13 JSONL metrics surface: a `metrics` event emitted once per
  account per cycle (source success/failure and sample age, used/target
  fraction and seconds to reset per window, current/desired workers, the
  decision reason and binding window, and whether actuation was attempted
  and succeeded) regardless of that cycle's success or failure, plus a
  `cycle_metrics` event emitted once per cycle with loop duration and
  scheduling drift (late/skipped cycles). See
  [schema/metrics-event.schema.json](schema/metrics-event.schema.json).

This prerelease uses configuration schema `version: 1`; see the
[compatibility policy](docs/notes/compatibility.md). It is intended for
observation-only rollout.
