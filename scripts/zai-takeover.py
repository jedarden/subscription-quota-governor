#!/usr/bin/env python3
"""Retire takeover-listed GLM workers idle-only once a prefixed replacement runs.

Plan §26.2: each pool declares a takeover list of roster units and adopted
stragglers. A listed worker is retired only when a subgov-prefixed
replacement in the same pool is running, and only while it is verifiably
idle: pidfd pins the supervisor identity, SIGSTOP freezes every thread, the
children check ignores zombies, and retirement is a plain SIGTERM (via the
systemd unit where one exists). `needle cleanup` is never used and a busy
worker is never signalled. Runs outside the governor's sandbox because the
gate needs /proc and signal reach the subgov unit does not have.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time
from datetime import datetime, timezone

from subgov_adapter_common import atomic_text, live_children, locked, read_json

IDENTIFIER = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]*$")
IDLE_STATES = {"SELECTING", "IDLE", "WAITING", "BACKOFF", "SLEEPING", "EXHAUSTED"}
# Budget for the freeze verification (all threads stopped) and, for a
# straggler with no unit to stop it, for the supervised exit after SIGTERM.
FREEZE_SECONDS = 0.3
EXIT_SECONDS = 2.0


def manifest_read(path):
    manifest = read_json(path)
    if manifest.get("version") != 1 or not IDENTIFIER.fullmatch(manifest.get("owner", "")):
        raise ValueError("invalid takeover manifest ownership")
    prefix = manifest.get("identifier_prefix", "")
    if not prefix or not IDENTIFIER.fullmatch(prefix):
        raise ValueError("takeover manifest requires an identifier_prefix")
    targets = manifest.get("targets")
    if not isinstance(targets, list) or not targets:
        raise ValueError("empty takeover list")
    seen = set()
    for target in targets:
        worker_id, agent, unit = (target.get(key, "") for key in ("worker_id", "agent", "unit"))
        if not IDENTIFIER.fullmatch(worker_id) or not IDENTIFIER.fullmatch(agent):
            raise ValueError("takeover target identity must be conservative")
        if worker_id.startswith(prefix):
            raise ValueError("takeover target must not carry the pool prefix")
        if unit and unit != f"needle-worker@{worker_id}.service":
            raise ValueError("takeover unit must match the worker identity")
        if worker_id in seen:
            raise ValueError("duplicate takeover target")
        seen.add(worker_id)
    return manifest


class Takeover:
    def __init__(self, args, manifest):
        self.args, self.manifest = args, manifest
        self.state_path = os.path.abspath(os.path.expanduser(args.state))
        if re.search(r"[\s%\\\n]", self.state_path):
            raise ValueError("state path contains unsupported systemd characters")
        self.identity = hashlib.sha256(json.dumps(manifest, sort_keys=True).encode()).hexdigest()
        self.base = ["systemctl", "--user"]
        self.unit_dir = Path(os.path.expanduser(args.unit_dir))
        self.heartbeat_dir = Path(os.path.expanduser(args.heartbeat_dir))
        self.proc = Path(args.proc_root)

    def systemd(self, *args):
        # Never forward service output: even systemd diagnostics can quote envs.
        result = subprocess.run(self.base + list(args), stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=2, check=False, text=True)
        if result.returncode:
            raise RuntimeError("systemd operation failed")
        return result.stdout

    def unit_main_pid(self, unit):
        output = self.systemd("show", unit, "--property=MainPID", "--no-pager")
        return int(dict(line.split("=", 1) for line in output.splitlines() if "=" in line).get("MainPID", "0"))

    def heartbeat(self, target, pid):
        identity = f"{target['agent']}-{target['worker_id']}"
        heartbeat = read_json(self.heartbeat_dir / (identity + ".json"))
        if heartbeat.get("qualified_id") != identity or heartbeat.get("worker_id") != target["worker_id"] or heartbeat.get("pid") != pid:
            raise ValueError("heartbeat identity differs from the takeover target")
        stamp = datetime.fromisoformat(heartbeat["last_heartbeat"].replace("Z", "+00:00"))
        age = (datetime.now(timezone.utc) - stamp).total_seconds()
        if not 0 <= age <= self.args.stale_after:
            raise ValueError("worker heartbeat is stale")
        return heartbeat

    def process_identity(self, target, pid):
        command = (self.proc / str(pid) / "cmdline").read_bytes().decode().split("\0")
        if not command or Path(command[0]).name not in {"needle", "needle-stable"} or "run" not in command:
            raise ValueError("pid is not a NEEDLE supervisor")
        for flag, expected in (("--identifier", target["worker_id"]), ("--agent", target["agent"])):
            if flag not in command or command.index(flag) + 1 >= len(command) or command[command.index(flag) + 1] != expected:
                raise ValueError("supervisor identity differs from the takeover target")
        # starttime protects against PID reuse between verification and signals.
        return (self.proc / str(pid) / "stat").read_text().rsplit(")", 1)[1].split()[19]

    @staticmethod
    def idle(heartbeat):
        empty = heartbeat.get("current_bead", "missing") is None and heartbeat.get("current_task") is None
        # NEEDLE marks admission holds non-idle although they have no task.
        return empty and ((heartbeat.get("is_idle") is True and heartbeat.get("state") in IDLE_STATES) or heartbeat.get("state") == "ADMISSION_BLOCKED")

    def replacements(self):
        """Fresh heartbeats whose worker id carries the pool prefix: the
        running prefixed replacements that authorize retirement (plan §26.2).
        A malformed heartbeat fails closed, like every other reader."""
        count = 0
        for path in self.heartbeat_dir.glob("*.json"):
            heartbeat = read_json(path)
            stamp = datetime.fromisoformat(heartbeat["last_heartbeat"].replace("Z", "+00:00"))
            age = (datetime.now(timezone.utc) - stamp).total_seconds()
            if 0 <= age <= self.args.stale_after and heartbeat["worker_id"].startswith(self.manifest["identifier_prefix"]):
                count += 1
        return count

    def paths(self, target):
        marker = Path(self.state_path + ".retired") / f"{target['worker_id']}.marker"
        return self.unit_dir / (target["unit"] + ".d") / "91-subgov-zai-takeover.conf", marker

    def dropin(self, target):
        _, marker = self.paths(target)
        return f"# subgov-zai-takeover owner={self.manifest['owner']} state={self.state_path}\n[Unit]\nConditionPathExists=!{marker}\n[Service]\nRestart=no\n"

    def install_guard(self, target):
        """Stop systemd restarting the unit after retirement: a retirement
        marker plus a Restart=no drop-in conditioned on it, both owned by this
        manifest identity (zai-fleet's mechanism, separate file names)."""
        dropin, marker = self.paths(target)
        if dropin.exists() and dropin.read_text() != self.dropin(target):
            raise ValueError("unit has another governor's takeover override")
        if marker.exists() and marker.read_text() != self.identity + "\n":
            raise ValueError("takeover marker ownership conflict")
        if not marker.exists():
            atomic_text(str(marker), self.identity + "\n")
        if not dropin.exists():
            atomic_text(str(dropin), self.dropin(target))
            self.systemd("daemon-reload")

    def stop_unit(self, target, pidfd):
        self.install_guard(target)
        self.systemd("stop", "--no-block", target["unit"])

    def stop_straggler(self, target, pidfd):
        # A straggler has no unit: SIGTERM the frozen supervisor itself. The
        # SIGCONT delivers the pending SIGTERM so the caller's await_exit can
        # verify the exit; anything still alive is left running for the next
        # cycle, no escalation, and `needle cleanup` is never used.
        signal.pidfd_send_signal(pidfd, signal.SIGTERM)
        try:
            signal.pidfd_send_signal(pidfd, signal.SIGCONT)
        except ProcessLookupError:
            pass

    def retire(self, limit):
        fresh = self.replacements()
        budget = min(limit, fresh)
        if budget <= 0:
            return {"retired": 0, "reason": "no running replacement" if fresh == 0 else "nothing to retire"}
        retired = []
        for target in self.manifest["targets"]:
            if len(retired) >= budget:
                break
            if self.stop_idle(target):
                retired.append(target["worker_id"])
        return {"retired": retired}

    def stop_idle(self, target):
        try:
            identity = f"{target['agent']}-{target['worker_id']}"
            pid = read_json(self.heartbeat_dir / (identity + ".json"), {}).get("pid")
            if not isinstance(pid, int) or pid <= 0:
                return False
            heartbeat = self.heartbeat(target, pid)
            if not self.idle(heartbeat):
                return False
            generation = self.process_identity(target, pid)
            pidfd = os.pidfd_open(pid)
            frozen = False
            try:
                if self.process_identity(target, pid) != generation:
                    return False
                frozen = True
                signal.pidfd_send_signal(pidfd, signal.SIGSTOP)
                limit = time.monotonic() + FREEZE_SECONDS
                while True:
                    tasks = list((self.proc / str(pid) / "task").iterdir())
                    if not tasks:
                        return False
                    stopped = all(re.search(r"^State:\s+[Tt]", (task / "status").read_text(), re.MULTILINE) for task in tasks)
                    if stopped:
                        break
                    if time.monotonic() >= limit:
                        return False
                    time.sleep(0.01)
                # Every thread can spawn children, not only the main thread.
                if any(live_children(task, self.proc) for task in tasks):
                    return False
                if target.get("unit") and self.unit_main_pid(target["unit"]) != pid:
                    return False
                if not self.idle(self.heartbeat(target, pid)) or self.process_identity(target, pid) != generation:
                    return False
                if target.get("unit"):
                    self.stop_unit(target, pidfd)
                else:
                    self.stop_straggler(target, pidfd)
                    self.await_exit(pid)
                return True
            finally:
                if frozen:
                    try:
                        signal.pidfd_send_signal(pidfd, signal.SIGCONT)
                    except ProcessLookupError:
                        pass
                os.close(pidfd)
        except (OSError, ValueError, RuntimeError, KeyError):
            # A target that cannot be verified this cycle is left untouched.
            return False

    def await_exit(self, pid):
        limit = time.monotonic() + EXIT_SECONDS
        while time.monotonic() < limit:
            if not (self.proc / str(pid)).exists():
                return
            time.sleep(0.05)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", required=True)
    parser.add_argument("--state", required=True)
    parser.add_argument("--heartbeat-dir", default="~/.needle/state/heartbeats")
    parser.add_argument("--unit-dir", default="~/.config/systemd/user")
    parser.add_argument("--proc-root", default="/proc", help=argparse.SUPPRESS)
    parser.add_argument("--stale-after", type=float, default=60)
    parser.add_argument("--limit", type=int, default=1)
    args = parser.parse_args()
    if args.stale_after <= 0 or args.limit < 1:
        parser.error("invalid freshness/retirement limit")

    def deadline(_signum, _frame):
        raise TimeoutError("takeover deadline exceeded")
    signal.signal(signal.SIGALRM, deadline)
    signal.alarm(12)
    try:
        manifest = manifest_read(args.manifest)
        takeover = Takeover(args, manifest)
        with locked(takeover.state_path):
            print(json.dumps(takeover.retire(args.limit)))
    except Exception as error:
        print(f"zai-takeover: operation failed ({type(error).__name__})", file=sys.stderr)
        return 1
    finally:
        signal.alarm(0)
    return 0


if __name__ == "__main__":
    sys.exit(main())
