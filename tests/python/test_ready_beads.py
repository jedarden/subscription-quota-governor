import hashlib
import json
from pathlib import Path
import subprocess
import shutil
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[2] / 'scripts'
sys.path.insert(0, str(SCRIPTS))
import subgov_ready_beads as demand

FIXTURE = SCRIPTS.parent / 'tests/fixtures/ready-beads/native-ready.json'


class ReadyBeadsTests(unittest.TestCase):
    def workspace(self, root, name='repo', policy='bead_cli:\n  backend: bead-rs\n'):
        path = root / name
        (path / '.beads').mkdir(parents=True)
        (path / '.beads/config.json').write_text('{}')
        (path / '.needle.yaml').write_text(policy)
        return path

    def settings(self, *paths):
        return {'host': 'codinghome', 'agent': 'codex-test', 'max_workers': 12,
                'demand': {'workspaces': [str(path) for path in paths]}}

    def test_yaml_flow_list_policy_is_accepted(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self.workspace(Path(tmp), policy='bead_cli:\n  backend: bead-rs\nstrands:\n  pluck:\n    exclude_labels: [deferred, human, blocked]\n')
            fields = demand.policy_fields(path / '.needle.yaml', {('strands', 'pluck', 'exclude_labels')})
            self.assertEqual(fields[('strands', 'pluck', 'exclude_labels')], ['deferred', 'human', 'blocked'])

    def test_unreadable_policy_blocks_only_that_workspace(self):
        with tempfile.TemporaryDirectory() as tmp:
            good = self.workspace(Path(tmp), 'good')
            bad = self.workspace(Path(tmp), 'bad', policy='bead_cli:\n  backend: bead-rs\nstrands:\n  pluck:\n    exclude_labels: [unterminated\n')
            with patch.object(demand, 'ready_issues', return_value=[]):
                result = demand.observe(self.settings(good, bad))
            self.assertIn({'workspace': str(bad), 'reason': 'policy_unreadable'}, result['blocked'])

    def test_native_fixture_filters_holds_and_counts_unlabelled_ready_beads(self):
        with tempfile.TemporaryDirectory() as temp:
            route = self.workspace(Path(temp))
            settings = self.settings(route)
            settings['demand']['external_holds'] = [
                {'workspace': str(route), 'id': 'fixture-held', 'reason': 'external dependency'}]
            result = subprocess.CompletedProcess([], 0, FIXTURE.read_text(), '')
            with patch.object(demand.subprocess, 'run', return_value=result) as run:
                measured = demand.observe(settings)
            self.assertEqual(measured['ready_bead_count'], 2)
            self.assertEqual(measured['eligible_backlog_capacity'], 2)
            self.assertEqual({job['id'] for job in measured['ready']}, {'fixture-ready', 'fixture-expired'})
            self.assertEqual(len(measured['blocked']), 5)
            self.assertEqual(run.call_args.args[0], ['bead', 'list', '--ready', '--json', '--limit', '999999'])
            self.assertEqual(run.call_args.kwargs['cwd'], str(route))

    def test_nonready_native_rows_never_become_capacity(self):
        with tempfile.TemporaryDirectory() as temp:
            route = self.workspace(Path(temp))
            base = json.loads(FIXTURE.read_text())[0]
            for change in ({'status': 'in_progress'}, {'status': 'deferred'}, {'status': 'closed'},
                           {'assignee': 'other'}, {'manual_blocked': True},
                           {'labels': ['external-hold:credential']}, {'external_hold': 'wait'}):
                with self.subTest(change=change), patch.object(demand, 'ready_issues', return_value=[{**base, **change}]):
                    self.assertEqual(demand.observe(self.settings(route))['eligible_backlog_capacity'], 0)
            # Dependency filtering belongs to the native ready frontier; never
            # supplement it with an unfiltered list/show or stale checkpoint.
            with patch.object(demand, 'ready_issues', return_value=[]):
                self.assertEqual(demand.observe(self.settings(route))['ready_bead_count'], 0)

    def test_workspace_slots_busy_claims_dedup_and_pool_ceiling(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            rust = self.workspace(root, 'rust')
            docs = self.workspace(root, 'docs', 'workspace:\n  max_workers: 3\n')
            (rust / 'Cargo.toml').touch()
            settings = self.settings(rust, rust, docs)
            issues = [{'id': f'job-{i}', 'status': 'open', 'labels': []} for i in range(10)]
            with patch.object(demand, 'ready_issues', return_value=issues + issues):
                measured = demand.observe(settings)
                self.assertEqual(measured['ready_bead_count'], 20)
                self.assertEqual(measured['eligible_backlog_capacity'], 4)
                busy = [{'workspace': str(rust), 'id': 'claimed'}]
                measured = demand.observe(settings, busy)
                self.assertEqual(measured['eligible_backlog_capacity'], 4)
                self.assertEqual(len(measured['ready']), 3)
                self.assertTrue(all(job['workspace'] == str(docs) for job in measured['ready']))
                settings['max_workers'] = 2
                self.assertEqual(demand.observe(settings, busy)['eligible_backlog_capacity'], 2)

    def test_backend_host_agent_gate_and_custom_label_restrictions(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            foreign = self.workspace(root, 'foreign', 'queue:\n  owner_host: lab\n')
            legacy = self.workspace(root, 'legacy', 'bead_cli:\n  backend: legacy\n')
            agent = self.workspace(root, 'agent')
            gate = self.workspace(root, 'gate')
            repo = self.workspace(root, 'normal', 'strands:\n  pluck:\n    exclude_labels:\n    - test-only\n')
            settings = self.settings(foreign, legacy, agent, gate, repo)
            settings['demand']['workspace_agents'] = {str(agent): ['claude-print']}
            with patch.object(demand, 'gate_degraded', side_effect=lambda s, p: 'workspace_gate_degraded' if p == str(gate) else None), \
                    patch.object(demand, 'ready_issues', return_value=[
                        {'id': 'excluded', 'status': 'open', 'labels': ['test-only']}]) as query:
                result = demand.observe(settings)
            self.assertEqual(result['eligible_backlog_capacity'], 0)
            query.assert_called_once_with(str(repo))

    def test_unreadable_gate_records_block_only_their_workspaces(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            empty = self.workspace(root, 'empty')
            truncated = self.workspace(root, 'truncated')
            healthy = self.workspace(root, 'healthy')
            absent = self.workspace(root, 'absent')
            degraded = self.workspace(root, 'degraded')
            gate_dir = root / 'gate-health'
            gate_dir.mkdir()
            for route, value in ((empty, ''), (truncated, '{"degraded":')):
                key = hashlib.sha256(str(route.resolve()).encode()).hexdigest()[:12]
                (gate_dir / f'{key}.json').write_text(value)
            key = hashlib.sha256(str(healthy.resolve()).encode()).hexdigest()[:12]
            (gate_dir / f'{key}.json').write_text('{"degraded": false}')
            key = hashlib.sha256(str(degraded.resolve()).encode()).hexdigest()[:12]
            (gate_dir / f'{key}.json').write_text('{"degraded": true}')
            settings = self.settings(empty, truncated, healthy, absent, degraded)
            settings['gate_health_dir'] = str(gate_dir)
            with patch.object(demand, 'ready_issues', side_effect=lambda route: [
                    {'id': Path(route).name + '-ready', 'status': 'open', 'labels': []}]) as query:
                measured = demand.observe(settings)
            self.assertEqual(measured['ready_bead_count'], 2)
            self.assertEqual(measured['eligible_backlog_capacity'], 2)
            self.assertEqual({job['id'] for job in measured['ready']}, {'healthy-ready', 'absent-ready'})
            self.assertEqual({(row['workspace'], row['reason']) for row in measured['blocked']},
                             {(str(empty), 'gate_state_unreadable'),
                              (str(truncated), 'gate_state_unreadable'),
                              (str(degraded), 'workspace_gate_degraded')})
            self.assertEqual({call.args[0] for call in query.call_args_list}, {str(healthy), str(absent)})

    def test_query_failure_malformed_or_truncated_output_is_unknown(self):
        with tempfile.TemporaryDirectory() as temp:
            route = self.workspace(Path(temp))
            for error in (subprocess.TimeoutExpired('bead', 8), subprocess.CalledProcessError(1, 'bead')):
                with patch.object(demand.subprocess, 'run', side_effect=error):
                    with self.assertRaises(type(error)):
                        demand.observe(self.settings(route))
            for output in ('{}', '', '[{}]', '[true]', '{"id":"not-array"}'):
                with patch.object(demand.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, output, '')):
                    with self.assertRaises(ValueError):
                        demand.observe(self.settings(route))
            with patch.object(demand, 'LIMIT', 1), patch.object(demand.subprocess, 'run', return_value=
                    subprocess.CompletedProcess([], 0, '[{"id":"truncated"}]', '')):
                with self.assertRaises(ValueError):
                    demand.observe(self.settings(route))

    def test_discovery_respects_needle_pins_and_skips_hidden_build_and_symlink_trees(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            repo = self.workspace(root, 'nested/repo')
            for name in ('.scratch/repo', 'target/repo', 'node_modules/repo'):
                self.workspace(root, name)
            (root / 'linked').symlink_to(repo, target_is_directory=True)
            self.assertEqual(demand.discover(root), {str(repo)})
            config = root / 'config.yaml'
            config.write_text(f'strands:\n  explore:\n    workspaces:\n    - {repo}\n    workspace_root: /nonexistent\n')
            settings = {'agent': 'codex-test', 'demand': {'needle_config': str(config)}}
            self.assertEqual(demand.routes(settings), [str(repo)])
            config.write_text(f'strands:\n  explore:\n    workspaces: []\n    workspace_root: {root}\n')
            self.assertEqual(demand.routes(settings), [str(repo)])

    def test_complex_duplicate_or_malformed_policy_fails_closed(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / 'policy.yaml'
            for text in ('queue: {owner_host: lab}\n', 'queue:\n  owner_host: lab\n  owner_host: codinghome\n',
                         'bead_cli:\n  backend: &backend bead-rs\n'):
                path.write_text(text)
                with self.assertRaises(ValueError):
                    demand.workspace_policy(path)

    def test_global_exclusions_are_inherited_and_native_store_needs_no_yaml(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            route = self.workspace(root)
            (route / '.needle.yaml').unlink()
            config = root / 'config.yaml'
            config.write_text('strands:\n  pluck:\n    exclude_labels: ["reserved"]\n')
            settings = self.settings(route)
            settings['demand']['needle_config'] = str(config)
            with patch.object(demand, 'ready_issues', return_value=[{'id': 'filtered', 'status': 'open', 'labels': ['reserved']}]):
                self.assertEqual(demand.observe(settings)['eligible_backlog_capacity'], 0)

    @unittest.skipUnless(shutil.which('bead'), 'native bead-rs CLI not installed')
    def test_native_store_dependency_assignment_and_manual_block_frontier(self):
        with tempfile.TemporaryDirectory(prefix='subgov-ready-native-', dir='/tmp') as temp:
            root = Path(temp)
            def bead(*args):
                return subprocess.run(['bead', *args], cwd=root, capture_output=True, text=True,
                                      check=True, timeout=15).stdout.strip()
            bead('init', '--prefix', 'fixture')
            blocker = bead('create', '--title', 'External prerequisite', '--label', 'human')
            bead('create', '--title', 'Blocked dependent', '--depends-on', blocker)
            bead('create', '--title', 'Assigned task', '--assignee', 'other-worker')
            manual = bead('create', '--title', 'Manual block')
            bead('update', manual, '--status', 'blocked')
            ready = bead('create', '--title', 'Available task')
            result = demand.observe(self.settings(root))
            self.assertEqual(result['eligible_backlog_capacity'], 1)
            self.assertEqual([job['id'] for job in result['ready']], [ready])

    def test_legacy_external_hold_survives_migration(self):
        with tempfile.TemporaryDirectory() as temp:
            route = self.workspace(Path(temp))
            settings = self.settings(route)
            settings['work'] = [{'workspace': str(route), 'id': 'held', 'external_hold': 'wait'}]
            with patch.object(demand, 'ready_issues', return_value=[{'id': 'held', 'status': 'open', 'labels': []}]):
                self.assertEqual(demand.observe(settings)['eligible_backlog_capacity'], 0)

    def test_seed_order_spreads_new_workers_across_workspaces(self):
        # Plan §26.5: each new worker takes the ready workspace with the
        # fewest running workers, not the first route in enumeration order.
        with tempfile.TemporaryDirectory() as temp:
            first, second = self.workspace(Path(temp), 'a-repo'), self.workspace(Path(temp), 'b-repo')
            jobs = [{'workspace': str(first), 'id': 'j1'}, {'workspace': str(first), 'id': 'j2'},
                    {'workspace': str(second), 'id': 'j3'}]
            seeds = demand.seed_order(jobs, {})
            self.assertEqual(seeds.count(str(first)), 2)
            self.assertEqual(seeds.count(str(second)), 1)
            self.assertEqual(seeds[0], str(first))
            self.assertEqual(seeds[1], str(second), seeds)
            self.assertEqual(seeds[2], str(first))

    def test_seed_order_prefers_the_least_occupied_ready_workspace(self):
        with tempfile.TemporaryDirectory() as temp:
            busy_repo, idle_repo = self.workspace(Path(temp), 'a-repo'), self.workspace(Path(temp), 'b-repo')
            jobs = [{'workspace': str(busy_repo), 'id': 'j1'}, {'workspace': str(idle_repo), 'id': 'j2'}]
            seeds = demand.seed_order(jobs, {str(busy_repo): 2, str(idle_repo): 0})
            self.assertEqual(seeds, [str(idle_repo), str(busy_repo)])

    def test_seed_order_ties_resolve_deterministically_and_normalize_paths(self):
        with tempfile.TemporaryDirectory() as temp:
            first, second = self.workspace(Path(temp), 'a-repo'), self.workspace(Path(temp), 'b-repo')
            jobs = [{'workspace': str(first), 'id': 'j1'}, {'workspace': str(second), 'id': 'j2'}]
            # Unresolved keys on the occupancy side must not skew the pick.
            seeds = demand.seed_order(jobs, {str(first) + '/./': 0, str(second): 0})
            self.assertEqual(seeds, [str(first), str(second)])

    def test_seed_order_returns_one_seed_per_ready_job_and_no_bare_extras(self):
        with tempfile.TemporaryDirectory() as temp:
            route = self.workspace(Path(temp))
            self.assertEqual(demand.seed_order([], {}), [])
            jobs = [{'workspace': str(route), 'id': 'j1'}]
            self.assertEqual(demand.seed_order(jobs, {str(route): 9}), [str(route)])


if __name__ == '__main__':
    unittest.main()
