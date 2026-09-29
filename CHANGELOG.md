# Changelog

All notable changes to `subgov` are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
`subscription-governor` binary follows [Semantic Versioning](https://semver.org/).
The YAML `version:` field inside `governor.yaml` is a separate, independently
versioned config-schema contract; see
[docs/notes/compatibility.md](docs/notes/compatibility.md).

## [Unreleased]

No tagged release exists yet; `v1.0.0-rc.1` is the first planned cut (§16 WP7
of [the implementation plan](docs/plan/plan.md)). The entries below describe
what `main` actually does today, not a diff against a prior tag.

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
- A §13 "planned metrics" JSONL surface: a `metrics` event emitted once per
  account per cycle (source success/failure and sample age, used/target
  fraction and seconds to reset per window, current/desired workers, the
  decision reason and binding window, and whether actuation was attempted
  and succeeded) regardless of that cycle's success or failure, plus a
  `cycle_metrics` event emitted once per cycle with loop duration and
  scheduling drift (late/skipped cycles). See
  [schema/metrics-event.schema.json](schema/metrics-event.schema.json).

This is pre-release software. See [the implementation plan](docs/plan/plan.md)
§16 for what remains open in WP5 through WP7 -- signal-aware shutdown,
per-source contract-test fixtures, operational runbooks, and the release
pipeline itself.
