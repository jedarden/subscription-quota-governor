# Subscription Governor: build and production-readiness plan

## 1. Purpose

Build one account-aware governor that paces subscription quota for AI coding
workers using Claude Code with Anthropic, Codex, and Claude Code with Z.AI. The
governor must consume observable utilization and reset data, normalize every
source into one contract, and derive a safe worker target without confusing an
agent's wire protocol with the subscription account paying for its work.

This document is the implementation specification and delivery checklist. The
repository contains a working v0.1 baseline, but the project is not considered
production-ready until the v1 acceptance criteria in section 18 are complete.

Status notation:

- `[x]` exists in the v0.1 baseline;
- `[ ]` remains to be implemented or proven for v1;
- `[~]` exists but needs the hardening described here.

## 2. Goals

1. Govern `claude-anthropic`, `codex`, and `claude-zai` as distinct billable
   accounts in a single process.
2. Make desired utilization configurable at account and reset-window scope.
3. Pace consumption toward each reset rather than merely stopping at a fixed
   ceiling.
4. Allow every observable provider window to constrain the fleet.
5. Fail conservatively: stale, malformed, or missing quota data must never
   cause scale-up.
6. Keep provider credentials and site-local provider details out of logs,
   state, command arguments, examples, and public interfaces.
7. Support multiple fleet managers through small observer and actuator
   boundaries instead of embedding scheduler-specific behavior.
8. Be safe to run first in observation mode and straightforward to roll back.
9. Extend governed capacity beyond subscription quota to per-host machine
   resources (CPU, RAM), and distribute each account's quota-safe worker
   total across that account's configured hosts by available headroom —
   bin-packing workers onto hosts with room rather than ever scaling the
   account total past what quota already allows. See §22.

## 3. Non-goals

- Scheduling individual prompts, models, or tasks.
- Combining independent subscriptions into a fungible quota pool.
- Predicting provider pricing or token costs when the subscription surface
  already reports utilization.
- Managing provider login workflows.
- Redeeming, consuming, or otherwise mutating banked reset credits. The
  governor may recommend redemption, but a human performs it.
- Embedding private deployment topology or provider-specific infrastructure.
- Mutating a Kubernetes fleet directly. A Kubernetes installation should write
  desired state through its normal GitOps or controller boundary.
- Treating a successful poll as proof that actuation is safe.
- Auto-discovering hosts or measuring a host's total capacity. Hosts and their
  resource ceilings are configured explicitly, the same as accounts and their
  windows.
- Coordinating NEEDLE bead claims or repository work across hosts. Placement
  sets a worker *count* per host; avoiding duplicate claims on a repository
  worked from more than one host remains an operational NEEDLE practice
  outside this project (see §22.9).
- Cost- or task-aware placement optimization beyond proportional headroom
  distribution in the initial release (see §22.7 future hardening).

## 4. Vocabulary and ownership boundaries

| Term | Meaning |
| --- | --- |
| Account | One independently metered subscription. It is the primary isolation key. |
| Source | Adapter that returns a normalized snapshot for exactly one account. |
| Window | One provider quota constraint with utilization and reset time. |
| Generation | The observations sharing the same window reset timestamp. |
| Observer | Adapter that reports the current worker count for an account. |
| Actuator | Adapter that publishes or applies a desired worker count. |
| Decision | Immutable explanation of one account's computed target. |
| Cycle | One collect, observe, evaluate, optionally actuate, and persist pass. |
| Host | One machine (or other reachable target) where an account's workers may run. Configured explicitly per account, never auto-discovered. |
| Resource snapshot | A host's observed CPU/RAM headroom, normalized like a quota snapshot but with no reset time. |
| Placement | The pure function that distributes an account's quota-safe total across its hosts by resource headroom. Runs strictly after the controller and never raises its total. |
| Binding resource | The scarcer of a host's CPU or RAM headroom for one placement cycle — mirrors "binding window." |

Account identity is configured explicitly. A worker can speak an Anthropic
compatible protocol while consuming a non-Anthropic subscription; protocol
labels must never choose the quota source.

## 5. System architecture

```mermaid
flowchart LR
    A[Account configuration] --> S[Quota source adapter]
    S --> N[Normalized snapshot]
    O[Worker observer] --> C[Controller]
    N --> C
    P[Prior generation sample] --> C
    C --> D[Decision record]
    D --> X{Observe only?}
    X -->|yes| L[Structured event]
    X -->|no| T[Fleet actuator]
    D --> W[Atomic state update]
    T --> L
    W --> L
```

Required separation:

- `source` owns provider transport and parsing only;
- `model` owns the provider-neutral contract;
- `controller` is a deterministic pure calculation;
- `fleet` owns worker observation and target application;
- `state` owns locking and durable samples;
- `main` owns orchestration, lifecycle, and exit behavior.

Provider branches are allowed in `source`; they are prohibited in the
controller and fleet modules.

Resource-aware multi-host placement extends this same pipeline with a second,
strictly downstream pure stage (`placement`) that runs after the controller
and before the fleet actuators; it never feeds back into the controller. See
§22.

## 6. Normalized quota contract

Every source must return exactly one snapshot:

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

### 6.1 Snapshot semantics

- `observed_at` is when the provider data was obtained, not when the controller
  happens to read a cache.
- `fresh` represents the source's own freshness judgment. The controller also
  enforces its configured maximum age.
- `windows` contains every active consumption limit relevant to the account.
- An empty `windows` array is invalid, not unlimited capacity.

### 6.2 Window semantics

- `id` is stable within one source and account. It need not be globally unique.
- `used_fraction` is finite and inclusive in `[0, 1]`.
- `resets_at` is an absolute UTC timestamp.
- `duration_minutes` is optional metadata; pacing depends on `resets_at`.
- `reached` is an explicit provider signal. The controller also treats
  `used_fraction >= target` as binding.
- Duplicate IDs, missing reset timestamps, non-finite values, and timestamps
  that cannot be parsed fail the snapshot.

### 6.3 Forward compatibility

New provider windows inherit account-level policy by default. They must not be
silently ignored because an older configuration does not name them. Operators
can explicitly disable a known non-binding window.

## 7. Provider source requirements

### 7.1 Claude Code with Anthropic

Baseline: `[~]`

The native adapter reads the Claude Code OAuth credential document, obtains a
valid access token, polls the configurable usage endpoint, and normalizes both
legacy named windows and generic limit entries.

Build requirements:

- [x] Expand `~` in the configured credential path.
- [x] Validate each access, refresh, and expiry field before it is used.
- [x] Refresh within five minutes of expiry.
- [x] Preserve unknown credential fields during an atomic refresh write.
- [x] Never include a token or response body in an error.
- [x] Accept null/inactive legacy windows without failing the entire account.
- [x] Prefer a generic limit over a same-ID legacy field.
- [ ] Lock refresh writes or otherwise prove safe coexistence with Claude Code.
- [ ] Preserve original file owner, mode, and parent-directory durability.
- [ ] Add recorded contract fixtures for legacy, generic, scoped, null, and
  forward-compatible payloads with all identifiers anonymized.
- [ ] Add mock HTTP tests for success, refresh, timeout, status failure, and
  malformed JSON.
- [ ] Document that endpoint compatibility is upstream-dependent and make an
  adapter failure non-actuating.

### 7.2 Codex

Baseline: `[~]`

The native adapter uses the installed Codex app-server process. It must perform
the protocol initialization handshake before requesting a full rate-limit
snapshot.

Build requirements:

- [x] Start `codex app-server --listen stdio://` without a shell.
- [x] Send `initialize`, wait for its response, then send `initialized`.
- [x] Call `account/rateLimits/read`.
- [x] Prefer every entry in `rateLimitsByLimitId` over the compatibility bucket.
- [x] Normalize primary and secondary windows as
  `<limit_id>.primary` and `<limit_id>.secondary`.
- [x] Bound request time and terminate the child on success or failure.
- [x] Keep authentication inside the Codex process.
- [x] Capture the authoritative banked-reset balance and optional expiration
  details from `account/rateLimits/read`.
- [x] Support a configurable banked-reset pace with a 2x default floor and
  deadline-aware acceleration.
- [x] Emit a structured manual-redemption recommendation at the configured
  utilization threshold and drain the governed fleet at that boundary.
- [x] Exclude reset-credit consumption from configuration, CLI commands,
  provider adapters, and state. Redemption remains human-controlled per
  [ADR-0001](../adr/0001-human-controlled-reset-redemption.md).
- [ ] Add a fake app-server executable for handshake, interleaved notification,
  timeout, child-exit, sparse-window, and protocol-error tests.
- [ ] Replace per-poll process startup with a supervised long-lived session.
- [ ] Consume rate-limit update notifications while periodically reconciling
  with authoritative full reads.
- [ ] Restart with capped exponential backoff and jitter.
- [ ] Bound stdout frame size and ignore unrelated frames without unbounded
  buffering.

