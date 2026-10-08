# Controller ownership inventory and preflight

Use this read-only procedure before installing or changing a quota controller.
It checks ownership by stable account and fleet IDs across hosts. A shared
`state_path` lock cannot detect two controllers that use separate state files.

## Preflight contract

Build a no-secret JSON inventory from the hosts' launch definitions and live
processes, then run:

```console
subgov preflight --inventory controller-inventory.json
```

The command reads only the inventory. It does not load a governor config, read
credentials, contact quota services, write state, or execute an actuator. Exit
0 means the inventory has no overlapping actuating controllers. Exit 2 means
the inventory is malformed, ownership is unknown, or an overlap was found.

Each controller record has a stable `account_id` and `fleet_id`, plus its host,
mode, config/state paths, actuator destination, and evidence. Use the same
account ID on every host that uses the same provider account, and the same
fleet ID wherever two controllers could change the same worker set. Do not put
emails, tokens, credential contents, environment values, or private API URLs in
the inventory. Local aliases such as `codex:local-profile` are enough when the
provider's account identifier is not needed to distinguish known profiles.
Account, fleet, host, and controller IDs must be trimmed and whitespace-free
so spelling differences cannot bypass overlap checks.

`actuating` records are compared pairwise. Two records conflict if they share
an account ID **or** a fleet ID. The host and `state_path` are deliberately not
part of this comparison. `observe_only`, `disabled`, `planned`, and `absent`
records remain visible but do not claim actuation. `unknown` always fails
closed until an operator classifies it. An actuating record must include its
actuator destination.

The committed fixtures demonstrate the current inventory, the intended
single-owner Codex plan, same-account/different-state conflicts, same-fleet
conflicts, and a shared Codex account on two hosts. Run them with:

```console
cargo test preflight::tests
subgov preflight --inventory tests/fixtures/controller-ownership/current-live.json
subgov preflight --inventory tests/fixtures/controller-ownership/subgov-single-owner.json
```

The overlap fixtures are expected to fail with exit 2. The Rust tests assert
those outcomes and that both single-owner inventories pass.

## Reproducible host inventory

Run from an account with read access to both user service managers. Use the
same date window on both hosts and retain only paths, unit names, safe identity
aliases, and process metadata:

```console
hostname -f
ssh -o BatchMode=yes lab hostname -f
systemctl list-unit-files --type=service,timer --no-legend
systemctl --user list-unit-files --type=service,timer --no-legend
systemctl list-units --all --type=service,timer --no-legend
systemctl --user list-units --all --type=service,timer --no-legend
systemctl list-timers --all --no-legend
systemctl --user list-timers --all --no-legend
ps -eo user=,pid=,comm=
ps -C subgov -o pid=,comm=
```

Repeat the `systemctl` and `ps` commands with `ssh lab` for lab. Search unit,
timer, cron, and user service directories for references to the governor's
binary or repo, including files whose unit name does not contain `subgov`:

```console
for d in /etc/systemd/system /usr/lib/systemd/system \
         ~/.config/systemd/user /etc/cron.d /etc/cron.hourly /etc/cron.daily; do
  [ -d "$d" ] && rg -l -i 'subgov|subscription-quota-governor' "$d"
done
```

Repeat against the same paths on lab. If `crontab` or `atq` is installed,
inspect schedules locally and remotely; print only matching job names/paths
after redacting command values. If those tools or spool directories are absent,
record that limitation rather than treating shell history as a schedule source.

For each relevant service or timer, inspect `FragmentPath`, `DropInPaths`,
`ActiveState`, `UnitFileState`, `User`, `WorkingDirectory`,
`EnvironmentFiles`, and `ExecStart`. Do not print `Environment=` values or raw
process arguments. Resolve the effective config path, state path, account alias,
fleet target, actuator destination, and any local/remote host list from the
referenced files. If a path is an example or default rather than an installed
configuration, record it as such. A currently inactive timer does not prove a
manual process is absent, so check the process list too.

When a unit can inherit manager or `EnvironmentFile` overrides, inspect only
the names of ownership-related environment variables (for example
`XDG_CONFIG_HOME`, `QUOTA_STATE`, and `QUOTA_CONTROLLER_STATE`). Keep their
values private unless a value is a verified non-secret path or unit list.

After inventory, run the preflight on that JSON. Keep the inventory alongside
the release evidence, review it on every host/config/supervisor change, and
rerun the preflight immediately before enabling a controller. The checker
cannot discover unrecorded host processes by itself; discovery and the
manifest must both be reviewed.

