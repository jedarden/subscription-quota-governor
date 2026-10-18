# Codinghome Codex governor

This deployment runs Codex at verified productive capacity while banked reset
credits remain. `banked_resets.full_speed: true` bypasses burn learning and weekly
pacing, retaining the 12-owned-worker ceiling, two-worker upward step, one-worker
downward step, provider short-window constraints, source freshness, and manual
redemption boundary. Missing demand holds; known no-work drains safely. A busy
worker is retained until it is task-free, fresh, and child-free.

`codex-productive.json` is a reviewed work manifest, not an admission mechanism.
Each ID needs existing operator admission evidence. A native ready query, human
and time-bound holds, backend/host ownership, exact reviewed bead revision, and
NEEDLE's real workspace gate-health state all constrain eligibility. Recorded
external waits contribute no work. Changes since review require reviewing the
owning record and updating the manifest, not discarding its holds. A current
owned claim is counted only after matching the live supervisor and bead assignee.
Unowned workers on the observed adapter remain an accounted-for floor.

NEEDLE's existing positive-label lanes bind the twelve explicit identifiers
`sg-codex-1` through `sg-codex-12` to `subgov-codex-productive`, with
`when_empty: idle`. Both home selection and roaming obey that label. Only already
admitted, reviewed records receive it. Label-free generated proposals are not
capacity. New workers also disable NEEDLE's Weave generation gate through a
per-worker environment override. No tmux-server or other worker environment is
changed.

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
`codex-productive.py`, `subgov_adapter_common.py`, and `zai-source-refresh.py`
from `scripts/` into `~/.local/lib/subgov-codex/`; only the conservative workspace
policy parser is reused from the Z.AI publisher. Verify source/install hashes.
Install `subgov-codex-banked.conf` as
`~/.config/systemd/user/subgov-codex.service.d/zzzz-subgov-banked.conf` and the
source service/timer into `~/.config/systemd/user/`.

```bash
python3 -B scripts/codex-productive.py \
  --settings ~/.config/subgov/codex-productive.json \
  install-lane ~/.config/needle/config.yaml
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

Initial rollout evidence/backups and the exact local build/install references
belong to `subgov-564a5b9a`; earlier pacing-only activation is `subgov-df1e9e75`.
