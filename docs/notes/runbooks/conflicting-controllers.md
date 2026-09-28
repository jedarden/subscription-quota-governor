# Runbook: conflicting controllers on the same fleet

Restates [the plan's state and concurrency section](../../plan/plan.md#10-state-and-concurrency)
(§10, "only one controller may own a fleet") as an operator procedure,
cross-checked against `src/state.rs` and `src/fleet.rs`. This covers two
controllers -- two `subgov` instances, or `subgov` alongside a legacy or
hand-rolled governor/autoscaler -- both acting on the same fleet at once.

## The one thing `subgov` enforces automatically, and its limit

`StateLock::acquire` takes a non-blocking exclusive `flock` on
`<state_path>.lock` and refuses to start if another process already holds
it (`src/state.rs`) -- the resulting error surfaces as exit code `3`
("state ownership or persistence failure", `src/main.rs`) with a message
containing `another governor owns <path>`. **This only protects two
processes that share the exact same configured `state_path`.** Per §10:
"different state paths are not permission to run competing governors
against the same actuator; deployment tooling must enforce that
operational invariant" -- `subgov` has no way to know that two different
state paths both ultimately point at the same underlying fleet. A second
instance started with a different `--config` (different `state_path`, same
`fleet.observer`/`fleet.actuator` targets) starts up cleanly, takes its own
lock at its own path, and fights the first one with no error from either
side.

A non-`subgov` legacy governor doesn't use `subgov`'s state file or lock at
all, so this protection provides nothing against it -- detection there is
entirely behavioral (below).

## Detecting it

There is no `provider_failure`/`stale_drain`-style readiness state for
this condition specifically -- a conflicting controller looks like the
fleet's *observed* state disagreeing with what `subgov` itself last
decided, not like a source or actuation error. Symptoms:

- **Worker count oscillates or "fights back."** Compare consecutive
  `decision` events' `current_workers` against the *previous* cycle's
  `desired_workers` for the same account. In a fleet with a single
  controller, this cycle's `current_workers` should reflect the last
  cycle's actuation (once the observer/actuator round-trip has had time to
  settle). A `current_workers` that keeps landing somewhere `subgov` didn't
  just set it to -- especially oscillating rather than trending toward
  `subgov`'s own target -- is the primary signal.
- **`observed worker count ... is outside the configured fleet range**
  `[min_workers, max_workers]``.** With the default
  `observer_reconciliation: strict`, an observed count outside `subgov`'s
  configured range fails that account's cycle outright (`provider_failure`
  in `subgov status`, `account_error` with `category: "observation"`).
  This is `src/fleet.rs::reconcile_observed_range`'s explicit documented
  case for "a fleet whose real state legitimately drifts outside the
  configured range (e.g. another reconciler is also touching it)" -- read
  literally, an unexplained `strict` rejection here is one of the more
  direct signals this runbook covers. If `observer_reconciliation: clamp`
  is configured instead, the same drift is silently absorbed (the count is
  clamped into range and the cycle proceeds normally) -- which avoids the
  hard failure but also hides the signal; if you suspect a conflicting
  controller, temporarily switching that account to `strict` surfaces it
  explicitly instead of masking it.
- **A `target_file` actuator's file changes between `subgov` cycles in a
  way `subgov` didn't write.** `subgov`'s own writes are atomic
  (temp-file-then-rename, the same pattern documented in
  [state backup and removal](../state-backup-and-removal.md)), so this
  won't look like a corrupt file -- it will look like a *fully valid* file
  containing a value `subgov` never decided on. There is no built-in
  conflict detection on this file; whichever writer runs last simply wins,
  silently.
- **A `command` actuator/observer script itself is shared** with another
  tool (the same script invoked by two schedulers, or a script that itself
  talks to an orchestrator another controller also drives) -- this doesn't
  show up as a `subgov`-side anomaly at all until the worker count drifts
  per the first bullet; check the script's own invocation history/logs
  outside `subgov`.

## Diagnosing which second controller it is

1. **Another `subgov` instance, same `state_path`.** Confirms itself: you
   will see the exit-code-3 "another governor owns" failure the moment the
   second one starts, so this case doesn't produce silent fighting -- if
   you're diagnosing oscillation, this isn't it.
2. **Another `subgov` instance, different `state_path`, same fleet.**
   Check for more than one `subgov` process (`ps aux | grep subgov`, or the
   process supervisor's own unit list -- e.g. `systemctl status
   'subgov*'`) and compare each instance's config for the same
   `fleet.observer`/`fleet.actuator` target (same file path, same command,
   or the same underlying worker pool by whatever name the observer/
   actuator command addresses it).
3. **A non-`subgov` governor or manual script.** Check for cron jobs,
   other systemd units, or manually-run scripts that write to the same
   `target_file` path or invoke the same worker-management command
   `subgov`'s actuator does. This is the case §10 explicitly puts outside
   `subgov`'s own ability to detect -- it is deployment-topology knowledge,
   not something the running process can discover about itself.

## Remediation

There is no `subgov` mechanism to resolve this automatically -- ownership
of a fleet is an operational invariant enforced by deployment topology, not
by code (§10). Pick exactly one controller per fleet:

1. Stop every controller acting on the fleet except the one you intend to
   keep.
2. If keeping `subgov`, verify only one instance runs against that fleet's
   `observer`/`actuator` targets going forward -- not just one instance per
   `state_path` (which the lock already enforces), but one instance,
   period, per underlying fleet.
3. If moving *off* `subgov` for that fleet, stop the `subgov`
   process/unit rather than leaving it running with a `none` actuator "just
   in case" -- an idle-but-running `subgov` with `none` at least can't
   actuate, but it still consumes a source poll and a lock every cycle for
   no purpose, and someone re-enabling its actuator later without
   realizing a different controller is now authoritative recreates this
   exact problem.

## Verification

- Only one process holds `<state_path>.lock` (confirm via `lsof
  <state_path>.lock` or the process list from diagnosis above).
- `decision` events' `current_workers` consistently reflects `subgov`'s own
  prior `desired_workers`/actuation, cycle over cycle, without unexplained
  jumps.
- No further `observed worker count ... is outside the configured fleet
  range` failures (with `observer_reconciliation: strict`) or unexplained
  clamps (with `clamp`).
