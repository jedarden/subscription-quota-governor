"""Read-only NEEDLE ready-bead demand, shared by all provider pools.

Native bead-rs owns dependency readiness; this module adds NEEDLE's dispatch
holds, pool/workspace boundaries and dispatch health. A route contributes
launch slots only when NEEDLE's own claim path would admit it: an explicit
authoritative `bead_cli.backend` binding (`bead-rs`/`bead`/`br`) plus an
aligned, published checkpoint (`bead sync status --format json`, the same
contract NEEDLE's sync_guard reads) with no other worker holding the sync
lock past the probe budget. A failed query or failed health observation
marks only that workspace unknown; demand goes unknown (never zero) only
when every selected workspace fails.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import time

from subgov_adapter_common import read_json

LIMIT = 999999
DEFAULT_READY_TIMEOUT = 8
DEFAULT_HEALTH_TIMEOUT = 30
# Version of the dispatch-health observation contract this module emits in
# `dispatch_health_version`: mirror of NEEDLE bead_store::sync_guard's
# admission predicate over `bead sync status --format json` (bead-rs), plus
# the authoritative backend binding from `.needle.yaml` and the
# needle-sync.lock busy probe. Bump when the mirrored contract changes.
DISPATCH_HEALTH_VERSION = 1
# bead-rs sync relationships NEEDLE repairs itself (flush-only/reconcile)
# before deciding admission: a read-only probe cannot know whether that
# repair would succeed, so these stay unknown rather than refusals.
# `covered-ahead-integrity-failure` — and anything else — is not a remedy
# state; NEEDLE hard-bails on it, so it is a verified refusal.
REPAIRABLE_RELATIONSHIPS = frozenset({'behind', 'absent', 'remote-advanced'})
READY_QUERY_ERRORS = (subprocess.TimeoutExpired, subprocess.CalledProcessError,
                      ValueError, OSError)
BUILD_MARKERS = ('Cargo.toml', 'go.mod', 'CMakeLists.txt', 'pom.xml', 'build.gradle',
                 'build.gradle.kts', 'Makefile', 'package.json')


def scalar(value):
    value = value.strip()
    if value.startswith(('"', '[')):
        try:
            return json.loads(value)
        except ValueError:
            # YAML flow sequence of plain words, e.g. [deferred, human].
            body = value.split('#', 1)[0].strip()
            if body.startswith('[') and body.endswith(']'):
                items = [item.strip() for item in body[1:-1].split(',') if item.strip()]
                if all(re.fullmatch(r'[A-Za-z0-9_./~:@+=-]+', item) for item in items):
                    return items
            raise
    if value.startswith("'"):
        if not value.endswith("'"):
            raise ValueError('invalid quoted policy scalar')
        return value[1:-1].replace("''", "'")
    value = value.split('#', 1)[0].strip()
    if value in ('true', 'false'):
        return value == 'true'
    if value.isdigit():
        return int(value)
    if not value or not re.fullmatch(r'[A-Za-z0-9_./~:@+=-]+', value):
        raise ValueError('complex policy scalar')
    return value


def policy_fields(path, fields):
    """Read selected scalar/list paths without interpreting arbitrary YAML.

    Reject complex or duplicate relevant policy instead of silently widening
    access. Unrelated sections (including prompt templates) are not evaluated.
    Supports NEEDLE's block and indentless lists and JSON-style inline lists.
    """
    result, stack, seen = {}, [], set()
    ancestors = {key[:n] for key in fields for n in range(1, len(key) + 1)}
    for raw in Path(path).read_text().splitlines():
        line = raw.lstrip(' ')
        if not line or line.startswith('#'):
            continue
        indent = len(raw) - len(line)
        if line.startswith('\t'):
            raise ValueError('tab-indented policy')
        if line.startswith('- '):
            if stack and stack[-1][1] in fields and indent >= stack[-1][0]:
                key = stack[-1][1]
                if not isinstance(result.get(key), list):
                    raise ValueError('policy is not a list')
                result[key].append(scalar(line[2:]))
            continue
        while stack and stack[-1][0] >= indent:
            stack.pop()
        match = re.match(r'([A-Za-z_][A-Za-z0-9_-]*)\s*:(?:\s+(.*)|\s*)$', line)
        if not match:
            quoted = re.match(r"[\"']([A-Za-z_][A-Za-z0-9_-]*)[\"']\s*:", line)
            quoted_key = (stack[-1][1] if stack else ()) + ((quoted[1],) if quoted else ())
            if ((quoted and quoted_key in ancestors)
                    or (line.startswith('<<:') and (not stack or stack[-1][1] in ancestors))):
                raise ValueError('quoted or merged policy key')
            if stack and stack[-1][1] in ancestors:
                raise ValueError('complex policy mapping')
            continue
        key = (stack[-1][1] if stack else ()) + (match[1],)
        value = (match[2] or '').strip()
        if key in ancestors:
            if key in seen:
                raise ValueError('duplicate policy key')
            seen.add(key)
            if value and not value.startswith('#'):
                if key not in fields:
                    raise ValueError('complex policy section')
                result[key] = scalar(value)
            elif key in fields:
                result[key] = []
        stack.append((indent, key))
    return result


def workspace_policy(path):
    values = policy_fields(path, {('bead_cli', 'backend'), ('queue', 'owner_host'),
                                  ('workspace', 'max_workers'), ('strands', 'pluck', 'exclude_labels')})
    # The backend is deliberately not defaulted here: NEEDLE dispatch requires
    # an explicit authoritative binding (binding_refusal), so an absent key
    # must stay distinguishable from a written `bead-rs`.
    return {'bead_cli': {'backend': values.get(('bead_cli', 'backend'))},
            'queue': {'owner_host': values.get(('queue', 'owner_host'))},
            'max_workers': values.get(('workspace', 'max_workers')),
            **({'exclude_labels': values[('strands', 'pluck', 'exclude_labels')]}
               if ('strands', 'pluck', 'exclude_labels') in values else {})}


def binding_refusal(route, policy):
    """Classify the route's bead backend binding the way NEEDLE dispatch does.

    NEEDLE's open_configured (src/bead_store/mod.rs) bails with "no
    authoritative bead backend binding" when bead_cli.backend resolves to
    `auto` — which is exactly what a missing `.needle.yaml` or an absent key
    resolves to — and config resolution (src/config/mod.rs) refuses the retired
    bead-forge names with a migration hint before backend resolution. The
    dispatchable spellings are exactly NEEDLE's BeadBackend::Bead serde
    spellings: `bead-rs`, its `bead` alias, and the deprecated `br` alias
    (which resolve_bead_cli maps to the same `bead` binary). Returns the
    refusal reason, or None when bound.
    """
    if not (Path(route) / '.needle.yaml').exists():
        return 'backend_binding_missing'
    backend = (policy or {}).get('bead_cli', {}).get('backend')
    if backend in (None, 'auto'):
        return 'backend_binding_missing'
    if backend in ('bf', 'bead-forge'):
        return 'backend_binding_retired'
    if backend not in ('bead-rs', 'bead', 'br'):
        return 'backend_binding_invalid'
    return None


def route_policy(route):
    route = Path(route)
    return (workspace_policy(route / '.needle.yaml')
            if (route / '.needle.yaml').exists() else {'bead_cli': {}, 'queue': {}})


def policy_ok(route, host):
    route = Path(route)
    if not (route / '.beads/config.json').is_file():
        return False
    policy = route_policy(route)
    if binding_refusal(route, policy):
        return False
    return policy.get('queue', {}).get('owner_host') in (None, host)


def route_block_reason(route, host):
    """Name why policy_ok refused a route, mirroring NEEDLE's refusal classes.

    Binding authority is decided first because NEEDLE refuses it before the
    queue-owner check in its own claim path.
    """
    route = Path(route)
    if not (route / '.beads/config.json').is_file():
        return 'backend_binding_missing'
    try:
        return binding_refusal(route, route_policy(route)) or 'queue_owner_mismatch'
    except (ValueError, OSError):
        return 'policy_unreadable'


def gate_degraded(settings, route):
    if not settings.get('gate_health_dir'):
        return None
    key = hashlib.sha256(str(Path(route).resolve()).encode()).hexdigest()[:12]
    try:
        value = read_json(Path(settings['gate_health_dir']) / (key + '.json'), {})
    except (json.JSONDecodeError, UnicodeError, OSError):
        return 'gate_state_unreadable'
    if not isinstance(value, dict):
        return 'gate_state_unreadable'
    return 'workspace_gate_degraded' if value.get('degraded', False) is not False else None


def summarize_reasons(reasons, budget=240):
    """Bound the status payload's reason text for the blocked record."""
    return '; '.join(str(reason) for reason in (reasons or [])[:4])[:budget]


