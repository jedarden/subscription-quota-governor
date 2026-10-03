# Configuration notes

## Accounts are the isolation boundary

Each `accounts` key identifies one billable subscription. Give separate keys to
Anthropic Claude Code, Codex, and Claude Code routed through Z.AI even when the
same worker launcher manages all three. State, burn-rate samples, reset
generations, policy, and fleet actuation remain within that key.

## Utilization policy

An account must set exactly one of:

- `target_utilization`: fraction intended to be consumed when a window resets;
- `reserve_fraction`: fraction intentionally retained at reset.

Window entries inherit the account target and strategy. An entry can replace the
target, replace it with a reserve, select a strategy, or set `enabled: false`.
Unknown/unlisted provider windows inherit account policy so a newly introduced
limit is conservative by default.

`linear_to_reset` computes:

```text
observed per-worker burn = change in used_fraction / hours / previous workers
required account burn    = (target - used_fraction) / hours until reset
raw workers              = ceil(required account burn / per-worker burn)
```

The controller will not calculate that rate across a reset boundary, from an
interval shorter than `minimum_sample_seconds`, from a negative delta, or from a
zero-worker sample. The most restrictive active window wins. Fleet bounds and
per-cycle step limits are applied last.

When a fresh, below-target account has no prior sample and zero current workers,
`bootstrap_workers` starts a bounded probe. This avoids a zero-worker deadlock
without inventing a burn rate.

Provider percentages may be quantized. A zero delta causes a hold, not a scale
up, because the governor cannot prove that capacity is free.

## Sources

### `anthropic_oauth`

Reads the same credential shape as Claude Code and polls the Anthropic OAuth
usage endpoint (`usage_url`, defaulting to `https://api.anthropic.com/api/oauth/usage`).
It refreshes tokens within five minutes of expiry and atomically updates the
credential file while preserving unknown JSON fields. No token is persisted in
governor state.

**This endpoint's shape and availability are controlled by Anthropic, not by
this project, and may change without notice.** It is not documented as a
stable public API; it is the same internal usage surface Claude Code itself
polls. `subgov` only claims to normalize the response shapes it has observed
(legacy named windows and the generic `limits[]` array -- see
[provider surfaces](../research/provider-surfaces.md)), and both `usage_url`
and `token_url` are configurable so a deployment can point at a changed or
mirrored endpoint without a code change.

A parse failure, an HTTP error, or an unexpected response shape from this
source fails that account's cycle before any fleet decision is made --
`source::collect` returns an error, `run_cycle` (`src/main.rs`) catches it,
counts it, and moves on to the next account. The account is never actuated,
scaled, or assumed-healthy on a failed poll, and other accounts in the same
process are unaffected. See the README's
[safety boundaries](../../README.md#safety-boundaries).

### `codex_app_server`

Starts `codex app-server --listen stdio://`, completes initialization, calls
`account/rateLimits/read`, and exits the child. Every entry in
`rateLimitsByLimitId` is retained. Window ids are `<limit_id>.primary` and
`<limit_id>.secondary`; the older `rateLimits` field is a fallback only.

The same read also captures `rateLimitResetCredits.availableCount` and any
optional detail rows. The count, not the row count, is authoritative. The
governor does not call the reset-consumption method; redemption remains in an
interactive, human-controlled Codex surface.

## Banked-reset policy

`banked_resets.enabled` activates pacing whenever a source reports a positive
credit balance. It does not itself authorize redemption. For weekly windows:

```text
minimum required burn = minimum_pace_multiplier * target / window duration
deadline required burn = remaining generations / time to credit expiry
effective required burn = max(minimum required burn, each known deadline rate)
```

The default multiplier is `2.0`. When a banked reset exists, the effective
weekly target is at least `redeem_at_utilization` (default `1.0`), even if the
ordinary policy preserves a reserve. This makes the next credit ready for a
human to redeem.
Shorter windows continue to constrain the final worker target.

Detailed credit rows may be absent. In that case the governor still enforces
the minimum pace from the authoritative balance, but cannot calculate an
expiry deadline. `deadline_safety_seconds` (default six hours) advances each
known deadline to leave operational margin.

At the threshold, the controller sets
`manual_redemption_recommended: true`, reports the reason
`weekly_window_awaiting_manual_redemption`, and drains toward `min_workers`.
An operator then verifies the account and redeems manually. There is no
`auto_redeem` option, redemption CLI, or reset-consumption call in the
governor. See [ADR-0001](../adr/0001-human-controlled-reset-redemption.md).

### `normalized_http`

Reads the normalized JSON contract directly from an HTTP endpoint. This is
appropriate for a loopback or otherwise access-controlled site-local collector.
Provider credentials remain outside the governor. There is no header, token,
or credential field in this source's configuration at all, by design -- see
[secure HTTP deployment for the Z.AI collector](zai-collector-http-deployment.md)
for how to secure the endpoint itself instead.

### `normalized_file` and `command`

These consume the normalized JSON contract in the README. They are the escape
hatch for provider changes and site-specific collectors. Command stdout is
reserved for the JSON object; diagnostics belong on stderr.

## Fleet integration

The observer measures current workers. Supported types are `static`, `file`,
and `command`. A command prints either a decimal integer or
`{"current_workers": N}`.

The actuator accepts `none`, `target_file`, `command`, and `needle_run`. Target
files are atomic handoffs to an external reconciler. Command argv is executed
directly, with every `{desired_workers}` occurrence replaced. Shell expansion,
pipes, and redirection do not occur.

`needle_run` takes a repository path and adapter, for example:

```yaml
actuator:
  type: needle_run
  repo: /home/coding/project
  adapter: claude-print
```

It counts tmux sessions matching NEEDLE's `needle-<adapter>-*` naming pattern,
launches the difference with `needle run -w <repo> -a <adapter>`, and scales
down through `needle stop --identifier <full-session-name>`. Scale-down skips
attached sessions and fails without stopping workers if there are too few
detached matches. Use a dedicated adapter for each independently governed pool;
the repo selects the launch workspace, while the adapter's session namespace
defines the pool.

Start with `none` or `--observe-only`. Confirm two or more same-generation
samples and decision output before granting process-management authority.

A cross-host observer or actuator reached over SSH (`argv: [ssh, <host>,
...]`) uses this same `command` primitive with no special transport --
see [the SSH remote-command policy](ssh-remote-command-policy.md) for the
injection-safety boundary that shifts once SSH re-parses the remote
command through the remote shell.
