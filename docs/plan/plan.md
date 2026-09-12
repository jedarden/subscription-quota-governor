# Subscription governor implementation plan

## Objective

Replace provider-specific control loops with one account-aware governor for
`claude-anthropic`, `codex`, and `claude-zai`. Preserve each provider's native
authentication boundary while sharing quota normalization, utilization policy,
state, safety behavior, and fleet actuation.

## Delivered in v0.1

1. A normalized quota model with arbitrary named reset windows.
2. Native Anthropic OAuth and Codex app-server collectors, plus provider-neutral
   command, file, and HTTP adapters for Z.AI.
3. File and command sources for extensibility and deterministic fixtures.
4. Account- and window-level utilization/reserve configuration.
5. Linear-to-reset pacing and explicit ceiling-only burst policy.
6. Conservative multi-window arbitration, stale-data behavior, fleet bounds,
   and per-cycle step limits.
7. Static, file, and command fleet observers; none, file, and command
   actuators.
8. Atomic state/target updates and exclusive state ownership.
9. Example configurations for all three subscriptions and unit tests for
   parsing, pacing, staleness, and arbitration.

## Rollout

1. Install `subgov` and validate the production configuration with `check`.
2. Stop existing provider-specific governors before starting this governor;
   there must be one control-plane owner per fleet.
3. Run `--observe-only` through at least two polling intervals and inspect JSON
   decisions for every account.
4. Continue observation across a reset to verify generation boundaries.
5. Enable one actuator at a time with conservative worker and step limits.
6. Compare provider dashboards, normalized snapshots, and worker targets.
7. Retire the old timers only after the new process is continuously healthy.

## Follow-up hardening

- Long-lived Codex app-server sessions can consume update notifications instead
  of spawning a process per poll.
- Durable sample histories and robust regression can reduce percentage
  quantization noise beyond the current two-point conservative estimator.
- Structured metrics and health endpoints can expose source freshness,
  decisions, and actuation results without scraping JSON logs.
- Provider contract fixtures should be refreshed when upstream clients change;
  the normalized contract and controller remain stable.
- Fleet adapters for a specific scheduler belong behind the existing observer
  and actuator interfaces, not in provider collectors.

## Invariants

- A quota sample controls only the account that produced it.
- No stale or failed source may cause scale-up.
- All active windows constrain the target; the smallest target wins.
- A reset generation is never compared with the previous generation.
- Secrets never enter config examples, argv, logs, state, or target files.
- Observe-only mode performs no fleet mutation.
