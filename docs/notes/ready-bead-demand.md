# Ready-bead demand (WP23)

All three provider publishers use `scripts/subgov_ready_beads.py`. They query
`bead list --ready --json --limit 999999` in native bead-rs workspaces. The
native frontier owns dependency readiness; assigned, non-open and manually
blocked rows are also rejected defensively. Human/proposal labels, timed holds,
recorded external holds, NEEDLE internal artifacts, custom excluded labels,
foreign queue ownership and degraded workspace gates contribute no ready work.
A query failure, malformed output or possible truncation makes demand unknown.
The publisher withholds capacity, marks the combined sample incomplete
(`fresh: false`) and records the failure type; controller freshness policy holds
scale-up. Quota collection failures leave the prior snapshot untouched.

The `demand` object replaces the old `work` allowlist. By default the maintained
settings read the authoritative NEEDLE configuration at
`~/.config/needle/config.yaml`: a nonempty Explore workspace list restricts the
scope, otherwise discovery follows `workspace_root`. Discovery skips hidden,
`target`, `node_modules` and symlink directories. An absent local `.needle.yaml`
uses the native store binding; an explicit incompatible backend is never queried.
The adapter never reads a legacy store with the bead-rs CLI or repairs any store.

Optional demand settings:

- `workspaces`: exact workspace scope, replacing NEEDLE discovery for this pool.
- `home_workspaces`: additional authorized seed/home workspaces when Explore is
  disabled or pinned elsewhere.
- `workspace_agents`: canonical workspace path to allowed adapter-name list;
  restrictions exclude a route before querying it. Unlisted workspaces inherit
  the pool's configured scope. NEEDLE's `agent.default` selects a default, and
  does not itself prohibit other CLI-selected agents.
- `external_holds`: objects with `workspace`, `id`, and nonempty `reason`.
  Previously recorded holds are preserved across all pools. Legacy `work`
  entries are read only to retain their external holds during migration.
- `workspace_limits`: additional per-workspace ceilings. Build-heavy repositories
  (Cargo, Go, CMake, Maven, Gradle, Make or Node build manifests) always contribute
  at most one slot. Native `workspace.max_workers` can further reduce capacity.

`ready_bead_count` counts eligible unclaimed beads before slot limits. `ready`
is the launchable subset after slot limits; it is not a persistent assignment.
`workspaces` records ready counts, busy claims, worker limits and eligible slots.
`eligible_backlog_capacity` includes validated executing claims as retention
capacity and is bounded by the pool's `max_workers`. An executing claim consumes
its workspace slot and never appears as new ready demand. Existing busy claims
are retained even when workspace eligibility changes. Worker retirement still
uses the existing idle-only safety checks. Pool arbitration/host limits are
WP24 work; WP26 orders `ready` into per-launch seeds — see
[seed-workspace-selection.md](seed-workspace-selection.md).

Each publisher atomically writes `demand_state`, with `observed_at` and these
fields. Codex and Anthropic use the shared `refresh` implementation, with separate
native quota configs; Z.AI combines its existing collector with the same demand
reader. Ordinary quota pacing now honors a supplied backlog cap as well as
banked-reset and saturation policies. No work evidence can bootstrap a worker
merely because quota is available.

## Install-ready migration

WP23 delivers source and tested artifacts; it does not change live units,
workers, installed configs or NEEDLE policy. Install the committed binary and
all adapter scripts together through `scripts/install-adapter-scripts.sh`, then
run its `--verify` mode. The installer includes `subgov_ready_beads.py` in every
provider directory. Preserve the current shared binary installation mechanism.

The maintained settings are `deploy/codinghome/{codex,anthropic}-productive.json`
and `deploy/codinghome/zai-source-refresh.json`. Copy the Anthropic native quota
input (`anthropic-quota-source.yaml`) and source service/timer alongside its
settings before switching `claude-anthropic.yaml` to the normalized snapshot.
Publish and inspect a fresh sample first. All three demand files are under
`~/.local/state/subgov/`; the Anthropic timer invokes the wrapper's `refresh`
mode. No OAuth credential is copied into settings or demand evidence.

The old positive-label Codex/Anthropic lanes restrict workers to curated records.
Remove only those exact owned lanes with the existing `remove-lane` helper and
the matching prior settings as part of an authorized deployment; preserve other
lanes and all human/proposal/hold labels. Ensure existing workers have loaded the
new selection policy or retire/relaunch them safely before expecting newly
eligible beads to dispatch. Do not install a new curated lane for ready-demand
mode. Verify any workspace-local lane restrictions before expanding pool scope.

For a read-only source observation without quota collection, worker control or
snapshot writes:

```sh
python3 -B scripts/subgov_ready_beads.py --settings deploy/codinghome/codex-productive.json
python3 -B scripts/subgov_ready_beads.py --settings deploy/codinghome/anthropic-productive.json
python3 -B scripts/subgov_ready_beads.py --settings deploy/codinghome/zai-source-refresh.json
```

These report unclaimed demand; runtime publishers additionally validate executing
claims against their supervisor/heartbeat identities. Record live observations
with timestamps because the frontier changes as workers claim or finish work.
