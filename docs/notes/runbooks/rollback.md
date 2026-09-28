# Runbook: rolling a governed account back

Restates [WP8's staged rollout requirements](../../plan/plan.md#wp8-staged-production-rollout)
(§16, "maintain a one-command rollback to observe-only or the previous
controller") as an operator procedure, cross-checked against `src/main.rs`.
There are two distinct rollback targets, and they have different scope and
different mechanics -- pick the one that matches what you actually need to
undo.

| Rollback target | Scope | Requires a restart? |
| --- | --- | --- |
| Observe-only | Whole process (every account it runs) | Yes, but with a flag change only |
| The previous controller | One account at a time | Yes, plus stopping/starting an external process |

## Rollback 1: to observe-only

Use this when `subgov`'s own decisions are suspect (a bad reset-rollover
interaction, an unexpected `desired_workers`, anything you want to stop
*acting on* without losing quota telemetry or burn-rate learning). This is
the same mechanism [the README](../../README.md#install-and-try-it) and the
[systemd service doc](../../deploy/systemd/README.md) already recommend for
*first* enabling an account -- it's the identical lever in reverse.

**The one command:** restart `subgov run` with `--observe-only` added.
There is no live toggle -- config and CLI flags are read once at process
start (`Cli::parse()` / `Config::load` in `src/main.rs::run_cli`, with no
file-watching or reload path) -- so this is "one flag, one restart," not a
signal or API call to a running process.

- **Systemd deployment:** `systemctl edit subgov` (or edit the drop-in) to
  append `--observe-only` to the unit's `ExecStart=` line, then
  `systemctl daemon-reload && systemctl restart subgov`.
- **Foreground/manual:** stop the running `subgov run` process (send it
  `SIGTERM`/`SIGINT` -- `install_shutdown_flag` in `src/main.rs` handles
  this gracefully, finishing the in-flight cycle and persisting state
  before exiting, rather than being killed mid-write) and start it again
  with `--observe-only` appended to the same command line.

**Scope:** `--observe-only` is a whole-process flag (`Commands::Run {
once, observe_only }` in `src/main.rs`) -- it applies to every account that
`subgov` instance runs, not one account selectively. If you need to roll
back only *one* account while others keep actuating normally under the
same process, that's not this lever -- see "Rolling back one account among
several" below.

**What does not change:** observation and learning continue exactly as
before. `run_cycle` in `src/main.rs` still calls `source::collect`,
`evaluate`, and (for a non-stale decision) `AccountState::record` --
`observe_only` only gates the `fleet::actuate` call and the `actuated` flag
in the `decision` event, per §12 ("print decisions and persist observations
without changing targets"). Rolling back to observe-only does not reset or
pause burn-rate history, and does not require deleting or touching
`state.json`.

## Rollback 2: to the previous controller

Use this when `subgov` needs to be taken out of the loop entirely for an
account -- most commonly during the staged rollout WP8 describes, if a
problem surfaces after switching an account over and you need to hand it
back to whatever governed it before.

1. **Stop `subgov`'s actuation for that account first, without stopping
   observation yet**, using Rollback 1 above (`--observe-only`) if you
   want a brief overlap window to compare before fully cutting over, or
   stop the `subgov` process/unit outright if not.
2. **Start the previous controller for that account.** This is the exact
   reverse of WP8 step 3 ("stop the existing governor for one account
   before starting `subgov` in observe-only mode for that same account"),
   run backward.
3. **Avoid an overlap window where both controllers actuate the same
   fleet.** This is precisely the condition the
   [conflicting-controllers runbook](conflicting-controllers.md) covers --
   read it before doing this if you haven't already. `subgov`'s
   `StateLock` provides no protection here at all: it only locks against
   another *`subgov`* instance sharing the same `state_path`, not against
   a differently-shaped previous controller. The safe sequence is stop-then-start,
   not start-then-stop: bring the previous controller's actuation online
   only after `subgov` has genuinely stopped actuating that account (fully
   stopped, or confirmed `--observe-only`), not before.

**What doesn't need cleanup:** `subgov`'s `state.json` is irrelevant to any
other controller -- nothing about it needs to be deleted, reset, or handed
off (see [state backup and removal](../state-backup-and-removal.md) if you
do want to archive it for later reference before decommissioning). If the
actuator was `target_file`, the previous controller almost certainly
doesn't read that same file (it's `subgov`'s own internal handoff format,
not a standard one), so there's nothing to reconcile there either -- the
one exception is if you deliberately built the previous controller to
consume `subgov`'s `target_file` output as an interim measure, in which
case that file's last-written value is exactly what the previous
controller should pick up as read as a starting point, not a state.json.

## Rolling back one account among several

Per WP8, rollout is staged per account -- accounts don't all move to
`subgov` at once, and neither `--observe-only` nor stopping the whole
process is scoped to one account. To roll back a single account while
`subgov` keeps actuating others in the same process:

1. Edit that account's `fleet.actuator` to `none` in the config file (or
   swap the whole account block back to whatever it was before, if rolling
   back to a previous controller for just that account).
2. Restart `subgov` (same config-is-read-once caveat as above -- there is
   no per-account live toggle).

This is a config edit plus a full-process restart, not a single flag --
"one-command rollback" in WP8's sense is about the mechanism being simple
and pre-planned (no code changes, no manual state surgery), not literally
one shell invocation in every case.

## Verification

- **Observe-only rollback:** the next `decision` event shows
  `"observe_only": true, "actuated": false` regardless of what
  `desired_workers` says; `subgov status` continues updating (still
  learning), just with no actuation.
- **Previous-controller rollback:** confirm only the previous controller's
  process is actuating (per the conflicting-controllers runbook's
  detection steps) and that `subgov` for that account is either fully
  stopped or confirmed `observe_only`/`actuator: none` -- never both
  processes actuating simultaneously, even briefly.
- **Single-account rollback:** that account's `decision` events show
  `"actuated": false` (or the new controller's own signal, if handed off
  entirely) while other accounts in the same `subgov` process continue
  actuating normally.