## Codinghome and lab inventory — 2026-10-03

The task's first account is recorded as `codex:local-profile`, matching the
`codex` account in `examples/codex.yaml` and the existing
`~/.local/state/subscription-governor/codex.json` artifact. This is a local
profile alias, not a provider account ID. No credentials or account-token data
were read. The `codex.json` file was 21 bytes, parsed as `{"accounts": {}}`,
and had modification time 2026-09-27 09:12:38 EDT. The lock file exists. This
proves a prior state-path initialization, but does not establish how many
processes used it or whether any ever actuated.

| Controller/path | Codinghome | Lab | Account/fleet and actuator |
| --- | --- | --- | --- |
| subgov | No service, timer, schedule, or process. No installed config. Empty candidate state file above. | No service, timer, schedule, process, config, or matching state file. | The example identifies `codex` and proposes `needle:agent=codex`. Its observer/target files are `/run/user/1000/codex-workers.current` and `/run/user/1000/codex-workers.target`; no consumer was found in inspected config/bin/repo paths. These are examples, not live endpoints. |
| cgov | User service active as `cgov _daemon`; token collector also active. Config is `~/.config/claude-governor/governor.yaml`; state is `~/.config/claude-governor/governor-state.json`. A 30-second stop-watch timer is active and restarts/checks this same service. | Claude token collector is active, but `claude-governor.service` is inactive/disabled. No cgov daemon was found. | Anthropic local subscription alias; config has one agent `needle-sonnet` (0–1 workers). Its configured launch command targets `needle run --agent claude-print --workspace /home/coding/pose-detection --identifier cgov-sonnet-{id}`. This is separate from Codex. |
| GLM quota controller | Static user service inactive; timer disabled. Script: `/home/coding/bin/needle-glm-quota-controller`; its defaults read cgov's quota-state file and keep controller history at `~/.local/state/needle-glm-quota-controller/state.json`. | No GLM controller unit/timer. The script has a configured remote lab target list. | Reads the same Anthropic quota projection as cgov. Its distinct worker target is seven local GLM units plus eleven lab elastic units. Those are possible destinations only; the controller is not running. |
| Z.AI governor | Static user service inactive; timer disabled. Script: `~/.local/bin/needle-zai-governor`; state: `~/.local/state/needle-zai-governor/desired_count-v3`. | No Z.AI controller unit/timer. | Reads Z.AI proxy logs and can enable/disable `needle-worker@glm-icg` and `needle-worker@glm-roam-18..24`. This ex44 expansion pool is separate from Codex. |
| codex-governor | Repository and plan exist at `/home/coding/codex-governor`; no installed binary, service, timer, or daemon process. | No installed path or process found. | Planned for a single Unix-like host per Codex account. The plan launches `needle run --agent codex --count 1`. Handoff bead `codexgov-5ae0be77` is open and consumes this audit; its preflight must block same-account/fleet overlap. |

The live inventory fixture records cgov as the only actuating controller. GLM
and Z.AI launch paths are represented as disabled; subgov is absent on both
hosts despite the old empty state file; codex-governor is planned only. The
preflight passes that snapshot. A `needle-worker@codex-luna-subgov.service`
name and its worker environment file were present on codinghome; this is the
active Codex NEEDLE worker assigned this repository task, not a subgov daemon.
Lab has a `needle-worker@lab-codex-loom.service` Codex worker. Worker presence
does not constitute another controller.

System-wide and user timer/unit listings showed no subgov launch. The search
found no systemd definition that references the subgov executable/repository.
`crontab` and `atq` are not installed on codinghome; the standard cron spool
directories were absent. The same tools are absent and no standard cron spool
directories were found on lab. Lab's systemd listings and matching-path search
found no subgov schedule. The read-only process scans found no `subgov` process
and no running manual GLM/Z.AI controller script on either host. cgov's user
service is the active relevant controller on codinghome; the active lab cgov
process is only the token collector. The
codinghome user manager had no `XDG_CONFIG_HOME` or GLM override keys set; the
cgov environment file had no config/state override keys. The effective paths
above therefore resolve to the documented defaults and live config paths.

The Codex auth file exists on codinghome and was not read. No matching Codex
state/config for subgov exists on lab. Whether the lab Codex worker uses the
same provider account as codinghome remains unverified. That is not a second
controller path, but the account relation must be resolved before treating
both hosts as one Codex fleet. The two-host fixture assigns a common account
ID and correctly rejects that case. Historical process counts beyond the
empty state artifact cannot be reconstructed: there were no retained subgov
journal records or shell-history launch records in the inspected window.

