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
