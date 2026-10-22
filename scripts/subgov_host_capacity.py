"""Measured host admission shared by independently running pool adapters.

Only aggregate resource evidence is persisted. The host lock spans admission
and launch; reservations cover asynchronous starts until observations catch up.
"""
from contextlib import contextmanager
from datetime import datetime, timezone
import json
import math
import os
from pathlib import Path
import subprocess
import sys
import time

from subgov_adapter_common import atomic_json, locked, read_json

GIB = 1024 ** 3
MEM_RESERVE = 8 * GIB
SLICE_RATIO = .85
LOAD_RATIO = .85
PSI_LIMIT = 10.0
PRESSURE_SECONDS = 60
FRESH_SECONDS = 900
HISTORY_SECONDS = 86400
COLD_RSS = 6 * GIB  # shephrd's incomplete-sample working-memory fallback


def percentile95(values):
    ordered = sorted(value for value in values if value > 0)
    return ordered[math.ceil(.95 * len(ordered)) - 1] if ordered else COLD_RSS


def split_targets(targets, demand, ceiling):
    """Capped proportional allocation with deterministic largest remainders."""
    result = dict.fromkeys(targets, 0)
    remaining = max(0, ceiling)
    active = {key for key, value in targets.items() if value > 0}
    while active and remaining:
        weights = {key: max(0, demand.get(key, 0)) for key in active}
        # Pools with no ready work may retain busy workers in otherwise spare
        # slots, but never displace a pool with ready demand.
        if not sum(weights.values()):
            weights = {key: targets[key] for key in active}
        total = sum(weights.values())
        capped = {key for key in active if remaining * weights[key] >= targets[key] * total}
        if capped:
            for key in capped:
                result[key] = targets[key]
                remaining -= targets[key]
            active -= capped
            continue
        shares = {key: divmod(remaining * weights[key], total) for key in active}
        for key in active:
            result[key] = shares[key][0]
        spare = remaining - sum(shares[key][0] for key in active)
        for key in sorted(active, key=lambda key: (-shares[key][1], key))[:spare]:
            result[key] += 1
        break
    return result


def process_sample(pools, proc=Path('/proc')):
    """Attribute descendants once to each outermost NEEDLE run supervisor.

    Do not read environments or persist command lines. Non-pool NEEDLE roots
    reserve host slots, including workers not yet adopted by a pool.
    """
    processes, roots = {}, {}
    for entry in proc.iterdir():
        if not entry.name.isdigit():
            continue
        try:
            args = []
            fields = (entry / 'stat').read_text().rsplit(')', 1)[1].split()
            if fields[0] in ('Z', 'X'):
                continue
            pid = int(entry.name)
            processes[pid] = (int(fields[1]), max(0, int(fields[21])) * os.sysconf('SC_PAGE_SIZE'))
            if entry.stat().st_uid != os.getuid():
                continue
            args = (entry / 'cmdline').read_bytes().decode().split('\0')
            if Path(args[0]).name not in ('needle', 'needle-stable') or 'run' not in args:
                continue
            agent = args[args.index('--agent') + 1]
            name = args[args.index('--identifier') + 1]
            matches = [key for key, pool in pools.items()
                       if name.startswith(tuple(pool.get('prefixes', [])))
                       or name in pool.get('identifiers', [])]
            if len(matches) > 1:
                raise ValueError('overlapping host pool ownership')
            roots[pid] = matches[0] if matches else None
        except (FileNotFoundError, ProcessLookupError):
            continue
        except (ValueError, IndexError, UnicodeError):
            # A NEEDLE run without an explicit identity is still a host cost.
            if args and Path(args[0]).name in ('needle', 'needle-stable') and 'run' in args:
                roots[pid] = None
        # Permission errors are deliberately fatal: incomplete process visibility
        # must not authorize new workers from an underestimated host footprint.
    def ancestors(pid):
        seen = set()
        while pid in processes and pid not in seen:
            seen.add(pid)
            pid = processes[pid][0]
            yield pid
    roots = {pid: pool for pid, pool in roots.items() if not any(parent in roots for parent in ancestors(pid))}
    rss = dict.fromkeys(roots, 0)
    for pid, (_, memory) in processes.items():
        root = pid if pid in roots else next((p for p in ancestors(pid) if p in roots), None)
        if root is not None:
            rss[root] += memory
    counts = {key: sum(pool == key for pool in roots.values()) for key in pools}
    return counts, sum(pool is None for pool in roots.values()), list(rss.values())


