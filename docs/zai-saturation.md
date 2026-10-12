# Z.ai throughput saturation policy

`utilization.zai_saturation: {}` explicitly enables a bounded search for the
smallest worker count that retains useful completed-request throughput. It is
independent of the older `backpressure` brake. Omitting it preserves existing
account behavior and serialization; old snapshots and persisted account state
remain readable. State schema 6 adds the optional experiment record.

The controller assumes no provider concurrency limit and no fixed worker count.
Workers spend time in tools and builds, so request throughput can continue to
improve when there are more workers than active upstream requests. Decisions
measure completed requests per second across the full proxy, rather than using
quota burn as a proxy for concurrency saturation.

## Evidence and safety

Collectors may add this optional normalized field:

```json
"saturation": {
  "completed_requests": 600,
  "window_seconds": 300,
  "attempts": 650,
  "upstream_concurrency_fraction": 0.03
}
```

Counts are deltas across one complete stable-replica interval. Completed
requests must not exceed attempts. The fraction is provider error **1302**
divided by upstream attempts. Generic HTTP 429, request-start counters and quota
consumption cannot substitute for these signals. A collector with trustworthy
completion and attempt deltas but no classified error metric omits the fraction;
the controller reports it as unknown and can cautiously probe throughput alone.
Restart, reset, first samples, missing replicas or unavailable completion data
withhold the whole object. A fresh quota reading does not make an old proxy
observation fresh; collectors must publish both observations together.

Defaults require an interval of at least 300 seconds and 30 attempts. A missing,
short, malformed, repeated or out-of-order sample cannot start an experiment.
A quota-generation change or usage-counter decrease discards the experiment.
A gap longer than `stale_after_seconds` discards attribution and holds for a new
baseline. No new experiment starts without explicit eligible backlog capacity
supporting both current and candidate worker counts.

The startup exception is a stopped fleet with no unowned roster: fresh quota
below every hard ceiling, a future reset and explicit nonzero eligible backlog
permit the existing bootstrap count, bounded by fleet and per-cycle limits.
This obtains the initial throughput evidence. A downward search preserves one
producing worker when there is no unowned roster; quota exhaustion may still
stop it. A post-settle full-duration sample with fewer than 30 attempts can
prove a pending probe failed when sufficient backlog remains. This lets a
throughput collapse restore the previous count instead of waiting indefinitely
for traffic the smaller fleet cannot generate.

Configured worker caps, per-cycle limits and the most restrictive quota window
remain binding. Saturation may lift `learning_burn_rate`, `no_observed_burn` or
bootstrap holds when quota is demonstrably below target. Measured
`paced_to_reset` limits remain caps. Stale data, reached quota and reset-due
windows never permit a saturation increase. Even if a tiny quota burn estimate
asks for the maximum, only one bounded throughput experiment can increase the
fleet. The older backpressure brake runs afterwards and can further reduce it.
Roster workers remain outside actuation.

## Experiments and persisted state

Defaults are `step_workers: 1`, `dwell_seconds: 600`,
`minimum_window_seconds: 300`, `minimum_attempts: 30`,
`plateau_fraction: 0.05`, `concurrency_fraction: 0.10`, and
`reprobe_seconds: 3600`. Durations and counts must be positive; fractions are
finite in `[0,1)`. Unknown policy keys are rejected.

1. Record a baseline at the observed worker count. After the dwell, use a
   complete fresh interval to request one growth step. Known concurrency
   rejection at or above 10% instead selects a shrink step.
2. Retain the pending target while launches or graceful drains complete. Do
   not launch another experiment or credit mixed-worker throughput. Once the
   candidate count is observed, start the settle clock.
3. Compare only a complete interval starting after settlement and at least
   600 seconds after the settle observation. Growth is useful only when its
   throughput exceeds the baseline by more than 5%. Otherwise restore the
   previous smaller count and begin a downward search after recovery/cooldown.
4. A shrink is accepted if it retains at least 95% of the throughput plateau
   recorded at the start of the downward search. Successive shrink steps cannot
   compound the allowed loss. Useful growth or a later search rebases the reference.
   Continue shrinking after dwell. A greater loss restores the previous count.
   Recovery itself must settle; it cannot trigger a chained rollback.
5. Failed experiments enforce a 3600-second cooldown before the next search.
   At a downward floor, wait the same interval before probing demand again.

Persisted baseline, observed count, sample timestamp, pending target, recovery
marker, dwell and cooldown survive restart. Unexpected worker changes discard
attribution. Quota/backlog caps cancel incompatible probes. Decision events add
`saturation` with `reason`, throughput, nullable pressure, target and pending
status. Reasons distinguish `baseline`, `baseline_dwell`, `probe_growth`,
`probe_shrink`, `waiting_for_workers`, `probe_settling`, `probe_dwell`,
`growth_improved_throughput`, `shrink_retained_throughput`,
`restore_previous_count`, `recovery_settled`, `bootstrap_for_evidence`,
`insufficient_signal`, `insufficient_backlog`, `repeated_sample`, `signal_gap`,
`worker_count_changed`, `worker_or_quota_bound`, `quota_or_freshness_bound`, and
`quota_or_backlog_bound`.

This is a local hill-climbing heuristic, not proof of a global optimum. Proxy
throughput includes external clients and workload variability; it cannot fully
attribute changes to NEEDLE or distinguish fewer useful tasks from harder
requests. Require full-replica stable interval collection, eligible-backlog
observations and observe-only traces before production enablement. It does not
waive WP22's existing deferred brake transition-rate gate. The example is
opt-in configuration, not an enablement instruction.
