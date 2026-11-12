import contextlib
import io
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[2] / "scripts"
sys.path.insert(0, str(SCRIPTS))
spec = importlib.util.spec_from_file_location("zai_source_refresh", SCRIPTS / "zai-source-refresh.py")
refresh = importlib.util.module_from_spec(spec)
spec.loader.exec_module(refresh)


class RefreshTests(unittest.TestCase):
    def test_shared_ready_demand_is_published_for_zai(self):
        with tempfile.TemporaryDirectory() as root:
            p = Path(root)
            roster = p / "roster.json"
            roster.write_text('{"workers":[]}')
            settings = {"roster": str(roster), "heartbeat_dir": root, "agent": "glm"}
            expected = {"ready_bead_count": 9, "eligible_backlog_capacity": 2, "ready": []}
            with patch.object(refresh, "ready_demand", return_value=expected) as reader:
                self.assertEqual(refresh.backlog(settings), expected)
                reader.assert_called_once_with(settings, [])

    def _elastic_heartbeat(self, root, worker_id, pid, bead="armor-1",
                           workspace="/home/coding/ARMOR", agent="claude-code-glm-5.3-flash"):
        from datetime import datetime, timezone
        path = Path(root) / f"{agent}-{worker_id}.json"
        path.write_text(json.dumps({
            "worker_id": worker_id, "qualified_id": f"{agent}-{worker_id}",
            "pid": pid, "current_bead": bead, "workspace": workspace,
            "last_heartbeat": datetime.now(timezone.utc).isoformat()}))
        return path

    def _settings(self, root, workers=()):
        from datetime import datetime, timezone
        roster = Path(root) / "roster.json"
        roster.write_text(json.dumps({"workers": list(workers)}))
        return {"roster": str(roster), "heartbeat_dir": root, "host": "codinghome",
                "agent": "claude-code-glm-5.3-flash", "identifier_prefix": "sg-zai-"}

    @staticmethod
    def _proc_open(argv_by_pid):
        real_open = open
        class FakeProc:
            def __init__(self, argv):
                self.argv = argv
            def read(self):
                return b"\0".join(item.encode() for item in self.argv) + b"\0"
            def __enter__(self):
                return self
            def __exit__(self, *args):
                return False
        def opener(path, *args, **kwargs):
            if isinstance(path, str) and path.startswith("/proc/") and path.endswith("/cmdline"):
                pid = int(path.split("/")[2])
                if pid in argv_by_pid:
                    return FakeProc(argv_by_pid[pid])
                raise OSError(2, "no such process")
            return real_open(path, *args, **kwargs)
        return opener

    def _claim_output(self, bead="armor-1", assignee="claude-code-glm-5.3-flash-sg-zai-4"):
        return subprocess.CompletedProcess([], 0, json.dumps(
            {"id": bead, "status": "in_progress", "assignee": assignee}), "")

    def test_elastic_busy_claim_is_validated_and_counted(self):
        # subgov-a5c14e89: a live sg-zai-* claim occupied no slot because
        # backlog() only inspected the legacy roster.
        with tempfile.TemporaryDirectory() as root:
            self._elastic_heartbeat(root, "sg-zai-4", 4242)
            settings = self._settings(root)
            argv = ["/needle", "run", "--identifier", "sg-zai-4"]
            with patch("builtins.open", self._proc_open({4242: argv})), \
                 patch.object(refresh.subprocess, "run",
                              return_value=self._claim_output()) as claim, \
                 patch.object(refresh, "policy_ok", return_value=True), \
                 patch.object(refresh, "ready_demand", return_value={}) as reader:
                refresh.backlog(settings)
            self.assertIn(["bead", "show", "armor-1", "--json"], claim.call_args_list[0].args)
            reader.assert_called_once_with(
                settings, [{"workspace": "/home/coding/ARMOR", "id": "armor-1"}])

    def test_elastic_dead_pid_is_not_busy(self):
        with tempfile.TemporaryDirectory() as root:
            self._elastic_heartbeat(root, "sg-zai-4", 4242)
            settings = self._settings(root)
            with patch("builtins.open", self._proc_open({})), \
                 patch.object(refresh, "ready_demand", return_value={}) as reader:
                refresh.backlog(settings)
            reader.assert_called_once_with(settings, [])

    def test_elastic_pid_of_another_worker_is_not_busy(self):
        # A recycled pid carrying a different --identifier cannot pass as
        # this worker: the worker is gone, so its stale claim frees the slot.
        with tempfile.TemporaryDirectory() as root:
            self._elastic_heartbeat(root, "sg-zai-4", 4242)
            settings = self._settings(root)
            argv = ["/needle", "run", "--identifier", "sg-zai-9"]
            with patch("builtins.open", self._proc_open({4242: argv})), \
                 patch.object(refresh, "ready_demand", return_value={}) as reader:
                refresh.backlog(settings)
            reader.assert_called_once_with(settings, [])

    def test_elastic_qualified_id_mismatch_fails_demand(self):
        with tempfile.TemporaryDirectory() as root:
            path = self._elastic_heartbeat(root, "sg-zai-4", 4242)
            body = json.loads(path.read_text())
            body["qualified_id"] = "claude-code-glm-5.3-flash-sg-zai-9"
            path.write_text(json.dumps(body))
            settings = self._settings(root)
            with patch("builtins.open", self._proc_open({4242: ["n", "--identifier", "sg-zai-4"]})):
                with self.assertRaises(ValueError):
                    refresh.backlog(settings)

    def test_elastic_stale_heartbeat_is_skipped(self):
        from datetime import datetime, timedelta, timezone
        with tempfile.TemporaryDirectory() as root:
            path = self._elastic_heartbeat(root, "sg-zai-4", 4242)
            body = json.loads(path.read_text())
            body["last_heartbeat"] = (datetime.now(timezone.utc)
                                      - timedelta(seconds=61)).isoformat()
            path.write_text(json.dumps(body))
            settings = self._settings(root)
            with patch.object(refresh, "ready_demand", return_value={}) as reader:
                refresh.backlog(settings)
            reader.assert_called_once_with(settings, [])

    def test_duplicate_claims_occupy_one_slot(self):
        # The cg-l0v0kc shape: two live workers holding one claim. The pair
        # occupies exactly one slot, never two and never zero.
        with tempfile.TemporaryDirectory() as root:
            self._elastic_heartbeat(root, "sg-zai-4", 4242, bead="armor-1")
            self._elastic_heartbeat(root, "sg-zai-7", 5353, bead="armor-1")
            settings = self._settings(root)
            procs = {4242: ["n", "--identifier", "sg-zai-4"],
                     5353: ["n", "--identifier", "sg-zai-7"]}
            claims = [self._claim_output(assignee="claude-code-glm-5.3-flash-sg-zai-4"),
                      self._claim_output(assignee="claude-code-glm-5.3-flash-sg-zai-7")]
            with patch("builtins.open", self._proc_open(procs)), \
                 patch.object(refresh.subprocess, "run", side_effect=claims), \
                 patch.object(refresh, "policy_ok", return_value=True), \
                 patch.object(refresh, "ready_demand", return_value={}) as reader:
                refresh.backlog(settings)
            reader.assert_called_once_with(
                settings, [{"workspace": "/home/coding/ARMOR", "id": "armor-1"}])

    def test_peer_host_prefix_is_not_this_pool(self):
        # ownership rule: sg-zai-gmktec-3 belongs to the peer host's pool.
        with tempfile.TemporaryDirectory() as root:
            self._elastic_heartbeat(root, "sg-zai-gmktec-3", 4242)
            settings = self._settings(root)
            with patch.object(refresh, "ready_demand", return_value={}) as reader:
                refresh.backlog(settings)
            reader.assert_called_once_with(settings, [])

    def test_legacy_roster_and_elastic_busy_merge(self):
        from datetime import datetime, timezone
        with tempfile.TemporaryDirectory() as root:
            legacy = {"worker_id": "glm-1", "agent": "glm", "unit": "needle-worker@glm-1.service"}
            roster = Path(root) / "roster.json"
            roster.write_text(json.dumps({"workers": [legacy]}))
            (Path(root) / "glm-glm-1.json").write_text(json.dumps({
                "current_bead": "native-1", "pid": 111, "workspace": root,
                "last_heartbeat": datetime.now(timezone.utc).isoformat()}))
            self._elastic_heartbeat(root, "sg-zai-4", 4242, bead="armor-1",
                                    workspace="/home/coding/ARMOR")
            settings = {"roster": str(roster), "heartbeat_dir": root, "host": "codinghome",
                        "agent": "claude-code-glm-5.3-flash", "identifier_prefix": "sg-zai-"}
            outputs = [
                subprocess.CompletedProcess([], 0, "111", ""),   # systemctl MainPID
                self._claim_output("native-1", assignee="glm-glm-1"),
                self._claim_output("armor-1"),
            ]
            with patch("builtins.open", self._proc_open(
                    {4242: ["n", "--identifier", "sg-zai-4"]})), \
                 patch.object(refresh.subprocess, "run", side_effect=outputs), \
                 patch.object(refresh, "policy_ok", return_value=True), \
                 patch.object(refresh, "ready_demand", return_value={}) as reader:
                refresh.backlog(settings)
            expected = sorted([{"workspace": "/home/coding/ARMOR", "id": "armor-1"},
                               {"workspace": str(Path(root).resolve()), "id": "native-1"}],
                              key=lambda job: (job["workspace"], job["id"]))
            reader.assert_called_once_with(settings, expected)

    def test_missing_prefix_keeps_roster_only_scan(self):
        with tempfile.TemporaryDirectory() as root:
            self._elastic_heartbeat(root, "sg-zai-4", 4242)
            settings = self._settings(root)
            del settings["identifier_prefix"]
            with patch.object(refresh, "ready_demand", return_value={}) as reader:
                refresh.backlog(settings)
            reader.assert_called_once_with(settings, [])

    def test_validated_busy_claim_accepts_current_native_object(self):
        from datetime import datetime, timezone
        with tempfile.TemporaryDirectory() as root:
            p = Path(root)
            worker = {"worker_id": "glm-1", "agent": "glm", "unit": "needle-worker@glm-1.service"}
            (p / "roster.json").write_text(json.dumps({"workers": [worker]}))
            (p / "glm-glm-1.json").write_text(json.dumps({"current_bead": "native-1", "pid": 123,
                "workspace": root, "last_heartbeat": datetime.now(timezone.utc).isoformat()}))
            settings = {"roster": str(p / "roster.json"), "heartbeat_dir": root, "host": "codinghome"}
            outputs = [subprocess.CompletedProcess([], 0, '123', ''), subprocess.CompletedProcess([], 0,
                '{"id":"native-1","status":"in_progress","assignee":"glm-glm-1"}', '')]
            with patch.object(refresh.subprocess, "run", side_effect=outputs), \
                 patch.object(refresh, "policy_ok", return_value=True), \
                 patch.object(refresh, "ready_demand", return_value={}) as reader:
                refresh.backlog(settings)
            reader.assert_called_once_with(settings, [{"workspace": root, "id": "native-1"}])

    def test_failed_collection_preserves_previous_snapshot(self):
        with tempfile.TemporaryDirectory() as root:
            dest = Path(root) / "snapshot.json"
            dest.write_text('{"previous":true}')
            settings = {"snapshot": str(dest), "collector": "collector", "collector_args": []}
            with patch.object(refresh.subprocess, "run", side_effect=subprocess.CalledProcessError(1, "collector")):
                with self.assertRaises(subprocess.CalledProcessError):
                    refresh.refresh(settings)
            self.assertEqual(json.loads(dest.read_text()), {"previous": True})

    def test_failed_demand_is_unknown_instead_of_zero(self):
        with tempfile.TemporaryDirectory() as root:
            dest = Path(root) / "snapshot.json"
            settings = {"snapshot": str(dest), "collector": "collector", "collector_args": []}
            sample = {"observed_at": "2026-10-13T22:00:00Z", "fresh": True, "windows": []}
            result = subprocess.CompletedProcess([], 0, json.dumps(sample), "")
            with patch.object(refresh.subprocess, "run", return_value=result), \
                    patch.object(refresh, "backlog", side_effect=TimeoutError):
                refresh.refresh(settings)
            self.assertNotIn("eligible_backlog_capacity", json.loads(dest.read_text()))
            self.assertFalse(json.loads(dest.read_text())["fresh"])


    def test_refresh_failure_names_the_underlying_cause(self):
        # subgov-28a476df: a bare class name hid the replica-count mismatch.
        with tempfile.TemporaryDirectory() as root:
            settings_path = Path(root) / "settings.json"
            settings_path.write_text(json.dumps(
                {"snapshot": str(Path(root) / "snapshot.json"),
                 "collector": "collector", "collector_args": []}))
            error = subprocess.CalledProcessError(1, ["collector"],
                                                  stderr="replica count 3 != 2\n")
            with patch.object(refresh.subprocess, "run", side_effect=error), \
                    contextlib.redirect_stderr(io.StringIO()) as captured:
                self.assertEqual(refresh.main(["--settings", str(settings_path)]), 1)
            self.assertIn("replica count 3 != 2", captured.getvalue())
            self.assertIn("CalledProcessError", captured.getvalue())

    def test_failed_demand_logs_the_exception_message(self):
        with tempfile.TemporaryDirectory() as root:
            dest = Path(root) / "snapshot.json"
            settings = {"snapshot": str(dest), "collector": "collector", "collector_args": []}
            sample = {"observed_at": "2026-10-13T22:00:00Z", "fresh": True, "windows": []}
            result = subprocess.CompletedProcess([], 0, json.dumps(sample), "")
            with patch.object(refresh.subprocess, "run", return_value=result), \
                    patch.object(refresh, "backlog", side_effect=ValueError("busy claim identity mismatch")), \
                    contextlib.redirect_stderr(io.StringIO()) as captured:
                refresh.refresh(settings)
            self.assertIn("busy claim identity mismatch", captured.getvalue())

if __name__ == "__main__":
    unittest.main()