### 7.3 Claude Code with Z.AI

Baseline: `[x]` provider-neutral boundary; `[x]` collector contract; `[ ]` production rollout.

No private endpoint or deployment component is part of this repository. A
site-local collector must emit the normalized contract through one of:

- `command`, with JSON on stdout and diagnostics on stderr;
- `normalized_file`, using an atomic producer write;
- `normalized_http`, on a loopback or otherwise access-controlled endpoint.

Build requirements:

- [x] Provide a `claude-zai` example using a generic command.
- [x] Keep provider credentials outside `subgov` and outside command arguments.
- [x] Apply exactly the same freshness and window validation as native sources.
- [x] Publish a standalone JSON Schema for collector authors.
- [x] Add contract tests that run an anonymous fixture-producing helper.
- [x] Document secure HTTP deployment options; the v1 governor will not add
  arbitrary secret headers to configuration.
- [x] Verify reset-generation transitions and percentage/absolute-usage
  normalization in the site-local collector before production rollout.

Collector normalization rules (locked):

- A site-local collector may read the quota gauges that a Z.AI proxy already
  exports rather than call the provider itself. Its `used_fraction`,
  `resets_at`, and freshness come from that sample; `observed_at` is the
  sample's own time (now minus the sample age), not the collector's run time.
- Z.AI windows start on first use. An idle account reports the previous,
  lapsed reset. Because the controller holds the current count on a lapsed
  reset (§9.3, reason `reset_due`), a lapsed window would block bootstrap
  from zero workers. The collector therefore projects a lapsed window with
  `used_fraction == 0` forward to `now + duration`. A lapsed window with
  usage above zero is passed through unchanged, so §9.3 still holds the count
  while the provider has not published the new generation. The contract and
  controller are unchanged by this rule.
- The proxy path exposes only the five-hour window. No weekly window is
  emitted until the provider quota response supplies one; a missing weekly
  window is a known gap, not unlimited weekly capacity, and the operator
  decides whether to run without a weekly bound.
- Upstream 429 pressure is an optional, separate contract extension defined
  in §24; a collector that does not emit it is still conforming.

### 7.4 Generic source behavior

- Commands are argv arrays and are never interpreted by a shell.
- Source stdout contains one JSON document only.
- HTTP requests have mandatory finite timeouts and no automatic credential
  injection.
- Files and HTTP bodies have explicit maximum sizes in v1.
- Redirects are disabled for credential-bearing native requests and either
  disabled or same-origin-only for normalized HTTP.
- A source error affects only its account and cannot actuate its fleet.

## 8. Configuration contract

Configuration is versioned YAML with strict unknown-field rejection.

```yaml
version: 1
poll_interval_seconds: 300
state_path: ~/.local/state/subscription-governor/state.json

accounts:
  codex:
    source:
      type: codex_app_server
      executable: codex
      timeout_seconds: 15
    fleet:
      min_workers: 0
      max_workers: 12
      bootstrap_workers: 1
      max_scale_up_per_cycle: 2
      max_scale_down_per_cycle: 3
      observer:
        type: file
        path: /run/user/1000/codex-workers.current
      actuator:
        type: target_file
        path: /run/user/1000/codex-workers.target
    utilization:
      target_utilization: 0.90
      strategy: linear_to_reset
      stale_after_seconds: 900
      stale_behavior: min_workers
      minimum_sample_seconds: 300
      windows:
        codex.primary:
          reserve_fraction: 0.15
```

Validation rules:

- `version` must be supported exactly.
- At least one account is required.
- Account keys must be non-empty and unique by YAML mapping semantics.
- Exactly one account-level `target_utilization` or `reserve_fraction` is
  required.
- A window override may set at most one of those fields.
- Targets are in `(0, 1]`; reserves are in `[0, 1)`.
- `max_workers >= min_workers`.
- `bootstrap_workers <= max_workers`.
- Poll, staleness, sample, and transport durations are positive and bounded.
- Command argv is non-empty and contains no empty element.
- Command actuators contain `{desired_workers}` at least once.
- Paths expand only a leading `~/`; environment and shell expansion do not run.
- Account names and window IDs are included in errors, but source payloads and
  credentials are not.

Configuration compatibility policy:

- Adding an optional field with a safe default is backward-compatible.
- Renaming a field or changing semantics requires a new top-level version.
- Deprecated fields receive at least one minor release of warning before
  removal.
- `subgov check` validates without contacting sources or actuating fleets.

## 9. Controller algorithm

The controller must remain a pure function of configuration, current time,
current workers, one snapshot, and prior state. It performs no I/O.

### 9.1 Freshness gate

Treat a snapshot as stale when either:

```text
snapshot.fresh == false
now - observed_at > stale_after_seconds
```

For stale data:

- `hold` returns the current count;
- `min_workers` moves toward the minimum, still respecting the maximum
  scale-down step;
- neither behavior may increase workers;
- the stale sample does not replace the last good burn sample.

### 9.2 Policy resolution

For each observed window:

1. Load the account default.
2. Apply a matching window override.
3. Convert `reserve_fraction` to `1 - reserve_fraction`.
4. Skip only when `enabled: false` is explicit.
5. Fail if no enabled observed window remains.

### 9.3 Reset-generation safety

Two samples can be compared only when their `resets_at` values are identical.
A changed reset timestamp begins a new generation and returns to learning mode.
If a reset is due but the provider has not published a new generation, hold
the current count; do not extrapolate across the boundary.

### 9.4 Linear-to-reset pacing

Given current utilization `u1`, previous utilization `u0`, elapsed hours `dt`,
workers during the interval `w0`, target `t`, and hours until reset `h`:

```text
per_worker_burn = (u1 - u0) / dt / w0
required_burn   = max(0, t - u1) / h
raw_workers     = ceil(required_burn / per_worker_burn)
```

The estimate is usable only when:

- both samples are in the same generation;
- `dt >= minimum_sample_seconds`;
- `w0 > 0`;
- `u1 >= u0`;
- calculated values are finite;
- `per_worker_burn > 0`.

Otherwise:

- zero workers with no usable sample requests `bootstrap_workers`;
- non-zero workers hold while learning;
- a zero observed delta holds because quantized percentages do not prove zero
  consumption.

Floating-point ratios within `1e-9` of an integer use the nearest integer before
ceiling, preventing `1.0000000001` from allocating an unintended extra worker.

### 9.5 Ceiling-only policy

- Below target: request `max_workers`.
- At target or when explicitly reached: request `min_workers`.
- Per-cycle step limits still apply.

This strategy is intentionally aggressive and must not be the default.

### 9.6 Multi-window arbitration

Compute a raw desired count for every enabled window. The account result is the
minimum of those counts. Then:

1. clamp to `[min_workers, max_workers]`;
2. cap upward movement by `max_scale_up_per_cycle`;
3. cap downward movement by `max_scale_down_per_cycle`.

The emitted decision retains every per-window result and reason, even when a
different window wins.

### 9.7 Banked-reset pacing and human handoff

When `banked_resets.enabled` and `available_count > 0`, the controller raises
the weekly target to at least `redeem_at_utilization` and computes:

```text
minimum banked burn = minimum_pace_multiplier * target / window duration
expiry burn         = remaining generations / time to expiry safety deadline
required banked burn = max(minimum banked burn, each known expiry burn)
```

The resulting worker count is a floor on ordinary weekly pacing, but a reached
shorter window remains binding. At the redemption threshold, the controller
sets `manual_redemption_recommended: true` and drains toward `min_workers`.
The governor then waits for a human redemption and observes the new generation
through the normal read path. It has no reset-credit mutation path. This
boundary is normative per
[ADR-0001](../adr/0001-human-controlled-reset-redemption.md).

### 9.8 Future estimator hardening

The two-point estimator is the v0.1 baseline. Before v1 production scaling:

- [ ] retain a bounded history per generation;
- deferred — model provider percentage quantization as an interval rather
  than a precise point (the exact interval model is one of §21's
  evidence-gated forks; not a bead until observation-mode traces exist);
- [ ] use a robust slope estimator that cannot learn zero from censored data
  (the general property is buildable now; only the exact parametric form is
  the §21 fork, refined later with evidence);
