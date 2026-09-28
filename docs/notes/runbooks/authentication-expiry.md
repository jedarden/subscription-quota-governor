# Runbook: expired or invalid provider credentials

Restates
[the plan's native provider source requirements](../../plan/plan.md#7-provider-source-requirements)
(§7.1 Anthropic, §7.2 Codex) as an operator procedure, cross-checked against
`src/source.rs`. This covers the two native sources, `anthropic_oauth` and
`codex_app_server`. The generic sources (`command`, `normalized_file`,
`normalized_http`) have no credential concept inside `subgov` at all --
whatever a site-local collector does for its own authentication is entirely
outside this runbook; see the
[stale/failed sources runbook](stale-sources.md) for those instead.

Every authentication problem surfaces as `provider_failure` in `subgov
status` and an `account_error` with `category: "observation"` in the JSONL
event stream, exactly like any other observation failure -- see that
runbook for the general detection mechanics (`subgov status`, the
`account_error` event shape, `run --once` exit code 4). This runbook is
about telling an authentication problem apart from the other causes of
`provider_failure`, and what to do once you have.

## `anthropic_oauth`: Claude Code's OAuth credentials

`subgov` never manages its own login. It reads the same credentials file
Claude Code itself maintains (`~/.claude/.credentials.json` by default,
configurable as `credentials_path`), refreshes the access token there when
it's near expiry, and writes the refreshed token back to that same file --
there is no separate `subgov`-owned credential store. Every failure mode
below is visible only via the `account_error` event's `error` text
(`src/source.rs::AnthropicSourceError`); none of them ever include the
token or the raw response body (§7.1 "never include a token or response
body in an error").

| Error text contains | Meaning | Action |
| --- | --- | --- |
| `failed to open Claude Code credentials ... for locking` | The file doesn't exist, or isn't readable/writable by the account `subgov` runs as | Confirm `credentials_path` and that `subgov` runs as the same unprivileged user that owns Claude Code's login (README's systemd setup notes this explicitly) |
| `failed to lock Claude Code credentials ... (a concurrent writer may be holding it)` | Another process held the exclusive lock past `timeout_seconds` | Usually transient; if persistent, something is holding the lock abnormally long -- check for a stuck Claude Code process |
| `... are missing required field 'claudeAiOauth'` / `'expiresAt'` / `'accessToken'` / `'refreshToken'` | The credentials file exists but doesn't have the shape `subgov` expects -- most commonly, **never logged in**, or **fully logged out** (Claude Code removes or empties these fields on logout) | Run Claude Code interactively and complete login (`claude` and follow its own auth flow); `subgov` has no login flow of its own and never will |
| `were refreshed by another process during this poll; skipping this cycle` | `ConcurrentRefresh` -- Claude Code itself refreshed the token in the same instant `subgov` tried to. Not an authentication problem | No action; this is expected to happen occasionally when both run on the same host and resolves itself next cycle. Only escalate if it recurs on *every* cycle (would suggest a tighter, unexpected refresh loop) |
| `Anthropic token refresh request failed` | The refresh HTTP call itself failed (network, or the token endpoint returned an error status) -- ureq surfaces an HTTP error status here as part of the underlying error, which is what an **expired or revoked refresh token** looks like from a live provider rejection | Log in again via Claude Code; a rejected refresh means the offline grant itself is no longer valid, not something `subgov` can recover from by retrying |
| `Anthropic token refresh response is missing required field ...` / `was not valid JSON` | The token endpoint responded successfully but with an unexpected shape (provider-side API change, or `token_url` misconfigured to point somewhere else) | Confirm `token_url` matches the default (`https://platform.claude.com/v1/oauth/token`) unless deliberately overridden; if the default itself started failing, this is a provider-side compatibility break, not a local config problem -- check for a Claude Code update |
| `failed to write refreshed Claude Code credentials ...` | The refresh succeeded but writing the result back to disk failed (permissions, disk full, filesystem gone read-only) | Fix the filesystem condition; the in-memory refreshed token was still usable for this cycle even though the write failed, so this is not urgent in the way a failed refresh is, but will recur every cycle until fixed |
| `Anthropic usage request failed` / response errors | Authentication itself succeeded (a valid access token was obtained); the *usage* endpoint call failed. Not a credential problem -- see the stale/failed sources runbook | -- |

Two things worth knowing about the refresh mechanism, since they explain
what "normal" looks like and rule out false alarms:

- **Refresh happens automatically, five minutes before expiry**
  (`REFRESH_THRESHOLD_MILLIS = 300_000` in `src/source.rs`), on the poll
  cycle where that becomes true -- not on a separate schedule. An account
  whose `poll_interval_seconds` is large relative to five minutes will
  simply refresh on whichever cycle first crosses that threshold; this is
  expected, not a sign of thrashing.
- **A refresh in flight from `subgov` and a refresh from Claude Code
  itself are safe to overlap.** `subgov` takes an exclusive file lock for
  the whole read-decide-refresh section, and additionally re-reads the file
  immediately before writing the refreshed token, discarding its own
  result rather than clobbering a legitimate concurrent refresh if the
  refresh token on disk no longer matches the one it started with (see
  `apply_refreshed_credentials`). This is what produces the
  `ConcurrentRefresh` skip above -- it is the safety mechanism working, not
  a bug.

## `codex_app_server`: Codex's own session

Authentication for Codex is deliberately kept entirely inside the `codex`
process itself (§7.2 "keep authentication inside the Codex process") --
`subgov` never reads or writes a Codex credential file at all, and there is
no equivalent of the Anthropic refresh flow to reason about. An expired or
missing Codex login surfaces only as a protocol-level rejection from the
app-server process `subgov` spawns:

| Error text contains | Meaning | Action |
| --- | --- | --- |
| `failed to start ... app-server` | The configured `executable` (default `codex` on `PATH`) isn't installed or isn't executable | Confirm the Codex CLI is installed for the account `subgov` runs as, and that `executable` in config points at it if not on `PATH` |
| `timed out initializing the Codex app-server` | The process started but never completed the `initialize`/`initialized` handshake within `timeout_seconds` | Run `codex app-server --listen stdio://` manually as the same user to see whether it hangs outside `subgov` too -- this isolates whether the problem is Codex itself or `subgov`'s invocation of it |
| `Codex app-server rejected initialization (code ...)` | The app-server responded to `initialize` with a JSON-RPC error. This is the shape an unauthenticated or logged-out Codex session takes | Run `codex login` (or whatever the installed Codex CLI's interactive login command is) as that same user; `subgov` has no way to drive that login itself and never will, matching the Anthropic case above |
| `Codex app-server rejected the rate-limit request (code ...)` | Initialization succeeded, but the specific `account/rateLimits/read` call was rejected. Depending on the code this can also indicate an authentication problem that only manifests once a real API call is attempted, not just at handshake time | Same remediation as the row above -- re-authenticate the Codex CLI session |
| `Codex rate-limit response contained no usable quota windows` | Authentication succeeded; Codex returned a response with no windows `subgov` can normalize. Not a credential problem | See the stale/failed sources runbook |

Only the JSON-RPC error `code` (a small standard integer) is ever surfaced
in the error text -- the `message`/`data` fields on a Codex protocol error
are provider-controlled free text and are deliberately discarded rather
than risked into logs (`src/source.rs` comment above `CodexSourceError`).
If a `code` alone isn't enough to diagnose the rejection, reproduce it by
running the app-server manually and inspecting its raw output directly,
outside of anything `subgov` logs.

## What happens to the account while credentials are broken

Exactly the same safety behavior as any other `provider_failure`: no
decision is computed for that account this cycle, so nothing actuates and
nothing in that account's persisted state changes (state keeps the last
values from the last cycle that succeeded). Only the failing account is
affected -- per §7.4, one account's source error never touches another
account's evaluation or actuation. There is no automatic drain or
fail-safe scale-down triggered specifically by an authentication failure
beyond that; if you need the fleet to drain while credentials are being
fixed, that's the same lever as any extended outage (lower `min_workers` in
config, or switch the actuator to `none` and manage the fleet manually
until authentication is restored).

## Verification

After re-authenticating (Claude Code login, or Codex CLI login):

- `subgov --config PATH snapshot ACCOUNT` succeeds and prints a normalized
  snapshot -- this performs no state write or actuation, so it's the
  cheapest way to confirm the fix before waiting for the next scheduled
  cycle.
- `subgov status` returns to `healthy_learning` or `intentional_hold` for
  that account on the next completed cycle.
- No further `account_error` events with `category: "observation"` for
  that account.
