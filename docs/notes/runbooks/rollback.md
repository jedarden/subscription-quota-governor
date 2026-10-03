# Runbook: rolling a governed account back

This runbook implements [WP8 step 8](../../plan/plan.md#wp8-staged-production-rollout): keep a one-command rollback ready before enabling an actuator. The command is [`scripts/rollback.sh`](../../../scripts/rollback.sh); it changes the systemd launch command, restarts `subgov`, and verifies the effective command before reporting success.

| Target | Command | Result |
| --- | --- | --- |
| Observe-only | `sudo scripts/rollback.sh observe-only` | `subgov.service` keeps collecting and recording observations but cannot actuate. |
| Previous system service | `sudo scripts/rollback.sh previous cgov.service` | `subgov.service` is first restarted in observe-only, then the named prior controller is enabled and started. |
| Previous user service | `scripts/rollback.sh --user previous cgov.service subgov.service` | Both units are managed in the current user's systemd manager; the prior controller starts only after subgov is observe-only. |

These defaults match `deploy/systemd/subgov.service`: `/usr/local/bin/subgov`, `/etc/subgov/governor.yaml`, and the system unit `subgov.service`. Set `SUBGOV_BIN` or `SUBGOV_CONFIG` when the deployed paths differ, passing them through `sudo env`, for example `sudo env SUBGOV_CONFIG=/etc/subgov/codex.yaml scripts/rollback.sh observe-only`. Pass the subgov unit as the final argument when it is not `subgov.service`:

```sh
sudo env SUBGOV_CONFIG=/etc/subgov/codex.yaml scripts/rollback.sh observe-only subgov@codex.service
sudo env SUBGOV_CONFIG=/etc/subgov/codex.yaml scripts/rollback.sh previous cgov-codex.service subgov@codex.service
```

For a user service such as the existing cgov deployment, install subgov as a user service too and run both through the same user manager. The script's `--user` mode writes under `${XDG_CONFIG_HOME:-~/.config}/systemd/user`; the user manager must be available, and linger must be enabled if the service has to survive logout or start after reboot. `--user` applies to both subgov and the prior controller, so it cannot hand off between a system unit and a user unit.

The script requires systemd service units and absolute binary/config paths without whitespace or systemd specifier characters. It writes its owned drop-in at `<unit-dir>/<subgov-unit>.d/zzzz-subgov-rollback.conf`, reloads systemd, restarts the service, and checks that the effective `ExecStart` contains `--observe-only`. In previous-controller mode it discovers systemd activation units (such as timers), disables and stops them, then stops the prior controller before changing subgov. After verifying subgov is observe-only, it enables and starts the prior service and restores its activation units. If a start fails, subgov remains observe-only.

## Scope and safety

`--observe-only` applies to the entire `subgov` process. If one process has several accounts configured, rolling it back pauses actuation for all of them. To roll back just one account, run that account in its own systemd unit/configuration; then name that unit in the command above. The prior-controller unit must govern the same account/fleet, and the operator should check [controller ownership](controller-ownership.md) before enabling it.

The sequence prevents an overlap between actuating controllers: activation units and the previous service are stopped, then `subgov` is restarted and verified non-actuating before the previous service is started again. If any step fails, the sequence favors a gap with no controller acting over overlapping controllers. The process continues observing in the previous-controller mode. The rollback does not reset `state.json` or discard quota history.

If the rollback command fails, inspect its error and `systemctl status <unit>`. A failed observe-only restart prevents starting the previous controller; in previous-controller mode the prior unit will be stopped, so inspect both units before resuming control. A failed previous-controller start leaves `subgov` observe-only.

## Returning control to subgov

After the incident, stop and disable the previous controller before removing the observe-only drop-in and restarting `subgov` in its configured mode. For system services:

```sh
sudo systemctl disable --now cgov.service
sudo rm /etc/systemd/system/subgov.service.d/zzzz-subgov-rollback.conf
sudo systemctl daemon-reload
sudo systemctl restart subgov.service
```

For user services, use `systemctl --user` and remove the drop-in from `${XDG_CONFIG_HOME:-~/.config}/systemd/user`. Substitute the actual unit names used in the rollback. Confirm only the intended controller is actuating before resuming normal service.

## Verification

- **Observe-only:** the script exits successfully only after `systemd` reports the service active and its effective `ExecStart` includes `--observe-only`. The next `decision` event should show `"observe_only": true, "actuated": false`; `subgov status` continues to update.
- **Previous controller:** the script starts it only after observe-only has been verified. Confirm that only the prior controller is actuating and that the named prior unit is active.
- **Account isolation:** use an account-scoped subgov unit if other accounts must continue actuating during rollback.
