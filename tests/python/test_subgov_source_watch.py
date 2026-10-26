"""subgov_source_watch: a failed source unit or stale snapshot is a violation."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[2] / "scripts"
sys.path.insert(0, str(SCRIPTS))
spec = importlib.util.spec_from_file_location("subgov_source_watch", SCRIPTS / "subgov_source_watch.py")
watch = importlib.util.module_from_spec(spec)
spec.loader.exec_module(watch)


def loaded(result="LoadState=loaded\nActiveState=inactive\nResult=success\nExecMainStatus=0\n"):
    return subprocess.CompletedProcess([], 0, result, "")


class SourceWatchTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.settings = self.root / "source-settings.json"
        self.snapshot = self.root / "snapshot.json"
        self.settings.write_text(json.dumps({"snapshot": str(self.snapshot)}))

    def fresh_snapshot(self):
        self.snapshot.write_text('{"observed_at":"now"}')

    def stale_snapshot(self, age_seconds=1800):
        self.snapshot.write_text('{"observed_at":"old"}')
        past = time.time() - age_seconds
        os.utime(self.snapshot, (past, past))

    def test_failed_unit_is_a_violation_naming_the_unit(self):
        self.fresh_snapshot()
        failed = loaded("LoadState=loaded\nActiveState=failed\nResult=exit-code\nExecMainStatus=1\n")
        with patch.object(watch.subprocess, "run", return_value=failed):
            lines = watch.violations("subgov-zai-source.service", str(self.settings), 900)
        self.assertEqual(len(lines), 1)
        self.assertIn("subgov-zai-source.service is failed", lines[0])
        self.assertIn("Result=exit-code", lines[0])

    def test_stale_snapshot_is_a_violation_naming_the_age(self):
        self.stale_snapshot(age_seconds=1800)
        with patch.object(watch.subprocess, "run", return_value=loaded()):
            lines = watch.violations("subgov-codex-source.service", str(self.settings), 900)
        self.assertEqual(len(lines), 1)
        self.assertIn("snapshot is stale", lines[0])
        self.assertIn("30 minutes ago", lines[0])

    def test_fresh_snapshot_and_active_unit_are_healthy(self):
        self.fresh_snapshot()
        with patch.object(watch.subprocess, "run", return_value=loaded()):
            self.assertEqual(watch.violations("subgov-zai-source.service", str(self.settings), 900), [])

    def test_missing_settings_file_is_a_violation(self):
        with patch.object(watch.subprocess, "run", return_value=loaded()):
            lines = watch.violations("subgov-zai-source.service", str(self.root / "gone.json"), 900)
        self.assertEqual(len(lines), 1)
        self.assertIn("settings file missing", lines[0])

    def test_settings_without_snapshot_key_is_a_violation(self):
        self.settings.write_text('{"host":"codinghome"}')
        with patch.object(watch.subprocess, "run", return_value=loaded()):
            lines = watch.violations("subgov-zai-source.service", str(self.settings), 900)
        self.assertEqual(len(lines), 1)
        self.assertIn("settings unusable", lines[0])

    def test_missing_snapshot_file_is_a_violation(self):
        with patch.object(watch.subprocess, "run", return_value=loaded()):
            lines = watch.violations("subgov-zai-source.service", str(self.settings), 900)
        self.assertEqual(len(lines), 1)
        self.assertIn("snapshot file missing", lines[0])

    def test_unloaded_unit_is_skipped_for_hosts_without_that_source(self):
        not_found = loaded("LoadState=not-found\nActiveState=inactive\nResult=success\nExecMainStatus=0\n")
        with patch.object(watch.subprocess, "run", return_value=not_found):
            self.assertEqual(watch.violations("subgov-anthropic-source.service", str(self.root / "gone.json"), 900), [])

    def test_main_exits_one_and_prints_each_violation(self):
        self.stale_snapshot()
        failed = loaded("LoadState=loaded\nActiveState=failed\nResult=exit-code\nExecMainStatus=1\n")
        other = self.root / "other-settings.json"
        other.write_text(json.dumps({"snapshot": str(self.snapshot)}))
        def run_side_effect(argv, **kwargs):
            return failed if "subgov-zai-source.service" in argv else loaded()
        with patch.object(watch.subprocess, "run", side_effect=run_side_effect):
            code = watch.main(["--max-age-seconds", "900",
                               "--source", f"subgov-zai-source.service:{self.settings}",
                               "--source", f"subgov-codex-source.service:{other}"])
        self.assertEqual(code, 1)

    def test_main_is_quiet_zero_when_all_sources_are_healthy(self):
        self.fresh_snapshot()
        with patch.object(watch.subprocess, "run", return_value=loaded()):
            self.assertEqual(watch.main(["--source", f"subgov-zai-source.service:{self.settings}"]), 0)

    def test_main_rejects_a_source_spec_without_a_colon(self):
        self.assertEqual(watch.main(["--source", "subgov-zai-source.service"]), 2)


if __name__ == "__main__":
    unittest.main()