- deferred — distinguish externally consumed quota from governed-worker burn
  where supporting signals exist (§21: "whether external-consumption
  estimation is reliable enough to expose" is unresolved, not a bead);
- deferred — add hysteresis or minimum target dwell time after proving it
  does not violate binding windows (§21: the proof this bullet requires
  doesn't exist yet, so there is nothing to build against);
- [ ] simulate estimator behavior against bursty, idle, and reset-heavy traces.

The three "deferred" bullets above are the same forks §21 already lists as
needing observation-mode evidence before they can be decided, not merely
unbuilt work — per this project's rule that an unresolved decision is never a
bead, they stay here as plan text rather than becoming one. The other three
bullets are concrete, buildable requirements and are beaded.

## 10. State and concurrency

State contains no credentials, raw provider bodies, prompts, or account tokens.
For each account it stores the latest good sample per window and last desired
target.

Requirements:

- [x] Default under the platform state directory.
- [x] Expand a configured leading `~/`.
- [x] Acquire a non-blocking exclusive lock associated with the state file.
- [x] Refuse a second governor using the same state path.
- [x] Serialize to a same-directory temporary file, `fsync`, and rename.
- [x] Keep account state isolated in a map keyed by configured account name.
- [x] Add an explicit state schema version.
- [ ] Preserve the last valid state when a write fails.
- [ ] `fsync` the parent directory after rename on Unix.
- [ ] Set restrictive mode on state and lock files.
- [ ] Quarantine malformed state rather than silently starting empty.
- [ ] Test crash points before write, after write, after file sync, and after
  rename.

Only one controller may own a fleet. Different state paths are not permission
to run competing governors against the same actuator; deployment tooling must
enforce that operational invariant.

## 11. Fleet integration

### 11.1 Observers

Supported baseline observers:

- `static`: deterministic development and tests;
- `file`: one unsigned decimal integer;
- `command`: integer or `{"current_workers": N}` on stdout.

V1 requirements:

- bound file and stdout size;
- reject trailing non-whitespace data;
- apply a command timeout and kill the complete child process group;
- reject counts outside the configured fleet range unless a documented
  reconciliation mode is selected;
- capture no environment values in diagnostic output.

### 11.2 Actuators

Supported baseline actuators:

- `none`: no mutation;
- `target_file`: atomic integer handoff to another reconciler;
- `command`: direct argv execution with `{desired_workers}` substitution.

V1 requirements:

- do not invoke an actuator when desired equals observed;
- report `actuated: false` for `none` even when desired differs;
- pass no quota or credential data to actuator arguments;
- apply command timeout and process-group cleanup;
- record success only after a zero exit status or successful atomic rename;
- retain the prior state on actuation failure so the next cycle reconciles;
- optionally support an idempotency token through a non-secret environment
  variable after its contract is specified.

## 12. CLI and runtime behavior

Target command surface:

```text
subgov --config PATH check
subgov --config PATH snapshot ACCOUNT
subgov --config PATH run --once [--observe-only]
subgov --config PATH run [--observe-only]
subgov --config PATH doctor
```

### `check`

- Parse and validate configuration only.
- Perform no network requests, child execution, state writes, or actuation.
- Print the configuration version and account count.

### `snapshot`

- Collect exactly one named account.
- Print only the normalized snapshot.
- Perform no fleet observation, state write, or actuation.
- Never print authentication material or raw provider errors.

### `run --once`

- Acquire the state lock.
- Process every account even if another account fails.
- Save successful account observations once at the end of the cycle.
- Exit non-zero when any account fails.

### Continuous `run`

- Use a monotonic schedule so cycle duration does not accumulate drift.
- Add bounded jitter to avoid synchronized upstream polls.
- Handle SIGTERM/SIGINT by finishing or abandoning the current non-destructive
  operation, saving valid state, terminating children, and exiting promptly.
- Never overlap cycles for the same account.

### `doctor`

- Check each configured `needle_run` adapter with `needle test-agent` without
  starting or stopping workers.
- Fail with a non-zero exit if NEEDLE reports the adapter as unavailable, its
  CLI probe fails, or the check times out.
- Repeat the same bounded readiness check immediately before any
  `needle_run` actuator inspects sessions or changes workers.

Proposed stable exit codes:

| Code | Meaning |
| --- | --- |
| 0 | Success. |
| 2 | CLI or configuration error. |
| 3 | State ownership or persistence failure. |
| 4 | One or more source/observer failures. |
| 5 | Actuation failure. |
| 6 | Doctor readiness check failure. |

## 13. Observability

JSON Lines on stdout is the baseline event interface. Human diagnostics go to
stderr. Every event must contain `event`, `account` when applicable, and a UTC
timestamp.

Decision events include:

- observation time and freshness;
- current and desired workers;
- observe-only and actuated flags;
- every window's utilization, target, reset, proposed count, and reason;
- derived burn rate when usable;
- the binding window ID in v1.
- banked-reset balance, expiry pressure, and whether manual redemption is
  recommended.

Planned metrics:

- source success/failure and sample age by account;
- used and target fraction by account/window;
- seconds until reset;
- current and desired workers;
- decision reason and binding window;
- actuation attempts/failures;
- loop duration and skipped/late cycles.

Metrics must use configured account names only. They must not expose tokens,
provider payloads, model prompts, raw errors, or unbounded provider labels.

## 14. Security and privacy requirements

1. Secrets never appear in repository files, examples, argv, logs, state,
   metrics, panic messages, or test snapshots.
2. Native credentials are read only from their owning client's established
   credential boundary.
3. Generic HTTP does not accept plaintext credential configuration.
4. Configured commands execute directly without a shell.
5. OAuth and quota HTTP clients reject cross-origin redirects.
6. Source bodies, command output, and state files have bounded read sizes.
7. Temporary credential and state files use restrictive permissions and are
   cleaned up after failure.
8. Error types carry safe classifications; raw response bodies remain local to
   parsers and are discarded.
9. Fixtures are synthetic or irreversibly anonymized and contain no private
   infrastructure names.
10. Dependency advisories and license compatibility are checked before release.

## 15. Repository and module map

| Path | Responsibility |
| --- | --- |
| `src/model.rs` | Normalized quota types and validation-facing semantics. |
| `src/config.rs` | Versioned YAML types, defaults, strict validation, path expansion. |
| `src/source.rs` | Native and generic quota transports and normalization. |
| `src/controller.rs` | Pure pacing, reset, staleness, and arbitration decisions. |
| `src/placement.rs` | Pure multi-host bin-packing of one account's controller total by resource headroom (§22.7). |
| `src/state.rs` | Durable samples, atomic persistence, and exclusive ownership. |
| `src/fleet.rs` | Worker observers and target actuators, including the NEEDLE-native pair (`src/fleet/needle.rs`, §22.9). |
| `src/main.rs` | CLI, cycles, error isolation, and process lifecycle. |
| `examples/` | Safe configurations and synthetic normalized data. |
| `docs/notes/` | Operator-facing configuration and future design decisions. |
| `docs/adr/` | Accepted architectural decisions and their consequences. |
| `docs/research/` | Public provider-surface evidence and compatibility notes. |
| `docs/plan/plan.md` | Build sequence, contracts, and acceptance criteria. |

Provider-specific parsing should move into `src/source/` submodules during WP1
so no single file becomes the coupling point for unrelated providers.

## 16. Implementation work packages

### WP0: contracts and test harness

Dependencies: none.

- [x] Define Rust normalized snapshot/window types.
- [x] Define version-1 strict YAML types.
- [ ] Add `schema/quota-snapshot.schema.json`.
- [ ] Add `schema/governor-config.schema.json` generated or tested against Rust
  deserialization.
- [ ] Add reusable fake clock, fake source, observer, and actuator.
- [ ] Add a fixture-safety check for secrets and private identifiers.

Definition of done: schemas reject the same invalid inputs as Rust, and tests
can execute a complete cycle without network, wall-clock sleeps, or real child
processes.

### WP1: source adapter hardening

Dependencies: WP0.

- [~] Complete the Anthropic adapter requirements in section 7.1.
- [~] Complete the Codex adapter requirements in section 7.2.
- [~] Bound and harden generic command, file, and HTTP sources.
- [ ] Create contract-test suites per source.
- [ ] Add source-specific error enums with safe display text.

Definition of done: all success and failure paths are deterministic under test;
no adapter can leak a credential or leave a child process running.

### WP2: controller correctness

Dependencies: WP0.

- [x] Implement stale gate, policy inheritance, two strategies, reset-generation
  checks, bootstrap, and conservative multi-window selection.
- [x] Unit-test bootstrap, staleness, rate learning, and binding-window choice.
- [ ] Add table-driven boundary tests for every target/reserve limit.
- [ ] Add property tests for clamping and monotonic safety invariants.
- [ ] Add trace simulations and the robust estimator described in section 9.7.
- [ ] Emit the binding window explicitly.

Definition of done: property tests prove desired workers remain within bounds,
stale inputs never scale up, adding a binding window never raises the target,
and reset generations are never mixed.

### WP3: durable state

Dependencies: WP0 and WP2.

- [~] Version and harden state as specified in section 10.
- [ ] Add migration tests from every supported state version.
- [ ] Add corruption and crash-consistency tests.
- [ ] Add bounded per-generation history with retention limits.

Definition of done: state survives abrupt termination at each tested write
boundary without accepting partial JSON or silently losing the last good
generation.

### WP4: fleet boundaries

Dependencies: WP0.

- [~] Harden observers and actuators as specified in section 11.
- [ ] Introduce traits so controller-cycle tests use in-memory doubles.
- [ ] Add timeout and process-group termination support.
- [ ] Add idempotent reconciliation tests after partial actuation failure.

Definition of done: a hung or failed external helper is bounded, isolated to
its account, and cannot be reported as successful actuation.

### WP5: orchestration and lifecycle

Dependencies: WP1 through WP4.

- [x] Implement check, snapshot, one-shot, observe-only, and continuous modes.
- [x] Isolate account failures within a cycle.
- [ ] Add signal-aware shutdown, monotonic scheduling, and bounded jitter.
- [ ] Implement stable exit codes and structured error events.
- [ ] Prevent state advancement when actuation fails.
- [ ] Add cycle-level integration tests using injected adapters.

Definition of done: continuous operation exits cleanly, never overlaps account
cycles, and produces deterministic one-shot behavior in tests.

### WP6: observability and operations

Dependencies: WP5.

- [ ] Stabilize the JSONL event schema.
- [ ] Add metrics and a minimal readiness/status surface.
- [ ] Supply a hardened service example with restart limits and filesystem
  protections.
- [ ] Write runbooks for stale sources, authentication expiry, reset rollover,
  conflicting controllers, and rollback.
- [ ] Document backup and removal of state.

Definition of done: an operator can distinguish healthy learning, intentional
hold, provider failure, stale drain, and actuation failure without inspecting
credentials or raw provider traffic.

### WP7: release engineering

Dependencies: WP0 through WP6.

- [ ] Pin and audit dependencies.
- [ ] Verify formatting, Clippy, tests, docs, schemas, and secret scanning in the
  configured Argo workflow.
- [ ] Produce versioned Linux artifacts with checksums and provenance.
- [ ] Add changelog and compatibility policy.
- [ ] Cut `v1.0.0-rc.1` for observation-only rollout.

Definition of done: a clean clone can reproduce the release artifact and every
published checksum from documented commands.

### WP8: staged production rollout

Dependencies: WP7.

1. Install the release candidate without stopping existing governors.
2. Run snapshots only and compare normalized windows with provider-visible
   values.
3. Stop the existing governor for one account before starting `subgov` in
   observe-only mode for that same account.
4. Observe at least two useful samples and one reset generation.
5. Compare recommended targets with recorded worker activity and quota deltas.
6. Enable a target-file actuator for one low-risk account.
7. Enable remaining accounts one at a time.
8. Maintain a one-command rollback to observe-only or the previous controller.
9. Retire old control loops only after the v1 acceptance window passes.

Definition of done: all three accounts run for seven consecutive days, including
at least one reset each, without conflicting actuation, stale-driven scale-up,
or unaccounted quota crossover.

## 17. Test strategy

### 17.1 Unit tests

- Configuration defaulting, inheritance, strictness, and invalid boundaries.
- Provider payload normalization and forward-compatible unknown fields.
- Every decision reason and boundary condition.
- Step limiting at `u32` boundaries.
- Reset changes, future/expired reset times, clock skew, and stale timestamps.
- Worker count parsing and placeholder substitution.

### 17.2 Property tests

For generated valid configurations and snapshots:

- desired count is always within fleet bounds;
- stale decisions never exceed current workers;
- reached/at-target windows never request above the minimum before step limits;
- adding another enabled window cannot increase the raw account target;
- disabling one window cannot make another window's calculation less safe;
- state from another account cannot change the decision.

### 17.3 Contract tests

- Anonymous Anthropic legacy and generic payload fixtures.
- Scripted Codex app-server JSONL exchanges.
- Normalized file, command, and HTTP producer fixtures.
- Unknown windows and extra fields.
- Missing fields, oversized bodies, redirects, timeout, and malformed data.

### 17.4 Integration tests

- Full observe-only cycle across three accounts.
- One account failure while two succeed and persist.
- Actuator failure with unchanged durable state.
- Second-process lock refusal.
- Graceful termination during poll and between cycles.
- Atomic target/state readers never observe partial contents.

### 17.5 Soak and simulation tests

Replay synthetic week-long traces with:

- steady burn;
- bursty workers;
- integer-quantized percentages;
- interactive consumption outside governed workers;
- source gaps and stale recovery;
- clock skew;
- reset timestamp shifts;
- multiple simultaneous binding windows.

Success means utilization approaches the configured target without exceeding it
in the safety scenarios, oscillation remains bounded, and no stale interval
causes scale-up.

## 18. V1 acceptance criteria

V1 is ready only when all of the following are true:

- [ ] A clean clone builds using the documented stable Rust toolchain.
- [ ] `cargo fmt --all -- --check` passes.
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` passes.
- [ ] `cargo test --all-targets --all-features` passes.
- [ ] Configuration and normalized snapshot schemas are published and tested.
- [ ] All three account examples pass `subgov check`.
- [ ] Native source contract and failure tests pass without live credentials.
- [ ] The generic Z.AI example contains no private endpoint, credential, or
  deployment identifier.
- [ ] Stale, failed, empty, malformed, and unknown-window inputs are covered.
- [ ] Property tests prove the controller safety invariants.
- [ ] External commands have timeouts and complete process cleanup.
- [ ] State has a version, migration path, restrictive permissions, and crash
  tests.
- [ ] Observe-only mode is proven to perform no actuator mutation.
- [ ] Contract tests prove no governor path can call a reset-credit consumption
  method and that a ready credit produces a manual recommendation.
- [ ] JSONL and metrics contain no secrets or unbounded provider labels.
- [ ] Security/advisory and fixture-secret scans pass.
- [ ] Service, operations, and rollback documentation is complete.
- [ ] The release candidate completes the staged acceptance window in WP8.

## 19. Build and verification commands

Development baseline:

```console
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo run -- --config examples/claude-code.yaml check
cargo run -- --config examples/codex.yaml check
cargo run -- --config examples/claude-zai.yaml check
cargo run -- --config examples/all-three.yaml run --once --observe-only
```

Before running the final command, use a temporary state path or remove only the
test-generated state afterward. Production verification must begin with
`--observe-only`.

## 20. Recommended implementation order

The critical path is:

```text
WP0 contracts
  ├── WP1 sources ──┐
  ├── WP2 control ──┼── WP5 runtime ── WP6 operations ── WP7 release ── WP8 rollout
  ├── WP3 state ────┤
  └── WP4 fleet ────┘
```

WP1 through WP4 can proceed independently after the contracts and test doubles
are fixed. Controller correctness and crash-safe state are release blockers;
metrics or service packaging must not be used to mask gaps in either.

Resource-aware placement (WP9 through WP14, §22.13) depends on WP2 and WP4
being stable — placement consumes the controller's account total and reuses
the fleet observer/actuator contract — and must land before WP8, since WP8's
staged rollout has to exercise both the single-host and multi-host paths
(§22.14).

## 21. Decisions intentionally deferred

These require evidence from observation-mode traces:

- exact robust estimator and quantization interval model;
- hysteresis and target dwell duration;
- poll jitter range;
- bounded history length;
- whether external-consumption estimation is reliable enough to expose;
- whether a long-lived Codex session materially improves reliability;
- which fleet-manager-specific adapters merit first-party support;
- whether/when to adopt a cost-aware placement objective in place of
  proportional-by-headroom distribution (§22.7), pending real multi-host
  placement traces.

Record architectural decisions in `docs/adr/` and operational evidence in
`docs/notes/`. Changes to the normalized contract or safety invariants require
an explicit plan revision.

## 22. Resource-Aware Multi-Host Placement

Status notation matches §1: `[x]` exists, `[ ]` remains to build, `[~]` exists
but needs hardening. Everything in this section is currently `[ ]` — it is a
plan addendum, not yet implemented.

### 22.1 Purpose

An account's workers do not have to run on one machine. Today a single
`fleet.observer`/`fleet.actuator` pair implicitly assumes one target. This
section extends the governor so one account can be configured with several
hosts, each with its own worker observer, actuator, and machine-resource
headroom, and adds a second pure decision stage — placement — that
distributes the controller's already-safe account total across those hosts
by available CPU/RAM rather than by quota. Quota answers "how many workers
total can this account safely run"; placement answers "which of this
account's hosts should run them." Placement can never increase the total the
controller already decided; it can only decide where.

This directly targets bin-packing NEEDLE workers across machines (e.g.
codinghome and lab) by both subscription headroom (existing controller) and
machine resource headroom (new), while preserving every existing safety
invariant in §2 goal 5 and §9.

### 22.2 Backward compatibility

An account with no `fleet.hosts` key behaves exactly as it does in v0.1: the
existing top-level `fleet.observer`/`fleet.actuator` are used directly and the
entire controller total is sent to that one implicit target. This is
mechanically identical to declaring a single host with no `resource_source`
(see §22.4) — an unconstrained host always has full headroom, so placement
degenerates to "one host gets everything," which is today's behavior. No
config version bump is required (§8's "adding an optional field with a safe
default is backward-compatible" applies); `fleet.hosts` is optional and
additive.

### 22.3 Configuration shape

```yaml
accounts:
  claude-anthropic:
    source: { ... }          # unchanged — account-level quota source
    utilization: { ... }     # unchanged — windows still govern the total
    fleet:
      min_workers: 0
      max_workers: 8
      bootstrap_workers: 1
      max_scale_up_per_cycle: 1
      max_scale_down_per_cycle: 2
      # `observer`/`actuator` here and `hosts` below are mutually exclusive.
      # Setting both is a validation error at `subgov check`.
      hosts:
        codinghome:
          max_workers: 6                     # optional; defaults to account max_workers
          resource_reserve:
            cpu_reserve_fraction: 0.25
            mem_reserve_mb: 4096              # headroom no placement may consume
          resource_source:
            type: command
            argv: [/usr/local/bin/resource-probe]
          observer:
            type: command
            argv: [/usr/local/bin/count-ai-workers, claude-anthropic]
          actuator:
            type: command
            argv: [/usr/local/bin/set-ai-worker-target, claude-anthropic, "{desired_workers}"]
        lab:
          max_workers: 8
          resource_reserve:
            cpu_reserve_fraction: 0.30
            mem_reserve_mb: 8192
          resource_source:
            type: command
            argv: [ssh, lab.tailnet, resource-probe]
          observer:
            type: command
            argv: [ssh, lab.tailnet, needle-worker-count, claude-anthropic]
          actuator:
            type: command
            argv: [ssh, lab.tailnet, needle-set-target, claude-anthropic, "{desired_workers}"]
```

Validation rules (extend §8):

- A host key is non-empty and unique within its account, same rule as account
  keys.
- `resource_reserve` is required whenever `resource_source` is present; a
  host with a `resource_source` but no reserve is a validation error (there
  is no safe default reserve — see §22.8's OOM note).
- A host's `max_workers` must not exceed the account's `max_workers`.
- At least one host is required whenever `hosts` is present at all.

### 22.4 Normalized resource contract

Every resource source returns exactly one snapshot, structurally parallel to
§6 but without reset semantics:

```json
{
  "observed_at": "2026-09-28T12:00:00Z",
  "fresh": true,
  "host_id": "lab",
  "cpu_available_fraction": 0.42,
  "mem_available_mb": 12288,
  "mem_total_mb": 65536
}
```

- `cpu_available_fraction` is finite and inclusive in `[0, 1]` — the fraction
  of the host's total CPU currently free, however the site-local probe
  chooses to measure it (load-average-derived, cgroup-quota-derived, or
  otherwise); the governor does not prescribe the method, only the
  normalized output, mirroring the Z.AI boundary in §7.3.
- `mem_available_mb` and `mem_total_mb` are finite, non-negative integers;
  `mem_available_mb <= mem_total_mb`.
- `fresh` and `observed_at` follow the same semantics as §6.1.
- A missing or malformed field fails the snapshot, same as §6.2.

### 22.5 Resource source requirements

Reuses the generic `command`/`file`/`normalized_http` transport already
specified in §7.4 verbatim — no new transport type. Build requirements:

- [ ] Add `ResourceSnapshot` alongside the existing quota `Snapshot` type in
  `src/model.rs`.
- [ ] Extend `src/source.rs`'s generic collectors to also parse
  `ResourceSnapshot`, sharing the same size-bounded, non-shell,
  finite-timeout rules as §7.4.
- [ ] Add a `resource-probe` example script under `examples/` that reports
  local CPU/RAM from `/proc/loadavg` and `/proc/meminfo` (or the host's
  cgroup limits where narrower), documented as a starting point, not a
  shipped daemon.
- [ ] Contract tests for missing fields, out-of-range fractions, and
  `mem_available_mb > mem_total_mb`.

### 22.6 Architecture

```mermaid
flowchart LR
    C[Controller decision: account total] --> PL[Placement]
    R1[Host resource source] --> PL
    R2[Host resource source] --> PL
    OB1[Host observer] --> PL
    OB2[Host observer] --> PL
    PL --> D1[Per-host decision record]
    D1 --> ACT1[Host actuator]
    D1 --> ACT2[Host actuator]
    D1 --> L[Structured event, per host]
```

`placement` is a pure function of: the controller's account total, the set of
configured hosts, each host's latest resource snapshot, each host's current
observed worker count, each host's `max_workers` and step limits, and each
host's `resource_reserve`. It performs no I/O, exactly like `controller`
(§9's opening constraint extends here verbatim).

### 22.7 Placement algorithm

```text
Given account_target (from §9 controller output, unchanged) and, for each
configured host h:
  current[h]        = host's observed worker count
  ceiling[h]         = host max_workers, default account max_workers
  headroom[h]        = min(cpu_available_fraction[h],
                            (mem_available_mb[h] - mem_reserve_mb[h]) / mem_total_mb[h])
                        clamped to [0, 1]; a host with no resource_source has
                        headroom[h] = 1 always (see §22.2)
  fresh[h]           = resource snapshot passes the §22.8 freshness gate

eligible = { h : fresh[h] and headroom[h] > 0 }

if eligible is empty:
    target[h] = current[h] for every host        # hold, never scale up blind

else:
    weight[h]     = headroom[h] / sum(headroom[h'] for h' in eligible)
    raw[h]        = round(account_target * weight[h])   for h in eligible
    raw[h]        = 0                                     for h not in eligible
    clamped[h]    = clamp(raw[h], 0, ceiling[h])

    # Remainder redistribution: any shortfall from clamping is handed to the
    # eligible host with the most *unused* headroom, one worker at a time,
    # until account_target is met or every eligible host is at its ceiling.
    while sum(clamped) < account_target and some eligible h has clamped[h] < ceiling[h]:
        h* = argmax_{h in eligible, clamped[h] < ceiling[h]} (headroom[h] - clamped[h]/ceiling[h])
        clamped[h*] += 1

    target[h] = clamped[h]

# Per-cycle step limits (§9.6) apply per host. Each host's
# max_scale_up_per_cycle / max_scale_down_per_cycle overrides the corresponding
# account limit independently when present; an absent host value inherits it.
```

Invariants, mirroring §9.6/§17.2's property-test style:

- `sum(target[h] for all h) <= account_target`, always — placement never
  raises the total the controller already decided.
- A host absent from `eligible` never receives more workers than it already
  has; it can still be scaled *down* toward the account's step limits if the
  controller's account total itself dropped.
- Placement is deterministic given identical inputs (no random tie-breaking
  — ties in `argmax` break by host key, ascending).

Future placement hardening (parity with §9.8, not blocking v1.1):

- deferred — replace proportional-by-headroom with a cost-aware objective
  (per-host $-cost, mirroring cgov's `distribute_workers_by_cost_priority`)
  once v1.1 ships and real placement traces exist; like §9.8's evidence-gated
  bullets, there is nothing to build against yet, so this is plan text, not
  a bead, until those traces exist.
- [x] Retain the most recent 16 per-host placement decisions with timestamps.
  A caller-selected transition threshold can detect targets alternating
  between two worker counts; placement trace simulation verifies detection
  during alternating headroom and expiry after a stable split.

### 22.8 Freshness and safety for resources

- Staleness gate: a host's resource snapshot is stale when
  `now - observed_at > stale_after_seconds` (defaults to the account's
  `utilization.stale_after_seconds`, overridable per host) or `fresh == false`.
- A stale host is *frozen*, not drained: it is excluded from `eligible` (so it
  can never gain workers) but existing workers on it are not force-killed by
  staleness alone — mirrors the existing `stale_behavior: hold` default and
  non-goal "treating a successful poll as proof that actuation is safe."
  Draining a stale host only happens if the controller's account total itself
  drops and step limits reach that host through the normal scale-down path.
- If every host is stale or absent a `resource_source`-based read for an
  account that declares `hosts`, placement holds every host at `current[h]`
  — same shape as the controller's own stale-data rule in §9.1.
- `mem_reserve_mb` has **no safe default** and must be configured explicitly
  per host (§22.3). This is a direct lesson from a real lab incident: a
  systemd unit's cgroup `MemoryMax` sized without headroom for the
  supervising process tree caused repeated OOM kills that took down the
  entire worker session server at once (not merely one worker). The operator
  is responsible for setting `mem_reserve_mb` at least as large as the
  largest observed single-worker footprint on that host; the governor does
  not attempt to auto-detect it in v1.1.

### 22.9 NEEDLE-native fleet adapter

The generic `command`/`file` observer and actuator (§11) remain sufficient
for any orchestrator. For NEEDLE specifically, add a first-party pair so a
site-local wrapper script is not required for the common case:

- [x] `observer: { type: needle_status }` — reads the heartbeat directory
  directly and returns the fresh worker count for the configured NEEDLE
  agent/adapter on that host.
- [x] `actuator: { type: needle_run }` — launches or stops workers via
  `needle run -w <repo> -a <adapter>` / session-pattern-matched stop, the
  same shell-out boundary `claude-governor`'s plan already documents and
  justifies (`docs/plan/plan.md` §"Separation of concerns" in that repo):
  subgov decides a number, NEEDLE still owns worker lifecycle, bead
  claiming, and prompt templating entirely.
- [x] A `needle_adapter_parity` check (mirroring `cgov doctor`'s
  `claude_print_parity`) that fails closed when a host's configured NEEDLE
  adapter is missing or unreachable, rather than actuating blind. `subgov
  doctor` checks each configured adapter without mutation; `needle_run` repeats
  the bounded check immediately before actuation.
- Explicitly out of scope here (§3 non-goal): coordinating NEEDLE bead claims
  across hosts. This adapter only ever sets a target *count* per host: it
  does not choose which repository a host's workers roam into, and it does
  not solve NEEDLE's documented lack of cross-host claim coordination.
  Placing a resource-governed account's hosts safely still requires the
  existing operational practice of partitioning which repos each host's
  NEEDLE fleet is allowed to roam into.

### 22.10 Remote transport

Cross-host `command` sources/observers/actuators (the `lab` example in
§22.3) run over SSH using the existing generic `command` primitive — no new
transport type or credential model is introduced. Requirements:

- [ ] Document that `argv: [ssh, <host>, <remote-argv...>]` is the sanctioned
  shape; `ssh` itself is exec'd directly (never through a local shell, per
  §7.4), but note explicitly that SSH re-joins and re-parses its remote
  command server-side through the remote user's shell — so, unlike a local
  `command` source, argv-level injection safety is the *remote script's*
  responsibility, not subgov's. Fixed, non-interpolated remote scripts only;
  never build remote argv from live provider data.
- [ ] SSH invocations use key-based auth already present on the host
  (matches this environment's existing Tailscale SSH model) and a bounded
  `ConnectTimeout`; subgov never manages or stores an SSH credential itself.
- [ ] The command timeout in §7.4/§11.1 applies to the whole SSH round trip,
  not just local execution.

### 22.11 State and observability extensions

- [x] State is keyed by `(account, host)` for per-host samples in addition to
  the existing per-account key (§10); the account-level "last desired total"
  is unchanged and remains the controller's, not placement's, output.
- [x] Decision events (§13) gain a per-host placement record alongside the
  existing per-account decision: resource snapshot, headroom, eligibility,
  and the placed target, keyed by account and host.
- [x] Metrics (§13) add per-host resource utilization and placed-worker count
  alongside the existing per-account/per-window metrics.

### 22.12 CLI extensions

- [x] `subgov snapshot ACCOUNT --host HOST` prints only that host's resource
  snapshot, same non-actuating contract as plain `snapshot` (§12).
- [x] `run`/`run --once` decision output includes a per-host breakdown when
  `hosts` is configured; unchanged single-line output when it is not.

### 22.13 Work packages

#### WP9: resource contract and sources

Dependencies: WP0, WP2.

- [ ] `ResourceSnapshot` type and validation (§22.4).
- [ ] Generic resource source support in `src/source.rs` (§22.5).
- [ ] `fleet.hosts` config parsing, validation, and the backward-compatible
  single-implicit-host default (§22.2, §22.3).

Definition of done: an account can declare `hosts` with only
`resource_source`/`observer`/`actuator` per host, `subgov check` validates
it, and an account with no `hosts` key is provably unaffected (existing WP0
test doubles pass unchanged).

#### WP10: placement algorithm

Dependencies: WP9.

- [ ] `src/placement.rs` pure function implementing §22.7.
- [ ] Unit tests for the boundary cases in §22.7/§22.8 (no eligible host,
  single host, ceiling-clamped host, remainder redistribution, stale host
  frozen not drained).
- [ ] Property tests: placed total never exceeds `account_target`; disabling
  one host never increases another host's headroom-derived weight beyond its
  own; identical inputs are deterministic.

Definition of done: property tests prove placement can never raise the
controller's total, and a host that goes stale mid-run only ever holds or
loses share, never gains it.

#### WP11: multi-host fleet integration

Dependencies: WP4, WP10.

- [x] Per-host observer/actuator wiring in `src/fleet.rs`.
- [x] Per-`(account, host)` state (§22.11).
- [x] Per-host decision events and metrics (§22.11).
- [x] Integration test: one account, two hosts, one host stale — the healthy
  host absorbs the account total up to its ceiling and the stale host holds.

Definition of done: a full observe-only cycle across a two-host account
produces one decision record per host, and killing one host's resource
source mid-run degrades to holding that host without affecting the other.

#### WP12: NEEDLE-native fleet adapter

Dependencies: WP11.

- [x] `needle_status` observer and `needle_run` actuator (§22.9).
- [x] `needle_adapter_parity` doctor-style check.
- [ ] Example config wiring an account across two real hosts.

Definition of done: `subgov run --once --observe-only` against a live
two-host NEEDLE fleet produces a correct per-host observed count with no
actuation, verified against `needle status` on each host directly.

#### WP13: remote transport hardening

Dependencies: WP9.

- [ ] SSH command timeout, key-auth-only, and the argv-safety documentation
  in §22.10.
- [ ] Contract tests for a slow/hanging/unreachable SSH target (bounded,
  isolates to that host only, per §22.8).

Definition of done: an unreachable remote host degrades exactly like a stale
local one — held, not drained, isolated to that host.

#### WP14: staged multi-host rollout

Dependencies: WP8, WP12, WP13.

Extends §16 WP8's staged rollout: run placement in observe-only mode for at
least one full account cycle across every configured host before enabling
any host's actuator; enable one host's actuator before the second; keep a
one-command rollback to a single-implicit-host config (§22.2) throughout.

Definition of done: a resource-governed, multi-host account runs for seven
consecutive days, including at least one quota reset, without a host being
over-placed relative to its configured reserve.

### 22.14 V1.1 acceptance criteria

Additive to, and dependent on, the §18 v1 criteria for every account/host
this applies to:

- [ ] An account with no `hosts` key behaves byte-for-byte as it does today
  (regression-tested against the v1 test suite).
- [ ] Property tests prove placement never exceeds the controller's total.
- [ ] A stale or unreachable host is provably held, never drained, by
  staleness alone.
- [ ] `mem_reserve_mb` is required (not defaulted) wherever a
  `resource_source` is configured.
- [ ] The NEEDLE-native adapter passes parity checks on both configured
  hosts before its actuator is enabled.
- [ ] The WP14 seven-day multi-host observation window passes with no
  over-placement incident.

### 22.15 Decisions locked by this addendum

Recorded here, not as open forks, per this project's convention that a plan
decides every fork it raises rather than deferring it to an ADR written
mid-build:

1. One governor process still owns an entire account (§10's existing rule
   extends across hosts) — never one process per host for the same account.
   A shared subscription used from multiple hosts must be governed
   centrally or it double-books, the same failure shape §10 already
   forbids.
2. Placement is strictly subordinate to the controller: it distributes, it
   never authorizes more workers than the controller already decided.
3. Cross-host SSH reuses the existing generic `command` primitive rather
   than adding a native transport type, at the cost of pushing argv-safety
   responsibility onto the remote script (documented, not hidden).
4. Cross-host NEEDLE bead-claim coordination is explicitly not solved here —
   it is called out as a non-goal (§3) and left to NEEDLE's own per-host
   repo-partitioning practice.
5. `mem_reserve_mb` has no auto-detected default; under-provisioning it is
   an operator error the governor refuses to paper over, given the real
   cgroup OOM precedent in §22.8.
6. v1 (quota-only) and v1.1 (resource-aware placement) are separate
   acceptance gates (§22.14); v1.1 for a given account depends on that
   account already meeting the relevant v1 criteria.

## 23. Elastic Tier over the NEEDLE Roster

Status: plan locked 2026-10-10; WP15–WP17 delivered. **§26.2 supersedes the
ownership floor below for any pool that declares a `takeover` list** (see the
amendments noted in §23.2 and §23.4).

### 23.1 Purpose

The v1/v1.1 governor manages one pool (`claude-print`, `max_workers: 1`). The
codinghome fleet is about 40 systemd-managed `needle-worker@` units from the
source-controlled roster (`NEEDLE/fleet/ex44/workers.tsv`): roughly 22 GLM
(Z.AI), 18 Codex, and one Claude pool. subgov must manage the elastic part of
that fleet: workers launched and retired by the governor, paced against each
provider's quota, on top of the roster's fixed floor.

### 23.2 Ownership model (locked)

1. **The roster is the floor.** subgov never starts, stops, restarts, or
   edits any `needle-worker@*` unit, and never stops a NEEDLE process it did
   not launch. The roster remains owned by `apply-ex44-fleet.sh`.
   *(§26.2 supersedes this for a pool with a `takeover` list: listed units
   are retired idle-only, outside systemd for the signal reach, once a
   prefixed replacement runs in the same pool. Unlisted roster units keep
   this guarantee.)*
2. **Elastic workers are the governor's.** They are launched by the
   `needle_run` actuator and carry an explicit `identifier_prefix` (by
   convention `sg-<account>-`; opting in requires setting it). Anything without
   the prefix is not owned, whatever its adapter.
3. **One adapter per account.** An account owns one quota source and one
   NEEDLE adapter. A second adapter on the same provider quota (for example
   Codex Luna and Sol) is a separate roster concern and is not governed in
   this tier; two accounts sharing one quota would double-book it, the failure
   shape §10 already forbids.
4. **One host.** This tier governs codinghome only. The lab is a separate
   governor instance with its own accounts; cross-host elastic placement stays
   with §22 and is out of scope here.

### 23.3 Observer scoping (locked)

The `needle_status` observer currently counts every fresh heartbeat for an
adapter (§11.1). With roster workers on the same adapter this overstates the
governor's own fleet, and §11.2 `needle_run` reconciles only tmux sessions, so
observer and actuator disagree and the account never converges.

- `needle_status` gains `identifier_prefix` (explicit; no default). It reports two counts:
  `owned` (heartbeats whose worker id starts with the prefix) and
  `unowned` (fresh heartbeats for the same adapter without it).
- `needle_run` takes the same prefix (config validation requires observer and
  actuator to match), launches one worker per `--identifier <prefix><n>` and
  reconciles only sessions carrying the prefix.
- With no `identifier_prefix`, behaviour is byte-for-byte today's. The
  existing `claude-anthropic` account keeps working unchanged until it opts in.

### 23.4 Pacing with a floor (locked)

Unowned workers spend the same quota, so they must be inside the burn
estimate or pacing undercounts. Per cycle:

- the controller sizes `desired_total` from `owned + unowned` observed workers
  exactly as it sizes a pool today (§9);
- `desired_elastic = clamp(desired_total - unowned, min_workers, max_workers)`,
  and `max_workers` bounds only the elastic tier;
- placement (§22) and the actuator see `desired_elastic` only.

A floor that alone reaches the target drives `desired_elastic` to
`min_workers`; the governor never stops roster workers to make room.
*(§26.2 amendment: takeover-listed workers are counted separately from
`unowned`, so they hold no floor — otherwise a roster satisfying demand
would keep `desired_elastic` at `min_workers` forever and the takeover
could never start. Elastic reconcile is followed by one idle-only
retirement step through the pool's takeover helper, which removes a listed
worker only while a prefixed replacement runs.)*

### 23.5 Provider specifics (locked)

- **claude-anthropic:** existing source; opt in to `identifier_prefix` and raise
  `max_workers` from 1 only after the observe-only comparison in §23.7.
- **codex:** `codex_app_server` source. When the weekly window is at its limit,
  pacing holds the elastic tier at `min_workers`. Redeeming banked reset
  credits stays a human handoff (§9.7).
- **claude-zai:** `command` source reading the proxy's quota gauges (§7.3).
  Proxy saturation, not quota, is the observed binding constraint for GLM, so
  `max_workers` is set conservatively (at most 2) and raised on evidence once
  the §24 backpressure input is live and validated.

### 23.6 Work packages

#### WP15: ownership-scoped observer and actuator

Dependencies: WP4.

- [x] `identifier_prefix` on `needle_status` and `needle_run`, with `owned` and
  `unowned` counts, and an unchanged path when it is absent.
- [x] Scale-down selects only prefixed sessions, and a prefix matched by a
  longer unrelated session fails closed, as in §11.2 today.
- [x] Tests: a roster heartbeat on the same adapter is never counted as owned
  and a same-adapter session outside the prefix is never stopped.

#### WP16: floor-aware controller

Dependencies: WP15, WP2.

- [x] Implement §23.4 (`controller::evaluate_pool`) and extend the decision
  event: `current_workers` is owned, `desired_workers` is the elastic target,
  and `unowned_workers`/`desired_total` appear when a floor exists.
- [x] Property test: `desired_elastic` never exceeds `max_workers`, and a
  floor at or above target yields `min_workers`.

#### WP17: per-pool accounts

Dependencies: WP15, WP16, §7.3.

- [x] Account config and systemd unit for `codex` and `claude-zai`, observe-only
  first, running alongside `claude-anthropic` in separate processes with the
  preflight (§10) passing for every pair (`examples/elastic-*.yaml`,
  `tests/fixtures/controller-ownership/elastic-tier-observe-only.json`).
- [x] `subgov doctor` passes the NEEDLE adapter parity check for each adapter.

#### WP18: staged elastic rollout

Dependencies: WP17, WP8.

- [ ] Observe-only run per account across one quota reset, then actuation on
  one account at a time, with the roster unchanged throughout.
- [ ] Definition of done: all three accounts actuate for seven consecutive
  days with no roster unit touched, no duplicate claim, and no account over
  its configured ceiling.

### 23.7 Acceptance criteria

- [ ] Starting or stopping elastic workers leaves every roster unit's state
  and PID unchanged (checked from `systemctl` before and after).
- [ ] An account without `identifier_prefix` is regression-tested unchanged.
- [ ] The cgov/subgov preflight reports zero actuation-capable owners for any
  pool other than the one subgov governs.

## 24. Provider Backpressure Input (Z.AI 429)

Status: plan locked 2026-10-11; nothing below is implemented yet. This is the
explicit plan revision §21 requires for a normalized-contract change, and it
resolves the 429 exclusion recorded in §7.3 and §23.5.

### 24.1 Purpose and scope

Quota windows say how much budget is left; they do not say whether the
provider is currently refusing work. Z.AI throttles with 429 while quota
remains, and the zai-proxy already absorbs most of that with retries and its
own adaptive ceiling. This input gives the governor a worker-count brake for
the elastic tier (§23) when upstream refusals persist. It is optional per
account, ships first for `claude-zai`, and is a hardening layer, not a
precondition for the §23 rollout (`claude-zai` actuation stays at
`max_workers <= 2` until this input is live and validated).

Out of scope: per-request throttling (the proxy's job), prompt or task
scheduling (§3), and any change to roster workers.

### 24.2 Signal definition (locked)

- The signal is the **upstream 429 fraction** over a trailing window:
  `upstream_429 / attempts`, where `upstream_429` is the increase of the
  proxy's `zai_proxy_upstream_errors_total{error_type="429"}` and `attempts`
  is the increase of `zai_proxy_requests_total` (all statuses), minus the
  increase of `zai_proxy_rate_limit_rejections_total`, plus the increase of
  `zai_proxy_retry_attempts_total{reason="retry"}`. The request counter
  includes local capacity-gate 503s, while only the `reason="retry"` series
  records an additional upstream attempt; other retry-counter reason series
  record the triggering failure and must not be added to the denominator.
- The proxy's own capacity-gate rejections are excluded. They are local
  admission, not provider pressure.
- Client-visible 429s (`requests_total{status_code="429"}`) are not used: the
  proxy hides most upstream 429s behind retries, so they understate pressure.
- A sample with fewer than `min_attempts` (default 30) attempts carries no
  fraction; it is "insufficient", not zero.

### 24.3 Contract extension (locked)

The normalized snapshot (§6) gains an optional top-level object:

```json
"backpressure": {
  "upstream_429_fraction": 0.31,
  "attempts": 412,
  "window_seconds": 300
}
```

- `upstream_429_fraction` is in `[0, 1]` and is omitted when attempts are
  insufficient; the object may then carry only `attempts` and `window_seconds`.
- Absence of the whole object means "no signal". Absence never means "no
  pressure".
- Older readers ignore it (the structs are open), and an account without
  `backpressure` policy ignores it.

### 24.4 Controller behaviour (locked)

Per account with a `utilization.backpressure` policy:

| Setting | Default | Meaning |
|---|---|---|
| `hold_above` | 0.20 | enter HOLD: no elastic scale-up |
| `step_down_above` | 0.50 | enter STEP_DOWN: drop one elastic worker per cycle |
| `recover_below` | 0.10 | a quiet cycle |
| `quiet_cycles` | 3 | consecutive quiet cycles to return to NORMAL |
| `signal_loss_release_seconds` | 1800 | release a non-NORMAL state if no usable sample arrives |

- States are NORMAL, HOLD, STEP_DOWN, persisted in account state (§10).
  Transitions: NORMAL to HOLD above `hold_above`; HOLD to STEP_DOWN above
  `step_down_above`; STEP_DOWN to HOLD and HOLD to NORMAL after
  `quiet_cycles` consecutive samples below `recover_below`.
- The brake can only lower `desired_workers` (elastic terms, §23.4):
  HOLD caps it at the current owned count; STEP_DOWN caps it at the current
  owned count minus one, floored at `min_workers`. It never raises, never
  stops unowned workers, and composes after the quota controller with a
  minimum.
- A missing, stale or insufficient sample is no signal: it does not enter a
  state and does not count as a quiet cycle. A non-NORMAL state with no usable
  sample for `signal_loss_release_seconds` releases to NORMAL and emits an
  event, so a dead collector cannot freeze the fleet.
- The decision event records the backpressure state, the fraction, and
  whether the brake changed the result.

### 24.5 Collector requirements (locked)

The site-local Z.AI collector (§7.3) derives the fraction from the proxy's
counters. Those are cumulative and per replica, behind a load-balanced route:

- The collector keeps a small state file of the last reading per replica,
  keyed by the replica's `process_start_time_seconds`, and computes per-replica
  deltas, then sums them. A restart (new start time, or a counter that went
  down) discards that replica's baseline instead of producing a negative delta.
- Each run scrapes the route several times to see both replicas; a replica seen
  for the first time contributes no delta until its next reading.
- Only readings at least 60 seconds apart contribute, and the state file is
  written atomically and holds no credentials.

### 24.6 Work packages

#### WP19: contract and schema

Dependencies: WP1.

- [ ] Add the optional `backpressure` object to the quota snapshot model, the
  published JSON Schema and the collector fixtures, with validation of the
  fraction range and a test that older payloads still parse.

#### WP20: collector deltas

Dependencies: WP19, §7.3.

- [x] Implement §24.2 and §24.5 in the site-local collector, with fixture
  tests for a replica restart, a first-seen replica, counter reset and an
  insufficient sample.
- [x] Check the denominator against real proxy data before trusting it: the
  fraction must stay in `[0, 1]` and move with observed throttling. The live
  replicas' cumulative counters yielded 1,256,217 upstream attempts and 671,797
  upstream 429s (fraction 0.534778); synthetic low/high samples yielded 0.02
  and 0.32. Two live 60+ second windows with no new attempts correctly omitted
  the fraction.

#### WP21: controller state machine

Dependencies: WP19, WP16.

- [x] Implement §24.4 with persisted state, the event fields, and config
  validation (thresholds ordered, `quiet_cycles >= 1`).
- [x] Property tests: the brake never raises `desired_workers`, never goes below
  `min_workers`, and a run of no-signal samples releases within
  `signal_loss_release_seconds`.

#### WP22: observe-only validation and enablement

Dependencies: WP20, WP21, WP17.

- [ ] Run observe-only against the live proxy across at least one real
  throttling episode and record the fraction against the brake's would-be
  decisions.
- [ ] Enable the policy on `claude-zai` only after the recorded episode shows
  no flapping (no more than one state change per `quiet_cycles` cycles).

### 24.7 Acceptance criteria

- [ ] An account without a `backpressure` policy is byte-for-byte unchanged.
- [ ] A snapshot without the `backpressure` object never enters or advances a
  brake state.
- [ ] The brake is provably monotone: it only ever lowers the elastic target.
- [ ] No roster unit is touched by a backpressure transition.

## 25. Opt-in Z.ai saturation adapter (authorized 2026-10-12)

The user authorized an additive adapter policy that keeps request concurrency
productive with the smallest useful NEEDLE fleet. Historical worker estimates
and proxy slot counts are not controller constants. This scope includes stable
full-replica completed-request telemetry, classified provider-1302 evidence when
available, a bounded throughput search with persisted experiments, conservative
quota/backlog/settlement gates, strict config/schema compatibility and focused
regression tests. See [the policy contract](../zai-saturation.md) and
[the opt-in example](../../examples/zai-saturation.yaml).

The rollout gate is separate: install and inspect observe-only decisions with
isolated state before considering production actuation. Existing §24/WP22
`NORMAL -> HOLD -> STEP_DOWN` transition-rate validation remains deferred under
`subgov-d80695e2`; this additive policy does not change or satisfy that gate.
No production policy is enabled by the example or implementation.

Superseded as the end state (2026-10-22): §26.2 reconciles this managed
roster with §23 — the roster's 18 units plus adopted stragglers become the
Z.AI pool's `takeover` list, `sg-zai-` elastic workers replace them, and the
idle-only takeover helper retires each listed worker once a prefixed
replacement runs. The saturation policy itself (this section) still governs
the pool's pacing.

## 26. Whole-fleet management (decided 2026-10-22)

Goal: every NEEDLE worker on codinghome is launched and retired by subgov in
response to provider capacity and real work. The static roster is a transition
state, not a floor. Decisions below are locked; no open forks.

**26.1 Scope.** All workers run on this host. Inventory on 2026-10-22: ten
`needle-worker@glm-*` systemd units (already enumerated by the Z.AI roster, §25),
`sg-claude-anthropic-N`, `sg-codex-N`, and stragglers started outside systemd
(for example a `needle-stable` tmux worker). Out of scope: interactive
Claude/Codex herdr sessions, bare NATO tmux sessions, and coordinators that are
not NEEDLE workers (`options-continuous-research`).

**26.2 Takeover supersedes the "never touch the roster" floor of §23.** The
floor shrinks to zero. Each pool declares a `takeover` list of roster units. A
roster unit is retired (idle-only: pidfd, SIGSTOP, children check ignoring
zombies, SIGTERM; never `needle cleanup`) only when a subgov-prefixed
replacement in the same pool is running. When the list is empty the pool is
fully elastic. The §25 Z.AI managed roster is the transitional form of this for
the Z.AI pool. Cutover is stopping the fixed workers directly; there is no
shadow mode.

**26.3 Pools.** Claude (`sg-claude-anthropic-`), Codex (`sg-codex-`), Z.AI
(`sg-zai-`). Every worker belongs to exactly one pool; a worker on any other
backend needs an adapter (§27) or an explicit exclusion.

**26.4 Demand is ready beads.** NEEDLE workers consume beads, so the demand
signal is the count of ready beads per pool, not a hand-curated `work` list.
Ready means unblocked, unclaimed, in a workspace the pool's agent may work, and
not held by `proposal-pending`, `human` or a recorded external hold. Spare
quota alone never launches a worker. `eligible_backlog_capacity` is derived from
this count (one worker per workspace for build-heavy repos, bounded by
`max_workers`).

**26.5 Placement is NEEDLE's.** subgov chooses how many workers, not what they
work on. `needle run -w <repo>` drains that repo and then roams, and a bare
launch is forbidden, so subgov seeds each new worker with the ready workspace
having the fewest running workers and lets NEEDLE's own roaming take over. No
pinning, no repo-to-worker mapping.

**26.6 The ceiling is measured, not configured.** Each cycle computes a host
worker ceiling as the minimum of: `needle.slice` memory headroom divided by the
observed p95 per-worker RSS, `MemAvailable` headroom the same way, and a PSI
(memory/io `avg10`) and load brake that stops scale-up under pressure and steps
down when it persists. It reuses the observed RAM/CPU admission approach from
shephrd. Pool `max_workers` remains a quota-side cap. When the sum of pool
targets exceeds the ceiling, the shortfall is split in proportion to each
pool's ready demand, and scale-down takes idle workers first.

**26.7 Work packages.**
- WP23 ready-bead demand adapter (all pools) replacing hand-curated `work`.
- WP24 measured host ceiling and cross-pool arbitration.
- WP25 takeover: `sg-zai-` elastic workers, roster retirement, reconcile §25
  with §23, and straggler adoption (delivered 2026-10-22, `subgov-f602b666`:
  `fleet.takeover` lists worker_id/agent/unit targets that leave the §23
  floor; after each cycle's elastic reconcile the `takeover_helper` runs one
  idle-only retirement, gated on fresh prefixed heartbeats;
  `scripts/zai-takeover.py` verifies identity, idleness, children, and unit
  MainPID before pidfd/SIGSTOP/SIGTERM — never `needle cleanup` — and
  records each retirement durably at `--state`; `--dry-run` rehearses the
  read-only gates against the live roster;
  `examples/takeover-claude-zai.yaml`, the deployed
  `deploy/codinghome/zai-takeover-governor.yaml`, and
  `deploy/codinghome/zai-takeover-manifest.json` carry the cutover config).
- WP26 seed-workspace selection.
- Already tracked: one binary and one adapter installer (done 2026-10-22, all
  three services on `~/.local/lib/subgov/current`), backpressure live gate
  `subgov-d80695e2`, NEEDLE zombie reaping `needle-064b92ef`.

## 27. Adapter protocol (stub)

One core binary plus out-of-process provider adapters over the existing JSON
contract (§7.3). To be specified after WP23-26 land; the Claude, Codex and Z.AI
scripts are the first conformance targets.
