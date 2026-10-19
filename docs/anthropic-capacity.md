# Anthropic productive capacity on codinghome

The maintained configuration is `deploy/codinghome/claude-anthropic.yaml`, installed
at `~/.config/subgov/claude-anthropic.yaml`. Its native OAuth source and existing
state history remain in use. The owned fleet ceiling is two, matching NEEDLE's
existing Anthropic provider limit. The minimum stays zero; neither a ceiling nor
available quota admits work.

`utilization.windows.<id>.burn_rate_window_seconds` opts that window into a rolling
measured burn estimate. Omission retains the original two-point calculation.
The lookback must be at least `minimum_sample_seconds`. The estimator sums
observed account usage over measured worker-hours, using the
existing bounded aggregate history. Every interval must stay in the current
reset generation, have a positive worker count, contain nondecreasing usage and
span no more than `stale_after_seconds`. Missing history, a flat series or a bad
interval produces a hold. Freshness, reset-due and reached-target checks still
run first. All observed usage is conservatively attributed to the governed fleet;
interactive usage can reduce the target rather than grant extra capacity.

Weekly windows use 24 hours; the five-hour window uses 20 minutes. This prevents
one quantized percentage point over five minutes from being interpreted as a
sustained weekly rate while preserving independently measured short-window
pacing. The 85% ceilings and 15% reserves are unchanged. Unknown windows inherit
the default policy. The existing `weekly_scoped` exclusion remains specific to a
different model; both `seven_day` and `weekly_all` remain enabled.

The Anthropic wrapper reuses the maintained productive adapter implementation in
`scripts/codex-productive.py`. Copy it together with `anthropic-productive.py`,
`subgov_adapter_common.py` and `zai-source-refresh.py` into the isolated
`~/.local/lib/subgov-anthropic/` directory. No Codex settings, source publisher,
binary symlink or worker prefix is used by the Anthropic wrapper. The command
actuator runs through the existing user `systemd-run` mechanism to inspect worker
identities outside the governor's `ProtectProc=invisible` sandbox.

`deploy/codinghome/anthropic-productive.json` records reviewed ordinary outcomes.
Each launch rechecks the exact reviewed revision, native ready frontier, positive
lane label, human/proposal/time-bound holds, backend, host ownership and workspace
gate health. The positive-label NEEDLE lane binds only
`sg-claude-anthropic-1` and `sg-claude-anthropic-2`, with `when_empty: idle`.
New workers disable generation using a per-worker environment override. Busy
claims must match their real supervisor and bead assignee. Scale-down retains
busy workers; retirement requires a fresh task-free heartbeat and uses pidfds,
frozen-thread identity checks and a child-free check before sending SIGTERM.

## Installation and verification

Back up the installed config, user unit, state and local NEEDLE configuration under
`/home/coding/scratch/<owning-bead>/`. Resolve symlinks before editing. The base
`subgov.service` and global NEEDLE configuration are intentionally local files;
this repository owns the Anthropic configuration, wrapper and reviewed manifest.
Preserve the base unit's `KillMode=process` so governor restarts leave workers alive.

Build and verify the committed revision on codinghome with the Cargo wrapper.
Install only its `subgov` executable into a commit-named directory under
`~/.local/lib/subgov/anthropic-<commit>/`, recording the binary SHA-256, source
commit, Cargo.lock SHA-256 and toolchain. Use an Anthropic-only service drop-in
to point `ExecStart` at that binary. Older binaries reject the new optional
configuration field, so install the binary and config together. No existing
provider binary or shared `~/.local/bin/subgov` is replaced.

Copy the two Anthropic config/manifest files with mode 600. Install the positive
lane through the wrapper's `install-lane` command, which preserves unrelated
configuration bytes and refuses unfamiliar layouts. An existing worker must
finish its task before being safely retired and relaunched to load its lane;
never kill a busy worker to apply configuration. Run `subgov check`, `doctor`,
read-only quota/ownership checks, then restart only `subgov.service`. Verify actual fresh owned heartbeats, matching
supervisor PIDs, real task claims and descendant execution; a launch exit code
alone does not establish productive execution.

Record runtime evidence, pending retirees, delivery references and any incomplete
checks on the owning bead. The retired cgov service and stop-watch remain
disabled/inactive. No lab or multi-host ownership changes are included.

## Rollback

Stop only `subgov.service`, preserving worker processes with `KillMode=process`.
Restore the previous Anthropic config and Anthropic-only executable drop-in,
then reload the user manager and restart the governor. Remove the exact owned
lane through `remove-lane` only after unnecessary workers are task-free; never
restore the entire NEEDLE config over another operator's later edits. Preserve
in-flight claims and use the same safe retirement checks when reducing capacity.
Do not re-enable cgov while subgov is actuating.
