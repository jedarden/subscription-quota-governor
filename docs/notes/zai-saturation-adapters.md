# Z.ai saturation command adapters

These stdlib Python adapters use the existing `command` source, observer and
actuator contracts. Install `zai-collector.py`, `zai-fleet.py` and
`subgov_adapter_common.py` together. Credentials do not belong in their argv,
roster, baseline, or governor configuration. The scripts emit generic diagnostics
without HTTP bodies, subprocess stderr, credential values, or exception messages.

## Collector

Use one explicit **stable replica** metrics URL for every replica:

```sh
python3 scripts/zai-collector.py --state /persistent/subgov/zai-counters.json \
  --metrics-url http://replica-one:8080/metrics \
  --metrics-url http://replica-two:8080/metrics
```

Alternatively, `--kube-api http://readonly-api:8001 --namespace mcp
--selector app=zai-proxy` discovers pods by UID, reads each pod's port-8080 metrics
proxy, and checks membership again afterward. This requires read-only pod-list
and pod-proxy access. It issues no Kubernetes mutations.

For sites with read-only pod-list access but no pod-proxy permission, bounded
load-balanced coverage is available:

```sh
python3 scripts/zai-collector.py --state /persistent/subgov/zai-counters.json \
  --metrics-url https://site-local-proxy/metrics --expected-replicas 2 \
  --membership-api http://readonly-api:8001 --namespace mcp --selector app=zai-proxy
```

This mode makes six concurrent scrapes for two expected replicas, deduplicates by
`process_start_time_seconds`, and requires exactly two distinct process identities
plus the same complete healthy pod-UID membership before and after collection.
The previous baseline must have the same identities and UID membership. Failure
to reach both replicas withholds saturation; repeated hits on one process never
become a pool total. Process start values must uniquely identify the expected
replicas; collisions fail the coverage check. Configure expected membership from
the actual deployment, including all traffic-serving replicas.

Up to eight concurrent replicas/scrapes are supported. Metrics HTTP operations
have eight-second socket timeouts; discovery has two-second timeouts. A hard
12-second overall deadline terminates stuck requests before subgov's 15-second
command limit. The site-local TLS exception is explicit `--insecure-tls`;
verification is otherwise enabled. Redirects and URLs with credentials, queries,
or fragments are rejected.

Quota normalization preserves the existing site-local rules: a lapsed five-hour
window is projected forward 300 minutes **only when usage is zero**. Nonempty
lapsed windows remain lapsed. Poller sample age determines `observed_at` and
`fresh`, with a default 900-second age limit.

Saturation requires all replicas to have fresh quota, supported counters, stable
process generations, nondecreasing counters, and a shared interval between 60 and
900 seconds. Partial coverage, disappearance, restart, reset or expired baselines
clear the whole-pool baseline. Short intervals retain their older baseline;
intervals already emitted cannot be reused. Baselines are locked nonblocking,
atomically replaced, and mode 0600.

`completed_requests` counts successful 2xx POST completions on `/v1/messages` or
`/messages` using `zai_proxy_request_duration_seconds_count`. Attempts are the
delta of matching generation `zai_proxy_requests_total`, minus local capacity
rejections, plus only retry series with `reason="retry"`. GET model listings,
count-token calls, and other probes do not contribute generation counts.
An unused Prometheus capacity-rejection CounterVec may omit its entire family.
The collector accepts that absence as zero only when all matching lifetime
generation request series have valid status labels and their status-503 total
is zero. The proxy's local admission gate always increments a generation 503
alongside its capacity-rejection counter (`proxy/handler.go`). Any generation
503 or unknown status without the capacity family withholds saturation; it does
not assume that an absent metric universally means zero.
`upstream_concurrency_fraction` counts the initialized
`zai_proxy_upstream_concurrency_rejections_total` counter over attempts, falling
back to `zai_proxy_zai_errors_total{code="1302"}` on compatible older proxies.
When both exist, the dedicated counter wins; errors are never counted twice.
Its initialized family also identifies the proxy contract in which an unused
retry CounterVec is omitted until the first retry; that absence means zero
retries. Older proxies still need explicit retry-family support.
Generic 429 errors and retry-trigger reason series do not substitute
for classification or retries. Fewer than 30 attempts produce a null fraction.
An absent classification family also produces null, even if other 429 counters
exist. A declared classification metric family is evidence that absent 1302
series means zero; absence of the entire family is unknown pressure.

