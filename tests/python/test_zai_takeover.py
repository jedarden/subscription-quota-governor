"""Synthetic contract tests for the §26.2 takeover helper: no network, real
services, or worker processes."""
import argparse
from datetime import datetime, timezone
import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[2] / "scripts"
sys.path.insert(0, str(SCRIPTS))


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


takeover_module = load("takeover", "zai-takeover.py")
from subgov_adapter_common import atomic_json, read_json


def manifest(targets=None):
    return dict(
        version=1,
        owner="subgov-claude-zai",
        identifier_prefix="sg-zai-",
        targets=targets if targets is not None else [
            dict(worker_id="glm-front-herdr", agent="claude-code-glm-5.3-flash", unit="needle-worker@glm-front-herdr.service"),
            dict(worker_id="glm-roam-25", agent="claude-code-glm-5.3"),
        ],
    )


def idle_heartbeat(pid=100):
    return dict(current_bead=None, current_task=None, is_idle=True, state="SELECTING", pid=pid)


class FakeTakeover(takeover_module.Takeover):
    """Records systemd and signal effects; keeps the real freeze/child checks
    against a fake /proc tree so the retirement sequence stays under test."""

    def __init__(self, args, manifest):
        super().__init__(args, manifest)
        self.events = []
        self.heartbeats = {}
        self.busy = set()
        self.stale = set()
        self.live_children_count = 0

    def systemd(self, *args):
        self.events.append(args)
        return "MainPID=0\n"

    def unit_main_pid(self, unit):
        return 100

    def heartbeat(self, target, pid):
        if target["worker_id"] in self.stale:
            raise ValueError("worker heartbeat is stale")
        heartbeat = dict(idle_heartbeat(pid))
        if target["worker_id"] in self.busy:
            heartbeat.update(current_bead="claimed", is_idle=False)
        return heartbeat

    def process_identity(self, target, pid):
        return "generation1"

    def replacements(self):
        return self._replacements

    def await_exit(self, pid):
        self.events.append(("exit-verified", pid))


class TakeoverTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.args = argparse.Namespace(
            state=str(self.root / "state.json"),
            unit_dir=self.root / "units",
            heartbeat_dir=self.root / "heartbeats",
            proc_root=self.root / "proc",
            stale_after=60,
            limit=1,
        )
        self.heartbeats = self.root / "heartbeats"
        self.heartbeats.mkdir()
        self.fleet = FakeTakeover(self.args, manifest())
        self.fleet._replacements = 1

    def write_heartbeat(self, name, document):
        atomic_json(str(self.heartbeats / f"{name}.json"), document)

    def running(self, target, pid=100):
        """The heartbeat file stop_idle resolves the supervisor pid from."""
        self.write_heartbeat(
            f"{target['agent']}-{target['worker_id']}",
            dict(pid=pid, last_heartbeat=datetime.now(timezone.utc).isoformat()),
        )

    def test_manifest_rejects_bad_ownership_prefix_targets(self):
        path = self.root / "manifest.json"
        variants = []
        for field, value in [("version", 2), ("owner", ""), ("identifier_prefix", "")]:
            document = manifest()
            document[field] = value
            variants.append(document)
        variants.append(manifest(targets=[]))
        variants.append(manifest(targets=[dict(worker_id="sg-zai-1", agent="claude-code-glm-5.3-flash")]))
        variants.append(manifest(targets=[dict(worker_id="glm a", agent="claude-code-glm-5.3-flash")]))
        variants.append(manifest(targets=[dict(worker_id="glm-1", agent="claude-code-glm-5.3-flash", unit="other.service")]))
        variants.append(manifest(targets=[dict(worker_id="glm-1", agent="a"), dict(worker_id="glm-1", agent="b")]))
        for document in variants:
            atomic_json(str(path), document)
            with self.assertRaises(ValueError):
                takeover_module.manifest_read(path)

    def test_zero_replacements_blocks_retirement_completely(self):
        self.fleet._replacements = 0
        result = self.fleet.retire(1)
        self.assertEqual(result, {"retired": 0, "reason": "no running replacement"})
        self.assertFalse(any(event[0] == "stop" for event in self.fleet.events if isinstance(event, tuple)))

    def test_retire_limit_never_exceeds_running_replacements(self):
        for target in manifest()["targets"]:
            self.running(target)
        self.make_proc_tree()
        self.fleet._replacements = 1
        with patch.object(os, "pidfd_open", return_value=os.open(os.devnull, os.O_RDONLY)), \
             patch.object(takeover_module.signal, "pidfd_send_signal"), \
             patch.object(takeover_module, "live_children", return_value=[]):
            result = self.fleet.retire(5)
        self.assertEqual(len(result["retired"]), 1)
        self.assertEqual(result["retired"], [manifest()["targets"][0]["worker_id"]], "manifest order")

    def test_busy_target_is_never_signalled(self):
        target = manifest()["targets"][0]
        self.fleet.busy = {target["worker_id"]}
        self.running(target)
        self.make_proc_tree()
        with patch.object(os, "pidfd_open", side_effect=AssertionError("must not open a pidfd")), \
             patch.object(takeover_module.signal, "pidfd_send_signal", side_effect=AssertionError("must not signal")):
            result = self.fleet.retire(1)
        self.assertEqual(result, {"retired": []})

    def test_missing_heartbeat_skips_the_target(self):
        result = self.fleet.retire(1)
        self.assertEqual(result, {"retired": []}, "no worker is running")

    def make_proc_tree(self, pid=100, child_state=None):
        task = self.args.proc_root / str(pid) / "task" / str(pid)
        task.mkdir(parents=True)
        (task / "status").write_text("State:\tT (stopped)\n")
        (task / "children").write_text(f"{pid + 500}\n")
        child = self.args.proc_root / str(pid + 500)
        child.mkdir()
        (child / "stat").write_text(f"{pid + 500} (child) {child_state or 'S'} 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n")

    def test_idle_unit_target_retires_through_systemd_with_guard_files(self):
        target = manifest()["targets"][0]
        self.running(target)
        self.make_proc_tree()
        with patch.object(os, "pidfd_open", return_value=os.open(os.devnull, os.O_RDONLY)), \
             patch.object(takeover_module.signal, "pidfd_send_signal") as send, \
             patch.object(takeover_module, "live_children", return_value=[]):
            result = self.fleet.retire(1)
        self.assertEqual(result, {"retired": [target["worker_id"]]})
        stop = [event for event in self.fleet.events if event[:2] == ("stop", "--no-block")]
        self.assertEqual(stop, [("stop", "--no-block", target["unit"])])
        # The restart guard: a marker plus a Restart=no drop-in conditioned on it.
        dropin, marker = self.fleet.paths(target)
        self.assertTrue(marker.exists())
        self.assertEqual(marker.read_text(), self.fleet.identity + "\n")
        self.assertIn("Restart=no", dropin.read_text())
        self.assertIn(f"ConditionPathExists=!{marker}", dropin.read_text())
        self.assertIn(("daemon-reload",), self.fleet.events)
        # Freeze and resume bracketed the stop.
        sent = [call.args[1] for call in send.call_args_list]
        self.assertEqual(sent[0], takeover_module.signal.SIGSTOP)
        self.assertEqual(sent[-1], takeover_module.signal.SIGCONT)

    def test_retirement_appends_a_durable_record_at_state(self):
        target = manifest()["targets"][0]
        self.running(target)
        self.make_proc_tree()
        with patch.object(os, "pidfd_open", return_value=os.open(os.devnull, os.O_RDONLY)), \
             patch.object(takeover_module.signal, "pidfd_send_signal"), \
             patch.object(takeover_module, "live_children", return_value=[]):
            self.fleet.retire(1)
        state = read_json(str(self.root / "state.json"))
        self.assertEqual(state["manifest"], self.fleet.identity)
        self.assertEqual(len(state["retirements"]), 1)
        record = state["retirements"][0]
        self.assertEqual(record["worker_id"], target["worker_id"])
        self.assertEqual(record["unit"], target["unit"])
        self.assertEqual(record["pid"], 100)
        self.assertEqual(record["method"], "unit")
        self.assertIn("retired_at", record)

    def test_no_retirement_writes_no_state(self):
        self.fleet.busy = {manifest()["targets"][0]["worker_id"]}
        self.running(manifest()["targets"][0])
        self.make_proc_tree()
        with patch.object(os, "pidfd_open", side_effect=AssertionError("must not open a pidfd")), \
             patch.object(takeover_module.signal, "pidfd_send_signal", side_effect=AssertionError("must not signal")):
            self.assertEqual(self.fleet.retire(1), {"retired": []})
        self.assertFalse((self.root / "state.json").exists())

    def test_live_thread_child_blocks_and_resume_still_happens(self):
        self.running(manifest()["targets"][0])
        self.make_proc_tree(child_state="S")
        with patch.object(os, "pidfd_open", return_value=os.open(os.devnull, os.O_RDONLY)), \
             patch.object(takeover_module.signal, "pidfd_send_signal") as send, \
             patch.object(takeover_module, "live_children", return_value=["501"]):
            result = self.fleet.retire(1)
        self.assertEqual(result, {"retired": []})
        sent = [call.args[1] for call in send.call_args_list]
        self.assertEqual(sent[-1], takeover_module.signal.SIGCONT)
        self.assertFalse(any(event[:1] == ("stop",) for event in self.fleet.events if isinstance(event, tuple)))

    def test_zombie_child_does_not_block_retirement(self):
        target = manifest()["targets"][0]
        self.running(target)
        self.make_proc_tree(child_state="Z")
        with patch.object(os, "pidfd_open", return_value=os.open(os.devnull, os.O_RDONLY)), \
             patch.object(takeover_module.signal, "pidfd_send_signal"), \
             patch.object(takeover_module, "live_children", side_effect=takeover_module.live_children):
            result = self.fleet.retire(1)
        self.assertEqual(result, {"retired": [target["worker_id"]]})

    def test_straggler_retires_by_sigterm_without_a_unit(self):
        target = manifest()["targets"][1]
        self.running(target)
        self.make_proc_tree()
        with patch.object(os, "pidfd_open", return_value=os.open(os.devnull, os.O_RDONLY)), \
             patch.object(takeover_module.signal, "pidfd_send_signal") as send, \
             patch.object(takeover_module, "live_children", return_value=[]):
            result = self.fleet.retire(1)
        self.assertEqual(result, {"retired": [target["worker_id"]]})
        terms = [call for call in send.call_args_list if call.args[1] == takeover_module.signal.SIGTERM]
        self.assertEqual(len(terms), 1, "the straggler is retired by one SIGTERM")
        self.assertEqual([event for event in self.fleet.events if event[:1] == ("stop",)], [])
        # The resume precedes the exit check, or the frozen supervisor could
        # never deliver its pending SIGTERM and exit.
        signals = [call.args[1] for call in send.call_args_list]
        self.assertLess(signals.index(takeover_module.signal.SIGTERM), signals.index(takeover_module.signal.SIGCONT))
        self.assertIn(("exit-verified", 100), self.fleet.events)

    def test_unverifiable_target_is_left_untouched(self):
        # A stale heartbeat means the worker cannot be verified: skip it.
        target = manifest()["targets"][0]
        self.fleet.stale = {target["worker_id"]}
        with patch.object(os, "pidfd_open", side_effect=AssertionError("must not open a pidfd")), \
             patch.object(takeover_module.signal, "pidfd_send_signal", side_effect=AssertionError("must not signal")):
            result = self.fleet.retire(1)
        self.assertEqual(result, {"retired": []})

    def test_replacements_counts_only_fresh_prefixed_workers(self):
        fleet = takeover_module.Takeover(self.args, manifest())
        now = datetime.now(timezone.utc)
        stale = datetime(2020, 1, 1, tzinfo=timezone.utc)
        self.write_heartbeat("claude-code-glm-5.3-flash-sg-zai-1", dict(worker_id="sg-zai-1", last_heartbeat=now.isoformat()))
        self.write_heartbeat("claude-code-glm-5.3-flash-sg-zai-2", dict(worker_id="sg-zai-2", last_heartbeat=now.isoformat()))
        self.write_heartbeat("claude-code-glm-5.3-flash-luna-1", dict(worker_id="luna-1", last_heartbeat=now.isoformat()))
        self.write_heartbeat("claude-code-glm-5.3-flash-sg-zai-3", dict(worker_id="sg-zai-3", last_heartbeat=stale.isoformat()))
        (self.heartbeats / "notes.txt").write_text("not a heartbeat")
        self.assertEqual(fleet.replacements(), 2)

    def test_replacements_fail_closed_on_a_malformed_heartbeat(self):
        (self.heartbeats / "codex-garbled.json").write_text("not json")
        with self.assertRaises(json.JSONDecodeError):
            takeover_module.Takeover(self.args, manifest()).replacements()

    def test_main_actuates_locked_and_reports(self):
        atomic_json(str(self.root / "manifest.json"), manifest())
        argv = [
            "zai-takeover.py",
            "--manifest", str(self.root / "manifest.json"),
            "--state", str(self.root / "state.json"),
            "--heartbeat-dir", str(self.heartbeats),
            "--unit-dir", str(self.root / "units"),
            "--proc-root", str(self.root / "proc"),
        ]
        with patch.object(sys, "argv", argv):
            self.assertEqual(takeover_module.main(), 0)

    def test_dry_run_verdicts_every_target_without_pidfd_or_state(self):
        for target in manifest()["targets"]:
            self.running(target)
        self.make_proc_tree(child_state="Z")
        with patch.object(os, "pidfd_open", side_effect=AssertionError("dry run must not open a pidfd")), \
             patch.object(takeover_module.signal, "pidfd_send_signal", side_effect=AssertionError("dry run must not signal")):
            report = self.fleet.rehearse()
        self.assertEqual([entry["verdict"] for entry in report["verdicts"]], ["eligible", "eligible"])
        self.assertEqual(report["replacements"], 1)
        self.assertEqual(report["budget"], 1)
        # Read-only: no lock file, so a rehearsal never blocks a live run.
        self.assertFalse((self.root / "state.json.lock").exists())

    def test_dry_run_classifies_busy_stale_and_missing_targets(self):
        target = manifest()["targets"][0]
        self.fleet.busy = {target["worker_id"]}
        self.running(target)
        self.make_proc_tree()
        verdicts = {entry["worker_id"]: entry["verdict"] for entry in self.fleet.rehearse()["verdicts"]}
        self.assertEqual(verdicts[target["worker_id"]], "busy")
        self.assertEqual(verdicts[manifest()["targets"][1]["worker_id"]], "not running")
        self.fleet.stale = {target["worker_id"]}
        verdicts = {entry["worker_id"]: entry["verdict"] for entry in self.fleet.rehearse()["verdicts"]}
        self.assertEqual(verdicts[target["worker_id"]], "stale heartbeat")

    def test_dry_run_classifies_identity_and_unit_mismatch(self):
        target = manifest()["targets"][0]
        self.running(target)
        self.make_proc_tree(child_state="Z")
        with patch.object(self.fleet, "process_identity",
                          side_effect=ValueError("supervisor identity differs from the takeover target")):
            verdicts = {entry["worker_id"]: entry["verdict"] for entry in self.fleet.rehearse()["verdicts"]}
        self.assertEqual(verdicts[target["worker_id"]], "identity mismatch")
        with patch.object(self.fleet, "unit_main_pid", return_value=0):
            verdicts = {entry["worker_id"]: entry["verdict"] for entry in self.fleet.rehearse()["verdicts"]}
        self.assertEqual(verdicts[target["worker_id"]], "unit missing")

    def test_dry_run_reports_live_children_as_busy(self):
        target = manifest()["targets"][0]
        self.running(target)
        self.make_proc_tree(child_state="S")
        with patch.object(takeover_module, "live_children", side_effect=takeover_module.live_children):
            verdicts = {entry["worker_id"]: entry["verdict"] for entry in self.fleet.rehearse()["verdicts"]}
        self.assertEqual(verdicts[target["worker_id"]], "busy")

    def test_main_dry_run_reports_and_takes_no_lock(self):
        atomic_json(str(self.root / "manifest.json"), manifest())
        argv = [
            "zai-takeover.py", "--dry-run",
            "--manifest", str(self.root / "manifest.json"),
            "--state", str(self.root / "state.json"),
            "--heartbeat-dir", str(self.heartbeats),
            "--unit-dir", str(self.root / "units"),
            "--proc-root", str(self.root / "proc"),
        ]
        with patch.object(sys, "argv", argv), patch("sys.stdout", new_callable=io.StringIO) as out:
            with patch.object(os, "pidfd_open", side_effect=AssertionError("dry run must not open a pidfd")), \
                 patch.object(takeover_module.signal, "pidfd_send_signal", side_effect=AssertionError("dry run must not signal")):
                self.assertEqual(takeover_module.main(), 0)
        report = json.loads(out.getvalue())
        self.assertEqual(len(report["verdicts"]), len(manifest()["targets"]))
        self.assertFalse((self.root / "state.json.lock").exists())

    def test_main_dry_run_still_rejects_a_tampered_manifest(self):
        document = manifest(targets=[dict(worker_id="glm-1", agent="claude-code-glm-5.3-flash", unit="other.service")])
        atomic_json(str(self.root / "manifest.json"), document)
        argv = [
            "zai-takeover.py", "--dry-run",
            "--manifest", str(self.root / "manifest.json"),
            "--state", str(self.root / "state.json"),
        ]
        with patch.object(sys, "argv", argv):
            self.assertEqual(takeover_module.main(), 1)


if __name__ == "__main__":
    unittest.main()
