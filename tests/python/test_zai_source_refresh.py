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
            settings = {"roster": str(roster)}
            expected = {"ready_bead_count": 9, "eligible_backlog_capacity": 2, "ready": []}
            with patch.object(refresh, "ready_demand", return_value=expected) as reader:
                self.assertEqual(refresh.backlog(settings), expected)
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