`--backlog-capacity-file PATH` optionally reads a trusted, timestamped observation:

```json
{"eligible_backlog_capacity": 3, "observed_at": "2026-10-12T12:00:00Z"}
```

The collector omits capacity when that observation is stale or future dated.
There is no inferred backlog, provider concurrency limit, or worker duty factor.
The proxy's per-replica `max_workers` is a local gate, not provider capacity.

At the initial deployment investigation, pod metrics proxy access returned 403,
Prometheus held no proxy series, and the live proxy did not expose the 1302
classification family. LB coverage plus read-only pod membership can still supply
completion throughput; provider concurrency pressure remains unknown until the
proxy publishes classified counters. Neither adapter changes proxy deployment.

## Explicit systemd fleet

The fleet adapter accepts a credential-free roster, with exact unit and heartbeat
identities:

```json
{
  "version": 1,
  "owner": "subgov-claude-zai",
  "scope": "user",
  "workers": [
    {
      "unit": "needle-worker@glm-example.service",
      "agent": "claude-code-glm-5.3-flash",
      "worker_id": "glm-example"
    }
  ]
}
```

Only listed GLM identities are admitted. Duplicate units/worker IDs, unrelated
adapters, inconsistent unit names, mismatched routing/heartbeats/process argv,
and existing overrides owned by another controller fail before mutation. State
pins owner and roster hash; changing the admitted roster requires an explicit
migration of that state. The unit's routing file must select the listed
`NEEDLE_AGENT`. Active MainPID must be `needle` or `needle-stable run` with the
exact agent and identifier, and its heartbeat PID/model/qualified ID must agree.
Stale or malformed active-worker evidence fails observation rather than reporting
an invented zero. Workers outside the roster are never started, stopped, masked,
or counted by this adapter; account-level policy must account for any unowned
workers separately.

Configure the governor's command observer with:

```yaml
argv: [python3, /installed/zai-fleet.py, --roster, /persistent/zai-roster.json,
       --state, /persistent/zai-fleet.json, observe]
```

The actuator uses the same prefix followed by `actuate, '{desired_workers}'`.
`observe` returns `{"current_workers": N}` without service changes. Current
includes pending retirees until their MainPID has actually stopped, ensuring a
subsequent governor cycle still sees the unresolved scale-down difference.

Scale-up selects only unmasked inactive admitted units. It never invokes unmask.
Scale-down first persists desired count and pending retirees, then creates an
owned unit drop-in and persistent retirement marker. `Restart=no` prevents an
active retiring supervisor from restarting, and a negated `ConditionPathExists`
prevents that unit from starting after reboot while the marker exists. These
files are installed and daemon-reloaded before any stop. Busy workers retain
their current task; later actuator invocations retry safe draining.

An idle candidate requires an empty task/bead and a fresh idle/exhausted heartbeat.
`ADMISSION_BLOCKED` is also a candidate when its bead and task are empty. The
adapter pins the supervisor with a pidfd, briefly sends SIGSTOP, verifies every
thread is stopped and every `/proc/<pid>/task/*/children` file is empty, then
rechecks heartbeat and MainPID identity before `systemctl stop --no-block`.
SIGCONT runs in `finally`, including errors or a deadline. It does not stop a
worker with any child process, run NEEDLE cleanup, or kill tmux sessions.

Use persistent state on the same host as the service manager. Default paths are
user-scope `~/.config/systemd/user`, `~/.config/needle/workers` and
`~/.needle/state/heartbeats`. System scope requires explicit
`--unit-dir /etc/systemd/system` and adequate service-manager authority. State
paths cannot contain whitespace, percent signs or backslashes because they are
written literally into systemd conditions. Serialize all governors claiming this
roster with the same state/owner; exact owned-file content detects overlapping
controllers using different state locations. Other controllers must not target
these units.