def probe(pools):
    counts, unmanaged, rss = process_sample(pools)
    meminfo = dict((line.split(':')[0], int(line.split()[1]) * 1024)
                   for line in Path('/proc/meminfo').read_text().splitlines())
    def psi(kind):
        line = next(line for line in Path('/proc/pressure', kind).read_text().splitlines() if line.startswith('some '))
        return float(dict(part.split('=') for part in line.split()[1:])['avg10'])
    result = subprocess.run(['systemctl', '--user', 'show', 'needle.slice',
                             '--property=MemoryCurrent,MemoryHigh'], capture_output=True,
                            text=True, timeout=2, check=True)
    properties = dict(line.split('=', 1) for line in result.stdout.splitlines() if '=' in line)
    high = properties['MemoryHigh']
    # An unlimited slice supplies no finite limiter; MemAvailable still bounds it.
    high = None if high in ('infinity', str(2 ** 64 - 1)) else int(high)
    sample = dict(counts=counts, unmanaged=unmanaged, rss=rss,
                  mem_available=meminfo['MemAvailable'], slice_current=int(properties['MemoryCurrent']),
                  slice_high=high, memory_psi=psi('memory'), io_psi=psi('io'),
                  load=float(Path('/proc/loadavg').read_text().split()[0]),
                  cpus=len(os.sched_getaffinity(0)))
    for key in ('mem_available', 'slice_current', 'memory_psi', 'io_psi', 'load', 'cpus'):
        if not math.isfinite(sample[key]) or sample[key] < 0:
            raise ValueError('invalid resource sample')
    return sample


def measured_ceiling(sample, state, now):
    current = sum(sample['counts'].values()) + sample['unmanaged']
    history = [item for item in state.get('rss_history', []) if 0 <= now - item[0] <= HISTORY_SECONDS]
    history.extend([now, value] for value in sample['rss'] if value > 0)
    state['rss_history'] = history[-256:]
    cost = percentile95([item[1] for item in state['rss_history']])
    # Headroom describes ADDITIONAL slots; signed room also shrinks a host
    # already over budget. The live workers' memory is already charged.
    limits = {'mem_available': max(0, current + math.floor((sample['mem_available'] - MEM_RESERVE) / cost))}
    if sample['slice_high'] is not None:
        limits['slice_memory'] = max(0, current + math.floor((sample['slice_high'] * SLICE_RATIO - sample['slice_current']) / cost))
    pressure = [name for name in ('memory_psi', 'io_psi') if sample[name] >= PSI_LIMIT]
    if sample['load'] >= sample['cpus'] * LOAD_RATIO:
        pressure.append('load')
    if pressure:
        since = state.get('pressure_since')
        previous = state.get('last_probe_at')
        if previous is None or not 0 <= now - previous <= FRESH_SECONDS:
            since = None
        if since is None or since > now:
            since = now
        state['pressure_since'] = since
        limits['pressure'] = max(0, current - int(now - since >= PRESSURE_SECONDS))
    else:
        state['pressure_since'] = None
    state['last_probe_at'] = now
    ceiling = min(limits.values())
    return dict(ceiling=ceiling, limits=limits, p95_worker_rss=cost,
                rss_basis='observed' if history else 'cold_fallback', pressure=pressure,
                pressure_since=state['pressure_since'])


