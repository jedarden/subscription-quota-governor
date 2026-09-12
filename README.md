# Subscription Governor

`subgov` is a provider-neutral quota governor for subscription-backed AI coding
agents. It observes reset windows, learns quota burn per worker, and publishes or
applies a conservative worker target. One process can govern separate
`claude-anthropic`, `codex`, and `claude-zai` accounts without confusing the
protocol used by an agent with the subscription that pays for it.

The initial release includes native quota collectors for:

- Claude Code's Anthropic OAuth usage surface;
- Codex's documented `account/rateLimits/read` app-server method;
- normalized JSON over files, HTTP, or command output for Z.AI and custom
  integrations.

## Why account identity is explicit

An agent speaking the Anthropic protocol may actually consume Z.AI Coding Plan
quota. `subgov` therefore keys all policy and state by a configured account name.
Fleet labels and provider protocol names have no effect on quota attribution.

## Install and try it

```console
cargo install --path .
subgov --config examples/claude-code.yaml check
subgov --config examples/codex.yaml snapshot codex
subgov --config examples/all-three.yaml run --once --observe-only
```

Run in `--observe-only` mode for at least two polling intervals before enabling
actuation. The `linear_to_reset` strategy deliberately holds the current target
until it has two observations in the same reset generation; zero observed delta
is not treated as free capacity. An empty fleet starts at the configurable
`bootstrap_workers` value so the governor can learn an initial burn rate.

## Configurable utilization

Utilization is a fraction from `0.0` to `1.0`. Set either a consumption target or
a reserve; they are equivalent:

```yaml
utilization:
  target_utilization: 0.85 # consume up to 85% by reset
  # reserve_fraction: 0.15 # equivalent; do not set both
  strategy: linear_to_reset
  stale_after_seconds: 900
  stale_behavior: hold
  minimum_sample_seconds: 300
  windows:
    weekly_scoped:
      target_utilization: 0.90
    five_hour:
      reserve_fraction: 0.20
```

An override may also set `enabled: false`. Unlisted windows inherit the account
default. Every observed window is evaluated independently and the smallest
worker target wins, so a weekly limit can constrain a five-hour window and vice
versa.

Strategies:

- `linear_to_reset` learns burn per worker and aims to reach the configured
  utilization at reset.
- `ceiling_only` runs at `max_workers` below the target and `min_workers` at or
  above it. It is useful for bursty pools but intentionally aggressive.

Stale data never scales up. `stale_behavior: hold` retains the current worker
count; `min_workers` drains toward the configured minimum. Per-cycle scale-up
and scale-down limits apply to every decision.

## Quota and fleet adapters

Quota sources normalize to this JSON contract:

```json
{
  "observed_at": "2026-09-12T12:00:00Z",
  "fresh": true,
  "windows": [
    {
      "id": "weekly",
      "used_fraction": 0.42,
      "resets_at": "2026-09-17T00:00:00Z",
      "duration_minutes": 10080,
      "reached": false
    }
  ]
}
```

Use `source.type: command` for another observer; the command must emit exactly
one normalized object on stdout. Commands are argument arrays and never passed
through a shell.

Fleet observers can be a static value, a file containing an integer, or a
command printing an integer (or `{"current_workers": N}`). Actuators can write
an atomic target file or execute an argv array containing
`{desired_workers}`. A `none` actuator makes observe-only deployment the safe
default.

See [configuration notes](docs/notes/configuration.md), the
[implementation plan](docs/plan/plan.md), and the ready-to-edit
[Claude Code](examples/claude-code.yaml), [Codex](examples/codex.yaml), and
[Claude-on-Z.AI](examples/claude-zai.yaml) examples.

## Safety boundaries

- Only one process may own a state file; an advisory lock rejects a second.
- State and target files are replaced atomically.
- OAuth tokens are read from Claude Code's credential file and are never sent
  to logs, stdout, command arguments, or state. A refresh updates the credential
  file atomically with mode `0600`.
- Codex authentication remains inside the installed `codex` process.
- Z.AI authentication remains inside a site-local collector; `subgov` reads
  only the normalized, non-secret quota projection.
- Source or account failures are isolated; a failed account is not actuated.

## License

MIT
