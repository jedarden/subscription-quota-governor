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
    def test_held_proposals_and_assigned_work_are_not_ready_capacity(self):
        base = {"status": "open", "assignee": None, "labels": []}
        self.assertTrue(refresh.eligible(base))
        for label in ["human", "HUMAN:review", "proposal-pending", "deferred",
                      "quarantine-until:2099-01-01T00:00:00Z", "blocked:manual"]:
            self.assertFalse(refresh.eligible({**base, "labels": [label]}), label)
        self.assertFalse(refresh.eligible({**base, "assignee": "another-worker"}))
        self.assertFalse(refresh.eligible({**base, "manual_blocked": True}))

    def test_foreign_queue_is_not_queried(self):
        with tempfile.TemporaryDirectory() as root:
            p = Path(root)
            (p / ".beads").mkdir()
            (p / ".beads/config.json").write_text("{}")
            (p / ".needle.yaml").write_text("bead_cli:\n  backend: bead-rs\nqueue:\n  owner_host: lab\n")
            with patch.object(refresh.subprocess, "run") as run:
                self.assertEqual(refresh.ready(root, "codinghome"), set())
                run.assert_not_called()

    def test_complex_or_duplicate_ownership_fails_closed(self):
        with tempfile.TemporaryDirectory() as root:
            p = Path(root) / ".needle.yaml"
            for text in ["queue: {owner_host: lab}\n", "queue:\n  owner_host: lab\n  owner_host: codinghome\n"]:
                p.write_text(text)
                with self.assertRaises(ValueError):
                    refresh.workspace_policy(p)

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


if __name__ == "__main__":
    unittest.main()