def sync_lock_busy(route, budget=DEFAULT_HEALTH_TIMEOUT, poll=0.25):
    """Read-only mirror of the needle-sync.lock guard (NEEDLE sync_guard).

    bead-rs workers hold this exclusive flock only for a flush-duration while
    publishing checkpoints; NEEDLE waits out the same lock for its full sync
    budget and refuses dispatch (`workspace_sync_busy`) only when it stays
    held past that deadline — a transiently-held lock is claimable, not
    refused. This probe applies the same discipline without writing: poll the
    exclusive flock for up to `budget` seconds and count only a lock that
    never frees as busy. The probe never creates the file — a missing lock
    simply means no synchronized writer has ever run here — and never writes
    to it. Re-opening on each poll keeps the wait honest if the file is
    replaced mid-probe.
    """
    deadline = time.monotonic() + max(0, budget)
    while True:
        try:
            handle = os.open(Path(route) / '.beads/needle-sync.lock', os.O_RDONLY)
        except FileNotFoundError:
            return False
        try:
            fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
            return False  # acquired: nobody holds it; closing releases our probe
        except BlockingIOError:
            pass
        finally:
            os.close(handle)
        if time.monotonic() >= deadline:
            return True
        time.sleep(poll)


def sync_status(route, timeout=DEFAULT_HEALTH_TIMEOUT):
    """Read the checkpoint contract NEEDLE's sync_guard reads before any claim."""
    result = subprocess.run(['bead', 'sync', 'status', '--format', 'json'],
                            cwd=route, text=True, capture_output=True, timeout=timeout, check=True)
    status = json.loads(result.stdout)
    if not isinstance(status, dict):
        raise ValueError('invalid sync status contract')
    if not isinstance(status.get('relationship'), str) or not isinstance(status.get('ready_to_commit'), bool):
        raise ValueError('incomplete sync status contract')
    consistent = status.get('checkpoint_consistent')
    if consistent is not None and not isinstance(consistent, bool):
        raise ValueError('invalid checkpoint_consistent field')
    return status