Increasing desired count reverses this adapter's own retirement files and starts
permitted inactive units. Operator masks remain untouched. For full rollback,
stop the governor, preserve the fleet JSON, remove only matching owned
`90-subgov-zai.conf` files and their recorded `.retired/<unit>` markers, and run
`systemctl --user daemon-reload` (or the selected system manager). This restores
the preexisting restart policy; it does not automatically start any workers.

## Verification

`python3 -B -m unittest discover -s tests/python -p 'test_zai_*.py'` exercises
synthetic full-pool deltas, changing/restarted replicas, pressure classification,
freshness, state locking, idle/busy draining, process children, ownership,
operator masks and restoration. It never calls a live endpoint or service.
`tests/zai_adapter_scripts.rs` runs that suite within the normal `cargo test` gate
with an isolated HOME. Live verification can run only `observe`; activation and
service mutation belong to the explicit rollout step.

## Production fixed-fleet handoff — 2026-10-13

The user authorized direct verification with running workers and replacement of
fixed Z.AI capacity with subgov management, without an observe-only validation
phase. This supersedes the earlier roster-immutability prerequisite for the
explicit Z.AI roster only. Other providers and operator masks remain outside
this handoff. Owner: `subgov-77416d49`; classified proxy telemetry dependency:
`zaiproxy-f82552f3` in the zai-proxy repository.

On codinghome, `subgov-claude-zai.service` uses
`~/.config/subgov/claude-zai-managed.yaml` and the 18 identities in
`zai-saturation-roster.json`. Fixed boot enablement was removed without restarting
those workers. The same governor service now owns their command observer and
actuator; it retains the 18-worker ceiling and one-worker per-cycle limits.
Retirement guards prevent an external fixed-fleet start from reviving a retired
identity. The two legacy `sg-zai` tmux workers were retired only after verifying
idle, task-free supervisor identities; neither is a second actuation pool.

Fleet commands run through `systemd-run --user --wait --pipe --collect --quiet
--property=NoNewPrivileges=yes` with the absolute Python executable and the
normal `zai-fleet.py` arguments. These helpers have an independent process view:
the governor's hardened mount namespace cannot inspect worker PIDs directly.
Keep all other governor restrictions and `KillMode=process` intact.

Full-replica network sampling exceeded the direct command source's 15-second
budget on this host. `zai-source-refresh.py --settings PATH` instead publishes
an atomic `normalized_file` snapshot from a separate systemd service/timer.
The collector's default deadline remains 12 seconds; this asynchronous caller
uses `--deadline-seconds 45`. The timer waits 300 seconds after each refresh
finishes, so intervals remain at least 300 seconds and a completed throughput
sample is not overwritten by an intermediate quota-only refresh. Runtime freshness expires at
900 seconds. Failed collection preserves the prior snapshot and cannot make it
fresh. No collector credentials belong in settings or command arguments.

The settings JSON contains absolute `collector`, `collector_args`, `snapshot`,
`roster`, `routing_dir`, `heartbeat_dir` paths and `host`. Deployed settings are
`~/.config/subgov/zai-source-refresh.json`; user unit templates are under
`deploy/systemd/subgov-zai-source.*`. The publisher queries the bead-rs ready
frontier only in explicitly routed workspaces, respects queue host ownership,
excludes held/human/proposal work, and includes verified busy claims once.
Ambiguous policy YAML, backend errors or failed demand queries withhold capacity
instead of inventing zero demand. This conservative frontier does not infer
additional eligible work from other roaming destinations.

Before changing ownership, retain unit enablement, MainPIDs, configs and exact
drop-ins. The handoff backup and dated live evidence are in
`/home/coding/scratch/subgov-77416d49-cutover/`. For rollback, stop the Z.AI
governor, remove only its matching retirement files as described above, restore
the backed-up service configuration and prior enablement states, and reload the
user manager. Do not unmask excluded workers or restore all units indiscriminately.
