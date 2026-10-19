"""Synthetic contract tests: no network, real services, or worker processes."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import stat
import sys
import tempfile
import unittest
from datetime import datetime, timezone
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[2] / "scripts"
sys.path.insert(0, str(SCRIPTS))


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


collector = load("collector", "zai-collector.py")
fleet_module = load("fleet", "zai-fleet.py")
from subgov_adapter_common import atomic_json, locked, read_json


def metrics(start=10, requests=100, completed=80, pressure=3, classification=True, age=2, used=0, reset=900):
    lines = [f"process_start_time_seconds {start}",
             '# TYPE zai_proxy_requests_total counter',
             '# TYPE zai_proxy_request_duration_seconds histogram',
             '# TYPE zai_proxy_retry_attempts_total counter',
             '# TYPE zai_proxy_rate_limit_rejections_total counter',
             f'zai_proxy_requests_total{{method="POST",path="/v1/messages",status_code="200"}} {requests}',
             f'zai_proxy_request_duration_seconds_count{{method="POST",path="/v1/messages",status_code="200"}} {completed}',
             'zai_proxy_requests_total{method="GET",path="/models",status_code="200"} 90000',
             'zai_proxy_request_duration_seconds_count{method="POST",path="/v1/messages/count_tokens",status_code="200"} 80000',
             'zai_proxy_retry_attempts_total{reason="429"} 800',
             'zai_proxy_retry_attempts_total{reason="retry"} 4',
             'zai_proxy_rate_limit_rejections_total 2',
             'zai_proxy_upstream_errors_total{error_type="429"} 700',
             f'zai_proxy_quota_sample_age_seconds {age}',
             f'zai_proxy_quota_usage_ratio{{window="five_hour"}} {used}',
             f'zai_proxy_quota_reset_time_seconds{{window="five_hour"}} {reset}']
    if classification:
        lines += ['# TYPE zai_proxy_zai_errors_total counter', f'zai_proxy_zai_errors_total{{code="1302"}} {pressure}', 'zai_proxy_zai_errors_total{code="1113"} 6000']
    return '\n'.join(lines)


def sample(at=1000, start=10, requests=100, completed=80, concurrency=3, classified=True, http_429=0, http_429_supported=True):
    return dict(at=at, start=start, requests=requests, completed=completed, concurrency=concurrency, classified=classified, retries=4, capacity=2, http_429=http_429, http_429_supported=http_429_supported)


class CollectorTests(unittest.TestCase):
    def test_generation_and_classification(self):
        _, parsed = collector.parse_metrics(metrics(), 1000)
        self.assertEqual(parsed["requests"], 100)
        self.assertEqual(parsed["completed"], 80)
        self.assertEqual(parsed["retries"], 4)
        self.assertEqual(parsed["concurrency"], 3)
        self.assertEqual(parsed["http_429"], 700)
        self.assertTrue(parsed["http_429_supported"])

    def test_http_429_interval_is_separate_from_1302_with_same_full_pool_denominator(self):
        old = {"a": sample(http_429=100), "b": sample(start=20, http_429=50)}
        new = {"a": sample(at=1300, requests=140, completed=110, concurrency=5, http_429=108),
               "b": sample(at=1300, start=20, requests=120, completed=90, concurrency=4, http_429=54)}
        result = collector.update_saturation(new, {"replicas": old})
        self.assertEqual(result["upstream_concurrency_fraction"], 3 / 60)
        self.assertEqual(result["backpressure"], {"attempts": 60, "window_seconds": 300, "upstream_429_fraction": 12 / 60})

    def test_http_429_missing_family_old_baseline_reset_and_low_traffic_are_unknown(self):
        _, parsed = collector.parse_metrics('\n'.join(line for line in metrics().splitlines()
                                                     if 'upstream_errors_total' not in line), 1000)
        self.assertFalse(parsed['http_429_supported'])
        for old, new in [
            (sample(http_429_supported=False), sample(at=1300, requests=140)),
            (sample(), sample(at=1300, requests=140, http_429_supported=False)),
            ({k: v for k, v in sample().items() if not k.startswith('http_429')}, sample(at=1300, requests=140)),
            (sample(http_429=5), sample(at=1300, requests=140, http_429=4)),
            (sample(), sample(at=1300, requests=120)),
            (sample(), sample(at=1300, requests=140, http_429=41)),
        ]:
            result = collector.update_saturation({"a": new}, {"replicas": {"a": old}})
            self.assertIsNotNone(result)
            self.assertIsNone(result['backpressure']['upstream_429_fraction'])
        result = collector.update_saturation({"a": sample(at=1300, requests=140)}, {"replicas": {"a": sample()}})
        self.assertEqual(result['backpressure']['upstream_429_fraction'], 0)

    def test_unclassified_is_unknown(self):
        _, parsed = collector.parse_metrics(metrics(classification=False), 1000)
        self.assertFalse(parsed["classified"])
        state = {"replicas": {"a": sample(classified=False)}}
        output = collector.update_saturation({"a": sample(at=1060, requests=140, completed=110, classified=False)}, state)
        self.assertIsNone(output["upstream_concurrency_fraction"])
        self.assertEqual(output["completed_requests"], 30)

    def test_dedicated_concurrency_metric_is_known_zero_without_double_counting(self):
        body = metrics(pressure=9) + '\n# TYPE zai_proxy_upstream_concurrency_rejections_total counter\nzai_proxy_upstream_concurrency_rejections_total{variant="production"} 0'
        _, parsed = collector.parse_metrics(body, 1000)
        self.assertTrue(parsed["classified"])
        self.assertEqual(parsed["concurrency"], 0)
        _, parsed = collector.parse_metrics(body.replace('production"} 0', 'production"} 2'), 1000)
        self.assertEqual(parsed["concurrency"], 2)

    def test_unused_retry_family_is_zero_only_with_initialized_proxy_contract(self):
        body = '\n'.join(line for line in metrics(classification=False).splitlines()
                         if 'zai_proxy_retry_attempts_total' not in line)
        _, unsupported = collector.parse_metrics(body, 1000)
        self.assertIsNone(unsupported)
        body += '\n# TYPE zai_proxy_upstream_concurrency_rejections_total counter\nzai_proxy_upstream_concurrency_rejections_total{variant="production"} 0'
        _, supported = collector.parse_metrics(body, 1000)
        self.assertIsNotNone(supported)
        self.assertEqual(supported["retries"], 0)
        self.assertTrue(supported["classified"])

    def test_type_family_is_evidence_for_zero_classification(self):
        _, parsed = collector.parse_metrics(metrics(classification=False) + '\n# TYPE zai_proxy_zai_errors_total counter', 1000)
        self.assertTrue(parsed["classified"])
        self.assertEqual(parsed["concurrency"], 0)

    def test_lapsed_empty_only_and_freshness(self):
        quota, _ = collector.parse_metrics(metrics(), 1000)
        self.assertEqual(quota["windows"][0]["resets_at"], datetime.fromtimestamp(19000, timezone.utc).isoformat().replace('+00:00', 'Z'))
        quota, _ = collector.parse_metrics(metrics(used=.1, age=900), 1000)
        self.assertEqual(quota["windows"][0]["resets_at"], '1970-01-01T00:15:00Z')
        self.assertFalse(quota["fresh"])

    def test_invalid_metrics(self):
        for body in [metrics(age=-1), metrics().replace(' 100\n', ' NaN\n'), metrics().replace('process_start_time_seconds 10', 'process_start_time_seconds NaN')]:
            with self.assertRaises(ValueError):
                collector.parse_metrics(body, 1000)

    def test_absent_capacity_family_zero_requires_proven_no_generation_503(self):
        body = '\n'.join(line for line in metrics().splitlines() if 'rate_limit_rejections_total' not in line)
        _, parsed = collector.parse_metrics(body, 1000)
        self.assertIsNotNone(parsed)
        self.assertEqual(parsed['capacity'], 0)
        _, parsed = collector.parse_metrics(body+'\nzai_proxy_requests_total{method="POST",path="/v1/messages",status_code="503"} 1', 1000)
        self.assertIsNone(parsed)
        _, parsed = collector.parse_metrics(body+'\nzai_proxy_requests_total{method="POST",path="/v1/messages",status_code="503"} 0', 1000)
        self.assertIsNotNone(parsed)
        _, parsed = collector.parse_metrics(body.replace('status_code="200"} 100', '} 100'), 1000)
        self.assertIsNone(parsed)
        _, parsed = collector.parse_metrics(body+'\nzai_proxy_requests_total{method="GET",path="/models",status_code="503"} 99', 1000)
        self.assertIsNotNone(parsed)

    def test_full_replica_delta_and_consumption(self):
        state = {"replicas": {"a": sample(), "b": sample(start=20)}}
        current = {"a": sample(at=1060, requests=140, completed=110, concurrency=5), "b": sample(at=1060, start=20, requests=120, completed=100, concurrency=4)}
        result = collector.update_saturation(current, state)
        self.assertEqual(result["attempts"], 60)
        self.assertEqual(result["completed_requests"], 50)
        self.assertEqual(result["upstream_concurrency_fraction"], 3/60)
        self.assertIsNone(collector.update_saturation(current, state))

    def test_alternation_disappearance_restart_reset_and_old(self):
        for current in [{"b": sample(at=1060, start=20)},
                        {"a": sample(at=1060, start=20)},
                        {"a": sample(at=1060, requests=1)},
                        {"a": sample(at=2000, requests=140)}]:
            state = {"replicas": {"a": sample()}}
            self.assertIsNone(collector.update_saturation(current, state))
            self.assertEqual(state["replicas"], current)

    def test_one_new_replica_withholds_whole_pool(self):
        state = {"replicas": {"a": sample()}}
        self.assertIsNone(collector.update_saturation({"a": sample(at=1060, requests=140), "b": sample(at=1060, start=20)}, state))

    def test_short_baseline_retained_and_insufficient_attempts(self):
        state = {"replicas": {"a": sample()}}
        self.assertIsNone(collector.update_saturation({"a": sample(at=1059, requests=120)}, state))
        self.assertEqual(state["replicas"]["a"]["at"], 1000)
        output = collector.update_saturation({"a": sample(at=1060, requests=120)}, state)
        self.assertEqual(output["attempts"], 20)
        self.assertIsNone(output["upstream_concurrency_fraction"])

    def test_attempts_subtract_capacity_and_only_real_retry(self):
        old, new = sample(), sample(at=1060, requests=140, completed=100, concurrency=6)
        new.update(retries=10, capacity=8)
        result = collector.update_saturation({"a": new}, {"replicas": {"a": old}})
        self.assertEqual(result["attempts"], 40)
        self.assertEqual(result["upstream_concurrency_fraction"], 3/40)

    def args(self, root, **kwargs):
        fields = dict(metrics_url=["http://replica-a/metrics", "http://replica-b/metrics"], expected_replicas=None, membership_api=None, kube_api=None, insecure_tls=False, namespace="mcp", selector="app=zai-proxy", port=8080, state=str(root / "state.json"), stale_after=900, minimum_interval=60, maximum_interval=900, minimum_attempts=30, backlog_capacity_file=None)
        fields.update(kwargs)
        return argparse.Namespace(**fields)

    def test_failed_replica_clears_baseline_without_partial_zero(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            args = self.args(root)
            atomic_json(args.state, {"version": 1, "membership": None, "replicas": {"old": sample()}})
            def fetch(url, _):
                if "replica-b" in url:
                    raise OSError("fixture failure")
                return metrics()
            with patch.object(collector, "fetch", side_effect=fetch):
                result = collector.collect(args)
            self.assertNotIn("saturation", result)
            self.assertEqual(read_json(args.state)["replicas"], {})

    def test_lb_complete_coverage_deduplicates_and_membership_change_holds(self):
        with tempfile.TemporaryDirectory() as temp:
            args = self.args(Path(temp), metrics_url=["http://lb/metrics"], expected_replicas=2, membership_api="http://api")
            with patch.object(collector, "discover", return_value=({"pod1": "unused", "pod2": "unused"}, True)), patch.object(collector, "fetch", side_effect=[metrics(start=10), metrics(start=20)] * 3):
                result = collector.collect(args)
            self.assertNotIn("saturation", result)
            self.assertEqual(set(read_json(args.state)["replicas"]), {"10.0", "20.0"})
            with patch.object(collector, "discover", side_effect=[({"pod1": "x", "pod2": "x"}, True), ({"pod1": "x", "pod3": "x"}, True)]), patch.object(collector, "fetch", return_value=metrics()):
                result = collector.collect(args)
            self.assertNotIn("saturation", result)
            self.assertEqual(read_json(args.state)["replicas"], {})

    def test_explicit_backlog(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            capacity = root / "capacity.json"
            atomic_json(str(capacity), {"eligible_backlog_capacity": 3, "observed_at": datetime.now(timezone.utc).isoformat()})
            with patch.object(collector, "fetch", return_value=metrics()):
                result = collector.collect(self.args(root, backlog_capacity_file=str(capacity)))
            self.assertEqual(result["eligible_backlog_capacity"], 3)
            atomic_json(str(capacity), {"eligible_backlog_capacity": 3, "observed_at": "2020-01-01T00:00:00Z"})
            with patch.object(collector, "fetch", return_value=metrics()):
                result = collector.collect(self.args(root, backlog_capacity_file=str(capacity)))
            self.assertNotIn("eligible_backlog_capacity", result)

    def test_state_permissions_lock_and_atomic_cleanup(self):
        with tempfile.TemporaryDirectory() as temp:
            path = str(Path(temp) / "state")
            with locked(path):
                with self.assertRaises(BlockingIOError):
                    with locked(path):
                        pass
                atomic_json(path, {"test": 1})
            self.assertEqual(stat.S_IMODE(os.stat(path).st_mode), 0o600)
            self.assertEqual(stat.S_IMODE(os.stat(path + ".lock").st_mode), 0o600)
            self.assertFalse(any(item.name.startswith('.subgov-') for item in Path(temp).iterdir()))


class FakeFleet(fleet_module.Fleet):
    def __init__(self, args, roster):
        super().__init__(args, roster)
        self.events = []
        self.statuses = {worker['unit']: dict(pid=0, ActiveState='inactive', masked=False) for worker in roster['workers']}
        self.busy = set()
    def status(self, worker):
        return self.statuses[worker['unit']].copy()
    def check_routing(self, worker):
        pass
    def heartbeat(self, worker, pid):
        return dict(current_bead='claimed' if worker['unit'] in self.busy else None, current_task=None, is_idle=worker['unit'] not in self.busy, state='SELECTING')
    def process_identity(self, worker, pid):
        return 'generation1'
    def command(self, *args):
        self.events.append(args)
        if args[0] == 'start':
            self.statuses[args[-1]].update(pid=100, ActiveState='active')
        elif args[0] == 'stop':
            self.statuses[args[-1]].update(pid=0, ActiveState='inactive')
        return ''


class FleetTests(unittest.TestCase):
    def test_exhausted_and_admission_holds_are_safe_candidates_only_when_empty(self):
        for state, idle in [('EXHAUSTED', True), ('ADMISSION_BLOCKED', False)]:
            heartbeat = dict(current_bead=None, current_task=None, state=state, is_idle=idle)
            self.assertTrue(fleet_module.Fleet.idle(heartbeat))
            heartbeat['current_bead'] = 'claimed'
            self.assertFalse(fleet_module.Fleet.idle(heartbeat))

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.roster = dict(version=1, owner='subgov-zai', scope='user', workers=[dict(unit=f'needle-worker@glm-{i}.service', worker_id=f'glm-{i}', agent='claude-code-glm-5.3-flash') for i in range(2)])
        self.args = argparse.Namespace(state=str(self.root/'state.json'), unit_dir=str(self.root/'units'), routing_dir=str(self.root/'routing'), heartbeat_dir=str(self.root/'heartbeats'), proc_root=str(self.root/'proc'), stale_after=60)
        self.fleet = FakeFleet(self.args, self.roster)

    def test_roster_rejects_duplicates_non_glm_and_mismatch(self):
        path = self.root/'roster.json'
        variants = []
        for field, value in [('agent', 'codex'), ('unit', 'other.service')]:
            roster = json.loads(json.dumps(self.roster))
            roster['workers'][0][field] = value
            variants.append(roster)
        roster = json.loads(json.dumps(self.roster))
        roster['workers'][1] = roster['workers'][0]
        variants.append(roster)
        for roster in variants:
            atomic_json(str(path), roster)
            with self.assertRaises(ValueError):
                fleet_module.roster_read(path)

    def test_busy_drain_persisted_and_remains_counted(self):
        worker = self.roster['workers'][0]
        self.fleet.statuses[worker['unit']].update(pid=100, ActiveState='active')
        self.fleet.busy.add(worker['unit'])
        self.fleet.reconcile(0, self.fleet.load_state())
        self.assertFalse(any(event[0] == 'stop' for event in self.fleet.events))
        self.assertEqual(sum(bool(info['pid']) for _, info in self.fleet.inspect()), 1)
        self.assertEqual(read_json(self.args.state)['pending_retirees'], [worker['unit']])
        dropin, marker = self.fleet.paths(worker)
        self.assertIn('Restart=no', dropin.read_text())
        self.assertIn('ConditionPathExists=!', dropin.read_text())
        self.assertTrue(marker.exists())

    def test_masked_never_started_or_unmasked(self):
        self.fleet.statuses[self.roster['workers'][0]['unit']]['masked'] = True
        self.fleet.reconcile(1, self.fleet.load_state())
        starts = [event for event in self.fleet.events if event[0] == 'start']
        self.assertEqual(starts[0][-1], self.roster['workers'][1]['unit'])
        self.assertFalse(any(event[0] == 'unmask' for event in self.fleet.events))
        with self.assertRaises(ValueError):
            self.fleet.reconcile(2, self.fleet.load_state())

    def test_resume_removes_only_own_retirement_files(self):
        self.fleet.reconcile(0, self.fleet.load_state())
        self.fleet.reconcile(1, self.fleet.load_state())
        dropin, marker = self.fleet.paths(self.roster['workers'][0])
        self.assertFalse(dropin.exists())
        self.assertFalse(marker.exists())
        self.assertEqual(self.fleet.statuses[self.roster['workers'][0]['unit']]['pid'], 100)

    def test_other_owner_rejected_before_mutation(self):
        state = self.fleet.load_state()
        state['owner'] = 'other'
        atomic_json(self.args.state, state)
        with self.assertRaises(ValueError):
            self.fleet.load_state()
        dropin, _ = self.fleet.paths(self.roster['workers'][0])
        dropin.parent.mkdir(parents=True)
        dropin.write_text('# owned by someone else\n')
        with self.assertRaises(ValueError):
            self.fleet.inspect()
        self.assertEqual(self.fleet.events, [])

    def proc_fixture(self, child=''):
        path = self.root/'proc'/'100'/'task'/'100'
        path.mkdir(parents=True)
        (path/'status').write_text('State:\tT (stopped)\n')
        (path/'children').write_text(child)
        return path

    def test_idle_stop_freezes_checks_and_resumes(self):
        worker = self.roster['workers'][0]
        info = dict(pid=100, ActiveState='active', masked=False)
        self.fleet.statuses[worker['unit']] = info.copy()
        self.proc_fixture()
        with patch.object(os, 'pidfd_open', return_value=987), patch.object(os, 'close'), patch.object(fleet_module.signal, 'pidfd_send_signal') as signals:
            self.assertTrue(self.fleet.stop_idle(worker, info))
        self.assertEqual([call.args[1] for call in signals.call_args_list], [fleet_module.signal.SIGSTOP, fleet_module.signal.SIGCONT])
        self.assertIn(('stop', '--no-block', worker['unit']), self.fleet.events)

    def test_any_thread_child_prevents_stop_and_always_resumes(self):
        worker = self.roster['workers'][0]
        main = self.proc_fixture()
        sibling = main.parent / '101'
        sibling.mkdir()
        (sibling/'status').write_text('State:\tT (stopped)\n')
        (sibling/'children').write_text('1234')
        with patch.object(os, 'pidfd_open', return_value=987), patch.object(os, 'close'), patch.object(fleet_module.signal, 'pidfd_send_signal') as signals:
            self.assertFalse(self.fleet.stop_idle(worker, dict(pid=100)))
        self.assertEqual(signals.call_args_list[-1].args[1], fleet_module.signal.SIGCONT)
        self.assertFalse(any(event[0] == 'stop' for event in self.fleet.events))

    def test_routing_file_requires_exact_glm_identity(self):
        worker = self.roster['workers'][0]
        path = Path(self.args.routing_dir)/(worker['worker_id']+'.env')
        path.parent.mkdir()
        path.write_text('NEEDLE_AGENT=codex\n')
        with self.assertRaises(ValueError):
            fleet_module.Fleet.check_routing(self.fleet, worker)
        path.write_text('NEEDLE_AGENT='+worker['agent']+'\n')
        fleet_module.Fleet.check_routing(self.fleet, worker)

    def test_supervisor_process_identity_accepts_stable_only_with_exact_args(self):
        worker = self.roster['workers'][0]
        directory = self.root/'proc'/'100'
        directory.mkdir(parents=True)
        args = ['/path/needle-stable', 'run', '--identifier', worker['worker_id'], '--agent', worker['agent']]
        (directory/'cmdline').write_bytes(('\0'.join(args)+'\0').encode())
        (directory/'stat').write_text('100 (needle-stable) '+' '.join(['S']+['0']*18+['12345']))
        self.assertEqual(fleet_module.Fleet.process_identity(self.fleet, worker, 100), '12345')
        args[-1] = 'codex'
        (directory/'cmdline').write_bytes(('\0'.join(args)+'\0').encode())
        with self.assertRaises(ValueError):
            fleet_module.Fleet.process_identity(self.fleet, worker, 100)

    def test_stop_failure_always_resumes(self):
        worker = self.roster['workers'][0]
        self.fleet.statuses[worker['unit']].update(pid=100, ActiveState='active')
        self.proc_fixture()
        with patch.object(os, 'pidfd_open', return_value=987), patch.object(os, 'close'), patch.object(fleet_module.signal, 'pidfd_send_signal') as signals, patch.object(self.fleet, 'command', side_effect=RuntimeError):
            with self.assertRaises(RuntimeError):
                self.fleet.stop_idle(worker, dict(pid=100))
        self.assertEqual(signals.call_args_list[-1].args[1], fleet_module.signal.SIGCONT)

    def test_actual_heartbeat_identity_and_stale_checks(self):
        worker = self.roster['workers'][0]
        heartbeat = dict(qualified_id=f"{worker['agent']}-{worker['worker_id']}", worker_id=worker['worker_id'], model=worker['agent'], pid=100, last_heartbeat=datetime.now(timezone.utc).isoformat())
        path = Path(self.args.heartbeat_dir)/(heartbeat['qualified_id']+'.json')
        atomic_json(str(path), heartbeat)
        self.assertEqual(fleet_module.Fleet.heartbeat(self.fleet, worker, 100)['pid'], 100)
        with self.assertRaises(ValueError):
            fleet_module.Fleet.heartbeat(self.fleet, worker, 101)
        heartbeat['last_heartbeat'] = '2020-01-01T00:00:00Z'
        atomic_json(str(path), heartbeat)
        with self.assertRaises(ValueError):
            fleet_module.Fleet.heartbeat(self.fleet, worker, 100)


if __name__ == '__main__':
    unittest.main()