def dispatch_health(route, timeout=DEFAULT_HEALTH_TIMEOUT):
    """Classify one route's dispatch admission the way NEEDLE's claim path does.

    Returns {'state': 'admitted'} when a worker could claim here now,
    {'state': 'refused', 'reason', ...} for a verified refusal, or
    {'state': 'unknown', 'reason': 'dispatch_health_unavailable', ...} when the
    observation itself failed. Unknown is never treated as claimable and never
    as verified-empty work.

    Admission mirrors NEEDLE bead_store::sync_guard arm for arm, in its
    order: first the needle-sync lock (waited out for the probe budget,
    exactly the deadline NEEDLE gives a flushing worker before refusing
    `workspace_sync_busy`), then — once no publication is in flight — the
    status payload's own predicate: relationship `aligned` AND publication
    consistent (`checkpoint_consistent`, falling back to `ready_to_commit`
    on bead-rs builds predating the split). Secret quarantine, incomplete
    retained-generation coverage and uncommitted checkpoint files surface
    through the same status payload as publication failures and are verified
    refusals (`checkpoint_publication_unverified`), as is any relationship
    NEEDLE hard-bails on (`checkpoint_not_aligned`). The repairable
    relationships — `behind`, `absent`, `remote-advanced` — are different:
    NEEDLE repairs them under its own budget at claim time and refuses only
    when that repair fails, which a read-only observation cannot predict, so
    they are recorded as unknown (`checkpoint_repair_pending`): never
    counted demand, and never a verified-empty claim either.
    """
    try:
        if sync_lock_busy(route, timeout):
            return {'state': 'refused', 'reason': 'workspace_sync_busy'}
        status = sync_status(route, timeout)
    except READY_QUERY_ERRORS as error:
        return {'state': 'unknown', 'reason': 'dispatch_health_unavailable',
                'error': type(error).__name__, 'exception': error}
    relationship = status['relationship']
    if relationship != 'aligned':
        if relationship in REPAIRABLE_RELATIONSHIPS:
            return {'state': 'unknown', 'reason': 'checkpoint_repair_pending',
                    'detail': relationship,
                    'exception': ValueError(f'checkpoint repair pending: {relationship}')}
        return {'state': 'refused', 'reason': 'checkpoint_not_aligned',
                'detail': relationship}
    consistent = status.get('checkpoint_consistent')
    published = status['ready_to_commit'] if consistent is None else consistent
    if not published:
        return {'state': 'refused', 'reason': 'checkpoint_publication_unverified',
                'detail': summarize_reasons(status.get('not_ready_reasons'))}
    return {'state': 'admitted'}


