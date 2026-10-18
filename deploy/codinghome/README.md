# Codinghome Codex governor

`codex.yaml` maintains the existing local Codex elastic configuration and enables
banked-reset pacing. It retains the 12-worker ceiling, existing NEEDLE adapter,
5% ordinary quota reserve, and two-worker scale-up step. Banked resets raise the
weekly target to 100%, apply a 2x weekly pace floor, and account for each known
expiry with a six-hour safety margin. Credit redemption remains a human action
under [ADR-0001](../../docs/adr/0001-human-controlled-reset-redemption.md).

These files are maintained deployment sources. The existing base user unit and
binary remain local installations; this package does not replace either one.
Install from this repository on codinghome:

```bash
install -m 600 deploy/codinghome/codex.yaml ~/.config/subgov/codex.yaml
install -m 600 deploy/codinghome/subgov-codex-banked.conf \
  ~/.config/systemd/user/subgov-codex.service.d/zzzz-subgov-banked.conf
```

Before restarting, inventory existing controllers, run `subgov preflight`, and
back up conflicting overrides. The previous
`zzzz-subgov-rollback.conf` forced observe-only; the historical runtime
`subgov-7931-actuating.conf` is also superseded by the persistent source above.
Remove only those verified, backed-up overrides. Run `subgov check` and `doctor`
with the installed configuration, then `systemctl --user daemon-reload` and
`systemctl --user restart subgov-codex.service`. Preserve `KillMode=process` on
the existing base unit so governor restarts leave working agents alive.

The native Codex source reports credit balances and expiries but has no
`eligible_backlog_capacity`. The banked-reset controller therefore caps its
proposal at observed capacity. It also holds while the worker/exogenous burn
split is unknown. Enabling this policy is not proof of increased throughput or
of meeting the expiry: inspect `required_burn_per_hour`, the credit deadlines,
`feasible`, and actual completed work. Do not fabricate capacity from quota
headroom, label-free legacy proposals, or a nominal ready count whose notes
still carry an operator hold. Reviewed work and a trustworthy demand integration
are required before expanding this configuration beyond the observed fleet.

For rollback, restore the backed-up local configuration and observe-only drop-in,
reload and restart only the governor. Keep worker processes alive. Backups and
live verification for the initial deployment are recorded on `subgov-df1e9e75`.
