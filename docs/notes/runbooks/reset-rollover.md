# Runbook: quota window reset rollover

Restates
[the plan's reset-generation safety](../../plan/plan.md#93-reset-generation-safety)
(§9.3) and
[linear-to-reset pacing](../../plan/plan.md#94-linear-to-reset-pacing)
(§9.4) as an operator procedure, cross-checked against
`src/controller.rs::evaluate`. This covers the normal, expected behavior as
a quota window crosses its `resets_at` boundary, and how to tell a healthy
rollover apart from a stuck one.

## What a "generation" is

Every window (`five_hour`, `weekly`, or whatever the provider names it) has
a `resets_at` timestamp. Two samples of the same window are only comparable
-- and therefore only usable to estimate a burn rate -- when their
`resets_at` values are **identical** (§9.3). A window's `resets_at` moving
to a new value marks the start of a new generation; the controller
deliberately does not try to reconcile or extrapolate across that boundary,
it just starts learning fresh. This is a stricter check than "did enough
time pass" -- it is byte-for-byte timestamp equality
(`sample.resets_at == window.resets_at` in `src/controller.rs`).

## The three reasons you'll see across a rollover

Every `decision` JSONL event carries each window's `reason`. Across a
reset, an account with steady traffic normally walks through this sequence:

1. **`paced_to_reset`** (steady state before the reset) -- two same-generation
   samples exist, so the controller has an observed burn rate and is
   actively pacing toward the window's target.
2. **`reset_due`** -- `window.resets_at <= now`: the reset time has passed
   according to `subgov`'s clock, but the provider is still reporting the
   *old* generation (same `resets_at`, same or higher `used_fraction`). The
   controller holds at `current_workers` rather than guessing -- §9.3: "if
   a reset is due but the provider has not published a new generation,
   hold the current count; do not extrapolate across the boundary." **This
   is expected and can last anywhere from seconds to the provider's own
   publication lag; it is not itself a problem.**
3. **`bootstrap_burn_rate`** (once, immediately after the new generation
   appears with `current_workers == 0`) or **`learning_burn_rate`**
   (otherwise) -- the provider has published a new `resets_at` /
   `used_fraction` pair, no prior sample matches this new generation yet,
   so there is nothing to compute a rate from. The controller holds
   (`learning_burn_rate`) or requests `bootstrap_workers`
   (`bootstrap_burn_rate`) for exactly one cycle, then the *next* sample in
   this same generation lets `paced_to_reset` resume.

A window that never sees meaningful traffic can also land on
**`no_observed_burn`**: two same-generation samples exist, but the
resulting rate wasn't usable (see below) -- distinct from `reset_due` or
`learning_burn_rate`, and not itself a rollover problem.

Watch `resets_at` in the `decision` event's per-window output directly --
it is the ground truth for "has the generation actually changed yet,"
independent of the `reason` string.

## Diagnosing a stuck rollover

`reset_due` is only a problem if it persists well past when the window
should have rolled over -- e.g. a `five_hour` window still reporting
`reset_due` an hour later. When that happens:

- Confirm independently (outside `subgov`) that the provider itself has
  actually rolled the window over -- check the provider's own dashboard or
  API directly. If the provider hasn't rolled over either, this is a
  provider-side delay, not a `subgov` problem; keep holding.
- If the provider *has* rolled over but `subgov` still reports the old
  `resets_at`/`used_fraction`, the source is returning stale data -- this
  is now a staleness problem, not a rollover one. See the
  [stale/failed sources runbook](stale-sources.md): check `snapshot.fresh`
  and `stale_after_seconds` first.
- For `command`/`normalized_file`/`normalized_http` sources, confirm the
  site-local collector itself is polling the provider on a cadence tight
  enough to observe the rollover promptly -- a slow-polling collector
  looks identical to a provider delay from `subgov`'s side.

A window stuck at `learning_burn_rate` for more than two or three cycles
(rather than resolving to `paced_to_reset` on the very next same-generation
sample) means the second sample either arrived too soon or didn't clear
one of §9.4's usability conditions -- check, in order:

- **`dt >= minimum_sample_seconds`** -- if `poll_interval_seconds` is close
  to or smaller than `minimum_sample_seconds`, consecutive cycles can be
  individually too close together to produce a usable pair even though the
  generation hasn't changed. This resolves itself as soon as enough
  cycles accumulate; it is not a bug.
- **`used_fraction` went backward** (`u1 < u0`) within the same
  generation -- the controller treats this as not-yet-comparable rather
  than as a negative burn rate (`window.used_fraction >= sample.used_fraction`
  is one of the sample-reuse filters in `src/controller.rs`). A provider
  correction or a genuinely bogus decrease both look like this from
  `subgov`'s side; if the provider's own reporting is the cause, that's
  outside `subgov`'s control and self-resolves once usage passes the
  earlier high-water mark again.
- **the prior sample recorded zero workers** (`sample.workers > 0` is
  required) -- expected right after a `min_workers: 0` fleet has been
  fully idle; the next sample taken once workers are non-zero again
  becomes usable normally.

`no_observed_burn` specifically means a same-generation pair *was* found
but `estimate_burn_per_worker` rejected the resulting rate: non-finite, or
`<= 0` (`src/controller.rs::estimate_burn_per_worker`). A `<= 0` rate from
a non-negative usage delta happens when the delta quantizes to exactly
zero at the provider's reporting precision over the elapsed interval --
§9.4's "a zero observed delta holds because quantized percentages do not
prove zero consumption," deliberately not treated as a learned zero burn
rate. This resolves itself once enough usage accumulates between samples
to clear the provider's quantization step; widening
`poll_interval_seconds` for that account is the operator lever if it
recurs constantly, trading responsiveness for a larger per-sample delta.

## The weekly window plus banked resets

A weekly window's rollover interacts with banked-reset pacing
(§9.7) when `banked_resets.enabled`: hitting
`redeem_at_utilization` sets `manual_redemption_recommended: true` in the
`banked_resets` field of the `decision` event and drains toward
`min_workers`, independent of the ordinary per-window rollover reasons
above. That is a distinct operator situation (a human needs to redeem a
credit) covered by [ADR-0001](../../adr/0001-human-controlled-reset-redemption.md),
not by this runbook -- if you see `manual_redemption_recommended: true`,
that's the thing to act on, not the rollover mechanics here.

## What to expect, not do

For an ordinary rollover with no stuck condition, there is nothing for an
operator to do. `reset_due` and `learning_burn_rate`/`bootstrap_burn_rate`
are self-limiting states the controller walks through automatically and
safely -- per §9.1's neighboring invariant, none of these states can
increase workers beyond what's already justified, so there is no
overspend risk to react to by intervening early. Treat manual intervention
here as the exception (a genuinely stuck provider-side rollover or a
source problem), not the default response to seeing these reason strings
in the decision log.

## Verification

- The window's `decision` event reason returns to `paced_to_reset` (or
  `target_reached`/`below_ceiling`, depending on strategy and current
  utilization) within a small number of cycles after the provider's own
  rollover.
- `resets_at` in the decision event matches the new generation's actual
  value from the provider.
- `observed_burn_per_worker_hour` is populated again once pacing resumes.