def discover(root):
    """Mirror NEEDLE Explore: real directories, no hidden/build descendants."""
    pending = [Path(root).expanduser().resolve()]
    found = set()
    # The root is a scan boundary, not an implicitly authorized workspace.
    while pending:
        directory = pending.pop()
        for child in directory.iterdir():
            if (child.name.startswith('.') or child.name in ('target', 'node_modules')
                    or child.is_symlink() or not child.is_dir()):
                continue
            if (child / '.beads').is_dir():
                found.add(str(child.resolve()))
            pending.append(child)
    return found


def routes(settings):
    config = settings['demand']
    paths = set()
    if 'workspaces' in config:
        paths.update(str(Path(path).expanduser().resolve()) for path in config['workspaces'])
    elif config.get('needle_config'):
        values = policy_fields(Path(config['needle_config']).expanduser(), {
            ('strands', 'explore', 'workspaces'), ('strands', 'explore', 'workspace_root'),
            ('strands', 'explore', 'enabled')})
        if values.get(('strands', 'explore', 'enabled'), True) is True:
            explicit = values.get(('strands', 'explore', 'workspaces'), [])
            if not isinstance(explicit, list) or not all(isinstance(path, str) for path in explicit):
                raise ValueError('invalid NEEDLE workspace list')
            if explicit:
                paths.update(str(Path(path).expanduser().resolve()) for path in explicit)
            else:
                paths.update(discover(values.get(('strands', 'explore', 'workspace_root'), str(Path.home()))))
    else:
        raise ValueError('missing workspace scope')
    paths.update(str(Path(path).expanduser().resolve()) for path in config.get('home_workspaces', []))
    restrictions = {str(Path(path).expanduser().resolve()): agents
                    for path, agents in config.get('workspace_agents', {}).items()}
    return sorted(path for path in paths
                  if path not in restrictions or settings['agent'] in restrictions[path])


def ready_issues(route, timeout=DEFAULT_READY_TIMEOUT):
    result = subprocess.run(['bead', 'list', '--ready', '--json', '--limit', str(LIMIT)],
                            cwd=route, text=True, capture_output=True, timeout=timeout, check=True)
    issues = json.loads(result.stdout)
    if not isinstance(issues, list) or len(issues) >= LIMIT:
        raise ValueError('invalid or truncated native ready array')
    if any(not isinstance(issue, dict) or not isinstance(issue.get('id'), str) for issue in issues):
        raise ValueError('invalid native issue')
    return issues


def query_ready(route, timeout=DEFAULT_READY_TIMEOUT):
    """Run one workspace's ready query, returning the failure instead of raising.

    Unicode decode failures of stdout also arrive as ValueError subclasses, so
    this tuple covers timeouts, nonzero exits, malformed/truncated output and
    unreadable stores alike.
    """
    try:
        return ready_issues(route, timeout)
    except READY_QUERY_ERRORS as error:
        return error


