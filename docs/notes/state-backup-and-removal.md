# Backing up and removing governor state

This restates the state requirements from
[the plan's state and concurrency section](../plan/plan.md#10-state-and-concurrency)
(§10) as an operator procedure. There is no `subgov state` subcommand --
backup and removal are filesystem operations against the files described
below, not governor CLI features.

## What's on disk

| File | Purpose | Regenerable? |
|---|---|---|
| `<state_path>` (default `${XDG_STATE_HOME:-~/.local/state}/subscription-governor/state.json`, or `./subscription-governor/state.json` if the platform has no state-dir concept) | Per-account learned state: latest window samples, last desired target, bounded burn-rate history | No -- losing it resets learning |
| `<state_path>.lock` | Advisory exclusive lock (`flock`) that stops a second governor from opening the same state path | Yes -- safe to delete whenever no governor holds it |
| `<state_path>` sibling `status.json` (same directory, `status.json` filename) | Last cycle's readiness snapshot, written after each cycle | Yes -- recreated on the next completed cycle |
| `<state_path>` sibling `<name>.corrupt-<UTC timestamp>-<pid>` | A quarantined copy of a `state.json` that failed to parse (see below) | No -- it's the only surviving copy of whatever was in the corrupt file |

Only `state.json` needs backing up. Per §10, it contains no credentials, raw
provider response bodies, prompts, or account tokens -- only the account name
(as configured), a schema version, the latest sample per window, the last
computed target, and bounded burn-rate history. Restrict its permissions the
same as any operational data, but it carries no secret-handling requirement
beyond that.

On Unix, `subgov` writes both `state.json` and `state.json.lock` with mode
`0600` (`src/state.rs`, `restrict_permissions`), and re-applies that mode to
a pre-existing lock file on every `acquire` even if it was created with
looser permissions before this restriction existed.

## Backing up state.json

`subgov` writes state via a same-directory temporary file, `fsync`, atomic
`rename`, then `fsync`s the parent directory (§10; `State::save` in
`src/state.rs`). Because the rename is atomic, `state.json` is never observed
mid-write -- a backup taken at any moment, including while the governor is
running, reads either the previous complete state or the next complete state,
never a partial one. There is no need to stop the governor or hold the lock
to take a consistent backup.

```console
cp -p ~/.local/state/subscription-governor/state.json \
      ~/.local/state/subscription-governor/state.json.bak-"$(date -u +%Y%m%dT%H%M%SZ)"
```

Preserve the mode bit (`-p`, or `install -m 600` for the destination) so the
backup is not left world- or group-readable. Do not back up `state.json.lock`
-- an `flock` is process-lifetime, not file-content, so a copied lock file
carries no useful state and a stale one left on disk is already harmless (see
Removal, below).

### Restoring from a backup

1. Stop the governor process (or ensure none is running against this state
   path).
2. Copy the backup over the configured state path, preserving mode `0600`.
3. Restart the governor.

`State::load` rejects a file whose `schema_version` is newer than the
binary's own `STATE_SCHEMA_VERSION`, rather than misreading it (`src/state.rs`).
If a backup predates the running binary, it still loads: a missing
`schema_version` field defaults to current, and old schema-version-1 files
are readable by any binary that still implements version 1. Restoring a
backup taken from a *newer* binary onto an *older* one fails closed with a
"newer than the ... this binary supports" error -- if that happens, restore
that account's history by upgrading the binary first, not by editing the
file.

### Recovering from a corrupt (unparseable) state.json

This is a distinct failure mode from a schema-version mismatch above: the
file exists but is not valid JSON, or does not match `State`'s shape at all
(truncated write recovered from a crash before this repo's fsync-then-rename
hardening, hand-editing gone wrong, disk corruption). `State::load` does not
error out on this and does not silently start empty either -- it renames the
offending file aside to `<name>.corrupt-<UTC timestamp>-<pid>` in the same
directory, preserving its bytes for forensics, and proceeds with a fresh
default state (`src/state.rs`). `run`/`run --once` logs this to stderr as a
`state_quarantined` JSON Lines event (`event`, `time`, `state_path`,
`quarantined_path`, `error`) before continuing the cycle -- watch for that
event rather than expecting the process to exit.

Because quarantine already discards the corrupt file's learned history (the
process moves on with an empty state, exactly as if the file were missing),
recovery here means restoring a *backup* over the now-fresh `state.json`,
following the same stop-governor / copy / restart steps above, not
recovering the quarantined file itself -- there is no tooling to repair a
malformed state file in place. Keep the quarantined copy only long enough to
diagnose how it got corrupted; it is not consumed by anything and is safe to
delete once you're done, the same as any other backup-shaped file.

## Removing state.json

Deleting `state.json` is safe at any time; a missing file is not an error.
`State::load` treats `ErrorKind::NotFound` as an empty, default state
(`schema_version` current, no accounts) rather than failing (`src/state.rs`).
The next cycle starts every account's learning from scratch: no prior window
sample, no burn-rate history, no `last_target`.

Two removal cases behave differently:

- **Governor not running.** Delete the file. The next `run` (or `run
  --once`) starts clean, as above.
- **Governor running.** The process already loaded `state.json` into memory
  at startup and holds it there for the life of the run; it only reads the
  file once, at load. Deleting the on-disk file does **not** clear that
  in-memory state -- the next completed cycle's `save()` recreates
  `state.json` from what the process still has in memory, undoing the
  deletion. To actually reset learned state for a running deployment, stop
  the governor first, then delete the file, then restart.

`state.json.lock` does not need separate cleanup. `flock` releases
automatically when the owning process's file descriptor closes (process
exit, including a crash), so a lock file left over from a stopped or crashed
governor does not block the next governor from acquiring it -- `StateLock::acquire`
re-opens and re-locks it successfully. Removing the lock file itself is
never required for recovery, and never safe to do while a governor still
holds it (the running process still has the fd open against the deleted
inode; a *new* governor pointed at the same path would then create and lock
a fresh, unrelated inode of the same name, defeating the single-owner
guarantee §10 requires). If in doubt, confirm no governor process is running
against that state path before touching the lock file at all.

`status.json` can be deleted freely regardless of whether the governor is
running; it is derived output; it is recreated at the end of the next
completed cycle and the `check`/readiness surface treats its absence as "no
cycle has completed yet" rather than an error.

## Multiple accounts, one file

State for every configured account lives in the same `state.json`, keyed by
account name (§10, "keep account state isolated in a map keyed by configured
account name"). There is no per-account backup or removal granularity at the
file level -- removing learned history for one account means editing the
loaded state, not deleting the shared file, and `subgov` has no CLI for that.
If a single account's learning needs to be reset without affecting others,
that requires either a config change (rename the account, which is a fresh
key with no history) or direct edit-and-atomic-replace of the JSON following
the same "stop the governor, then replace" rule as a full restore.
