import importlib.util
import json
from pathlib import Path
import signal
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[2] / "scripts"
sys.path.insert(0, str(SCRIPTS))
spec = importlib.util.spec_from_file_location("codex_productive", SCRIPTS / "codex-productive.py")
adapter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(adapter)
import subgov_ready_beads as ready_beads


class ProductiveTests(unittest.TestCase):
    def settings(self, root):
        return {"host": "codinghome", "agent": "codex-test", "prefix": "sg-codex-",
                "max_workers": 12, "lane_label": "subgov-codex-productive",
                "gate_health_dir": str(root / "gates"), "snapshot": str(root / "snapshot.json"),
                "demand_state": str(root / "demand.json"), "actuator_state": str(root / "actuator.json"),
                "quota_binary": "subgov", "quota_config": "native.yaml",
                "demand": {"workspaces": [str(root)]}}

    def test_native_array_and_single_object_are_understood(self):
        self.assertEqual(adapter.document('[{"id":"test-1"}]')[0]["id"], "test-1")
        self.assertEqual(adapter.single('{"id":"test-1"}')["id"], "test-1")
        with self.assertRaises(ValueError):
            adapter.single('[{},{}]')

    def test_manifest_never_bypasses_human_or_timed_holds(self):
        base = {"status": "open", "assignee": None, "labels": []}
        self.assertTrue(adapter.allowed(base, adapter.current_time()))
        for label in ["human", "proposal-pending", "deferred", "quarantine-until:2099-01-01T00:00:00Z",
                      "deferred:invalid", "blocked:manual"]:
            self.assertFalse(adapter.allowed({**base, "labels": [label]}, adapter.current_time()), label)
        self.assertTrue(adapter.allowed({**base, "labels": ["deferred:2020-01-01T00:00:00Z"]}, adapter.current_time()))

    def test_native_internal_artifacts_are_not_productive_capacity(self):
        ordinary = {"status": "open", "assignee": None, "labels": [], "title": "Repair contract pin",
                    "description": "Verify installed version and update the compatibility tests."}
        self.assertTrue(adapter.allowed(ordinary, adapter.current_time()))
        for change in [{"labels": ["infra"]}, {"labels": ["fingerprint:123"]},
                       {"description": "## Scanner Finding\nTest failed"},
                       {"description": "Re-verify DEFAULT_EXCLUDE_LABELS against the installed version."},
                       {"title": "Starvation alert", "description": "Open beads exist in this workspace."},
                       {"title": "[Unravel] repair", "description": "Original bead: test-1"}]:
            self.assertFalse(adapter.allowed({**ordinary, **change}, adapter.current_time()), change)
        self.assertTrue(adapter.allowed({**ordinary, "title": "Starvation alert", "description": ""}, adapter.current_time()))

    def test_new_ready_bead_needs_no_curated_revision_or_lane(self):
        with tempfile.TemporaryDirectory() as root:
            settings = self.settings(Path(root))
            issue = {"id": "new-1", "revision": 99, "status": "open", "labels": []}
            with patch.object(ready_beads, "policy_ok", return_value=True), \
                 patch.object(ready_beads, "ready_issues", return_value=[issue]), \
                 patch.object(adapter, "workers", return_value=[]):
                measured = adapter.demand(settings)
            self.assertEqual(measured["eligible_backlog_capacity"], 1)
            self.assertEqual(measured["ready"][0]["id"], "new-1")

    def test_failed_demand_withholds_capacity_instead_of_zero(self):
        with tempfile.TemporaryDirectory() as root:
            s = self.settings(Path(root))
            with patch.object(adapter, "run", return_value='{"fresh":true,"eligible_backlog_capacity":99}'), \
                 patch.object(adapter, "demand", side_effect=RuntimeError("query failed")):
                adapter.refresh(s)
            value = json.loads(Path(s["snapshot"]).read_text())
            self.assertNotIn("eligible_backlog_capacity", value)
            self.assertFalse(value["fresh"])
            self.assertEqual(json.loads(Path(s["demand_state"]).read_text())["unknown"], "RuntimeError")

    def test_shared_refresh_publishes_anthropic_quota_and_demand_state(self):
        with tempfile.TemporaryDirectory() as root:
            settings = self.settings(Path(root))
            settings["quota_account"] = "claude-anthropic"
            measured = {"observed_at": "2026-10-22T00:00:00Z", "ready_bead_count": 3,
                        "eligible_backlog_capacity": 2, "ready": []}
            with patch.object(adapter, "run", return_value='{"fresh":true,"windows":[]}') as run, \
                 patch.object(adapter, "demand", return_value=measured):
                adapter.refresh(settings)
            self.assertEqual(run.call_args.args[0][-2:], ["snapshot", "claude-anthropic"])
            self.assertEqual(json.loads(Path(settings["demand_state"]).read_text()), measured)
            self.assertEqual(json.loads(Path(settings["snapshot"]).read_text())["eligible_backlog_capacity"], 2)

    def test_failed_quota_does_not_refresh_old_snapshot(self):
        with tempfile.TemporaryDirectory() as root:
            s = self.settings(Path(root))
            Path(s["snapshot"]).write_text('{"old":true}')
            with patch.object(adapter, "run", side_effect=RuntimeError("quota failed")):
                with self.assertRaises(RuntimeError):
                    adapter.refresh(s)
            self.assertEqual(Path(s["snapshot"]).read_text(), '{"old":true}')

    def test_live_owned_claim_is_capacity_and_foreign_claim_fails_closed(self):
        with tempfile.TemporaryDirectory() as root:
            s = self.settings(Path(root))
            s["demand"]["workspaces"] = []
            worker = {"worker_id": "sg-codex-1", "qualified_id": "codex-test-sg-codex-1",
                      "current_bead": "test-1", "workspace": root}
            manual = {"worker_id": "manual-worker"}
            issue = {"id": "test-1", "status": "in_progress", "assignee": worker["qualified_id"]}
            with patch.object(adapter, "policy_ok", return_value=True), \
                 patch.object(adapter, "workers", return_value=[worker, manual]), \
                 patch.object(adapter, "run", return_value=json.dumps(issue)):
                measured = adapter.demand(s)
                self.assertEqual(measured["eligible_backlog_capacity"], 2)
                self.assertEqual(len(measured["busy"]), 1)
                self.assertEqual(measured["unowned_floor"], 1)
            with patch.object(adapter, "policy_ok", return_value=True), \
                 patch.object(adapter, "workers", return_value=[worker]), \
                 patch.object(adapter, "run", return_value=json.dumps({**issue, "assignee": "someone-else"})):
                with self.assertRaises(ValueError):
                    adapter.demand(s)

    def test_busy_supervisor_never_signalled(self):
        with patch.object(adapter.signal, "pidfd_send_signal") as send:
            self.assertFalse(adapter.stop_idle({}, {"state": "EXECUTING", "current_bead": "test-1"}))
            send.assert_not_called()

    def test_frozen_supervisor_with_child_is_resumed_and_not_terminated(self):
        with tempfile.TemporaryDirectory() as root:
            task = Path(root) / "1"
            task.mkdir()
            (task / "status").write_text("State:\tT (stopped)\n")
            (task / "children").write_text("88")
            real_path = Path
            def path(*parts):
                return real_path(root) if parts == ("/proc", "123", "task") else real_path(*parts)
            worker = {"pid": 123, "worker_id": "sg-codex-1", "state": "SELECTING", "is_idle": True,
                      "current_bead": None, "current_task": None}
            with patch.object(adapter, "Path", side_effect=path), patch.object(adapter, "identity", return_value="generation"), \
                 patch.object(adapter.os, "pidfd_open", return_value=9), patch.object(adapter.os, "close"), \
                 patch.object(adapter.signal, "pidfd_send_signal") as send:
                self.assertFalse(adapter.stop_idle({"agent": "codex-test"}, worker))
            self.assertEqual([call.args[1] for call in send.call_args_list], [signal.SIGSTOP, signal.SIGCONT])

    def test_zombie_child_does_not_block_retiring_an_idle_supervisor(self):
        with tempfile.TemporaryDirectory() as root, tempfile.TemporaryDirectory() as proc_root:
            task = Path(root) / "1"
            task.mkdir()
            (task / "status").write_text("State:\tT (stopped)\n")
            (task / "children").write_text("88")
            proc = Path(proc_root)
            (proc / "88").mkdir(parents=True)
            (proc / "88" / "stat").write_text("88 (git) Z 123 123 123 0 -1\n")
            real_path = Path
            def path(*parts):
                return real_path(root) if parts == ("/proc", "123", "task") else real_path(*parts)
            worker = {"pid": 123, "worker_id": "sg-codex-1", "state": "SELECTING", "is_idle": True,
                      "current_bead": None, "current_task": None}
            real_live_children = adapter.live_children
            with patch.object(adapter, "Path", side_effect=path), patch.object(adapter, "identity", return_value="generation"), \
                 patch.object(adapter, "live_children", side_effect=lambda t: real_live_children(t, proc)), \
                 patch.object(adapter, "workers", return_value=[worker]), \
                 patch.object(adapter.os, "pidfd_open", return_value=9), patch.object(adapter.os, "close"), \
                 patch.object(adapter.signal, "pidfd_send_signal") as send:
                self.assertTrue(adapter.stop_idle({"agent": "codex-test"}, worker))
            self.assertEqual([call.args[1] for call in send.call_args_list],
                             [signal.SIGSTOP, signal.SIGTERM, signal.SIGCONT])

    def test_lane_install_preserves_unrelated_bytes_and_exact_scope(self):
        with tempfile.TemporaryDirectory() as root:
            s = self.settings(Path(root))
            p = Path(root) / "config.yaml"
            p.write_text("# keep comment\nstrands:\n  pluck:\n    lanes:\n    - label: other\n      workers:\n      - alpha\n      when_empty: normal\n    split_after_failures: 3\nother: untouched\n")
            original = p.read_text()
            adapter.install_lane(s, p)
            first = p.read_text()
            self.assertTrue(first.startswith("# keep comment\n"))
            self.assertTrue(first.endswith("other: untouched\n"))
            self.assertIn("- sg-codex-12", first)
            self.assertNotIn("sg-codex-13", first)
            adapter.install_lane(s, p)
            self.assertEqual(p.read_text(), first)
            adapter.install_lane(s, p, remove=True)
            self.assertEqual(p.read_text(), original)

    def test_actuator_rechecks_work_and_never_launches_without_it(self):
        with tempfile.TemporaryDirectory() as root:
            s = self.settings(Path(root))
            with patch.object(adapter, "workers", return_value=[]), \
                 patch.object(adapter, "demand", return_value={"ready": []}), patch.object(adapter, "run") as run:
                adapter.actuate(s, 3)
            run.assert_not_called()

    def test_actuator_leaves_unowned_and_busy_workers_alive(self):
        with tempfile.TemporaryDirectory() as root:
            s = self.settings(Path(root))
            owned = {"worker_id": "sg-codex-1"}
            unowned = {"worker_id": "manual-worker"}
            with patch.object(adapter, "workers", return_value=[owned, unowned]), \
                 patch.object(adapter, "stop_idle", return_value=False) as stop:
                adapter.actuate(s, 0)
            stop.assert_called_once_with(s, owned)
            self.assertEqual(json.loads(Path(s["actuator_state"]).read_text())["pending_retirees"], ["sg-codex-1"])

    def test_one_ready_outcome_launches_only_one_scoped_native_worker(self):
        with tempfile.TemporaryDirectory() as root:
            s = self.settings(Path(root))
            with patch.object(adapter, "workers", return_value=[]), \
                 patch.object(adapter, "demand", return_value={"ready": [{"workspace": root, "id": "test-1"}]}), \
                 patch.object(adapter.shutil, "which", return_value="/usr/bin/needle"), \
                 patch.object(adapter, "run") as run:
                adapter.actuate(s, 3)
            run.assert_called_once()
            argv = run.call_args.args[0]
            self.assertEqual(argv[:2], ["tmux", "new-session"])
            self.assertIn("NEEDLE_INNER=1", argv)
            self.assertIn("NEEDLE_STRANDS__GENERATION__ENABLED=false", argv)
            self.assertIn("--identifier sg-codex-1", argv[-1])

    def seed_routes(self, run):
        routes = []
        for call in run.call_args_list:
            argv = call.args[0][-1]
            self.assertIn("--workspace ", argv)
            routes.append(argv.split("--workspace ", 1)[1].split(" --identifier", 1)[0])
        return routes

    def test_new_workers_spread_across_ready_workspaces_least_loaded_first(self):
        # Plan §26.5: the second launch must not stack on the first route
        # when another ready workspace has no running workers yet.
        with tempfile.TemporaryDirectory() as root:
            s = self.settings(Path(root))
            first, second = Path(root) / "a-repo", Path(root) / "b-repo"
            first.mkdir()
            second.mkdir()
            ready = [{"workspace": first, "id": "j1"}, {"workspace": first, "id": "j2"},
                     {"workspace": second, "id": "j3"}]
            with patch.object(adapter, "workers", return_value=[]), \
                 patch.object(adapter, "demand", return_value={"ready": ready}), \
                 patch.object(adapter.shutil, "which", return_value="/usr/bin/needle"), \
                 patch.object(adapter, "run") as run:
                adapter.actuate(s, 2)
            self.assertEqual(self.seed_routes(run), [str(first), str(second)])

    def test_occupied_workspace_pushes_new_worker_seeds_elsewhere(self):
        with tempfile.TemporaryDirectory() as root:
            s = self.settings(Path(root))
            busy_repo, idle_repo = Path(root) / "a-repo", Path(root) / "b-repo"
            busy_repo.mkdir()
            idle_repo.mkdir()
            live = [{"worker_id": "sg-codex-1", "workspace": str(busy_repo)}]
            ready = [{"workspace": busy_repo, "id": "j1"}, {"workspace": idle_repo, "id": "j2"}]
            with patch.object(adapter, "workers", return_value=live), \
                 patch.object(adapter, "demand", return_value={"ready": ready}), \
                 patch.object(adapter.shutil, "which", return_value="/usr/bin/needle"), \
                 patch.object(adapter, "run") as run:
                adapter.actuate(s, 2)
            self.assertEqual(self.seed_routes(run), [str(idle_repo)])

    def adoption_settings(self, root):
        settings = self.settings(root)
        settings["unit_dir"] = str(root / "units")
        settings["adoption_env"] = str(root / "adopted.env")
        (root / "adopted.env").write_text("NEEDLE_STRANDS__GENERATION__ENABLED=false\n")
        settings["adopted_workers"] = [{"worker_id": "old-sol", "agent": "codex-sol",
                                        "workspace": str(root), "unit": "needle-worker@old-sol.service"}]
        return settings

    def test_adoption_owns_exact_adapter_and_identifier_without_converting_models(self):
        with tempfile.TemporaryDirectory() as root:
            settings = self.adoption_settings(Path(root))
            self.assertTrue(adapter.owned(settings, {"worker_id": "old-sol", "model": "codex-sol"}))
            self.assertFalse(adapter.owned(settings, {"worker_id": "old-sol", "model": "codex-other"}))
            self.assertFalse(adapter.owned(settings, {"worker_id": "other", "model": "codex-sol"}))
            self.assertFalse(adapter.owned(settings, {"worker_id": "sg-codex-1", "model": "codex-other"}))
            self.assertTrue(adapter.owned(settings, {"worker_id": "sg-codex-1", "model": "codex-test"}))

    def test_adoption_checks_all_conflicts_before_disabling_or_writing(self):
        with tempfile.TemporaryDirectory() as root:
            settings = self.adoption_settings(Path(root))
            entry = settings["adopted_workers"][0]
            guard, _ = adapter.retirement_paths(settings, entry)
            guard.parent.mkdir(parents=True)
            guard.write_text("another owner")
            with patch.object(adapter, "workers", return_value=[]), \
                 patch.object(adapter, "unit_info", return_value={"pid": 0}), \
                 patch.object(adapter, "check_adopted_routing"), patch.object(adapter, "run") as run:
                with self.assertRaises(ValueError):
                    adapter.install_adoption(settings)
            self.assertEqual(guard.read_text(), "another owner")
            run.assert_not_called()

    def test_adoption_disables_restart_without_stopping_running_tasks(self):
        with tempfile.TemporaryDirectory() as root:
            settings = self.adoption_settings(Path(root))
            worker = {"worker_id": "old-sol", "model": "codex-sol", "pid": 123}
            with patch.object(adapter, "workers", return_value=[worker]), \
                 patch.object(adapter, "unit_info", return_value={"pid": 123}), \
                 patch.object(adapter, "check_adopted_routing"), patch.object(adapter, "run") as run:
                adapter.install_adoption(settings)
            commands = [call.args[0] for call in run.call_args_list]
            self.assertEqual(commands, [["systemctl", "--user", "disable", "needle-worker@old-sol.service"],
                                        ["systemctl", "--user", "daemon-reload"]])
            guard, _ = adapter.retirement_paths(settings, settings["adopted_workers"][0])
            self.assertIn("Restart=no", guard.read_text())
            self.assertIn("ConditionPathExists=!", guard.read_text())

    def test_unexpected_account_worker_holds_command_observer_pool(self):
        with tempfile.TemporaryDirectory() as root:
            settings = self.adoption_settings(Path(root))
            guard, _ = adapter.retirement_paths(settings, settings["adopted_workers"][0])
            guard.parent.mkdir(parents=True)
            guard.write_text(adapter.retirement_guard(settings, settings["adopted_workers"][0]))
            worker = {"worker_id": "unexpected", "model": "codex-other",
                      "last_heartbeat": adapter.current_time().isoformat()}
            with patch.object(adapter, "workers", return_value=[worker]), \
                 patch.object(adapter, "check_adopted_routing"), \
                 patch.object(adapter, "unit_info", return_value={"pid": 0, "ActiveState": "inactive", "LoadState": "loaded",
                                                               "Restart": "no", "UnitFileState": "disabled",
                                                               "EnvironmentFiles": settings["adoption_env"] + " (ignore_errors=no)"}):
                with self.assertRaises(ValueError):
                    adapter.inspect_fleet(settings)

    def test_adopted_busy_worker_gets_retirement_guard_and_is_not_signalled(self):
        with tempfile.TemporaryDirectory() as root:
            settings = self.adoption_settings(Path(root))
            worker = {"worker_id": "old-sol", "model": "codex-sol", "state": "EXECUTING",
                      "current_bead": "busy-1", "pid": 123}
            with patch.object(adapter, "inspect_fleet", return_value=[worker]), \
                 patch.object(adapter.signal, "pidfd_send_signal") as send:
                adapter.actuate(settings, 0)
            send.assert_not_called()
            _, marker = adapter.retirement_paths(settings, settings["adopted_workers"][0])
            self.assertEqual(marker.read_text(), "subgov-codex\n")
            state = json.loads(Path(settings["actuator_state"]).read_text())
            self.assertEqual(state["pending_retirees"], ["old-sol"])

    def test_restored_systemd_slot_retains_original_model_routing(self):
        with tempfile.TemporaryDirectory() as root:
            settings = self.adoption_settings(Path(root))
            with patch.object(adapter, "inspect_fleet", return_value=[]), \
                 patch.object(adapter, "demand", return_value={"ready": [{"workspace": root}]}), \
                 patch.object(adapter, "unit_info", return_value={"LoadState": "loaded", "UnitFileState": "disabled"}), \
                 patch.object(adapter, "run") as run:
                adapter.actuate(settings, 1)
            run.assert_called_once_with(["systemctl", "--user", "start", "--no-block", "needle-worker@old-sol.service"])

    def test_busy_workers_are_kept_ahead_of_idle_slots_and_markers_cleared(self):
        with tempfile.TemporaryDirectory() as root:
            settings = self.adoption_settings(Path(root))
            busy = {"worker_id": "old-sol", "model": "codex-sol", "state": "EXECUTING", "current_bead": "task-1"}
            spare = {"worker_id": "sg-codex-1", "state": "IDLE", "current_bead": None, "is_idle": True}
            _, marker = adapter.retirement_paths(settings, settings["adopted_workers"][0])
            marker.parent.mkdir(parents=True)
            marker.write_text("subgov-codex\n")
            with patch.object(adapter, "inspect_fleet", return_value=[spare, busy]), \
                 patch.object(adapter, "stop_idle", return_value=False) as stop:
                adapter.actuate(settings, 1)
            stop.assert_called_once_with(settings, spare)
            self.assertFalse(marker.exists())

    def test_stale_live_supervisor_is_an_error_instead_of_zero_capacity(self):
        with tempfile.TemporaryDirectory() as root:
            settings = self.adoption_settings(Path(root))
            with patch.object(adapter, "workers", return_value=[{
                    "worker_id": "old-sol", "model": "codex-sol", "last_heartbeat": "2020-01-01T00:00:00Z"}]):
                with self.assertRaises(ValueError):
                    adapter.inspect_fleet(settings)

    def test_effective_restart_owner_is_checked_beyond_guard_file(self):
        with tempfile.TemporaryDirectory() as root:
            settings = self.adoption_settings(Path(root))
            guard, _ = adapter.retirement_paths(settings, settings["adopted_workers"][0])
            guard.parent.mkdir(parents=True)
            guard.write_text(adapter.retirement_guard(settings, settings["adopted_workers"][0]))
            with patch.object(adapter, "workers", return_value=[]), patch.object(adapter, "check_adopted_routing"), \
                 patch.object(adapter, "unit_info", return_value={"pid": 0, "ActiveState": "inactive", "LoadState": "loaded",
                                                               "Restart": "always", "UnitFileState": "disabled"}):
                with self.assertRaises(ValueError):
                    adapter.inspect_fleet(settings)

    def test_masked_restore_slot_does_not_block_next_usable_slot(self):
        with tempfile.TemporaryDirectory() as root:
            settings = self.adoption_settings(Path(root))
            settings["adopted_workers"].append({"worker_id": "next-sol", "agent": "codex-sol", "workspace": root,
                                                "unit": "needle-worker@next-sol.service"})
            def info(unit):
                return {"LoadState": "loaded", "UnitFileState": "masked" if "old-sol" in unit else "disabled"}
            with patch.object(adapter, "inspect_fleet", return_value=[]), \
                 patch.object(adapter, "demand", return_value={"ready": [{"workspace": root}]}), \
                 patch.object(adapter, "unit_info", side_effect=info), patch.object(adapter, "run") as run:
                adapter.actuate(settings, 1)
            run.assert_called_once_with(["systemctl", "--user", "start", "--no-block", "needle-worker@next-sol.service"])

    def test_child_bearing_idle_worker_does_not_block_retiring_another_idle_slot(self):
        with tempfile.TemporaryDirectory() as root:
            settings = self.settings(Path(root))
            busy = {"worker_id": "sg-codex-1", "state": "EXECUTING", "current_bead": "busy"}
            selecting = {"worker_id": "sg-codex-2", "state": "SELECTING", "is_idle": True, "current_bead": None}
            free = {"worker_id": "sg-codex-3", "state": "EXHAUSTED", "is_idle": True, "current_bead": None}
            def rank(worker):
                return ({"sg-codex-1": 0, "sg-codex-2": 1, "sg-codex-3": 2}[worker["worker_id"]], worker["worker_id"])
            with patch.object(adapter, "workers", return_value=[busy, selecting, free]), \
                 patch.object(adapter, "retention_order", side_effect=rank), \
                 patch.object(adapter, "stop_idle", return_value=True) as stop:
                adapter.actuate(settings, 2)
            stop.assert_called_once_with(settings, free)


if __name__ == "__main__":
    unittest.main()
