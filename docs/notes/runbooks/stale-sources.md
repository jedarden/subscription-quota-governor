# Runbook: stale or failed quota/resource sources

Restates [the plan's freshness gate](../../plan/plan.md#91-freshness-gate)
(§9.1) and [generic source behavior](../../plan/plan.md#74-generic-source-behavior)
(§7.4) as an operator procedure, cross-checked against `src/controller.rs`
and `src/main.rs`.

## Two different conditions, not one

This runbook covers two distinct, operator-visible conditions that both
result in workers not scaling up. Telling them apart is the first
diagnostic step, because they have different root causes and different
remediation:

- **`provider_failure`** -- the source (or fleet observer) call itself
  failed this cycle: network error, malformed response, command exited
  non-zero, timed out, credentials unusable. No decision was computed at
  all for that account this cycle.
- **`stale_drain`** -- the source call *succeeded* and returned a
  snapshot, but that snapshot is too old to trust: either it says so itself
  (`fresh: false` in the normalized contract) or its `observed_at` is
  further in the past than the account's configured `stale_after_seconds`.
  A decision *was* computed, but the freshness gate overrides it to a safe
  hold-or-drain.

Both are read-side problems for one account; per §7.4 ("a source error
affects only its account and cannot actuate its fleet"), neither ever
affects any other configured account, and neither can push desired workers
above the current count.

## Detecting it

Start with `subgov --config PATH status`. It prints one JSONL line per
configured account, shaped like
`{"event": "status", "account": ..., "status": {"state": ..., "reason": ...}}`,
reading the sidecar `status.json` written after every cycle. It needs no
credentials and never contacts a live source -- safe to run at any time,
including against a deployment you don't have provider credentials for.
Look for `"state": "provider_failure"` or
`"state": "stale_drain"` (the full state set is `healthy_learning`,
`intentional_hold`, `provider_failure`, `stale_drain`, `actuation_failure`
-- `src/main.rs::ReadinessState`). A `state: "unknown"` entry means no cycle
has completed yet for that account, not a failure.

For more detail than `status` carries, read the run loop's own JSONL
output:

- Every cycle prints one `{"event": "decision", ...}` line to **stdout**
  per successfully-observed account, with `decision.stale: true|false` and,
  when stale, an empty `windows: []` (a stale decision skips per-window
  evaluation entirely -- there is nothing to arbitrate).
- Every failed observation or actuation prints one
  `{"event": "account_error", "account": ..., "category": "observation"|"actuation", "error": "...", ...}`
  line to **stderr**. `category: "observation"` covers both source and
  fleet-observer failures (both map to `provider_failure`); the `error`
  text is deliberately classified/generic (§14 requirement 8: raw provider
  response bodies never appear here), so it tells you *what kind* of
  failure occurred without leaking provider payloads.
- `run --once` exits non-zero when any account failed: exit `4` for one or
  more observation failures, exit `5` for one or more actuation failures
  (checked after every account is processed, so one account's failure does
  not stop others from being attempted -- §12 "Process every account even
  if another account fails").

## Diagnosing `stale_drain`

Compare `decision.observed_at` (in the `decision` event, or `observed_at`
in the `status` sidecar) against wall-clock now, and against the account's
configured `utilization.stale_after_seconds`. Common root causes, roughly
in likelihood order:

- **The collector stopped running.** For a `command` source, the script
  that used to print fresh JSON each cycle isn't running or is hanging past
  its own refresh cadence. For a `normalized_http` daemon collector, the
  daemon process died or stopped polling upstream -- see
  [secure HTTP deployment for the Z.AI collector](../zai-collector-http-deployment.md)
  for how that collector should be deployed and supervised.
  For `normalized_file`, the producer stopped writing new snapshots.
- **The collector is up but explicitly reports `fresh: false`.** This is
  the collector's own signal (the normalized contract defaults `fresh` to
  `true` when the field is absent -- `src/model.rs` -- so an explicit
  `false` is a deliberate claim, not an accident) -- for example, a Z.AI
  collector that knows its own last successful upstream poll is old. Fix
  it on the collector side; `subgov` is correctly deferring to what the
  collector told it.
- **Clock skew** between the collector/provider and the `subgov` host, if
  the collector and `subgov` run on different hosts. `observed_at` is
  compared against this host's clock.
- **Upstream provider outage or rate limiting** causing the collector
  itself to receive stale or failed data from the provider, without the
  collector-to-subgov leg being at fault.

`stale_behavior` (per account, in `utilization` config) controls what
happens while this persists: `hold` keeps the current worker count exactly;
`min_workers` steps down toward the fleet floor, still respecting
`max_scale_down_per_cycle`. Neither can ever increase workers -- verified
in `src/controller.rs::evaluate` (the stale branch caps the raw target at
`current_workers` before step limits, specifically to keep this invariant
unconditional even when `min_workers` exceeds the currently observed
count). The stale sample is also never folded into burn-rate history
(`state.rs`'s `AccountState::record` is skipped for a stale decision, only
`last_target` is updated) -- so once fresh data resumes, learning picks up
from the last good sample rather than being corrupted by the gap.

## Diagnosing `provider_failure`

Read the `account_error` event's `error` text; it identifies the failure
class without leaking secrets or raw response bodies. Common causes by
source type:

- **`normalized_http`**: connection refused/timed out (collector process
  down, or a network/ACL boundary misconfigured --
  see the [Z.AI HTTP deployment doc](../zai-collector-http-deployment.md)),
  a redirect (`subgov` refuses every 3xx response outright, so a
  reverse-proxy redirect will surface here), or a response over the 1 MiB
  bound.
- **`command`**: the script exited non-zero, wasn't found/executable, hung
  past its process timeout, or printed something that isn't the expected
  single JSON document on stdout.
- **`normalized_file`**: the file is missing, unreadable, or not valid
  JSON (a half-written file from a non-atomic producer looks like this --
  the producer must write atomically, the same requirement `subgov` itself
  follows for `state.json`; see
  [state backup and removal](../state-backup-and-removal.md)).
- **`anthropic_oauth`**: a `ConcurrentRefresh` result (Claude Code itself
  refreshed the credentials file mid-poll) is expected to be transient and
  self-resolves next cycle without operator action -- it is deliberately
  skipped rather than risking a clobbered refresh. A credential that is
  actually expired or invalid is a different, non-transient condition --
  see the authentication-expiry runbook (not yet published in this index).
- **`codex_app_server`**: the configured Codex executable is missing,
  crashed, or failed the app-server handshake within its timeout.

## What `subgov` is already doing correctly while this is happening

No action is required to prevent harm during either condition -- both are
fail-safe by construction (§9.1, verified above): workers never scale up,
the account's own failure never touches any other account's state or
decision, and stale samples never pollute the burn-rate history used for
pacing. Treat remediation as restoring service quality (correct autoscaling
resumes), not as an emergency to stop workers from over-provisioning.

## Remediation

1. Fix the underlying collector, network path, or command per the
   diagnosis above. This is entirely outside `subgov` itself -- there is no
   `subgov` command that repairs a source.
2. No `subgov` restart is required. A continuous `run` process picks up a
   recovered source on its next scheduled cycle automatically. For `run
   --once` deployments (e.g. cron-driven), the next invocation picks it up.
3. If you need a result immediately rather than waiting for the next
   scheduled cycle, run `subgov --config PATH snapshot ACCOUNT` to confirm
   the source itself is fixed (this performs no state write or actuation,
   so it's safe to run speculatively), then `subgov --config PATH run
   --once` to apply it.

## Verification

- `subgov status` reports `healthy_learning` or `intentional_hold` for the
  account again.
- The next `decision` event shows `"stale": false` and a non-empty
  `windows` array.
- No new `account_error` events for that account.

## When not to just wait it out

Widening `stale_after_seconds` to ride out a known-slow-but-recovering
provider is a real lever, but it trades safety margin for tolerance: a
wider window means `subgov` keeps trusting older data as "fresh" before
the freshness gate would otherwise engage, which is exactly the guard this
runbook exists to preserve. Prefer fixing the collector; treat widening
this value as a deliberate, temporary, and reverted-afterward exception,
not a standing fix.
