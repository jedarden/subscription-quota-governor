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

The same read also captures `rateLimitResetCredits.availableCount` and any
optional detail rows. The count, not the row count, is authoritative. The
governor uses only the supported
`account/rateLimitResetCredit/consume` App Server request to redeem a credit.

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
ordinary policy preserves a reserve. This makes the next credit redeemable.
Shorter windows continue to constrain the final worker target.

Detailed credit rows may be absent. In that case the governor still enforces
the minimum pace from the authoritative balance, but cannot calculate an
expiry deadline. `deadline_safety_seconds` (default six hours) advances each
known deadline to leave operational margin.

`auto_redeem` defaults to false and is valid only with a `codex_app_server`
source. `--observe-only` disables it. When enabled, the governor:

1. waits until the weekly window reaches `redeem_at_utilization`;
2. writes a pending UUID idempotency key and earliest-expiring known credit id
   to its atomic state file;
3. requests redemption and retains that record across transport uncertainty;
4. clears it only for a documented definitive outcome; and
5. re-reads rate limits and re-evaluates before fleet actuation.

This prevents a lost response or process restart from consuming two credits.

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
