# Changelog

All notable changes to `subgov` are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
`subscription-governor` binary follows [Semantic Versioning](https://semver.org/).
The YAML `version:` field inside `governor.yaml` is a separate, independently
versioned config-schema contract; see
[docs/notes/compatibility.md](docs/notes/compatibility.md).

## [Unreleased]

### Added

- Plan §28.6 shared account total and cross-host placement (WP29): every
  federation instance derives one account total from peer records without
  exchanging decisions (`federation::shared_account_total`). The §28.6
  sample rule (`freshest_used_fraction`) takes the freshest held quota
  sample — this instance's own plus every fresh-or-stale peer's — and
  switches to the largest fraction when any sample disagrees by more than
  0.05 (the conservative reading); with no sample anywhere the §9.1 stale
  rule holds the combined current. The burn denominator is
  `governed_workers` summed across all hosts (`FederationHost`), fed
  through the pure §9 controller, and the total is split across hosts by
  the §22.7 placement engine (`split_account_total`, reusing
  `placement::distribute`): weights are each host's worker headroom from
  the peer records, each share is capped by the smaller of the host's
  reserve ceiling and the work its ready demand can absorb, a host with
  zero ready demand receives none of the new placement while keeping its
  running workers until the total shrinks, no share ever exceeds its
  reserve, and the split never exceeds the controller total. Per-host
  step limits stay with each instance's own actuation of its share.
  `PeerTable::peer_account_snapshot` feeds the derivation from the
  receiver's clock: fresh and stale peers contribute exactly what they
  last published, expired peers contribute zeros and no sample (§28.7).

- Plan §28.3/§28.7 federation peer pull, staleness and freeze-on-stale
  (WP28): `federation:` gains `peers: [{host_id, argv}]` (argv defaulting
  to the sanctioned `ssh -o ConnectTimeout=5 <host> subgov peer-state
  --json`), `peer_stale_after_seconds` (default 120) and
  `peer_expiry_seconds` (default 1800, validated to exceed the stale
  bound). A pull runs through the existing generic command primitive with
  the §7.4 bound covering the whole SSH round trip; any failure — spawn,
  nonzero exit, timeout, oversize, truncated or trailing output, unknown
  schema major — is a failed fetch, so a peer is ever stale or fresh,
  never partially applied. Records age on the receiver's monotonic clock
  from receipt, never from the peer's timestamp; a record more than 300 s
  ahead of the local clock, or claiming a host other than the peer it was
  pulled from, is rejected. While any configured peer is stale (including
  one never yet heard from) the instance may scale down but never up
  (`PeerTable::clamp_scale_up`) and assumes the stale peer still runs its
  last published `governed_workers`; after `peer_expiry` the peer counts
  as zero workers, a `peer_expired` event fires once per staleness
  episode, and scale-up may resume. Peers are never removed automatically.
  WP29 wires the table into the shared total and cross-host placement.

- Plan §28.4/§28.13 federation peer record (WP27): new `federation:` config
  block (`host_id`, optional `instance_id_file`/`record_seq_file`,
  `host_capacity_state`, per-account `account_identities`) and a read-only
  `subgov peer-state --json` command that prints the instance's versioned
  peer record: persisted random `instance_id` (mode 0600, created on first
  use), `host_id`, monotonic `record_seq`, `observed_at`, and per account
  and pool the account fingerprint (truncated SHA-256 of the provider's
  non-secret identifier — the identifier itself never leaves the instance),
  `governed_workers`, `pending_launches`, `desired`, the latest quota
  sample, `ready_demand`, the WP24 measured ceiling and launch
  reservations, the host `resource_reserve`, and the git origin URLs of
  seedable workspaces with embedded userinfo stripped (§14). An unknown
  `schema_version` major is rejected outright; unknown fields inside a
  known major stay compatible. A same-id/different-fingerprint peer
  account is classified as a different account with an
  `account_fingerprint_mismatch` error event (§28.5); distinct account ids
  remain ordinary independent accounts (§28.11).

- Source-publisher alerting (subgov-28a476df): the deployed source units gain
  `OnFailure=repo-traffic-alert@%n.service` so a failed cycle pages;
  `scripts/subgov_source_watch.py` (deployed via
  `subgov-source-watch.service`/`.timer`, `deploy/codinghome/`) exits 1 when a
  source unit is failed or its snapshot is older than three 300s cycles,
  naming the stale source, and its own `OnFailure=` pages with those lines;
  and the zai/codex/anthropic publishers log the exception message plus a
  failed subprocess's stderr (bare class names hid both 2026-10-23 incident
  causes). The shipped `deploy/codinghome/zai-source-refresh.json` pins the
  collector selector to `app=zai-proxy,variant=production` so a test-variant
  pod can no longer break the replica census.

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
  Each successful retirement appends a durable record (worker_id, agent,
  unit, pid, method, retired_at) at `--state` under the manifest identity
  hash, written atomically mode 0600 under the state lock; a cycle that
  retires nothing writes nothing. See `examples/takeover-claude-zai.yaml`,
  the deployed `deploy/codinghome/zai-takeover-governor.yaml` (full
  19-target list mirroring the manifest), and
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
