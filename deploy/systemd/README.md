# Running `subgov` under systemd

`subgov.service` in this directory is a hardened example unit for `subgov run`
(continuous mode). It is the deployment counterpart to the CLI described in
the [README](../../README.md) and satisfies §16 WP6 of
[the implementation plan](../../docs/plan/plan.md).

## Setup

1. Build or install the binary at `/usr/local/bin/subgov` (`cargo install
   --path .` from the repo root, then copy `~/.cargo/bin/subgov`).
2. Write `/etc/subgov/governor.yaml`, starting from one of the
   [examples](../../examples/). Run `subgov --config /etc/subgov/governor.yaml
   check` before installing the unit.
3. Copy `subgov.service` to `/etc/systemd/system/subgov.service` and replace:
   - `REPLACE_WITH_OPERATOR_USER` with the unprivileged account that already
     has a working Claude Code login (`~/.claude/.credentials.json`) or Codex
     CLI session. `subgov` does not manage its own credentials or spawn its
     own login flow, so it must run as an identity that already has one.
   - The `ReadWritePaths=` credential path, to match that user's home
     directory and the `credentials_path` set in `governor.yaml`.
   - `state_path` in `governor.yaml` should point inside
     `/var/lib/subgov` (systemd creates this directory via
     `StateDirectory=subgov` at 0700, owned by the unit's `User=`).
4. `systemctl daemon-reload && systemctl enable --now subgov.service`.
5. Watch the first cycle: `journalctl -u subgov -f`. Start with `--observe-only`
   in the `ExecStart=` line (or a `none` actuator in `governor.yaml`) until you
   have confirmed at least two same-generation samples, per the
   [README's safety guidance](../../README.md#install-and-try-it).

## Why these specific protections

- **Restart limits** (`StartLimitIntervalSec`/`StartLimitBurst`) stop systemd
  from endlessly relaunching a governor that is crash-looping against a
  broken credential or a misconfigured fleet actuator, rather than letting a
  crash loop turn into a rapid-fire actuation loop. `systemctl reset-failed
  subgov` clears the counter once the underlying issue is fixed.
- **`ProtectSystem=strict` + `ProtectHome=read-only` + targeted
  `ReadWritePaths=`** give subgov write access to exactly two things: its own
  state directory and the credential file it refreshes atomically. Everything
  else on disk is read-only or invisible to the process, which matches the
  plan's requirement (§7.1) that a credential refresh never has more reach
  than it needs.
- **`NoNewPrivileges`, the capability/namespace/kernel restrictions, and
  `RestrictAddressFamilies`** follow the standard `systemd-analyze security`
  hardening checklist for a long-running network client that has no need for
  privilege escalation, raw sockets, or non-INET address families.
- **`SystemCallFilter=@system-service`** is the one item here with a real
  caveat: seccomp filters are inherited by child processes, and the
  `codex_app_server` source spawns `codex app-server` as a child. If that
  source misbehaves only under this unit and not when you run the same
  command by hand, comment out `SystemCallFilter=` first before chasing a
  quota or network theory.

## Known limitation: shutdown

`subgov` does not yet install its own `SIGTERM` handler (`§16 WP5,
"signal-aware shutdown"`, still open in the plan). `Restart=on-failure` with
`TimeoutStopSec=30s` gives systemd's default TERM-then-KILL sequence, which
stops the process at its next blocking call rather than at a clean cycle
boundary. State is written after every completed cycle
(`state.save()` in `src/main.rs`), so a mid-cycle stop loses at most the
in-flight cycle's observation, not prior history.
