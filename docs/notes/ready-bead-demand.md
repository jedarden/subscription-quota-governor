# Ready-bead demand (WP23)

All three provider publishers use `scripts/subgov_ready_beads.py`. They query
`bead list --ready --json --limit 999999` in native bead-rs workspaces. The
native frontier owns dependency readiness; assigned, non-open and manually
blocked rows are also rejected defensively. Human/proposal labels, timed holds,
recorded external holds, NEEDLE internal artifacts, custom excluded labels,
authoritative-backend and queue-ownership refusals and degraded workspace gates
contribute no ready work.
A query failure, malformed output or possible truncation marks only that
workspace unknown (`blocked` reason `ready_query_unavailable`, with the
exception type in `error`): it contributes no seeds and its busy claims stay
counted as retention capacity. Demand goes unknown — withholding capacity,
`fresh: false` and the failure type recorded — only when every selected
workspace fails. Quota collection failures leave the prior snapshot untouched.
The per-query timeout defaults to 8 seconds (measured: p50 0.06s per store,
0.33s worst store across 86 workspaces; only host-wide stalls trip it) and is
settable as `demand.ready_timeout_seconds` (positive integer seconds).

The `demand` object replaces the old `work` allowlist. By default the maintained
settings read the authoritative NEEDLE configuration at
`~/.config/needle/config.yaml`: a nonempty Explore workspace list restricts the
scope, otherwise discovery follows `workspace_root`. Discovery skips hidden,
`target`, `node_modules` and symlink directories. A route without an explicit
authoritative `bead_cli.backend` binding (`bead-rs`, its `bead` alias, or the
deprecated `br` alias — exactly NEEDLE's BeadBackend::Bead spellings) is
refused before its frontier is read (see the dispatch-health gate below). The
adapter never reads a legacy store with the bead-rs CLI or repairs any store.

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
- `ready_timeout_seconds`: per-workspace `bead list --ready` timeout in seconds
  (default 8). The limit cannot be dropped from the query: the CLI's default
  `--limit 100` would silently truncate the frontier, and no cheaper projection
  exists (`--comments none` is already the default).
- `dispatch_health_timeout_seconds`: per-workspace `bead sync status` timeout
  for the dispatch-health gate (default 30), applied only to routes with
  demand (see the gate section above).

`ready_bead_count` counts eligible unclaimed beads before slot limits. `ready`
is the launchable subset after slot limits; it is not a persistent assignment.
`eligible` is the full unclaimed eligible set the count is taken from, one
`{id, workspace}` entry per bead (with `labelled` when a lane label is
configured). `workspaces` records ready counts, busy claims, worker limits and
eligible slots.
`eligible_backlog_capacity` includes validated executing claims as retention
capacity and is bounded by the pool's `max_workers`. An executing claim consumes
its workspace slot and never appears as new ready demand. Existing busy claims
are retained even when workspace eligibility changes. Worker retirement still
uses the existing idle-only safety checks. Pool arbitration/host limits are
WP24 work; WP26 orders `ready` into per-launch seeds — see
[seed-workspace-selection.md](seed-workspace-selection.md).

## Dispatch health gate (subgov-06b69094)

A counted bead must be work NEEDLE can actually claim now. The 2026-11-11/12
fleet measured exactly the failure mode that motivates this gate: the
publisher counted seven launch slots across bead-rs, halfonadouble.com,
investment-research-mcp and spaxel while NEEDLE Explore refused every one of
those routes — missing authoritative backend binding (halfonadouble.com had
no `.needle.yaml`), `secret_quarantined` publication withholding plus
sync-flush exit 4 and incomplete retained-generation coverage (bead-rs,
investment-research-mcp), and `secret_quarantined`/`workspace_sync_busy`
(spaxel). Counting refused routes launches workers that cannot claim, which
is a false scale-up signal and wasted provider quota.

Every route with demand is therefore gated by a versioned health observation
(`dispatch_health_version: 1` in the demand document) that mirrors NEEDLE's
own claim-path admission, verified from local read-only sources:

1. **Backend binding authority** — NEEDLE `open_configured`
   (`NEEDLE/src/bead_store/mod.rs`) bails with "no authoritative bead backend
   binding" unless `.needle.yaml` sets `bead_cli.backend` explicitly;
   `auto` — the resolution of a missing file or absent key — is refused, and
   the retired bead-forge names (`bf`, `bead-forge`) are refused with a
   migration hint (`NEEDLE/src/config/mod.rs`: "use 'bead-rs' or 'bead'
   instead"). The dispatchable spellings are exactly `BeadBackend::Bead`'s:
   `bead-rs`, `bead`, `br`. The publisher classifies these as
   `backend_binding_missing`, `backend_binding_retired` and
   `backend_binding_invalid` before a route is ever queried.
2. **Checkpoint admission** — the publisher reads
   `bead sync status --format json`, the same payload NEEDLE's sync_guard
   (`NEEDLE/src/bead_store/sync_guard.rs`) reads before any claim, and
   applies its match arms verbatim: `aligned` AND publication consistent
   (`checkpoint_consistent`, falling back to `ready_to_commit` on bead-rs
   builds predating the split) is admitted; aligned-but-unverified
   publication is a verified refusal (`checkpoint_publication_unverified` —
   secret quarantine, incomplete retained-generation coverage and
   uncommitted checkpoint files all surface this way; the bounded
   `not_ready_reasons` text rides the `blocked` row as `detail`); and a
   relationship that is not a remedy state — bead-rs's
   `covered-ahead-integrity-failure` — is a verified refusal
   (`checkpoint_not_aligned`), exactly the arm NEEDLE hard-bails on. The
   repairable relationships (`behind`, `absent`, `remote-advanced`) are
   **unknown**, not refusals: NEEDLE repairs those itself (flush-only /
   reconcile) under its own budget at claim time and refuses only when the
   repair fails, which a read-only observation cannot predict. They are
   recorded as `checkpoint_repair_pending` in `blocked` (with the
   relationship as `detail`), contribute no slots, and count toward the
   demand-unknown rule below instead of a verified-empty zero.
3. **Sync-lock contention** — a read-only `flock` probe of
   `.beads/needle-sync.lock` (never created, never written), applied with
   NEEDLE's own discipline: a flushing worker holds the lock only for a
   flush-duration, and sync_guard waits out a held lock for its full sync
   budget before refusing `workspace_sync_busy`. The probe likewise polls
   for up to the per-route health timeout and refuses only a lock that
   never frees — a transiently-held lock is claimable (NEEDLE would wait
   and claim), so an instant refusal there would systematically under-count
   exactly the busiest routes. The probe runs before the status read, so
   the payload is not observed mid-publication.

Scope and cost: only routes with at least one eligible unclaimed bead are
probed — zero-demand routes contribute no slots under any health verdict, so
probing them would only spend cycles. The full 86-workspace fleet probe
measures ~13s at 4-way parallelism when demand is broad; the per-route
health timeout defaults to 30 seconds and bounds both the lock wait and the
status read (NEEDLE's own sync budget is 180s). It is settable as
`demand.dispatch_health_timeout_seconds` (positive integer seconds); a
contended route can therefore cost up to twice its timeout before a
`workspace_sync_busy` refusal.

Unknown is not zero: a probe that times out, exits nonzero or returns a
malformed contract marks that route `dispatch_health_unavailable` in
`blocked` (with the exception type in `error`), and a repairable
relationship marks it `checkpoint_repair_pending`; both contribute no seeds
and keep their busy claims as retention capacity. When every selected route
either fails its ready query or cannot verify claimability, demand is
unknown — the same fail-closed rule as the ready query — so the controller
freezes at the last good demand instead of scaling down over unverifiable
work. Verified refusals are different: they are precise observations, so
they simply remove that route's slots while the underlying store stays
broken. Workspaces summaries carry the verdict per route
(`dispatch_health`: `admitted`, the refusal reason, `checkpoint_repair_pending`,
`dispatch_health_unavailable`, or `not_probed` for zero-demand routes). Busy
claims are retained even when a route's health verdict changes after their
dispatch.

Repairs stay out of scope by design: the publisher never flushes,
reconciles, redacts or reinitializes another repository's store. Recovery
belongs to the owning repository (fix the binding, clear the quarantine,
republish the checkpoint); subgov's job is to stop counting the route until
that happens. No NEEDLE-side interface change is required — the gate
consumes only documented local sources (`bead sync status`, `.needle.yaml`,
the sync lock).

## Lane dispatch and the starvation invariant (subgov-6db97205)

A lane binding in NEEDLE's `strands.pluck.lanes` confines its workers to beads
carrying the lane label (Explore `lane_admits`); unlabelled demand is invisible
to that lane. The dispatch label therefore belongs to the demand publisher,
not to admission paths: a label applied at each admission write-site is
incomplete by construction (the 2026-11-08 triage admitted 80 beads and
labelled none), and removing the positive-label requirement would unbind the
lane from verified demand entirely. Applying it here mechanizes the original
subgov-564a5b9a step — the label goes on exactly the records the publisher
already verified as admitted eligible demand — and keeps the lane bounded by
verified demand (the 10-18 directive).

A pool opts in with `"apply_lane_labels": true` alongside its `lane_label`
(validation rejects the flag without a label). At each refresh, after the
ready frontier is measured, the publisher applies the label to every counted
eligible bead that does not already carry it (`bead label add` is idempotent;
already-labelled beads are never re-written). The invariant is exact:
**every bead the demand document counts is claimable by an idle lane worker.**
A bead whose labelling fails is not counted that cycle — it moves to `blocked`
with reason `lane_labelling_failed`, its slot evidence is reduced
conservatively, and the next successful refresh counts it if the label landed.
Labels are sticky by design: a bead that later gains a hold still cannot
dispatch, because NEEDLE's own admission filters run after `lane_admits`.

The publisher also evaluates the starvation invariant each refresh:
`ready_bead_count > 0` while every fresh owned lane worker reports an
`EXHAUSTED` state — it scanned and found nothing its lane admits — for more
than 2 consecutive refreshes is `lane_starved: true`. The streak counter and
verdict are recorded in the demand document (`lane_starved`,
`lane_starved_refreshes`, `lane_starved_condition`); an unknown-demand cycle
publishes without them, so the next successful refresh restarts the streak at
1, and an evaluation error breaks the streak and is recorded as
`lane_starved_error` without blocking capacity publication. The verdict rides
the quota snapshot (`lane_starved`) into the governor, which surfaces it
verbatim in the account's `status.json` entry. It is an alarm surface only —
actuation never reads it; the labelling above is the fix, this is the detector.

Each successful refresh also publishes `demand_summary` in the normalized quota
snapshot and the account's `status.json` entry. It contains `dispatchable_ready`,
the count of the slot-limited `ready` set; `blocked_by_reason`, a count of every
row in `blocked`; and `gate_degraded_workspaces`, the number of distinct
workspaces blocked by an unhealthy gate. This keeps the daily operator readout
useful even when a large ready frontier has little immediately dispatchable
capacity. If demand becomes unknown, the last successful summary remains in
the demand document's `last_good` observation.

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
