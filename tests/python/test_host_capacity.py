"""Host admission contracts; synthetic /proc and metrics, never live workers."""
from contextlib import redirect_stderr
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'scripts'))
import subgov_host_capacity as host
from subgov_adapter_common import atomic_json, locked


def sample(**changes):
    value = dict(counts={'a': 4, 'b': 2, 'c': 0}, unmanaged=1,
                 rss=[host.GIB] * 7, mem_available=40 * host.GIB,
                 slice_current=20 * host.GIB, slice_high=100 * host.GIB,
                 memory_psi=0, io_psi=0, load=0, cpus=20)
    value.update(changes)
    return value


class CeilingTests(unittest.TestCase):
    def test_mem_available_limits_additional_workers_and_over_budget_retires(self):
        result = host.measured_ceiling(sample(mem_available=10 * host.GIB), {}, 100)
        self.assertEqual(result['ceiling'], 9)  # seven live plus two extra
        result = host.measured_ceiling(sample(mem_available=6 * host.GIB), {}, 100)
        self.assertEqual(result['ceiling'], 5)

    def test_slice_high_limits_and_over_budget_steps_down(self):
        for current, expected in [(15 * host.GIB, 9), (18 * host.GIB, 6)]:
            result = host.measured_ceiling(sample(slice_high=20 * host.GIB, slice_current=current), {}, 100)
            self.assertEqual(result['ceiling'], expected)
            self.assertEqual(result['limits']['slice_memory'], expected)

    def test_unlimited_slice_still_obeys_host_memory(self):
        result = host.measured_ceiling(sample(slice_high=None, mem_available=10 * host.GIB), {}, 100)
        self.assertEqual(result['ceiling'], 9)
        self.assertNotIn('slice_memory', result['limits'])

    def test_each_pressure_brake_holds_then_retires_and_recovers(self):
        for signal, value in [('memory_psi', 10), ('io_psi', 10), ('load', 17)]:
            with self.subTest(signal=signal):
                state = {}
                pressured = sample(**{signal: value})
                first = host.measured_ceiling(pressured, state, 100)
                self.assertEqual(first['ceiling'], 7)
                self.assertIn(signal, first['pressure'])
                state = json.loads(json.dumps(state))  # a new adapter process
                self.assertEqual(host.measured_ceiling(pressured, state, 159)['ceiling'], 7)
                self.assertEqual(host.measured_ceiling(pressured, state, 160)['ceiling'], 6)
                self.assertGreater(host.measured_ceiling(sample(), state, 161)['ceiling'], 7)
                self.assertIsNone(state['pressure_since'])

    def test_pressure_persistence_requires_continuous_valid_evidence(self):
        state = {}
        pressured = sample(memory_psi=20)
        host.measured_ceiling(pressured, state, 100)
        self.assertEqual(host.measured_ceiling(pressured, state, 1001)['ceiling'], 7)
        self.assertEqual(state['pressure_since'], 1001)
        # Backwards wall clock cannot turn one sample into persistent pressure.
        self.assertEqual(host.measured_ceiling(pressured, state, 900)['ceiling'], 7)
        self.assertEqual(state['pressure_since'], 900)

    def test_observed_tree_p95_and_bounded_history_survive_idle_cycle(self):
        state = {}
        values = [host.GIB] * 19 + [10 * host.GIB]
        result = host.measured_ceiling(sample(rss=values), state, 100)
        self.assertEqual(result['p95_worker_rss'], host.GIB)
        host.measured_ceiling(sample(rss=[4 * host.GIB] * 300), state, 101)
        self.assertEqual(len(state['rss_history']), 256)
        self.assertEqual(host.measured_ceiling(sample(rss=[]), state, 102)['p95_worker_rss'], 4 * host.GIB)
        self.assertEqual(host.measured_ceiling(sample(rss=[]), state, 100 + host.HISTORY_SECONDS + 2)['p95_worker_rss'], host.COLD_RSS)

    def test_empty_host_uses_memory_cost_fallback_not_fixed_worker_cap(self):
        empty = sample(counts={'a': 0, 'b': 0, 'c': 0}, unmanaged=0, rss=[],
                       mem_available=host.MEM_RESERVE + host.COLD_RSS * 5,
                       slice_high=None)
        result = host.measured_ceiling(empty, {}, 100)
        self.assertEqual(result['ceiling'], 5)
        self.assertEqual(result['rss_basis'], 'cold_fallback')

    def test_inactive_slice_is_a_zero_population_not_missing_evidence(self):
        # A drained pool leaves needle.slice inactive; systemd then reports
        # "[not set]" for both properties. That measures an empty slice --
        # int('[not set]') must not become a probe failure, or an idle host
        # could never publish a ceiling (gmktec WP32 rollout).
        self.assertEqual(
            host.slice_population({'MemoryCurrent': '[not set]', 'MemoryHigh': 'infinity'}),
            (0, None))
        self.assertEqual(
            host.slice_population({'MemoryCurrent': '[not set]', 'MemoryHigh': '[not set]'}),
            (0, None))
        self.assertEqual(
            host.slice_population({'MemoryCurrent': str(20 * host.GIB), 'MemoryHigh': str(100 * host.GIB)}),
            (20 * host.GIB, 100 * host.GIB))


