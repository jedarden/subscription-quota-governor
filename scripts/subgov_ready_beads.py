"""Read-only NEEDLE ready-bead demand, shared by all provider pools.

Native bead-rs owns dependency readiness; this module adds NEEDLE's dispatch
holds and pool/workspace boundaries. A failed query is unknown, never zero.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import re
import subprocess

from subgov_adapter_common import read_json

LIMIT = 999999
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
    return {'bead_cli': {'backend': values.get(('bead_cli', 'backend'), 'bead-rs')},
            'queue': {'owner_host': values.get(('queue', 'owner_host'))},
            'max_workers': values.get(('workspace', 'max_workers')),
            **({'exclude_labels': values[('strands', 'pluck', 'exclude_labels')]}
               if ('strands', 'pluck', 'exclude_labels') in values else {})}


def policy_ok(route, host):
    route = Path(route)
    if not (route / '.beads/config.json').is_file():
        return False
    policy = workspace_policy(route / '.needle.yaml') if (route / '.needle.yaml').exists() else {
        'bead_cli': {'backend': 'bead-rs'}, 'queue': {}}
    return (policy['bead_cli']['backend'] in ('bead-rs', 'bead', 'auto')
            and policy['queue'].get('owner_host') in (None, host))


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


def ready_issues(route):
    result = subprocess.run(['bead', 'list', '--ready', '--json', '--limit', str(LIMIT)],
                            cwd=route, text=True, capture_output=True, timeout=8, check=True)
    issues = json.loads(result.stdout)
    if not isinstance(issues, list) or len(issues) >= LIMIT:
        raise ValueError('invalid or truncated native ready array')
    if any(not isinstance(issue, dict) or not isinstance(issue.get('id'), str) for issue in issues):
        raise ValueError('invalid native issue')
    return issues


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



def observe(settings, busy=(), unowned_floor=0):
    """Return ready count plus a bounded total fleet capacity including claims.

    Claims already executing are retention evidence, never ready demand. A
    build-heavy workspace contributes one slot; existing claims occupy it.
    """
    now = datetime.now(timezone.utc)
    config = settings['demand']
    if type(settings['max_workers']) is not int or settings['max_workers'] < 0:
        raise ValueError('invalid pool ceiling')
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
            blocked.append({'workspace': route, 'reason': 'backend_or_queue_owner'})
        elif gate_reason := gate_degraded(settings, route):
            blocked.append({'workspace': route, 'reason': gate_reason})
        else:
            selected.append(route)
    ready, seeds, summaries, capacity = [], [], [], unowned_floor
    busy_keys = {(str(Path(job['workspace']).resolve()), job['id']) for job in busy}
    with ThreadPoolExecutor(max_workers=4) as pool:
        for route, issues in zip(selected, pool.map(ready_issues, selected)):
            route_path = Path(route)
            policy = workspace_policy(route_path / '.needle.yaml') if (route_path / '.needle.yaml').exists() else {}
            excludes = policy.get('exclude_labels', global_excludes)
            if not isinstance(excludes, list):
                raise ValueError('invalid excluded labels')
            jobs = {}
            for issue in issues:
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
                    jobs[issue['id']] = {'id': issue['id'], 'workspace': route}
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
            ready.extend(jobs)
            seeds.extend(jobs[:slots])
            capacity += active + slots
            summaries.append({'workspace': route, 'ready_beads': len(jobs), 'busy_claims': active,
                              'worker_limit': limit, 'eligible_slots': slots})
    # Keep validated claims even if scope/health changed since their dispatch.
    capacity += sum(workspace not in selected for workspace, _ in busy_keys)
    return {'observed_at': now.isoformat(), 'ready': seeds, 'ready_bead_count': len(ready),
            'busy': list(busy), 'blocked': blocked, 'workspaces': summaries,
            'unowned_floor': unowned_floor,
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
