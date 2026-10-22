# WP24 measured host worker admission

All three codinghome command adapters share `scripts/subgov_host_capacity.py`.
The Rust controller's desired count remains the quota-side request. The adapter
limits that request before any launch or retirement. Each observer cycle and
actuation emits a credential-free `host_worker_ceiling` JSON event on stderr
(inherited by the governor journal), including the live ceiling, limiter values,
p95 RSS, pool counts, ready demand, requested/allocated counts and granted target.
The same evidence and bounded history are saved atomically in the host state.

## Measurement and arbitration

The sampler identifies local `needle`/`needle-stable run` supervisors from `/proc`,
then sums each supervisor's descendants' RSS exactly once. Nested supervisors do
not double count. Interactive Herdr/Claude/Codex sessions are outside this worker
inventory, but their resource consumption still reduces host headroom. NEEDLE
workers outside the configured pool identifiers/prefixes reserve unmanaged host
slots; this module never adopts or stops them.

The nearest-rank p95 of up to 256 observed worker-tree RSS samples retained for
24 hours supplies the per-worker cost, including recent busy samples after workers
become idle. A cold host uses a 6 GiB working-memory fallback, following shephrd's
observed-admission design (`d58b9a4`; `5e394cb` checkpoints that work). There is no
fixed host worker count. Each memory limiter is:

```
current host workers + floor(signed headroom / p95 worker RSS)
```

Headroom is `MemAvailable - 8 GiB` and `0.85 * needle.slice MemoryHigh -
MemoryCurrent`. Negative headroom reduces the ceiling below the current count.
An explicitly unlimited MemoryHigh leaves MemAvailable as the memory limiter.
Missing/unreadable resource or process evidence blocks new admissions and holds
current workers, while still permitting a smaller quota request.

Memory or IO PSI `some avg10 >= 10`, or one-minute load at least 85% of the
sampler's CPU affinity count, immediately prevents scale-up in **each** pool.
After 60 seconds of continuing pressure the pressure ceiling becomes current
host workers minus one. Every pressured cycle recomputes against actual live
workers, permitting another idle retirement once the preceding worker exits.
Pressure persistence survives adapter restarts, resets on healthy/failed samples,
and requires valid samples no more than 900 seconds apart. These resource safety
margins follow shephrd's admission approach; they are not pool quota limits.

Fresh WP23 `ready_bead_count` weights the capped proportional allocation of host
slots among quota requests, with deterministic largest remainders and redistribution
when a quota cap binds. `ready` list length is not the weight: it is already bounded
by workspace launch slots. Unknown or older-than-900-second demand holds existing
capacity subject to the measured resource ceiling and never grants a launch.
Quota requests expire after 900 seconds; an absent pool retains its observed count.

A shared nonblocking host lock spans measurement, durable reservation, and the
adapter's launch/retirement work. Contention makes the observer/actuator retry on
its next normal cycle. Asynchronous launches reserve their target count before
launch. Lowering a target cannot release that in-flight reservation, and a pool
with pending launches cannot issue duplicate launches. Reservations settle when
the full target is observed, or expire without renewal after 900 seconds (well
beyond the adapter's bounded launch command). Failed launches may therefore delay
admission for that window. Busy workers and unmanaged workers always consume
slots until their PIDs actually disappear; a new pool cannot spend capacity merely
because another pool received a lower target.

Codex/Anthropic keep their existing busy-first, child-aware retention and frozen
pidfd idle-stop checks. Z.AI now orders busy workers before idle roster entries;
its existing identity, heartbeat and frozen-thread stop checks remain binding.
Pressure does not authorize interrupting a task, changing masks or claiming an
unowned worker. Pool `max_workers` continues to be the quota-side cap.

## Install-ready configuration

This change does not install anything or modify running workers/services.
For a coordinated future installation:

1. Copy `deploy/codinghome/host-capacity.json` to
   `~/.config/subgov/host-capacity.json` with mode 600. Its three pool entries
   reference WP23's existing demand-state paths. Preserve actual adopted worker
   identifiers when reconciling this configuration with a site roster. Prefixes
   must be disjoint. Use one shared state path for all pools.
2. Use the existing `scripts/install-adapter-scripts.sh` installer and its
   `--verify` mode. Its explicit manifest includes the host module in every
   adapter directory.
3. Install the maintained Codex/Anthropic productive settings and
   `claude-anthropic.yaml`. Both productive settings explicitly declare
   `host_capacity` and `host_pool`; Anthropic now uses a command observer so it
   samples and logs every cycle even when the target is unchanged.
4. Set `reconcile_unchanged: true` on the Z.AI command actuator. The Z.AI fleet CLI uses `~/.config/subgov/host-capacity.json` by default for
   both `observe` and `actuate`. An alternate location must be passed as
   `--host-capacity /absolute/path` before the subcommand in both argv lists.
   Existing roster and idle-retirement ownership checks still apply.
5. Run observer/observe-only cycles and inspect journal `host_worker_ceiling`
   events before enabling actuation through the separately authorized deployment.

Legacy productive settings without `host_capacity` retain their behavior for
other installations. All maintained codinghome productive settings enable it.
No live systemd operation is part of the WP24 delivery or verification.

## Verification

`tests/python/test_host_capacity.py` exercises both memory limits, each pressure
brake and persistence, p95/tree accounting, weighted allocation and cap
redistribution, unknown evidence, reservations and locking, and productive-adapter
admission. Z.AI contracts additionally check idle-first ordering and blocked
starts. These contracts run under the normal Rust integration test
`tests/zai_adapter_scripts.rs`. The adapter installer is verified against a
committed source export so it installs the same bytes as the tested bundle.

The command actuator option `reconcile_unchanged: true` is required for each
host-aware pool. It is enabled in maintained Codex and Anthropic governor YAML;
set it on the Z.AI command actuator when applying the staged configuration.
Without it the Rust equal-target fast path would skip host-driven retirements.
The option also re-observes actual worker counts after command success. Burn
history uses that actual count (including delayed starts and busy retirees), and
the decision event exposes `actual_workers` separately from quota desired. A
failed post-actuation observation retains prior history rather than recording a
requested target as a successful worker change. Observe-only never actuates.
