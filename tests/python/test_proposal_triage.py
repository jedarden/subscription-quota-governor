import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


REPO = Path(__file__).resolve().parents[2]
SCRIPT = REPO / "scripts" / "proposal-triage.py"
SPEC = importlib.util.spec_from_file_location("proposal_triage", SCRIPT)
triage = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(triage)


class CommandResult:
    def __init__(self, stdout="", stderr="", returncode=0):
        self.stdout = stdout
        self.stderr = stderr
        self.returncode = returncode


def issue(title, description, ident="subgov-proposal", created="2026-11-01T00:00:00Z"):
    return {
        "id": ident,
        "title": title,
        "description": description,
        "created_at": created,
        "labels": ["proposal-pending", "weave-generated"],
    }


class ProposalTriageTests(unittest.TestCase):
    def test_decide_admits_dated_live_evidence_when_gate_is_healthy(self):
        result = triage.decide(
            issue("Repair quota refresh crash", "Observed failing refresh on 2026-10-12; live evidence attached."),
            None,
            True,
        )
        self.assertEqual(result[0], "admit-evidence")

    def test_decide_rejects_documentation_without_hazard(self):
        result = triage.decide(issue("Document the quota runbook", "Observed on 2026-10-12."), None, True)
        self.assertEqual(result[0], "reject-doc")

    def test_decide_rejects_test_without_dated_defect(self):
        result = triage.decide(issue("Add coverage for the quota adapter", "Live evidence exists."), None, True)
        self.assertEqual(result[0], "reject-test")

    def test_decide_rejects_without_dated_live_evidence(self):
        result = triage.decide(issue("Improve quota scheduling", "A useful improvement."), None, True)
        self.assertEqual(result[0], "reject-noevidence")

    def test_decide_holds_sound_evidence_when_gate_is_unhealthy(self):
        result = triage.decide(
            issue("Fix observed quota regression", "Failed in a live run on 2026-11-04."), None, False
        )
        self.assertEqual(result[0], "leave")
        self.assertIn("gate unhealthy", result[1])

    def test_dated_evidence_accepts_each_supported_month(self):
        for month in ("09", "10", "11", "12"):
            with self.subTest(month=month):
                action, _ = triage.decide(
                    issue("Fix observed quota regression", f"Failed in a live run on 2026-{month}-14."),
                    None,
                    True,
                )
                self.assertEqual(action, "admit-evidence")

    def test_plan_workspace_marks_near_duplicate_and_decides_survivor(self):
        with tempfile.TemporaryDirectory() as temp:
            workspace = Path(temp)
            (workspace / "Cargo.toml").write_text("[package]\nname='fixture'\nversion='0.1.0'\n")
            first = issue("Fix quota refresh regression", "Observed failing refresh on 2026-10-12.", "subgov-rich")
            second = issue("Fix quota refresh regression now", "Observed failing refresh.", "subgov-dup")
            output = json.dumps([first, second])
            with patch.object(triage, "run", return_value=CommandResult(stdout=output)):
                plan = triage.plan_workspace(workspace, workspace / "gates")
        by_id = {item["id"]: item for item in plan}
        self.assertEqual(by_id["subgov-rich"]["action"], "admit-evidence")
        self.assertEqual(by_id["subgov-dup"]["action"], "reject-dup")

    def test_plan_workspace_skips_a_secret_quarantined_store(self):
        with tempfile.TemporaryDirectory() as temp:
            workspace = Path(temp)
            result = CommandResult(stdout=json.dumps({"secret_quarantined": True}), returncode=1)
            with patch.object(triage, "run", return_value=result) as mocked:
                plan = triage.plan_workspace(workspace, workspace / "gates")
        self.assertEqual(plan[0]["action"], "skip-secret-quarantined")
        self.assertEqual(mocked.call_count, 1)

    def test_error_text_secret_quarantine_is_also_skipped(self):
        result = CommandResult(stderr="bead list failed: secret_quarantined", returncode=1)
        self.assertTrue(triage._contains_secret_quarantine(result))

    def test_false_secret_quarantine_does_not_skip(self):
        with tempfile.TemporaryDirectory() as temp:
            workspace = Path(temp)
            (workspace / "Cargo.toml").write_text("[package]\nname='fixture'\nversion='0.1.0'\n")
            proposal = issue("Fix quota refresh regression", "Observed failing refresh on 2026-10-12.")
            result = CommandResult(stdout=json.dumps({"secret_quarantined": False, "issues": [proposal]}))
            with patch.object(triage, "run", return_value=result):
                plan = triage.plan_workspace(workspace, workspace / "gates")
        self.assertEqual(plan[0]["action"], "admit-evidence")


class ProposalTriageUnitTests(unittest.TestCase):
    def test_timer_has_calendar_and_monotonic_watchdog(self):
        timer = (REPO / "deploy/codinghome/subgov-proposal-triage.timer").read_text()
        self.assertIn("OnCalendar=", timer)
        self.assertIn("Persistent=true", timer)
        watchdog_timer = (REPO / "deploy/codinghome/subgov-proposal-triage-watchdog.timer").read_text()
        self.assertIn("OnCalendar=", watchdog_timer)

    def test_service_runs_apply_and_emits_a_named_journal_identifier(self):
        service = (REPO / "deploy/codinghome/subgov-proposal-triage.service").read_text()
        self.assertIn("proposal-triage.py --apply", service)
        self.assertIn("SyslogIdentifier=subgov-proposal-triage", service)

    def test_watchdog_is_independently_scheduled_and_checks_the_timer(self):
        service = (REPO / "deploy/codinghome/subgov-proposal-triage-watchdog.service").read_text()
        self.assertIn("proposal-triage-watchdog.py", service)
        watchdog = (REPO / "scripts/proposal-triage-watchdog.py").read_text()
        self.assertIn("NextElapseUSecRealtime", watchdog)


if __name__ == "__main__":
    unittest.main()
