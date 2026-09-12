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
usage endpoint. It refreshes tokens within five minutes of expiry and atomically
updates the credential file while preserving unknown JSON fields. No token is
persisted in governor state.

### `codex_app_server`

Starts `codex app-server --listen stdio://`, completes initialization, calls
`account/rateLimits/read`, and exits the child. Every entry in
`rateLimitsByLimitId` is retained. Window ids are `<limit_id>.primary` and
`<limit_id>.secondary`; the older `rateLimits` field is a fallback only.

### `normalized_http`

Reads the normalized JSON contract directly from an HTTP endpoint. This is
appropriate for a loopback or otherwise access-controlled site-local collector.
Provider credentials remain outside the governor.

### `normalized_file` and `command`

These consume the normalized JSON contract in the README. They are the escape
hatch for provider changes and site-specific collectors. Command stdout is
reserved for the JSON object; diagnostics belong on stderr.

## Fleet integration

The observer measures current workers. Supported types are `static`, `file`,
and `command`. A command prints either a decimal integer or
`{"current_workers": N}`.

The actuator accepts `none`, `target_file`, and `command`. Target files are
atomic handoffs to an external reconciler. Command argv is executed directly,
with every `{desired_workers}` occurrence replaced. Shell expansion, pipes, and
redirection do not occur.

Start with `none` or `--observe-only`. Confirm two or more same-generation
samples and decision output before granting process-management authority.