class SplitTests(unittest.TestCase):
    def test_weighted_split_and_quota_cap_redistribution(self):
        self.assertEqual(host.split_targets({'a': 10, 'b': 10}, {'a': 3, 'b': 1}, 8), {'a': 6, 'b': 2})
        self.assertEqual(host.split_targets({'a': 2, 'b': 10}, {'a': 3, 'b': 1}, 8), {'a': 2, 'b': 6})
        self.assertEqual(host.split_targets({'a': 10, 'b': 10}, {'a': 0, 'b': 1}, 8), {'a': 0, 'b': 8})

    def test_small_integer_allocations_never_exceed_targets_or_ceiling(self):
        for a in range(5):
            for b in range(5):
                for ceiling in range(10):
                    for weights in ({'a': 0, 'b': 0}, {'a': 3, 'b': 1}, {'a': 0, 'b': 1}):
                        result = host.split_targets({'a': a, 'b': b}, weights, ceiling)
                        self.assertEqual(sum(result.values()), min(a + b, ceiling))
                        self.assertTrue(0 <= result['a'] <= a)
                        self.assertTrue(0 <= result['b'] <= b)


class ArbitrationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.config = {'state_path': str(self.base / 'state.json'), 'pools': {}}
        for key in ('a', 'b', 'c'):
            path = self.base / (key + '.json')
            atomic_json(path, {'observed_at': '1970-01-01T00:16:40+00:00', 'ready_bead_count': 4})
            self.config['pools'][key] = {'demand_state': str(path), 'prefixes': [key + '-']}
        self.empty = sample(counts={'a': 0, 'b': 0, 'c': 0}, unmanaged=0, rss=[host.GIB],
                            mem_available=host.MEM_RESERVE + 4 * host.GIB, slice_high=None)

    def test_pending_launch_reserves_space_until_pid_appears_or_expiry(self):
        state = {}
        first = host.decide(self.config, 'a', 4, state, self.empty, 1000)
        self.assertEqual(first['granted'], 4)
        second = host.decide(self.config, 'b', 4, state, self.empty, 1001)
        self.assertEqual(second['allocations'], {'a': 2, 'b': 2, 'c': 0})
        self.assertEqual(second['granted'], 0)  # the first start is still pending
        host.decide(self.config, 'a', 0, state, self.empty, 1002)
        self.assertEqual(host.decide(self.config, 'b', 4, state, self.empty, 1003)['granted'], 0)
        self.assertEqual(state['reservations']['a'], {'count': 4, 'at': 1000})
        # Observing the delayed PIDs settles the reservation, but those live
        # workers still consume every slot until retirement really completes.
        occupied = {**self.empty, 'counts': {'a': 4, 'b': 0, 'c': 0},
                    'mem_available': host.MEM_RESERVE}
        self.assertEqual(host.decide(self.config, 'b', 4, state, occupied, 1004)['granted'], 0)
        self.assertNotIn('a', state['reservations'])
        self.assertEqual(host.decide(self.config, 'b', 4, state, self.empty, 1005)['granted'], 4)

    def test_own_pending_launch_cannot_be_repeated_and_expires_without_renewal(self):
        state = {}
        self.assertEqual(host.decide(self.config, 'a', 4, state, self.empty, 1000)['granted'], 4)
        self.assertEqual(host.decide(self.config, 'a', 4, state, self.empty, 1001)['granted'], 0)
        self.assertEqual(state['reservations']['a']['at'], 1000)
        atomic_json(self.config['pools']['a']['demand_state'],
                    {'observed_at': '1970-01-01T00:31:41+00:00', 'ready_bead_count': 4})
        self.assertEqual(host.decide(self.config, 'a', 4, state, self.empty, 1901)['granted'], 4)

    def test_unknown_demand_holds_existing_pool_before_allocating_remaining_room(self):
        atomic_json(self.config['pools']['a']['demand_state'], {})
        state = {}
        occupied = {**self.empty, 'counts': {'a': 2, 'b': 0, 'c': 0},
                    'mem_available': host.MEM_RESERVE + 2 * host.GIB}
        first = host.decide(self.config, 'a', 4, state, occupied, 1000)
        self.assertEqual(first['granted'], 2)
        second = host.decide(self.config, 'b', 4, state, occupied, 1001)
        self.assertEqual(second['allocations'], {'a': 2, 'b': 2, 'c': 0})
        self.assertEqual(second['granted'], 2)

    def test_busy_retirees_and_unmanaged_workers_do_not_free_admission(self):
        state = {}
        busy = {**self.empty, 'counts': {'a': 4, 'b': 0, 'c': 0},
                'mem_available': host.MEM_RESERVE, 'unmanaged': 0}
        host.decide(self.config, 'a', 0, state, busy, 1000)
        result = host.decide(self.config, 'b', 4, state, busy, 1001)
        self.assertEqual(result['granted'], 0)
        free = {**self.empty, 'unmanaged': 2,
                'mem_available': host.MEM_RESERVE + 2 * host.GIB}
        self.assertEqual(host.decide(self.config, 'b', 4, {}, free, 1002)['granted'], 2)

    def test_pressure_blocks_cross_pool_scale_up_even_when_total_can_rebalance(self):
        state = {'requests': {'a': {'target': 0, 'at': 1000}}}
        pressured = {**self.empty, 'counts': {'a': 4, 'b': 0, 'c': 0}, 'io_psi': 20}
        result = host.decide(self.config, 'b', 4, state, pressured, 1000)
        self.assertEqual(result['granted'], 0)

    def test_unknown_stale_future_or_malformed_demand_never_admits(self):
        path = self.config['pools']['a']['demand_state']
        for value in ({}, {'observed_at': '1970-01-01T00:00:00+00:00', 'ready_bead_count': 4},
                      {'observed_at': '2099-01-01T00:00:00+00:00', 'ready_bead_count': 4},
                      {'observed_at': '1970-01-01T00:16:40+00:00', 'ready_bead_count': True}):
            atomic_json(path, value)
            self.assertEqual(host.decide(self.config, 'a', 4, {}, self.empty, 1000)['granted'], 0)

    def test_shared_lock_covers_launch_and_reservation_is_durable_before_yield(self):
        path = self.base / 'config.json'
        atomic_json(path, self.config)
        settings = {'host_capacity': str(path), 'host_pool': 'a'}
        output = io.StringIO()
        with patch.object(host, 'probe', return_value=self.empty), patch.object(host.time, 'time', return_value=1000), redirect_stderr(output):
            with host.admission(settings, 4, current=0) as grant:
                self.assertEqual(grant, 4)
                self.assertEqual(json.loads(Path(self.config['state_path']).read_text())['reservations']['a']['count'], 4)
                with self.assertRaises(BlockingIOError):
                    with locked(self.config['state_path']):
                        self.fail('host launch lock was not held')
        event = json.loads(output.getvalue())
        self.assertEqual(event['event'], 'host_worker_ceiling')
        self.assertEqual(event['ceiling'], 4)
        self.assertEqual(os.stat(self.config['state_path']).st_mode & 0o777, 0o600)

    def test_observer_logs_each_cycle_and_probe_failure_holds_instead_of_admitting(self):
        path = self.base / 'config.json'
        atomic_json(path, self.config)
        settings = {'host_capacity': str(path), 'host_pool': 'a'}
        output = io.StringIO()
        with patch.object(host, 'probe', side_effect=PermissionError), redirect_stderr(output):
            with host.admission(settings, 8, current=2) as grant:
                self.assertEqual(grant, 2)
            with host.admission(settings, 1, current=2) as grant:
                self.assertEqual(grant, 1)
        self.assertEqual(len(output.getvalue().splitlines()), 2)
        self.assertIsNone(json.loads(output.getvalue().splitlines()[0])['ceiling'])

    def test_productive_adapters_log_observation_and_cannot_launch_under_pressure(self):
        import importlib.util
        script = Path(__file__).resolve().parents[2] / 'scripts/codex-productive.py'
        spec = importlib.util.spec_from_file_location('host_productive', script)
        productive = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(productive)
        path = self.base / 'config.json'
        atomic_json(path, self.config)
        settings = {'host_capacity': str(path), 'host_pool': 'a', 'max_workers': 4,
                    'actuator_state': str(self.base / 'fleet.json')}
        pressured = {**self.empty, 'memory_psi': 11}
        output = io.StringIO()
        with patch.object(host, 'probe', return_value=pressured), patch.object(host.time, 'time', return_value=1000), redirect_stderr(output), \
                patch.object(productive, 'workers', return_value=[]), \
                patch.object(productive, 'inspect_fleet', return_value=[]), \
                patch.object(productive, 'run') as run, patch.object(productive, 'demand') as demand:
            self.assertEqual(productive.observe(settings), {'current_workers': 0})
            productive.actuate(settings, 4)
        self.assertEqual(json.loads(Path(settings['actuator_state']).read_text())['desired'], 0)
        run.assert_not_called()
        demand.assert_not_called()
        self.assertEqual(len(output.getvalue().splitlines()), 2)


