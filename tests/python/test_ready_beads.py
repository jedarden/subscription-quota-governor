import fcntl
import hashlib
import json
import os
from pathlib import Path
import threading
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

    @staticmethod
    def admitted(*args, **kwargs):
        return {'state': 'admitted'}

    @staticmethod
    def status_payload(relationship='aligned', consistent=True, ready=None, reasons=None):
        payload = {'relationship': relationship, 'ready_to_commit': ready if ready is not None else True,
                   'checkpoint_present': True}
        if consistent is not None:
            payload['checkpoint_consistent'] = consistent
        if reasons:
            payload['not_ready_reasons'] = reasons
        return payload

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
            with patch.object(demand, 'dispatch_health', self.admitted), \
                    patch.object(demand.subprocess, 'run', return_value=result) as run:
                measured = demand.observe(settings)
            self.assertEqual(measured['ready_bead_count'], 2)
            self.assertEqual(measured['eligible_backlog_capacity'], 2)
            self.assertEqual(measured['dispatch_health_version'], demand.DISPATCH_HEALTH_VERSION)
            self.assertEqual({job['id'] for job in measured['ready']}, {'fixture-ready', 'fixture-expired'})
            self.assertEqual(measured['workspaces'][0]['dispatch_health'], 'admitted')
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
            docs = self.workspace(root, 'docs', 'bead_cli:\n  backend: bead-rs\nworkspace:\n  max_workers: 3\n')
            (rust / 'Cargo.toml').touch()
            settings = self.settings(rust, rust, docs)
            issues = [{'id': f'job-{i}', 'status': 'open', 'labels': []} for i in range(10)]
            with patch.object(demand, 'dispatch_health', self.admitted), \
                    patch.object(demand, 'ready_issues', return_value=issues + issues):
                measured = demand.observe(settings)
                self.assertEqual(measured['ready_bead_count'], 20)
                self.assertEqual(measured['eligible_backlog_capacity'], 4)
                busy = [{'workspace': str(rust), 'id': 'claimed'}]
                measured = demand.observe(settings, busy)
                self.assertEqual(measured['eligible_backlog_capacity'], 4)
                self.assertEqual(measured['busy_count'], 1)
                self.assertEqual(len(measured['ready']), 3)
                self.assertTrue(all(job['workspace'] == str(docs) for job in measured['ready']))
                settings['max_workers'] = 2
                self.assertEqual(demand.observe(settings, busy)['eligible_backlog_capacity'], 2)

    def test_backend_host_agent_gate_and_custom_label_restrictions(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            foreign = self.workspace(root, 'foreign', 'bead_cli:\n  backend: bead-rs\nqueue:\n  owner_host: lab\n')
            legacy = self.workspace(root, 'legacy', 'bead_cli:\n  backend: legacy\n')
            agent = self.workspace(root, 'agent')
            gate = self.workspace(root, 'gate')
            repo = self.workspace(root, 'normal',
                                  'bead_cli:\n  backend: bead-rs\nstrands:\n  pluck:\n    exclude_labels:\n    - test-only\n')
            settings = self.settings(foreign, legacy, agent, gate, repo)
            settings['demand']['workspace_agents'] = {str(agent): ['claude-print']}
            with patch.object(demand, 'gate_degraded', side_effect=lambda s, p: 'workspace_gate_degraded' if p == str(gate) else None), \
                    patch.object(demand, 'dispatch_health', self.admitted), \
                    patch.object(demand, 'ready_issues', return_value=[
                        {'id': 'excluded', 'status': 'open', 'labels': ['test-only']}]) as query:
                result = demand.observe(settings)
            self.assertEqual(result['eligible_backlog_capacity'], 0)
            self.assertIn({'workspace': str(foreign), 'reason': 'queue_owner_mismatch'}, result['blocked'])
            self.assertIn({'workspace': str(legacy), 'reason': 'backend_binding_invalid'}, result['blocked'])
            query.assert_called_once_with(str(repo), demand.DEFAULT_READY_TIMEOUT)

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
            with patch.object(demand, 'dispatch_health', self.admitted), \
                    patch.object(demand, 'ready_issues', side_effect=lambda route, timeout: [
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

    def test_slow_or_malformed_store_is_blocked_and_never_blanks_fleet_demand(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            slow, malformed, healthy = (self.workspace(root, name)
                                        for name in ('slow', 'malformed', 'healthy'))
            settings = self.settings(slow, malformed, healthy)

            def flaky(args, cwd=None, **kwargs):
                if args[:2] == ['bead', 'sync']:
                    # Health runs only for the route carrying demand.
                    assert cwd == str(healthy)
                    return subprocess.CompletedProcess([], 0, json.dumps(self.status_payload()), '')
                if cwd == str(slow):
                    raise subprocess.TimeoutExpired(args, 8)
                if cwd == str(malformed):
                    return subprocess.CompletedProcess(args, 0, '{"id": "not-an-array"}', '')
                return subprocess.CompletedProcess([], 0, json.dumps(
                    [{'id': 'healthy-ready', 'status': 'open', 'labels': []}]), '')

            with patch.object(demand.subprocess, 'run', side_effect=flaky) as run:
                measured = demand.observe(settings)
            self.assertEqual(measured['ready_bead_count'], 1)
            self.assertEqual([job['id'] for job in measured['ready']], ['healthy-ready'])
            self.assertEqual({(row['workspace'], row['reason'], row['error']) for row in measured['blocked']
                              if row['reason'] == 'ready_query_unavailable'},
                             {(str(slow), 'ready_query_unavailable', 'TimeoutExpired'),
                              (str(malformed), 'ready_query_unavailable', 'ValueError')})
            self.assertEqual({row['workspace'] for row in measured['blocked']} & {str(healthy)}, set())
            ready_calls = [call for call in run.call_args_list if call.args[0][:2] == ['bead', 'list']]
            self.assertEqual(sorted(call.kwargs['timeout'] for call in ready_calls), [8, 8, 8])
            health_calls = [call for call in run.call_args_list if call.args[0][:2] == ['bead', 'sync']]
            self.assertEqual([call.kwargs['timeout'] for call in health_calls],
                             [demand.DEFAULT_HEALTH_TIMEOUT])
            # A failed workspace keeps its busy claims as retention capacity and
            # contributes no seeds.
            busy = [{'workspace': str(slow), 'id': 'claimed'}, {'workspace': str(healthy), 'id': 'live'}]
            with patch.object(demand.subprocess, 'run', side_effect=flaky):
                measured = demand.observe(settings, busy)
            self.assertEqual(measured['eligible_backlog_capacity'], 3)
            self.assertEqual([job['id'] for job in measured['ready']], ['healthy-ready'])

    def test_all_selected_stores_failing_still_makes_demand_unknown(self):
        with tempfile.TemporaryDirectory() as temp:
            first, second = (self.workspace(Path(temp), name) for name in ('one', 'two'))
            settings = self.settings(first, second)
            with patch.object(demand.subprocess, 'run',
                              side_effect=subprocess.TimeoutExpired('bead', 8)):
                with self.assertRaises(subprocess.TimeoutExpired):
                    demand.observe(settings)
            with patch.object(demand.subprocess, 'run', return_value=
                    subprocess.CompletedProcess([], 0, 'not json at all', '')):
                with self.assertRaises(ValueError):
                    demand.observe(settings)

    def test_backend_binding_authority_gates_the_frontier(self):
        # NEEDLE open_configured refuses dispatch on any non-authoritative
        # binding; those routes must contribute no slots even with ready beads.
        # The admissible spellings are BeadBackend::Bead's own: bead-rs, bead
        # and the deprecated br alias all resolve to the same bead binary.
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            policies = {'noyaml': None, 'auto': 'bead_cli:\n  backend: auto\n',
                        'bf': 'bead_cli:\n  backend: bf\n',
                        'forge': 'bead_cli:\n  backend: bead-forge\n'}
            aliases = {'alias-bead': 'bead_cli:\n  backend: bead\n',
                       'alias-br': 'bead_cli:\n  backend: br\n'}
            refused = {}
            bound = [self.workspace(root, 'healthy')]
            for name, policy in policies.items():
                route = self.workspace(root, name)
                if policy is None:
                    (route / '.needle.yaml').unlink()
                else:
                    (route / '.needle.yaml').write_text(policy)
                refused[name] = route
            for name, policy in aliases.items():
                route = self.workspace(root, name)
                (route / '.needle.yaml').write_text(policy)
                bound.append(route)
            settings = self.settings(*bound, *refused.values())
            issue = {'id': 'ready-now', 'status': 'open', 'labels': []}
            with patch.object(demand, 'dispatch_health', self.admitted), \
                    patch.object(demand, 'ready_issues', return_value=[issue]) as query:
                measured = demand.observe(settings)
            self.assertEqual(measured['ready_bead_count'], 3)
            self.assertEqual(sorted(job['id'] for job in measured['ready']),
                             ['ready-now'] * 3)
            self.assertEqual({(row['workspace'], row['reason']) for row in measured['blocked']},
                             {(str(refused['noyaml']), 'backend_binding_missing'),
                              (str(refused['auto']), 'backend_binding_missing'),
                              (str(refused['bf']), 'backend_binding_retired'),
                              (str(refused['forge']), 'backend_binding_retired')})
            self.assertEqual(sorted(call.args[0] for call in query.call_args_list),
                             sorted(str(route) for route in bound))
            self.assertEqual([summary['dispatch_health'] for summary in measured['workspaces']],
                             ['admitted'] * 3)

    def test_dispatch_health_refuses_unpublished_and_quarantined_checkpoints(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            behind = self.workspace(root, 'behind')
            absent = self.workspace(root, 'absent')
            remote = self.workspace(root, 'remote')
            diverged = self.workspace(root, 'diverged')
            dirty = self.workspace(root, 'dirty')
            quarantined = self.workspace(root, 'quarantined')
            legacy_ok = self.workspace(root, 'legacy-ok')
            legacy_stale = self.workspace(root, 'legacy-stale')
            healthy = self.workspace(root, 'healthy')
            settings = self.settings(behind, absent, remote, diverged, dirty,
                                     quarantined, legacy_ok, legacy_stale, healthy)
            payloads = {
                # Repairable relationships: NEEDLE flushes/reconciles these at
                # claim time, so read-only claimability is unknown, not refused.
                str(behind): self.status_payload(relationship='behind'),
                str(absent): self.status_payload(relationship='absent'),
                str(remote): self.status_payload(relationship='remote-advanced'),
                # Not a remedy state: NEEDLE hard-bails, a verified refusal.
                str(diverged): self.status_payload(relationship='covered-ahead-integrity-failure'),
                str(dirty): self.status_payload(consistent=False, reasons=[
                    'checkpoint dirty: covered=9695, live=9696',
                    'checkpoint not fully committed (Git cannot reach every published file): ignored: .beads/checkpoint/objects/gen-61e5d711f44913a50851be74ccc2a31c.tmp']),
                str(quarantined): self.status_payload(consistent=False, reasons=[
                    'Conflict: secret_quarantined: 6 blocking finding(s), rules: unavailable, ruleset: 4, '
                    'retained-generation coverage incomplete: false; inspect '
                    "'bead doctor --scope secrets' and run 'bead redact --all-blocking'"]),
                str(legacy_ok): self.status_payload(consistent=None, ready=True),
                str(legacy_stale): self.status_payload(consistent=None, ready=False),
                str(healthy): self.status_payload(),
            }

            def status(route, timeout=demand.DEFAULT_HEALTH_TIMEOUT):
                return payloads[route]

            with patch.object(demand, 'sync_status', side_effect=status), \
                    patch.object(demand, 'sync_lock_busy', return_value=False), \
                    patch.object(demand, 'ready_issues', return_value=[
                        {'id': 'job', 'status': 'open', 'labels': []}]):
                measured = demand.observe(settings)
            self.assertEqual(measured['ready_bead_count'], 2)
            self.assertEqual({job['workspace'] for job in measured['ready']},
                             {str(healthy), str(legacy_ok)})
            self.assertEqual({(row['workspace'], row['reason'], row.get('detail'))
                              for row in measured['blocked']
                              if row['reason'].startswith('checkpoint')},
                             {(str(behind), 'checkpoint_repair_pending', 'behind'),
                              (str(absent), 'checkpoint_repair_pending', 'absent'),
                              (str(remote), 'checkpoint_repair_pending', 'remote-advanced'),
                              (str(diverged), 'checkpoint_not_aligned',
                               'covered-ahead-integrity-failure'),
                              (str(dirty), 'checkpoint_publication_unverified',
                               'checkpoint dirty: covered=9695, live=9696; checkpoint not fully '
                               'committed (Git cannot reach every published file): ignored: '
                               '.beads/checkpoint/objects/gen-61e5d711f44913a50851be74ccc2a31c.tmp'),
                              (str(quarantined), 'checkpoint_publication_unverified',
                               "Conflict: secret_quarantined: 6 blocking finding(s), rules: "
                               "unavailable, ruleset: 4, retained-generation coverage incomplete: "
                               "false; inspect 'bead doctor --scope secrets' and run "
                               "'bead redact --all-blocking'"),
                              (str(legacy_stale), 'checkpoint_publication_unverified',
                               None)})
            quarantine_row = next(row for row in measured['blocked'] if row['workspace'] == str(quarantined))
            self.assertIn('secret_quarantined', quarantine_row['detail'])
            self.assertLessEqual(len(quarantine_row['detail']), 240)
            self.assertEqual({summary['workspace']: summary['dispatch_health']
                              for summary in measured['workspaces']},
                             {str(behind): 'checkpoint_repair_pending',
                              str(absent): 'checkpoint_repair_pending',
                              str(remote): 'checkpoint_repair_pending',
                              str(diverged): 'checkpoint_not_aligned',
                              str(dirty): 'checkpoint_publication_unverified',
                              str(quarantined): 'checkpoint_publication_unverified',
                              str(legacy_ok): 'admitted',
                              str(legacy_stale): 'checkpoint_publication_unverified',
                              str(healthy): 'admitted'})

    def test_unavailable_dispatch_health_is_unknown_and_keeps_busy_retention(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            broken, healthy = self.workspace(root, 'broken'), self.workspace(root, 'healthy')
            settings = self.settings(broken, healthy)
            issue = {'id': 'job', 'status': 'open', 'labels': []}
            for failure in (subprocess.TimeoutExpired('bead', 30), subprocess.CalledProcessError(3, 'bead'),
                            ValueError('incomplete sync status contract')):
                def status(route, timeout=demand.DEFAULT_HEALTH_TIMEOUT, error=failure):
                    if route == str(broken):
                        raise error
                    return self.status_payload()
                with patch.object(demand, 'sync_status', side_effect=status), \
                        patch.object(demand, 'ready_issues', return_value=[issue]):
                    measured = demand.observe(settings)
                self.assertEqual(measured['ready_bead_count'], 1)
                self.assertEqual([job['workspace'] for job in measured['ready']], [str(healthy)])
                self.assertEqual(measured['blocked'],
                                 [{'workspace': str(broken), 'reason': 'dispatch_health_unavailable',
                                   'error': type(failure).__name__}])
                self.assertEqual({summary['workspace']: summary['dispatch_health']
                                  for summary in measured['workspaces']},
                                 {str(broken): 'dispatch_health_unavailable', str(healthy): 'admitted'})
            # A route whose claimability cannot be verified keeps its busy
            # claims as retention evidence and contributes no seeds. The
            # sibling route is fully busy, so it is never probed.
            def status(route, timeout=demand.DEFAULT_HEALTH_TIMEOUT):
                raise subprocess.TimeoutExpired('bead', 30)
            busy = [{'workspace': str(broken), 'id': 'claimed'}, {'workspace': str(healthy), 'id': 'job'}]
            with patch.object(demand, 'sync_status', side_effect=status), \
                    patch.object(demand, 'ready_issues', return_value=[issue]):
                measured = demand.observe(settings, busy)
            self.assertEqual(measured['eligible_backlog_capacity'], 2)
            self.assertEqual(measured['ready'], [])
            # When every selected route fails to verify claimability, demand is
            # unknown — never a published verified-empty zero.
            settings = self.settings(broken)
            with patch.object(demand, 'sync_status', side_effect=status), \
                    patch.object(demand, 'ready_issues', return_value=[issue]):
                with self.assertRaises(subprocess.TimeoutExpired):
                    demand.observe(settings)
            # A route whose repair NEEDLE would run at claim time is still
            # pending is equally unknown: alone it must withhold demand
            # instead of publishing a verified-empty zero for repairable work.
            def pending(route, timeout=demand.DEFAULT_HEALTH_TIMEOUT):
                return self.status_payload(relationship='behind')
            with patch.object(demand, 'sync_status', side_effect=pending), \
                    patch.object(demand, 'ready_issues', return_value=[issue]):
                with self.assertRaises(ValueError):
                    demand.observe(settings)

    def test_sync_lock_busy_refuses_slots_without_touching_the_lock(self):
        with tempfile.TemporaryDirectory() as temp:
            route = self.workspace(Path(temp))
            lock = route / '.beads/needle-sync.lock'
            self.assertFalse(demand.sync_lock_busy(route, budget=0))
            self.assertFalse(lock.exists())  # the probe never creates the file
            lock.write_text('')
            handle = os.open(lock, os.O_RDONLY)
            try:
                fcntl.flock(handle, fcntl.LOCK_EX)
                # A lock that stays held past the probe budget is the same
                # workspace_sync_busy NEEDLE refuses after its own deadline.
                self.assertTrue(demand.sync_lock_busy(route, budget=0))
                settings = self.settings(route)
                settings['demand']['dispatch_health_timeout_seconds'] = 1
                with patch.object(demand, 'sync_status',
                                  side_effect=lambda route, timeout=demand.DEFAULT_HEALTH_TIMEOUT:
                                  self.status_payload()), \
                        patch.object(demand, 'ready_issues', return_value=[
                            {'id': 'job', 'status': 'open', 'labels': []}]):
                    measured = demand.observe(settings)
                self.assertEqual(measured['ready_bead_count'], 0)
                self.assertEqual(measured['blocked'], [{'workspace': str(route),
                                                        'reason': 'workspace_sync_busy'}])
                self.assertEqual(measured['workspaces'][0]['dispatch_health'], 'workspace_sync_busy')
                # A lock held only for a flush-duration is waited out, the way
                # NEEDLE's sync_guard waits a full sync budget before refusing:
                # the probe must admit once the publishing worker releases.
                release = threading.Timer(0.4, lambda: fcntl.flock(handle, fcntl.LOCK_UN))
                release.start()
                try:
                    self.assertFalse(demand.sync_lock_busy(route, budget=5))
                finally:
                    release.join()
            finally:
                fcntl.flock(handle, fcntl.LOCK_UN)
                os.close(handle)
            self.assertFalse(demand.sync_lock_busy(route, budget=0))

    def test_dispatch_health_timeout_is_configurable_and_validated(self):
        with tempfile.TemporaryDirectory() as temp:
            route = self.workspace(Path(temp))
            settings = self.settings(route)
            settings['demand']['dispatch_health_timeout_seconds'] = 7
            with patch.object(demand, 'sync_status',
                              side_effect=lambda route, timeout=demand.DEFAULT_HEALTH_TIMEOUT:
                              self.status_payload()) as status, \
                    patch.object(demand, 'ready_issues', return_value=[
                        {'id': 'job', 'status': 'open', 'labels': []}]):
                demand.observe(settings)
            self.assertEqual(status.call_args.args[1], 7)
            for bad in (0, -1, '30', 2.5, None):
                settings['demand']['dispatch_health_timeout_seconds'] = bad
                with self.assertRaises(ValueError):
                    demand.observe(settings)

    def test_ready_timeout_is_configurable_and_validated(self):
        with tempfile.TemporaryDirectory() as temp:
            route = self.workspace(Path(temp))
            settings = self.settings(route)
            settings['demand']['ready_timeout_seconds'] = 31
            with patch.object(demand.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, '[]', '')) as run:
                demand.observe(settings)
            self.assertEqual(run.call_args.kwargs['timeout'], 31)
            for bad in (0, -1, '8', 2.5, None):
                settings['demand']['ready_timeout_seconds'] = bad
                with self.assertRaises(ValueError):
                    demand.observe(settings)

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

    def test_global_exclusions_are_inherited_and_unbound_store_is_never_queried(self):
        # NEEDLE dispatch requires an authoritative bead_cli.backend binding:
        # a native store without one is refused before its frontier is read.
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            route = self.workspace(root)
            unbound = self.workspace(root, 'unbound')
            (unbound / '.needle.yaml').unlink()
            config = root / 'config.yaml'
            config.write_text('strands:\n  pluck:\n    exclude_labels: ["reserved"]\n')
            settings = self.settings(route, unbound)
            settings['demand']['needle_config'] = str(config)
            with patch.object(demand, 'dispatch_health', self.admitted), \
                    patch.object(demand, 'ready_issues', return_value=[
                        {'id': 'filtered', 'status': 'open', 'labels': ['reserved']}]) as query:
                measured = demand.observe(settings)
            self.assertEqual(measured['eligible_backlog_capacity'], 0)
            self.assertEqual({row['workspace'] for row in measured['blocked']
                              if row['reason'] == 'backend_binding_missing'}, {str(unbound)})
            self.assertEqual([call.args[0] for call in query.call_args_list], [str(route)])

    @unittest.skipUnless(shutil.which('bead'), 'native bead-rs CLI not installed')
    def test_native_store_dependency_assignment_and_manual_block_frontier(self):
        with tempfile.TemporaryDirectory(prefix='subgov-ready-native-', dir='/tmp') as temp:
            root = Path(temp)
            def bead(*args):
                return subprocess.run(['bead', *args], cwd=root, capture_output=True, text=True,
                                      check=True, timeout=15).stdout.strip()
            bead('init', '--prefix', 'fixture')
            # Real end-to-end: the explicit authoritative binding and the live
            # `bead sync status` admission are both part of the frontier gate.
            (root / '.needle.yaml').write_text('bead_cli:\n  backend: bead-rs\n')
            blocker = bead('create', '--title', 'External prerequisite', '--label', 'human')
            bead('create', '--title', 'Blocked dependent', '--depends-on', blocker)
            bead('create', '--title', 'Assigned task', '--assignee', 'other-worker')
            manual = bead('create', '--title', 'Manual block')
            bead('update', manual, '--status', 'blocked')
            ready = bead('create', '--title', 'Available task')
            result = demand.observe(self.settings(root))
            self.assertEqual(result['eligible_backlog_capacity'], 1)
            self.assertEqual(result['workspaces'][0]['dispatch_health'], 'admitted')
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
