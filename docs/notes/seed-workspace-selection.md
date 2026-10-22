# Seed-workspace selection (WP26)

Plan §26.5: subgov chooses how many workers, not what they work on. A `needle
run -w <repo>` worker drains that repo and then roams, and a bare launch is
forbidden, so every launch path seeds each new worker with the ready workspace
that has the fewest running workers and leaves placement to NEEDLE's own
roaming afterwards. Nothing pins a worker beyond the seed: no repo-to-worker
mapping exists anywhere in the launch paths.

## Ordering

`seed_order(jobs, running)` in `scripts/subgov_ready_beads.py` (mirrored by
`seed_order` in `src/fleet/needle.rs`) repeatedly takes the ready workspace
with the lowest running-worker count, ties resolving to the lexicographically
smallest resolved path. Each pick consumes one ready entry of that workspace
and raises its occupancy, so a batch of new workers spreads across workspaces
instead of stacking on the first route. The result never outlives the ready
frontier — a caller must not launch without a seed.

## Launch paths

- **Productive pool adapters** (`scripts/codex-productive.py`, shared by
  `anthropic-productive.py`): when a scale-up launches new elastic slots, the
  occupancy map is counted from the live owned supervisors' heartbeat
  workspaces, and each launch takes `--workspace <seed>`. Adopted workers are
  restored onto their reviewed roster workspace instead; adoption is routing
  recovery, not placement.
- **`needle_run` actuator** (`src/fleet/needle.rs`): with
  `seed_demand_state` and `seed_heartbeats` configured as a pair, scale-up
  reads the WP23 demand-state document for the ready frontier and counts
  running workers from fresh NEEDLE heartbeats (any adapter, because §26.5
  occupancy is workers at a workspace, not this pool's claims; the freshness
  window is the same `0 <= age < 60` seconds the adapters use). Each worker
  launches separately with its own `-w <seed>`. Without the pair the
  configured `repo` stays the permanent single seed, unchanged from v1
  behaviour.

## Failure stance

A missing, malformed or `backlog_unavailable` demand state fails closed: an
actuation error this cycle instead of a launch seeded from no evidence. When
the readable frontier is shorter than the requested delta, the actuator
launches only what has a seed and the next cycle reconciles the rest. Both are
safe because the controller already caps `desired` at
`eligible_backlog_capacity` (WP23), so a seedless delta can only come from
demand moving between publish and actuation.

## Configuration

`needle_run` actuators accept the optional pair:

```yaml
actuator:
  type: needle_run
  repo: /home/coding/SEAM
  adapter: claude-code-glm-5.3-flash
  identifier_prefix: sg-zai-
  seed_demand_state: /home/coding/.local/state/subgov/zai-ready-demand.json
  seed_heartbeats: /home/coding/.needle/state/heartbeats
```

The paths are tilde-expanded and must be configured together. The demand state
is published by the pool's WP23 refresh (`zai-source-refresh.py` /
`codex-productive.py refresh`); seeding is only as fresh as that publisher.
Schema: `seed_demand_state`/`seed_heartbeats` in
[schema/governor-config.schema.json](../../schema/governor-config.schema.json).
