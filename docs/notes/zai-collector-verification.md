# Z.AI collector verification: reset-generation transitions and usage normalization

This closes [the plan's Claude Code with Z.AI section](../plan/plan.md#73-claude-code-with-zai)
(§7.3)'s last unchecked build requirement: "Verify reset-generation
transitions and percentage/absolute-usage normalization in the site-local
collector before production rollout." It is a verification record, not new
production code -- every check below runs through code and fixtures already
committed under earlier §7.3 work (the schema, the example, and the
`tests/zai_collector_contract.rs` contract suite).

## What cannot be verified, and why

**No Z.AI provider surface is assumed by this repository** (§7.3: "No private
endpoint or deployment component is part of this repository";
[provider-surfaces.md](../research/provider-surfaces.md): "No stable public
provider surface is assumed"). A real collector -- the thing that actually
talks to Z.AI, reads whatever raw shape its API returns, and computes a
`used_fraction` -- is an external process the operator writes and runs
themselves. There is no such binary in this repository to run against a real
or recorded Z.AI response, and there will not be one: keeping provider-specific
parsing at the edge, entirely outside `subgov`, is the deliberate design
(§7.3, §22.10 for the analogous cross-host case).

So this verification is scoped to what actually is in this repository: the
normalized contract (`schema/quota-snapshot.schema.json`,
`schema/zai-collector-snapshot.schema.json`), the generic `command` transport
a collector uses to hand off its output (`src/source.rs`), and the shared
controller (`src/controller.rs::evaluate`) every source feeds into
identically. What's verified below is that this contract and pipeline behave
correctly for both concerns the checklist item names, end-to-end through the
real command-transport collection path -- not that any particular real Z.AI
collector implementation is correct, which is out of scope by design.

## Reset-generation transitions

Two existing tests already covered pieces of this:

- `controller.rs`'s `reset_heavy_trace_restarts_learning_each_generation_and_holds_at_the_boundary`
  proves the *controller* handles a reset-generation boundary correctly, but
  drives `evaluate` with hand-built `QuotaWindow`/`WindowSample` literals,
  never through an actual source collection.
- `zai_collector_contract.rs`'s `a_reset_generation_rollover_fixture_is_accepted_despite_its_reset_time_already_having_passed`
  proves a *single* Z.AI-shaped snapshot taken right at a reset boundary
  collects successfully through the real `command` transport, but only
  exercises one cycle -- it cannot show whether a *prior* generation's learned
  burn rate would incorrectly carry across the boundary, because there is no
  prior state in that test.

Neither, by itself, proves the full pipeline -- real command-transport
collection feeding the real cross-cycle `AccountState` `main::run_cycle`
builds up -- handles a live transition correctly. Added
`a_reset_generation_transition_through_the_real_command_transport_never_extrapolates_the_prior_generations_rate`
closes that gap: three sequential Z.AI-shaped documents, each collected
through `subscription_governor::source::collect` over a real `command` source
exactly like `collect_via_command` does elsewhere in this suite, chained the
same way `main::run_cycle` chains cycles (each cycle's decision folded into
`AccountState` via `record` before the next cycle evaluates):

1. **First sample of generation one** (`resets_at` = 17:00): no prior sample
   exists, so the decision reason is `learning_burn_rate` -- nothing to pace
   against yet.
2. **Second sample of generation one**, one hour later, usage risen from 0.10
   to 0.30: the same generation (`resets_at` unchanged) and past
   `minimum_sample_seconds`, so the controller learns a real positive
   `observed_burn_per_worker_hour` and the reason becomes `paced_to_reset`.
3. **First sample of generation two** (`resets_at` advanced to 22:00, usage
   back down to 0.02 -- the same shape as the committed
   `reset_generation_rollover` fixture): the reason returns to
   `learning_burn_rate`, proving generation one's learned rate was not
   extrapolated across the boundary. This is the same-generation filter in
   `evaluate` (`sample.resets_at == window.resets_at`) doing its job when
   driven by real collected snapshots, not just by literals a test constructed
   directly.

## Percentage/absolute-usage normalization

The normalized contract (`schema/quota-snapshot.schema.json`) accepts exactly
one usage representation: `used_fraction`, a single finite value in `[0, 1]`.
There is no field for a raw percentage or a raw used/quota token pair
anywhere in the wire contract or in `QuotaWindow` (`src/model.rs`) --
whichever form a real Z.AI API response uses, the collector converts it to
`used_fraction` itself before `subgov` ever sees the document. This is
intentional: `subgov`'s policy and controller code never branches on a
provider name or a raw representation (`provider-surfaces.md`: "All provider
shapes terminate at `QuotaSnapshot`. Policy never switches on a provider
name.").

Added
`percentage_derived_and_absolute_derived_usage_normalize_to_identical_governor_behavior`
demonstrates this holds end-to-end for the two representations §7.3 names.
It computes the same real quota state (33% of a window consumed) two ways --
`33.0 / 100.0` (as a collector reading a percentage field would) and
`330_000.0 / 1_000_000.0` (as a collector reading raw used/quota token counts
would) -- asserts the two divisions produce the bit-identical `f64` (a
property IEEE 754's correctly-rounded division guarantees for two exactly
equal ratios, not a coincidence of these particular numbers), collects both
through the real `command` transport, and asserts `evaluate` produces
byte-identical `Decision` output for both. Because the wire contract carries
only the already-normalized fraction, this is necessarily also true for any
other pair of raw representations a real collector might use -- the governor
has no code path that could tell them apart.

## Verification commands

```console
$ cargo test --test zai_collector_contract
running 9 tests
test a_reset_generation_transition_through_the_real_command_transport_never_extrapolates_the_prior_generations_rate ... ok
test percentage_derived_and_absolute_derived_usage_normalize_to_identical_governor_behavior ... ok
... (7 pre-existing tests) ...
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Both new tests are `#[cfg(unix)]`, matching every other test in this file
that drives the real `command` transport (it shells out to `/bin/sh`).