def native_internal_artifact(issue):
    # Match NEEDLE src/internal.rs (75267dbf), shared by Pluck and Explore.
    # bead --ready does not project this additional native dispatch boundary.
    labels = {label.strip().lower() for label in issue.get("labels", [])}
    internal = {"alert", "crash", "starvation", "knot-starvation", "pluck-starvation",
                "gate-broken", "generation-ratio", "pulse-finding", "unravel-proposal",
                "worker-failure", "worker-loop", "infra"}
    if labels & internal or any(label.startswith("fingerprint:") for label in labels):
        return True
    body = (issue.get("description") or "").lower()
    if any(marker in body for marker in ["## agent crash report", "## gate execution error",
            "## worker failure", "## live worker loop detected", "## scanner finding", "## alternative for:"]):
        return True
    if not body.strip():
        return False
    title = (issue.get("title") or "").lower()
    if (any(marker in title for marker in ["starvation alert", "beads invisible to worker",
                                          "open beads exist but pluck found none"])
            and any(marker in body for marker in ["workspace", "open beads", "ready beads", "pluck",
                                                  "candidate", "invisible", "starvation"])):
        return True
    return (any(term in body for term in ["pluck configuration", "pluck config", "exclude_labels",
            "exclude labels", "bead discovery", "needle dispatch", "strand configuration",
            "worker configuration", "bead filtering", "candidate exclusion"])
            or "automated alternative proposal" in body
            or (title.startswith("[unravel]") and "original bead:" in body))


def allowed(issue, now):
    if issue.get("status") != "open" or issue.get("assignee") or issue.get("manual_blocked"):
        return False
    if native_internal_artifact(issue):
        return False
    for raw in issue.get("labels", []):
        label = raw.strip().lower()
        if label in {"human", "human-owned", "owner:human", "proposal-pending", "deferred",
                     "blocked", "manual_blocked", "manual-blocked", "blocked:manual",
                     "external-hold", "external_hold", "waiting-external"} or label.startswith(("human:", "external-hold:", "external_hold:", "waiting-external:")):
            return False
        if label.startswith(("deferred:", "quarantine-until:", "retry-cooldown-until:")):
            try:
                until = datetime.fromisoformat(raw.split(":", 1)[1].replace("Z", "+00:00"))
                if until > now:
                    return False
            except (ValueError, TypeError):
                return False
    return True



