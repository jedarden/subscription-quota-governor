# Codinghome Codex governor

This deployment runs Codex at verified productive capacity while banked reset
credits remain. `banked_resets.full_speed: true` bypasses burn learning and weekly
pacing, retaining the 12-owned-worker ceiling across all adapters, two-worker upward step, one-worker
downward step, provider short-window constraints, source freshness, and manual
redemption boundary. Missing demand holds; known no-work drains safely. A busy
worker is retained until it is task-free, fresh, and child-free.

`codex-productive.json` declares the owned pool and ready-bead workspace scope.
The shared adapter reads NEEDLE's native ready frontier, retaining human,
proposal, timed and external holds, backend/host ownership and workspace gates.
It caps build-heavy workspaces at one worker and validates current owned claims.
See [ready-bead demand](../../docs/notes/ready-bead-demand.md) for the contract,
all-pool settings, evidence fields and migration from curated positive lanes.
New workers keep NEEDLE generation disabled. The adopted roster retains its
existing identity and safe retirement checks.

The adopted systemd instances are disabled without stopping their live tasks.
Their exact `zzzz-subgov-codex.conf` drop-ins set `Restart=no`, disable generation
for future launches, and reject launches while a retirement marker exists.
The two `reset-*` workers are direct tmux supervisors with no restart loop.
Subgov adopts all current PIDs without restarting them. The six systemd workers already poll for configuration reload every 30 seconds,
so they load the expanded lane without a restart. The two reset workers and
original elastic worker have reload polling disabled; their previous lane settings
remain until safe retirement. Deployment must remove the obsolete owned positive lane before ready-demand dispatch. Busy claims and workers
with children remain pending until a fresh, task-free, child-free boundary is
verified. NEEDLE's `stop` command cancels work and is not a drain mechanism.
Restore prioritizes the existing adopted slots, preserving their models/routing;
additional elastic slots use the primary adapter. All count toward the same cap.

The read-only publisher combines the native Codex quota snapshot with measured
demand in an atomic normalized file. Its timer waits 300 seconds after completion;
source failure does not refresh old quota, and failed demand withholds the capacity
field rather than manufacturing zero. The actuator rechecks readiness before
starting anything. Scale-down uses pidfds and frozen-thread identity/child checks;
it never kills a busy claim or unowned worker. It runs outside the governor's
`ProtectProc=invisible` sandbox using the existing user `systemd-run` mechanism.

## Installation and provenance

These files and `scripts/codex-productive.py` are maintained sources. The base
user unit and global `~/.config/needle/config.yaml` remain intentionally local.
Before changing them, resolve symlinks, back up the active files, and inspect
ownership. `install-lane` preserves every unrelated byte and adds only this
lane; `remove-lane` removes only its exact owned block. Both recognize the existing simple
lane layout and refuse unfamiliar YAML instead of guessing. Every unrelated byte
is retained. Installation and runtime use only the Python standard library.

Build and verify on codinghome in this repository with the Cargo wrapper; all
artifacts stay in `/build/subscription-quota-governor`. Install the tested binary
in a commit-named directory, record its SHA-256, source commit, Cargo.lock hash,
and toolchain, then point `~/.local/lib/subgov/codex-current` at that directory.
This is a local deployment artifact, not an official release or release tag.
Only the Codex unit uses this binary; other provider units keep their binaries.

Copy these files with mode 600 into `~/.config/subgov/`: `codex.yaml`,
`codex-quota-source.yaml`, and `codex-productive.json`. Copy the three scripts
`codex-productive.py`, `subgov_adapter_common.py`, `subgov_ready_beads.py` and
`zai-source-refresh.py` from `scripts/` into `~/.local/lib/subgov-codex/`. Use the shared installer so
the Codex, Z.AI, and Anthropic copies are all taken from one committed `HEAD`
snapshot and checked byte-for-byte:

```bash
scripts/install-adapter-scripts.sh
scripts/install-adapter-scripts.sh --verify
```

The installer has an explicit manifest for each adapter directory and the
verification command fails if any expected deployed file is missing or differs
from `HEAD`. Run the verification command as part of the pre-restart doctor
check after updating the checkout.
Install `subgov-codex-banked.conf` as
`~/.config/systemd/user/subgov-codex.service.d/zzzz-subgov-banked.conf` and the
source service/timer into `~/.config/systemd/user/`. Copy `codex-adopted.env`
with mode 600 into `~/.config/subgov/`; the final owned environment file takes
precedence over earlier fleet/worker environment files when slots restart.

```bash
python3 -B scripts/codex-productive.py \
  --settings ~/.config/subgov/codex-productive.json \
  remove-lane ~/.config/needle/config.yaml
python3 -B scripts/codex-productive.py \
  --settings ~/.config/subgov/codex-productive.json install-adoption
systemctl --user daemon-reload
systemctl --user start subgov-codex-source.service
# Inspect the actual quota, demand, known blockers, and source freshness first.
~/.local/lib/subgov/codex-current/subgov --config ~/.config/subgov/codex.yaml check
subgov preflight --inventory /path/to/current-controller-inventory.json
systemctl --user enable --now subgov-codex-source.timer
systemctl --user restart subgov-codex.service
```

Preserve `KillMode=process` on the base unit so restarting the governor leaves
agents alive. Confirm effective `ExecStart` has no observe-only override. Verify
the actual source and controller cycles, claim identity, pending retirees, and
worker PIDs. Tests alone do not prove productive work was available or launched.
For an existing prefix-only installation, first remove its exact old lane using
the backed-up old settings, then install the expanded lane; the helper refuses
an unreviewed replacement. Install the systemd ownership guards before enabling
the command observer. Keep the previous settings/scripts until rollback is no
longer needed. Adoption evidence belongs to `subgov-4c4a3d24`.
`feasible` still describes the deadline burn estimate and can remain false while
the worker/exogenous split is unknown; it does not veto full-speed admitted work.

Reset-credit redemption remains human-only under
[ADR-0001](../../docs/adr/0001-human-controlled-reset-redemption.md). The governor
drains at the threshold and waits for a real new quota generation. No credit
consumption call or automatic redemption is introduced.

## Rollback

Stop/disable the source timer and stop the publisher. Restore the backed-up
Codex config and Codex drop-in, leaving worker processes alive; reload and
restart only the governor. Remove the lane with `remove-lane` after checking
that no longer-needed workers are idle. Do not replace the entire local NEEDLE
config from a backup over someone else's later edits. Keep the existing gate,
proposal, external-dependency, and other-provider ownership states intact.
For adopted workers, first stop only the governor (retaining workers), verify
each exact owned drop-in/marker, remove those owned files, and restore only the
six units' recorded enablement before reloading systemd. Remove the expanded
lane with its matching settings before reinstalling the previous prefix lane.
Do not restore automatic restart ownership while subgov still actuates the pool.

Initial rollout evidence/backups and the exact local build/install references
belong to `subgov-564a5b9a`; earlier pacing-only activation is `subgov-df1e9e75`.
