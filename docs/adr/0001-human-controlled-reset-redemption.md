# ADR-0001: Keep banked-reset redemption human-controlled

- Status: Accepted
- Date: 2026-09-27

## Context

Codex exposes earned reset-credit balance and expiry information through the
read-only `account/rateLimits/read` App Server method. Its protocol also has a
separate consumption method that irreversibly spends one credit and moves the
account into a new quota generation.

The governor needs balance and expiry information to decide how quickly a
worker fleet should consume the current generation. It does not need authority
to spend the credit. Redemption has account-level consequences, can alter the
next reset date, and benefits from a human verifying the visible account state
at the moment of use.

## Decision

Banked-reset redemption is always a human action.

The governor will:

- read the authoritative available count and optional credit expirations;
- increase worker pacing to meet the configured minimum and known deadlines;
- drain the governed fleet toward its configured minimum at the redemption
  threshold; and
- emit `manual_redemption_recommended: true` in the structured decision.

The governor will not:

- call `account/rateLimitResetCredit/consume`;
- expose automatic redemption configuration;
- expose a CLI or actuator that consumes a reset credit; or
- persist redemption requests or idempotency keys.

After a human redeems through an interactive Codex surface, the next ordinary
rate-limit read observes the changed reset generation and the controller
returns to burn-rate learning.

## Consequences

- Spending a scarce, expiring account benefit cannot happen as an incidental
  effect of fleet automation.
- Operators receive enough lead time and a clear decision signal to redeem the
  credit intentionally.
- Fully unattended operation pauses at the threshold until a human acts.
- A credit can expire if the operator ignores the recommendation; pacing and
  expiry reporting reduce but do not eliminate that operational risk.
- Tests and reviews can enforce a simple boundary: provider integration is
  read-only with respect to reset credits.

## Rejected alternatives

### Opt-in automatic redemption

An opt-in mode with durable idempotency would reduce operator intervention, but
it would still grant the governor authority to spend an account-level benefit.
That authority is outside the governor's role.

### Manual redemption command in `subgov`

A command would remain human-triggered, but it would mix quota observation and
fleet control with account mutation. The existing interactive Codex surfaces
already provide the appropriate human-controlled boundary.
