# gmktec federation member

This directory is the maintained source for gmktec's Codex observation stage.
The installed governor owns only `sg-codex-gmktec-N` workers. The configured
seed repository is `ibkr-service`, whose NEEDLE policy names gmktec as owner.
Interactive Codex and Claude sessions have no governed prefix and remain
outside `governed_workers`.

## Install and verify

Build `subgov` from a committed revision on codinghome. Install the one binary
as `/home/coding/.local/bin/subgov` on gmktec and compare its SHA-256 with the
source artifact. A codinghome Nix build may need its referenced runtime store
paths copied to gmktec with `nix copy --to ssh-ng://gmktec` before it executes;
root those runtime paths with `nix-store --realise --add-root ... --indirect`
under `~/.needle/subgov/` so garbage collection cannot remove them. Verify
the binary on gmktec before enabling the unit.

Install `codex.yaml` as `~/.config/subgov/codex.yaml` (mode 0600),
`needle-config.yaml` as `~/.config/needle/config.yaml`, and the Codex adapter
under `~/.config/needle/adapters/`. Install `resource-probe`,
`refresh-codex-demand.sh`, `measure-host-capacity.py`,
`host-capacity.json` (mode 0600), and `codex-demand-settings.json` under
`~/.needle/subgov/`; copy `scripts/subgov_ready_beads.py`,
`scripts/subgov_adapter_common.py`, and `scripts/subgov_host_capacity.py`
into `~/.needle/subgov/adapters/`. Create `~/.local/state/subgov/` for the
WP24 measured-ceiling state the publisher writes. The
settings and published `codex-ready-demand.json` must be separate files: the
publisher atomically replaces the latter on every run. Install the three
systemd files under `~/.config/systemd/user/`, reload the user manager, and
enable the demand timer and `subgov-codex.service`.

Check `needle test-agent codex-gpt-6-sol-xhigh-gmktec`, run
`subgov --config ~/.config/subgov/codex.yaml check`, and verify that a
`run --once --observe-only` cycle reports no observation or actuation failure.
`resource_reserve.mem_reserve_mb` is explicitly 16384; the local resource probe
must report a fresh gmktec snapshot. The service keeps `--observe-only` while
peer transport or repository admission is unresolved.

## Actuation gate

Before removing `--observe-only`, verify both SSH peer pulls, matching provider
account identity, fresh `subgov peers` on each host, distinct worker prefixes,
and no origin overlap for the seed repository. Require a ready, admitted bead
in that repository and a fresh positive demand file. A shared-origin repository
requires an explicit owner and exclusion from every other host's NEEDLE route.
Verify that the running binary applies the shared account total and that each
peer record publishes a capacity bound appropriate to its host. gmktec
publishes its own measured ceiling: the demand publisher's
`measure-host-capacity.py` step writes the WP24 evidence that
`federation.host_capacity_state` points at, so `subgov peer-state --json`
carries `host_ceiling`. The codinghome peer's record needs the equivalent
`host_capacity_state` on its three configs before both ends publish bounds.
Keep the combined step-up bound within the account worker reserve. Record the
first successful governed claim and its host on the owning bead.

The codinghome peer pull is blocked by a deliberate tailnet ACL property:
`policy.hujson`'s `tag:gmktec` TCP test denies `tag:codinghome:22` and no
accept rule grants that flow, so gmktec's peer fetch times out and reports
`stale (transport_failed)`. Enabling that flow is an operator decision that
must edit both the accept list and that deny test; until then the rollout
stays at this stage with codinghome at `peers: []`.

Expand the workspace list and other account pools only after the first pool
has stable placement. The seven-day acceptance window, one real quota reset,
and an induced peer partition are tracked on `subgov-55f75c16`; retain per-host
worker counts, targets, resource snapshots, and peer freshness through that
window. Restore observation by adding `--observe-only` to the unit and
restarting only the governor; `KillMode=process` leaves workers running.