def observe(settings, busy=(), unowned_floor=0, lane_label=None):
    """Return ready count plus a bounded total fleet capacity including claims.

    Claims already executing are retention evidence, never ready demand. A
    build-heavy workspace contributes one slot; existing claims occupy it.
    Routes are gated by NEEDLE's own dispatch admission before they count:
    authoritative backend binding, queue ownership, gate health, and — for
    routes with demand — a verified dispatch-health observation
    (`dispatch_health`), so no route NEEDLE would refuse contributes slots.

    When `lane_label` is given, every eligible job is annotated with
    `labelled` -- an exact, case-sensitive match against the bead's labels,
    mirroring NEEDLE Explore's `lane_admits`. A lane-bound worker claims only
    labelled beads, so `labelled: false` demand is invisible to that lane
    until its publisher applies the label (subgov-6db97205).
    """
    now = datetime.now(timezone.utc)
    config = settings['demand']
    if type(settings['max_workers']) is not int or settings['max_workers'] < 0:
        raise ValueError('invalid pool ceiling')
    # Fleet-wide ready queries measured p50 0.06s with a 0.33s worst store
    # (NEEDLE, ~6k beads); 8s only trips on host-wide stalls. Configurable so a
    # slower fleet can raise it without re-measuring.
    timeout = config.get('ready_timeout_seconds', DEFAULT_READY_TIMEOUT)
    if type(timeout) is not int or timeout <= 0:
        raise ValueError('invalid ready query timeout')
    health_timeout = config.get('dispatch_health_timeout_seconds', DEFAULT_HEALTH_TIMEOUT)
    if type(health_timeout) is not int or health_timeout <= 0:
        raise ValueError('invalid dispatch health timeout')
    if any(not item.get('reason') for item in config.get('external_holds', [])):
        raise ValueError('external hold has no reason')
    holds = {(str(Path(item['workspace']).expanduser().resolve()), item['id'])
             for item in config.get('external_holds', [])}
    # Retain legacy recorded holds during settings migration; never use work as
    # an allowlist or require its obsolete revision/admission/lane fields.
    holds.update((str(Path(item['workspace']).resolve()), item['id'])
                 for item in settings.get('work', []) if item.get('external_hold'))
    limits = {str(Path(path).expanduser().resolve()): limit
              for path, limit in config.get('workspace_limits', {}).items()}
    global_excludes = []
    if config.get('needle_config'):
        values = policy_fields(Path(config['needle_config']).expanduser(), {('strands', 'pluck', 'exclude_labels')})
        global_excludes = values.get(('strands', 'pluck', 'exclude_labels'), [])
        if not isinstance(global_excludes, list):
            raise ValueError('invalid global excluded labels')
    blocked = []
    selected = []
    for route in routes(settings):
        try:
            owned = policy_ok(route, settings['host'])
        except (ValueError, OSError):
            # One unreadable workspace policy must never fail the whole fleet.
            blocked.append({'workspace': route, 'reason': 'policy_unreadable'})
            continue
        if not owned:
            # Name the refusal the way NEEDLE's own claim path would report it.
            blocked.append({'workspace': route, 'reason': route_block_reason(route, settings['host'])})
        elif gate_reason := gate_degraded(settings, route):
            blocked.append({'workspace': route, 'reason': gate_reason})
        else:
            selected.append(route)
    ready, seeds, summaries, capacity = [], [], [], unowned_floor
    busy_keys = {(str(Path(job['workspace']).resolve()), job['id']) for job in busy}
    failures, health_unknown, observed = [], [], set()
    with ThreadPoolExecutor(max_workers=4) as pool:
        outcomes = list(pool.map(lambda route: query_ready(route, timeout), selected))
    for route, outcome in zip(selected, outcomes):
        route_path = Path(route)
        if isinstance(outcome, READY_QUERY_ERRORS):
            # One slow or unreadable store must never blank fleet demand: this
            # workspace stays unknown, keeps its busy claims as retention
            # evidence, and contributes no seeds.
            failures.append((route, outcome))
            blocked.append({'workspace': route, 'reason': 'ready_query_unavailable',
                            'error': type(outcome).__name__})
            continue
        observed.add(route)
        policy = workspace_policy(route_path / '.needle.yaml') if (route_path / '.needle.yaml').exists() else {}
        excludes = policy.get('exclude_labels', global_excludes)
        if not isinstance(excludes, list):
            raise ValueError('invalid excluded labels')
        jobs = {}
        for issue in outcome:
            reason = None
            if (route, issue['id']) in holds or issue.get('external_hold'):
                reason = 'recorded_external_hold'
            elif native_internal_artifact(issue):
                reason = 'native_internal_artifact'
            elif not allowed(issue, now) or set(issue.get('labels', [])) & set(excludes):
                reason = 'not_currently_claimable'
            if reason:
                blocked.append({'workspace': route, 'id': issue['id'], 'reason': reason})
            elif (route, issue['id']) not in busy_keys:
                job = {'id': issue['id'], 'workspace': route}
                if lane_label is not None:
                    job['labelled'] = lane_label in issue.get('labels', [])
                jobs[issue['id']] = job
        jobs = list(jobs.values())
        active = sum(workspace == route for workspace, _ in busy_keys)
        build_heavy = any((route_path / marker).exists() for marker in BUILD_MARKERS)
        limit = limits.get(route, 1 if build_heavy else settings['max_workers'])
        if type(limit) is not int or limit < 0:
            raise ValueError('invalid workspace capacity')
        if build_heavy:
            limit = min(limit, 1)
        native_limit = policy.get('max_workers')
        if native_limit is not None:
            if type(native_limit) is not int or native_limit < 0:
                raise ValueError('invalid native workspace capacity')
            if native_limit:
                limit = min(limit, native_limit)
        slots = min(len(jobs), max(0, limit - active))
        # Dispatch health gate (subgov-06b69094): counted demand must be work
        # NEEDLE can claim now. A route whose verified health refuses dispatch
        # — unbound backend, unverified or unrecoverable checkpoint, secret
        # quarantine, busy sync lock — contributes no slots; a route whose
        # claimability could not be verified (observation failed, or a repair
        # NEEDLE would run at claim time is still pending) likewise keeps
        # only its busy claims as retention evidence, and withholds demand
        # entirely when no route could be verified. Zero-demand routes are
        # not probed: they contribute no slots under any health verdict.
        health = 'not_probed'
        if jobs:
            verdict = dispatch_health(route, health_timeout)
            if verdict['state'] == 'admitted':
                health = 'admitted'
            else:
                health = verdict['reason']
                row = {'workspace': route, 'reason': verdict['reason']}
                if verdict.get('detail'):
                    row['detail'] = verdict['detail']
                if verdict.get('error'):
                    row['error'] = verdict['error']
                blocked.append(row)
                if verdict['state'] == 'unknown':
                    health_unknown.append((route, verdict['exception']))
                jobs, slots = [], 0
        ready.extend(jobs)
        seeds.extend(jobs[:slots])
        capacity += active + slots
        summaries.append({'workspace': route, 'ready_beads': len(jobs), 'busy_claims': active,
                          'worker_limit': limit, 'eligible_slots': slots,
                          'dispatch_health': health})
    if failures and len(failures) == len(selected):
        # Demand is unknown, never zero: a fleet where every selected store
        # failed still withholds capacity instead of publishing a partial 0.
        raise failures[0][1]
    if health_unknown and len(failures) + len(health_unknown) == len(selected):
        # Same rule for dispatch health: when every selected route that could
        # carry demand either failed its ready query or could not verify
        # claimability, demand is unknown — publishing verified-empty zeros
        # would tell the controller to scale down over unverifiable work.
        raise health_unknown[0][1]
    # Keep validated claims even if scope/health changed since their dispatch.
    capacity += sum(workspace not in observed for workspace, _ in busy_keys)
    return {'observed_at': now.isoformat(), 'ready': seeds, 'ready_bead_count': len(ready),
            'dispatch_health_version': DISPATCH_HEALTH_VERSION,
            # Every counted unclaimed job, unbounded by slot limits: the set a
            # lane publisher must make claimable so counted demand and
            # lane-dispatchable work stay equal (subgov-6db97205).
            'eligible': ready,
            'busy': list(busy), 'blocked': blocked, 'workspaces': summaries,
            'unowned_floor': unowned_floor,
            # Occupied, identity-validated claims: the retention floor the
            # caller's busy scan contributed (subgov-a5c14e89).
            'busy_count': len(busy_keys),
            'eligible_backlog_capacity': min(settings['max_workers'], capacity)}