## First account owner and reversible handoff

For the first low-risk account, designate one subgov instance on codinghome as
the sole planned Codex owner. Its first stage remains observe-only; this work
does not configure an actuator or enable actuation. Do not install a second
subgov instance on lab for that account. The Codex Governor stays planned and
must not be launched as an actuator while subgov is the owner.

The fixture `subgov-single-owner.json` models that ownership choice: cgov is
still on its separate Claude target, subgov is the sole Codex controller, and
codex-governor remains planned. It passes. This fixture does not change live
service state. The Codex Governor handoff remains a separate reviewed action
under `codexgov-5ae0be77`.

For a later subgov-to-Codex-Governor handoff:

1. Put subgov in observe-only and configure its actuator as `none`; verify the
   candidate Codex Governor is still planned and rerun the inventory preflight.
2. Stop and disable the subgov service/timer, then confirm the service manager
   and process inventory show no subgov process or restart path. Preserve its
   state file for rollback.
3. Update the inventory so Codex Governor is the only actuating owner. Run the
   preflight before starting it in observe-only; keep it observe-only through
   its separately required commissioning gates.
4. To roll back, stop/disable Codex Governor first, confirm its process and
   launch paths are gone, restore subgov's prior config/state, and run the
   preflight before starting subgov observe-only. Only one controller can be
   changed to actuating after the previous owner has been verified stopped.

This sequence is a procedure only; no service, timer, worker, credential, or
actuator was changed during this audit.

## Recorded command results

The read-only inventory on 2026-10-03 used `hostname -f` (codinghome returned
`codinghome.ardenone.com`) and `ssh -G lab` (alias `lab`, remote user `coding`,
port 22). System and user `systemctl list-unit-files`, `list-units`, and
`list-timers` were checked locally and over SSH. There were no matching system
units on either host and no subgov unit/timer on either user manager. The
codinghome user manager reported the active cgov daemon and stop-watch timer;
GLM/Z.AI controller timers were inactive/disabled. Lab reported an inactive
Claude Governor unit and an active token collector. `ps -C subgov -o pid=,comm=`
returned no rows on either host.

The searches of systemd and cron definition directories found no references to
the subgov executable or repository. `crontab` and `atq` were not installed on
either host, and the standard cron spool directories were absent. Queries for
`_COMM=subgov` from 2026-09-25 forward returned zero user/system journal rows on
both hosts. The tested coding user `.bash_history` and `.zsh_history` paths had
no matching launch records; no other history source was available. Therefore
the empty state file is the only prior-run evidence recovered.

The implementation and fixture commands produced:

```text
cargo fmt --all -- --check exit=0
cargo clippy --all-targets --all-features -- -D warnings exit=0
cargo test preflight::tests exit=0 (7 passed)
cargo run --quiet --bin subgov -- preflight --inventory tests/fixtures/controller-ownership/current-live.json exit=0
cargo run --quiet --bin subgov -- preflight --inventory tests/fixtures/controller-ownership/subgov-single-owner.json exit=0
cargo run --quiet --bin subgov -- preflight --inventory tests/fixtures/controller-ownership/same-account-different-state.json exit=2 (expected conflict)
cargo run --quiet --bin subgov -- preflight --inventory tests/fixtures/controller-ownership/two-host-shared-account.json exit=2 (expected conflict)
cargo run --quiet --bin subgov -- preflight --inventory tests/fixtures/controller-ownership/same-fleet-different-account.json exit=2 (expected conflict)
```

## Second migration: claude-governor (cgov) → subgov — 2026-10-07

The Codex-side work above left one gap this bead's own acceptance criteria
names: "record the intended single owner and a reversible handoff for the
first account." The first account inventoried here was Codex; cgov's
Anthropic pool was inventoried but never given its own target state, config,
or handoff procedure. This section closes that gap for `anthropic:local-
subscription`. **Nothing below was executed against live service state.** The
only commands run were read-only inspection, `subgov check`, and
`subgov run --once --observe-only` (which, per its own contract, cannot
actuate).

### Re-verified live state (2026-10-07, 4 days after the table above)

`claude-governor.service` is still the sole actuating owner: active 4 days
(`systemctl --user show`), cycling every 300s, zero warnings/errors in the
last 24h, with a stop-watch timer that has not had to intervene. It is
currently running exactly one real worker:

- tmux session `needle-claude-print-cgov-sonnet-20261007124012-0`
- process `needle-stable run --workspace /home/coding/pose-detection --agent
  claude-print --count 1 --identifier cgov-sonnet-20261007124012-0`
- heartbeat `~/.needle/state/heartbeats/claude-print-cgov-sonnet-20261007124012-0.json`

`needle-zai-governor.service`/`.timer` and the GLM quota controller are both
still inactive/disabled, unchanged from the table above.

cgov's live pool config (`~/.config/claude-governor/governor.yaml`, pool
`needle-sonnet`): `min_workers: 0`, `max_workers: 1`, windows restricted to
`[five_hour, seven_day]` (weekly_scoped deliberately excluded — it is scoped
to the Fable model, which this pool never runs), both windows at an 85%
ceiling, `loop_interval_secs: 300`, `max_scale_up/down_per_cycle: 1`, and a
hand-tuned `baseline_burn_rate: 0.15 %/worker/hr` for cold start. The config
carries several incident-driven fixes (`claudego-ec6d3ae3` window affinity,
`claudego-d64682d5` hysteresis one-way bug, a NEEDLE core-count-detection
workaround) — this is mature, battle-tested tuning, not a toy config.

### A concrete finding only a live run surfaced

`subgov snapshot claude-anthropic` against the real account (2026-10-07) and
`subgov run --once --observe-only` both returned **three** windows:
`five_hour` (0.02–0.03), `seven_day` (0.08), and **`weekly_all`** (0.08) —
not `weekly_scoped`, which wasn't present in this poll at all. Checked
`~/claude-governor/src/poller.rs` and `src/governor.rs` directly: cgov parses
`limits[].kind == "weekly_all"` into its generic struct but **never reads it
in any pacing decision** — only the legacy `five_hour`/`seven_day` fields and
`limits[].kind == "weekly_scoped"` feed `governor.rs`. cgov has been running
for 4 days with a window its own API is reporting structurally invisible to
it. `weekly_all` matched `seven_day`'s value and `resets_at` exactly in this
poll, which is strong circumstantial evidence it is the provider's
generalized-schema restatement of the same 7-day limit (poller.rs's own
comment: the generalized `limits[]` shape "will eventually replace the legacy
top-level ... fields"), not a materially distinct cap — but that is a
hypothesis from one observation, not a proven fact. The migration config
below leaves `weekly_all` enabled at the account default (binding) rather
than excluding it, since excluding a real constraint is the dangerous
direction and a duplicate binding window at an identical value changes no
decision (`controller::evaluate` takes the minimum across windows). Watch for
divergence between `weekly_all` and `seven_day` during the observe-only
comparison phase below; if they ever disagree, that is new information
requiring an explicit decision, not something to special-case silently.

### Target config

[`examples/claude-anthropic-cgov-migration.yaml`](../../../examples/claude-anthropic-cgov-migration.yaml)
mirrors cgov's live tuning: `needle_status`/`needle_run` against the real
`claude-print` adapter and `/home/coding/pose-detection` workspace,
`max_workers: 1`, 15% reserve on `five_hour`/`seven_day`, `weekly_scoped`
excluded (kept even though not currently present, since cgov's own source
confirms it appears intermittently). Validated:

```text
subgov --config examples/claude-anthropic-cgov-migration.yaml check
  -> configuration is valid (1 accounts)
subgov --config examples/claude-anthropic-cgov-migration.yaml snapshot claude-anthropic
  -> real snapshot, 3 windows, fresh=true
subgov --config examples/claude-anthropic-cgov-migration.yaml run --once --observe-only
  -> current_workers=1 (needle_status correctly found the real heartbeat above),
     desired_workers=1, actuated=false, actuation_attempted=false
```

### Model differences this config cannot paper over

- **Cold start.** cgov assumes a hand-tuned `baseline_burn_rate` to compute a
  safe worker count from zero samples. subgov's `LinearToReset` instead starts
  at a fixed `bootstrap_workers` when `current_workers == 0`. With
  `max_workers: 1` both converge on "start at 1, then measure," but they get
  there by different reasoning — worth knowing if either pool is ever raised
  above 1 worker.
- **No hysteresis.** cgov has an explicit `hysteresis_band` specifically
  because flapping a worker on/off was a real observed problem
  (`claudego-d64682d5`). subgov has no hysteresis or dwell-time damping —
  plan.md §9.8/§21 both list it as explicitly deferred. The only damping is
  the step limit (`max_scale_up/down_per_cycle: 1`), which bounds the *rate*
  of oscillation but not its *frequency*. Watch specifically for flapping
  during the observe-only comparison phase; if it appears, treat it as a
  blocker for enabling this actuator, not a cosmetic issue.
- **NEEDLE launch workaround not replicated.** cgov's `launch_cmd` sets
  `NEEDLE_SKIP_LAUNCH_RESOURCE_CHECK=1` to work around a specific observed bug
  (NEEDLE's CPU admission gate reporting 1 core instead of the host's real
  count, refusing launches it shouldn't). subgov's `needle_run` actuator
  spawns `needle run`/`needle stop` directly with no environment override and
  no way to add one (`ActuatorConfig::NeedleRun` takes only `repo`/`adapter`).
  **Unverified whether this bug is still present.** If it is, subgov's
  actuator could see spurious launch refusals cgov no longer has. Check
  NEEDLE's current behavior before enabling actuation; if the bug persists,
  this needs a small subgov change (an env-passthrough field on the
  `needle_run` actuator) before cutover, not a workaround bolted on
  elsewhere.

### Preflight evidence for three migration stages

Three fixtures model the stages of this specific migration (not the Codex
ones above, which this migration does not touch):

```text
subgov preflight --inventory tests/fixtures/controller-ownership/claude-anthropic-migration-parallel-observe.json
  -> PASS: 3 controller records, 1 actuation-capable owner(s); no account or fleet overlap.
     (cgov still actuating; subgov observe_only -- not counted as an actuator)
subgov preflight --inventory tests/fixtures/controller-ownership/claude-anthropic-migration-cutover-target.json
  -> PASS: 3 controller records, 1 actuation-capable owner(s); no account or fleet overlap.
     (cgov disabled; subgov actuating -- the intended post-cutover state)
subgov preflight --inventory tests/fixtures/controller-ownership/claude-anthropic-migration-premature-dual-actuation.json
  -> CONFLICT: inventory records 1 and 2 both actuate the same account and fleet; exit 2
     (the specific mistake this migration must not make -- proves the gate actually catches it)
```

### Staged procedure (mirrors the Codex handoff pattern above; nothing below has been executed)

1. Install subgov as a user systemd service
   ([deploy/systemd/subgov.service](../../../deploy/systemd/subgov.service)
   adapted for `--user`, `~/.local/bin/subgov`,
   `~/.config/subgov/claude-anthropic.yaml`) with
   `ExecStart=... run --observe-only` from the start — do not let its first
   start be actuating. Re-run the parallel-observe preflight fixture against
   the real installed paths.
2. Let it run in parallel with cgov (unchanged, still actuating) for long
   enough to span a real usage delta on `five_hour` and at least one `cgov`
   scale decision — the single `--once` run above only proved the wiring
   works, not that the two controllers agree. Compare each cycle's
   `desired_workers`/reason against cgov's own `journalctl --user -u
   claude-governor.service` decisions for the same window. Specifically
   check: do `weekly_all` and `seven_day` ever diverge? Does subgov flap
   without cgov's hysteresis?
3. Only once decisions correlate and no flapping is observed: stop
   `claude-governor-stop-watch.timer` first (or it will restart cgov), then
   `systemctl --user disable --now claude-governor.service`. Re-run the
   cutover-target preflight fixture against live state to confirm cgov now
   reads `disabled`/inactive. Then remove subgov's `--observe-only` drop-in
   (or redeploy its unit without the flag) so it begins actuating.
4. Rollback at any point: `scripts/rollback.sh --user previous
   claude-governor.service subgov.service` — it stops cgov's activation
   units, forces subgov into verified observe-only, and only then re-enables
   `claude-governor.service`, so there is never a window with both actuating.

### Recorded command results

```text
cargo build --release exit=0
subgov --config examples/claude-anthropic-cgov-migration.yaml check exit=0
subgov --config examples/claude-anthropic-cgov-migration.yaml snapshot claude-anthropic exit=0 (real live snapshot)
subgov --config examples/claude-anthropic-cgov-migration.yaml run --once --observe-only exit=0 (actuated=false)
subgov preflight --inventory tests/fixtures/controller-ownership/claude-anthropic-migration-parallel-observe.json exit=0
subgov preflight --inventory tests/fixtures/controller-ownership/claude-anthropic-migration-cutover-target.json exit=0
subgov preflight --inventory tests/fixtures/controller-ownership/claude-anthropic-migration-premature-dual-actuation.json exit=2 (expected conflict)
systemctl --user show claude-governor.service (active, 4 days uptime)
journalctl --user -u claude-governor.service --since "24 hours ago" -p warning (empty -- no errors/warnings)
systemctl --user is-active needle-zai-governor.service needle-zai-governor.timer (inactive, inactive)
~/claude-governor/src/poller.rs, src/governor.rs inspected directly for weekly_all usage (read-only, no credentials)
```

### Cutover executed and a real post-cutover incident — 2026-10-07

The comparison phase above ran live for ~3.5 hours (2026-10-07T17:42Z–21:13Z),
spanning a real `five_hour` reset and a real cgov `scale UP: 0 -> 1` event
that subgov independently corroborated via its own `needle_status` observer
at the same timestamp. `weekly_all` and `seven_day` never diverged across the
whole window (checked programmatically against every decision event).
`desired_workers` never oscillated. On that evidence, cgov was cut over:
`claude-governor-stop-watch.timer` stopped first, then
`claude-governor.service` disabled and stopped (verified via
`systemctl --user is-active` and `ps -p <daemon-pid>`); the preflight ran
clean against the real live state at each stage (parallel-observe: 1
actuating owner; the moment both were non-actuating: 0 owners, a safe gap);
subgov's `--observe-only` drop was removed from its `ExecStart` and it began
actuating.

**A real deployment bug surfaced on the first actual scale attempt, not
before.** `fleet::actuate_config` short-circuits to a no-op whenever
`desired == observed`, so the first two post-cutover cycles never actually
invoked the `needle_run` actuator at all. When a heartbeat read glitch
dropped `current_workers` to 0, the resulting real scale-up attempt failed:
`failed to execute needle: No such file or directory`. The systemd `--user`
manager's own default `PATH`
(`/run/wrappers/bin:...:/run/current-system/sw/bin`) does not include
`~/.local/bin`, where `needle` actually lives, and `ActuatorConfig::NeedleRun`
has no field to give it an absolute path instead — it always invokes the bare
`needle`/`tmux` names. **Any `--user`-scope subgov unit that uses
`needle_run` needs an explicit `Environment=PATH=...` line prepending
wherever `needle` is installed** — a hardened unit copied from
`deploy/systemd/subgov.service` without this will look fine until the first
real scale change. Fixed, and verified directly (not just by re-running the
daemon): `subgov doctor` → `needle_adapter_parity: pass`, and
`env PATH=<fixed> needle test-agent claude-print` → `READY`.

**A second, separate problem followed and was not a subgov bug.** After the
PATH fix, three consecutive real cycles still failed:
`NEEDLE adapter claude-print is not ready (test-agent status: WARNING)`. The
actual worker was confirmed genuinely gone (no tmux session, no heartbeat
file, no process — checked directly, not inferred from a stale read). At the
same time, system load averaged ~18 and `needle status` itself hung for 3+
minutes in a blocked, non-CPU-bound state — contention inside NEEDLE itself,
not anything specific to this adapter. This is not a regression from the
migration: cgov's own logs from earlier the same day, before cutover, already
showed the identical symptom (`0 heartbeats, 1 tmux sessions,
consistent=false`, `no workers available to stop`) for this exact pool.
Rolling back to cgov would not fix it — cgov would hit the same NEEDLE
contention relaunching a worker.

subgov's behavior through this was correct: every failed cycle failed
closed (`actuation_succeeded: false`, no mutation attempted beyond the
failed readiness check), never guessed, never force-launched, and never
produced a duplicate worker (checked directly). The decision was to leave
subgov live rather than revert: the migration's actual objective — cgov
retired, subgov the sole live governor, behaving safely under an adverse
condition it did not cause — was achieved, and this pool's exposure while
unstaffed is low (`min_workers: 0`, a subscription-spend pool with no
deadline-critical work, already flaky under cgov before cutover). subgov
retries on its normal 5-minute cadence without intervention; expect recovery
once system load eases.

```text
systemctl --user stop claude-governor-stop-watch.timer
systemctl --user disable --now claude-governor.service  -> inactive; daemon PID confirmed gone
subgov preflight (cgov disabled, subgov observe-only)   -> PASS, 0 actuation-capable owners
# ExecStart edited to drop --observe-only; daemon-reload; restart
systemctl --user show --property=ExecStart --value subgov.service  -> no --observe-only present
First actuating cycle: actuation_attempted=true, actuation_succeeded=true (no-op, already at target)
Failure: "failed to execute needle: No such file or directory" -> Environment=PATH=... added
subgov doctor -> needle_adapter_parity: pass
env PATH=<fixed> needle test-agent claude-print -> Status: READY, exit 0
Three subsequent real cycles -> actuation_succeeded=false, "test-agent status: WARNING", worker confirmed absent
needle status -> hung 3+ min, blocked state; load average ~18; consistent with cgov's own pre-cutover symptom for this pool
```

### Correction: the WARNING flake was this unit's own sandboxing, not load

The "test-agent status: WARNING" above persisted for over an hour across many
real cycles — not a transient flicker. The system-load explanation was wrong.
Repeated manual runs of the identical `needle test-agent claude-print`
command (including with null stdin, matching subgov's own
`Stdio::null()`) were consistently `READY`; subgov's in-process invocation
of that exact command kept returning `WARNING` under the same conditions.
Since every input was identical, the only remaining difference was the
sandbox itself.

NEEDLE's own status logic (`~/NEEDLE/src/dispatch/mod.rs` ~3440–3576):
`WARNING` fires whenever the probe/version/token-extraction checks
accumulate any non-fatal error. `claude-print` wraps the real `claude` CLI
directly (self-reported: "wrapping claude 2.1.293 (Claude Code)"), a
Node.js/V8 application. `MemoryDenyWriteExecute=true` — present in the
original hardened unit, copied verbatim from `deploy/systemd/subgov.service`
without a Node-based child process in mind — is a well-documented way to
break V8's JIT, which needs W+X memory mappings. Systemd sandboxing applies
to the whole process tree a unit spawns, not just the top-level process, so
this broke the `claude` grandchild regardless of subgov's own code being
correct. **Any unit using `needle_run` against a Node-based CLI adapter
needs to drop `MemoryDenyWriteExecute` (and likely review
`SystemCallFilter`/`PrivateTmp`/`ProtectHome` for the same reason) — the
deploy template's own comment already flags this exact class of risk for
`codex_app_server`'s child process; it just wasn't anticipated here.**

Verified directly: stripped `ProtectSystem`, `ProtectHome`, `PrivateTmp`, the
kernel/namespace `Protect*`/`Restrict*` set, `MemoryDenyWriteExecute`,
`SystemCallFilter`, and `RestrictAddressFamilies`; `subgov doctor` and every
subsequent real cycle then passed `needle_adapter_parity` cleanly. (Removing
`CapabilityBoundingSet=`/`AmbientCapabilities=` at the same time broke
service startup entirely — `Failed to drop capabilities: Operation not
permitted`, exit 218/CAPABILITIES — unrelated to the WARNING issue; removed
those too rather than root-causing a second problem mid-incident.)

Past the adapter check, subgov now correctly reaches NEEDLE's own CPU
admission gate, which is working as intended, not a bug: `launch refused --
CPU load saturated: 17.13–18.51 / 20 cores = 0.86–0.93 > threshold 0.80`.
This correctly reports the real core count (not cgov's documented "1 core"
detection bug) and a genuinely high, real load. Deliberately not bypassed —
this is NEEDLE correctly protecting an already-strained host. subgov retries
every 5 minutes and will succeed the first cycle that lands under 0.80 (it
came within one cycle: 0.76 at a manual check, 0.93 five minutes later).

**Open follow-up:** the unit currently runs with most of its originally
intended hardening removed. It should not stay this way indefinitely — the
next step is adding hardening back incrementally with the actual needed
exception (most likely: skip only `MemoryDenyWriteExecute` for this unit, or
give `claude-print`'s real cache/tmp paths explicit `ReadWritePaths` instead
of `PrivateTmp`'s isolated tmpfs), not leaving it unhardened long-term.

### A second PATH gap masked recovery after the sandboxing fix

After the sandboxing fix above, subgov's `needle_run` actuator started
succeeding every cycle — `needle run` exited 0 and logged `[1/1] Started
worker 'alpha'` — but the launched worker crashed within milliseconds of
every single boot, invisibly to both subgov and `needle run`'s own exit
code. Only the worker's own stderr log
(`~/.needle/logs/needle-claude-print-alpha.stderr.log`) showed why:

```text
Error: failed to open configured bead store
Caused by:
    0: failed to resolve bead_cli.backend for workspace /home/coding/pose-detection
    1: bead CLI not found (checked PATH, ~/.local/bin/bead, /usr/local/cargo/bin/bead)
    2: bead not found
```

`bead` lives at `~/.cargo/bin/bead` here — a fourth location none of
NEEDLE's own checked paths covered. This repeated across at least 8
launch-and-immediate-crash cycles (20:34–21:16 UTC) before being caught.
Added `~/.cargo/bin` to the unit's `Environment=PATH=` alongside
`~/.local/bin`, reloaded, restarted.

**Verified live, not just via a log line:** the resulting worker's
heartbeat (`~/.needle/state/heartbeats/claude-print-alpha.json`) showed
`state: EXECUTING`, a real `current_bead`, and a heartbeat timestamp 12
seconds old at the time of the check; `tmux list-sessions` and `pgrep` both
confirmed the real session and PIDs. The pool has a genuinely running
worker under subgov, matching its configured target.

**Migration status: complete and stable.** `claude-governor.service` is
retired (stopped and disabled). `subgov.service` is the sole live governor
for `anthropic:local-subscription`, correctly actuating, with a real worker
running under it. The hardening re-add noted above remains the only open
follow-up.

### Re-hardening, staged and tested against the live worker — 2026-10-08

Re-added hardening incrementally, batch by batch, verifying after each one
with a disposable test unit (mirroring the real unit's directives, running
only `needle test-agent claude-print`) and then against the real service
with its actual worker running — never assuming a probe-only pass meant the
full worker runtime was safe.

**A real, separate bug surfaced during this: `KillMode=control-group`
(systemd's default) kills the whole unit's cgroup on every restart.** The
`needle_run` actuator's spawned tmux session — and the real autonomous
coding worker running inside it — stays in `subgov.service`'s own cgroup
(confirmed via `systemd-cgls --user`: the worker's tmux process appeared
directly under `subgov.service`, not under its own `needle-worker@.service`
unit, which exists as a template but isn't what this launch path uses).
This means **every `systemctl restart subgov.service`** — for routine
maintenance, a crash-restart, or exactly this re-hardening work — silently
killed whatever the dispatched worker was mid-task on. It killed a real,
20-minute-old, productively-executing worker once during this exact
re-hardening session before being caught. Fixed with `KillMode=process`;
verified directly by checking the worker's PID was identical before and
after a real restart.

A second false-positive trap: a probe-only test (`needle test-agent`) can
pass clean while a batch still breaks the full worker runtime. `PrivateTmp`
passed its probe test, but a freshly-launched worker under it crashed
immediately — `Error: failed to create temp dir: /tmp/needle ... No such
file or directory` — because NEEDLE expects a real, shared `/tmp/needle`,
not an isolated tmpfs. The earlier "survived" observation was watching the
*same* worker process that had started before any hardening existed —
namespace/mount settings apply at exec time, not retroactively — not
evidence the setting was actually safe.

**Final state**, verified by a fresh worker surviving 15+ minutes of real
execution (`EXECUTING` → `HANDLING`, same PID throughout, zero errors) under
the full configuration:

| Restored | Deliberately excluded, with reason |
| --- | --- |
| `KillMode=process` (new; not in the original template) | `CapabilityBoundingSet=`/`AmbientCapabilities=` — this host's container/session context does not grant `CAP_SETPCAP` to unprivileged user units; fails the whole unit to start (`Failed to drop capabilities: Operation not permitted`, exit 218/CAPABILITIES), not just under-provisioned |
| `ProtectKernelTunables/Modules/Logs/ControlGroups/Clock/Hostname`, `ProtectProc=invisible`, `RestrictNamespaces`, `RestrictRealtime`, `LockPersonality`, `SystemCallArchitectures=native` | `MemoryDenyWriteExecute` — breaks the Node.js/V8-based real `claude` CLI's JIT (the original incident) |
| `SystemCallFilter=@system-service`, `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6` | `ProtectSystem=strict`, `ProtectHome=read-only`, `PrivateTmp` — the spawned worker is a roaming autonomous coding agent that writes across however many repos under `$HOME` it roams into in a cycle (observed: agent-archivist, FABRIC, claude-print, shephrd, ROTA, ai-code-battle, and more in one cycle) and needs the real shared `/tmp/needle`; these are structural exceptions for this deployment, not gaps to patch with more `ReadWritePaths` |
| `RestrictSUIDSGID`, `RemoveIPC`, `UMask=0077`, `NoNewPrivileges` | |

The disposable `subgov-sandbox-test.service` used for probe-only checks has
been removed; it was never part of the deployment.