class ProcessTests(unittest.TestCase):
    def test_descendants_are_counted_once_nested_supervisors_and_stragglers(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            def process(pid, parent, pages, args, status='S'):
                path = root / str(pid)
                path.mkdir()
                fields = ['0'] * 22
                fields[0], fields[1], fields[21] = status, str(parent), str(pages)
                (path / 'stat').write_text(str(pid) + ' (worker with spaces) ' + ' '.join(fields))
                (path / 'cmdline').write_bytes(b'\0'.join(arg.encode() for arg in args))
            process(10, 1, 10, ['needle', 'run', '--agent', 'codex', '--identifier', 'a-1'])
            process(11, 10, 20, ['agent'])
            process(12, 11, 30, ['cargo'])
            process(13, 10, 5, ['needle', 'run', '--agent', 'codex', '--identifier', 'a-1'])
            process(20, 1, 40, ['needle-stable', 'run', '--agent', 'glm', '--identifier', 'straggler'])
            process(21, 20, 80, ['dead'], status='Z')
            process(30, 1, 100, ['herdr', 'server'])
            counts, unmanaged, rss = host.process_sample({'a': {'prefixes': ['a-']}}, root)
            self.assertEqual(counts, {'a': 1})
            self.assertEqual(unmanaged, 1)
            self.assertEqual(sorted(rss), [40 * os.sysconf('SC_PAGE_SIZE'), 65 * os.sysconf('SC_PAGE_SIZE')])