def seed_order(jobs, running):
    """Order launch seeds for new elastic workers (plan.md §26.5).

    Each new worker is seeded with the ready workspace having the fewest
    running workers; NEEDLE's own roaming takes over after the seed. `jobs`
    is observe()'s ready list -- the launchable subset after per-workspace
    slot limits -- and `running` maps workspace path to the count of workers
    currently running there. Keys on both sides are resolved, and unresolved
    inputs are normalized here so callers cannot skew the comparison.

    Every pick consumes one ready entry of that workspace and raises its
    occupancy, so a batch of new workers spreads across workspaces instead
    of stacking on the first route. Ties resolve to the lexicographically
    smallest resolved path, keeping the selection deterministic across
    cycles. The result is never a bare seed: it simply holds fewer entries
    than the requested launch count when no ready workspace slot remains,
    and a caller must not launch without a seed.
    """
    remaining = {}
    for job in jobs:
        route = str(Path(job['workspace']).resolve())
        remaining[route] = remaining.get(route, 0) + 1
    occupancy = {str(Path(route).resolve()): count for route, count in running.items()}
    seeds = []
    for _ in range(len(jobs)):
        route = min((route for route in remaining if remaining[route]),
                    key=lambda route: (occupancy.get(route, 0), route))
        seeds.append(route)
        remaining[route] -= 1
        occupancy[route] = occupancy.get(route, 0) + 1
    return seeds


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--settings', required=True)
    args = parser.parse_args()
    # Read-only observe: no quota collection, heartbeats, worker or state writes.
    print(json.dumps(observe(read_json(args.settings))))


if __name__ == '__main__':
    main()