def ready_count(path, now):
    try:
        value = read_json(path)
        age = now - datetime.fromisoformat(value['observed_at'].replace('Z', '+00:00')).timestamp()
        count = value['ready_bead_count']
        if not 0 <= age <= FRESH_SECONDS or type(count) is not int or count < 0:
            raise ValueError('invalid demand sample')
        return count
    except (OSError, ValueError, KeyError, TypeError):
        return None


def decide(config, pool, desired, state, sample, now):
    pools = config['pools']
    if pool not in pools:
        raise ValueError('unknown host pool')
    evidence = measured_ceiling(sample, state, now)
    current = sample['counts']
    requests = state.setdefault('requests', {})
    if desired is not None:
        requests[pool] = {'target': desired, 'at': now}
    demand = {key: ready_count(value['demand_state'], now) for key, value in pools.items()}
    targets = {}
    for key in pools:
        request = requests.get(key, {})
        fresh = 0 <= now - request.get('at', 0) <= FRESH_SECONDS
        targets[key] = request['target'] if fresh else current[key]
        if demand[key] is None or evidence['pressure']:
            targets[key] = min(targets[key], current[key])
    budget = max(0, evidence['ceiling'] - sample['unmanaged'])
    # Unknown demand holds existing workers, still subject to the resource
    # ceiling. Do not give unknown pools a zero weight and silently drain them.
    held = split_targets({key: targets[key] for key in pools if demand[key] is None}, {}, budget)
    allocations = {**held, **split_targets({key: targets[key] for key in pools if key not in held},
                                           {key: value or 0 for key, value in demand.items()},
                                           budget - sum(held.values()))}
    reservations = state.setdefault('reservations', {})
    # A lowered target cannot cancel an asynchronous launch. Keep its high
    # water reservation until ALL expected PIDs have appeared or the bounded
    # startup/freshness window expires. While our own launch is pending, retry
    # observations only; never launch the same missing slots a second time.
    for key in list(reservations):
        reservation = reservations[key]
        if (key not in current or current[key] >= reservation['count']
                or not 0 <= now - reservation['at'] <= FRESH_SECONDS):
            del reservations[key]
    effective = {key: max(current[key], reservations.get(key, {}).get('count', 0)) for key in pools}
    room = max(0, budget - sum(value for key, value in effective.items() if key != pool))
    grant = min(allocations[pool], room)
    if evidence['pressure'] or demand[pool] is None or pool in reservations:
        grant = min(grant, current[pool])
    if desired is not None and grant > current[pool]:
        reservations[pool] = {'count': grant, 'at': now}
    return {**evidence, 'event': 'host_worker_ceiling', 'pool': pool, 'observed_at': now,
            'counts': current, 'unmanaged': sample['unmanaged'], 'ready_demand': demand,
            'requested': targets, 'allocations': allocations, 'granted': grant}


@contextmanager
def admission(settings, desired=None, current=0):
    """Yield the host-limited target; observe-only calls sample/log each cycle."""
    if not settings.get('host_capacity'):
        yield desired
        return
    config = read_json(settings['host_capacity'])
    path, pool = config['state_path'], settings['host_pool']
    with locked(path):
        state = read_json(path, {'version': 1})
        if state.get('version') != 1:
            raise ValueError('unsupported host capacity state')
        now = time.time()
        try:
            sample = probe(config['pools'])
            event = decide(config, pool, desired, state, sample, now)
            grant = event['granted']
        except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError, StopIteration) as error:
            # Unknown evidence holds the observed fleet and allows a quota-side
            # reduction; it never turns missing metrics into an admission.
            state['pressure_since'] = None
            state['last_probe_at'] = None
            grant = min(desired, current) if desired is not None else current
            event = {'event': 'host_worker_ceiling', 'pool': pool, 'observed_at': now,
                     'ceiling': None, 'granted': grant, 'error': type(error).__name__}
        state['last_evidence'] = event
        atomic_json(path, state)  # reserve BEFORE invoking the asynchronous launcher
        print(json.dumps(event, sort_keys=True, allow_nan=False), file=sys.stderr, flush=True)
        yield grant
