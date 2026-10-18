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


class ProductiveTests(unittest.TestCase):
    def settings(self, root):
        return {"host": "codinghome", "agent": "codex-test", "prefix": "sg-codex-",
                "max_workers": 12, "lane_label": "subgov-codex-productive",
                "gate_health_dir": str(root / "gates"), "snapshot": str(root / "snapshot.json"),
                "demand_state": str(root / "demand.json"), "actuator_state": str(root / "actuator.json"),
                "quota_binary": "subgov", "quota_config": "native.yaml",
                "work": [{"workspace": str(root), "id": "test-1", "reviewed_revision": 7,
                          "admission_evidence": "operator admitted"}]}

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

    def test_reviewed_revision_native_frontier_and_lane_all_required(self):
        with tempfile.TemporaryDirectory() as root:
            s = self.settings(Path(root))
            issue = {"id": "test-1", "revision": 7, "status": "open", "assignee": None,
                     "labels": [s["lane_label"]]}
            with patch.object(adapter, "policy_ok", return_value=True), patch.object(adapter, "workers", return_value=[]):
                with patch.object(adapter, "run", side_effect=[json.dumps([issue]), json.dumps(issue)]):
                    self.assertEqual(adapter.demand(s)["eligible_backlog_capacity"], 1)
                for change, reason in [({"revision": 8}, "changed_since_review"),
                                       ({"labels": []}, "missing_dispatch_lane")]:
                    with patch.object(adapter, "run", side_effect=[json.dumps([issue]), json.dumps({**issue, **change})]):
                        result = adapter.demand(s)
                    self.assertEqual(result["eligible_backlog_capacity"], 0)
                    self.assertEqual(result["blocked"][0]["reason"], reason)

    def test_gate_or_external_hold_prevents_frontier_query(self):
        with tempfile.TemporaryDirectory() as root:
            s = self.settings(Path(root))
            for external in [True, False]:
                s["work"][0]["external_hold"] = external
                with patch.object(adapter, "policy_ok", return_value=True), patch.object(adapter, "gate_degraded", return_value=True), \
                     patch.object(adapter, "workers", return_value=[]), patch.object(adapter, "run") as run:
                    result = adapter.demand(s)
                self.assertEqual(result["eligible_backlog_capacity"], 0)
                self.assertEqual(result["blocked"][0]["reason"], "recorded_external_hold" if external else "workspace_gate_degraded")
                run.assert_not_called()

    def test_failed_demand_withholds_capacity_instead_of_zero(self):
        with tempfile.TemporaryDirectory() as root:
            s = self.settings(Path(root))
            with patch.object(adapter, "run", return_value='{"fresh":true,"eligible_backlog_capacity":99}'), \
                 patch.object(adapter, "demand", side_effect=RuntimeError("query failed")):
                adapter.refresh(s)
            value = json.loads(Path(s["snapshot"]).read_text())
            self.assertNotIn("eligible_backlog_capacity", value)
            self.assertEqual(json.loads(Path(s["demand_state"]).read_text())["unknown"], "RuntimeError")

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
            s["work"] = []
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


if __name__ == "__main__":
    unittest.main()
