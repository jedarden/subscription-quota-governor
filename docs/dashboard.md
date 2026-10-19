# Live quota dashboard

`subgov dashboard` (also `subgov tui`) displays cached quota observations and
controller policy in a terminal. It refreshes the display without polling a
provider, launching workers, changing targets, or rewriting governor state.

```bash
subgov dashboard --configs ~/.config/subgov/claude-anthropic.yaml \
  ~/.config/subgov/codex.yaml ~/.config/subgov/claude-zai-managed.yaml
```

Use `--once` for a plain snapshot or `--once --json` for structured output.
`--interval-seconds 5` controls display refresh, independently of the governors'
poll intervals. Press `j`/`k` to scroll in a shorter terminal, or `q` or Ctrl-C
to exit the interactive view.

The account cards show quota used and remaining, reset countdowns, configured
ceilings/reserves, consumption in percentage points per hour, projected remaining
quota at reset, agent counts and controller policy. Rates describe the account's
measured usage, including interactive use. A projection assumes that measured
pace continues; it is not a promise or a causal estimate of each worker's burn.
Each rate identifies its measurement baseline. Flat quantized readings leave the
burn forecast unknown. Missing, stale, reset-crossing, regressing or incomplete
history must not appear as zero consumption or free capacity.

Controller-observed counts belong to their recorded observation time. Live
heartbeat counts, where available, are checked against supervisor process
identities and freshness. Busy heartbeats describe worker activity, rather than
guaranteeing that a task will complete. A requested reduction can coexist with
more live workers while safe retirement waits for a task-free, child-free
boundary. Eligible demand is separate from either count.

The view also exposes Codex banked-credit deadlines and demand holds, plus Z.AI
backpressure and saturation telemetry when those cached fields exist. Their
absence remains unknown. No credential files or provider authentication values
are needed or displayed. Errors are isolated so one unavailable account does
not hide the others.

## Codinghome metrics tab

The maintained launcher is
[`deploy/codinghome/subgov-dashboard.sh`](../deploy/codinghome/subgov-dashboard.sh).
It reads the same three production configurations used by the governors. Install
it as `~/.local/bin/subgov-dashboard`, then run it in a tab named **Subgov** in
the existing **📊 metrics** space. This is a display process, with no new
governor or provider account.

Build the verified committed source with the repository's Cargo wrapper; output
stays in `/build/subscription-quota-governor`. Install the executable into
`~/.local/lib/subgov/dashboard-<commit>/subgov`, record its source commit,
binary hash, lockfile hash and toolchain, and point
`~/.local/lib/subgov/dashboard-current` at that directory. This isolated local
artifact leaves all three running governor executables intact. The launcher
must match its maintained source. The owning bead records the exact tab, pane,
installation, provenance and actual verification.

The dashboard reads existing state, readiness and cached source/helper files.
It does not acquire a governor state lock, quarantine malformed files, invoke
an observer/actuator or fetch an OAuth/Codex/proxy snapshot. Its terminal
settings affect only its own display process.

To roll back, quit only the Subgov dashboard, restore the previous display
symlink and launcher if present, and relaunch that tab. Closing the display
does not stop agents or governors. Preserve every other metrics tab and any
later edits to the production configurations.
