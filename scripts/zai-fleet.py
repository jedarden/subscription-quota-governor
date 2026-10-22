#!/usr/bin/env python3
"""Observe/reconcile an explicitly owned GLM systemd roster without killing work."""
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

from subgov_adapter_common import atomic_json, atomic_text, live_children, locked, read_json

IDENTIFIER = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]*$")
AGENT = re.compile(r"^claude-code-glm-[A-Za-z0-9_.-]+$")
IDLE_STATES = {"SELECTING", "IDLE", "WAITING", "BACKOFF", "SLEEPING", "EXHAUSTED"}


def roster_read(path):
    roster = read_json(path)
    if roster.get("version") != 1 or roster.get("scope") not in {"user", "system"} or not IDENTIFIER.fullmatch(roster.get("owner", "")):
        raise ValueError("invalid roster ownership or scope")
    workers = roster.get("workers")
    if not isinstance(workers, list) or not workers:
        raise ValueError("empty roster")
    units, identities = set(), set()
    for worker in workers:
        worker_id, agent, unit = (worker.get(key, "") for key in ("worker_id", "agent", "unit"))
        if not IDENTIFIER.fullmatch(worker_id) or not AGENT.fullmatch(agent) or unit != f"needle-worker@{worker_id}.service":
            raise ValueError("roster unit must match an explicit GLM worker identity")
        if unit in units or worker_id in identities:
            raise ValueError("duplicate roster unit or worker identity")
        units.add(unit)
        identities.add(worker_id)
    return roster


class Fleet:
    def __init__(self, args, roster):
        self.args, self.roster = args, roster
        self.state_path = os.path.abspath(os.path.expanduser(args.state))
        # systemd condition arguments deliberately use a restricted literal path.
        if re.search(r"[\s%\\\n]", self.state_path):
            raise ValueError("state path contains unsupported systemd characters")
        self.identity = hashlib.sha256(json.dumps(roster, sort_keys=True).encode()).hexdigest()
        self.base = ["systemctl"] + (["--user"] if roster["scope"] == "user" else [])
        self.unit_dir = Path(os.path.expanduser(args.unit_dir))
        if roster["scope"] == "system" and self.unit_dir != Path("/etc/systemd/system"):
            raise ValueError("system scope requires explicit /etc/systemd/system unit directory")
        self.heartbeat_dir = Path(os.path.expanduser(args.heartbeat_dir))
        self.routing_dir = Path(os.path.expanduser(args.routing_dir))
        self.proc = Path(args.proc_root)

    def command(self, *args):
        # Never forward service output: even systemd diagnostics can quote envs.
        result = subprocess.run(self.base + list(args), stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=2, check=False, text=True)
        if result.returncode:
            raise RuntimeError("systemd operation failed")
        return result.stdout

    def status(self, worker):
        output = self.command("show", worker["unit"], "--property=Id,MainPID,ActiveState,SubState,UnitFileState,LoadState", "--no-pager")
        info = dict(line.split("=", 1) for line in output.splitlines() if "=" in line)
        if info.get("Id") != worker["unit"] or info.get("LoadState") not in {"loaded", "masked"}:
            raise ValueError("unit missing or identity mismatch")
        info["pid"] = int(info.get("MainPID", "0"))
        info["masked"] = info.get("LoadState") == "masked" or info.get("UnitFileState", "").startswith("masked")
        return info

    def check_routing(self, worker):
        path = self.routing_dir / (worker["worker_id"] + ".env")
        # Read only the identity; credentials are neither retained nor emitted.
        agents = [line.split("=", 1)[1].strip().strip("\"'") for line in path.read_text().splitlines() if line.startswith("NEEDLE_AGENT=")]
        if agents != [worker["agent"]]:
            raise ValueError("configured GLM identity differs from roster")

    def heartbeat(self, worker, pid):
        identity = f"{worker['agent']}-{worker['worker_id']}"
        heartbeat = read_json(self.heartbeat_dir / (identity + ".json"))
        if heartbeat.get("qualified_id") != identity or heartbeat.get("worker_id") != worker["worker_id"] or heartbeat.get("pid") != pid or heartbeat.get("model", worker["agent"]) != worker["agent"]:
            raise ValueError("heartbeat identity differs from systemd MainPID/roster")
        stamp = datetime.fromisoformat(heartbeat["last_heartbeat"].replace("Z", "+00:00"))
        age = (datetime.now(timezone.utc) - stamp).total_seconds()
        if not 0 <= age <= self.args.stale_after:
            raise ValueError("worker heartbeat is stale")
        return heartbeat

    def process_identity(self, worker, pid):
        command = (self.proc / str(pid) / "cmdline").read_bytes().decode().split("\0")
        if not command or Path(command[0]).name not in {"needle", "needle-stable"} or "run" not in command:
            raise ValueError("MainPID is not NEEDLE supervisor")
        for flag, expected in (("--identifier", worker["worker_id"]), ("--agent", worker["agent"])):
            if flag not in command or command.index(flag) + 1 >= len(command) or command[command.index(flag) + 1] != expected:
                raise ValueError("MainPID command identity differs from roster")
        # starttime protects against PID reuse between verification and signals.
        return (self.proc / str(pid) / "stat").read_text().rsplit(")", 1)[1].split()[19]

    def paths(self, worker):
        return self.unit_dir / (worker["unit"] + ".d") / "90-subgov-zai.conf", Path(self.state_path + ".retired") / worker["unit"]

    def dropin(self, worker):
        _, marker = self.paths(worker)
        return f"# subgov-zai owner={self.roster['owner']} state={self.state_path}\n[Unit]\nConditionPathExists=!{marker}\n[Service]\nRestart=no\n"

    def validate_owned_files(self):
        for worker in self.roster["workers"]:
            dropin, marker = self.paths(worker)
            if dropin.exists() and dropin.read_text() != self.dropin(worker):
                raise ValueError("unit has another governor's retirement override")
            if marker.exists() and marker.read_text() != self.identity + "\n":
                raise ValueError("retirement marker ownership conflict")

    def inspect(self):
        self.validate_owned_files()
        items = []
        for worker in self.roster["workers"]:
            self.check_routing(worker)
            info = self.status(worker)
            if info["pid"]:
                self.process_identity(worker, info["pid"])
                self.heartbeat(worker, info["pid"])
            elif info.get("ActiveState") not in {"inactive", "failed"}:
                raise ValueError("worker is transitioning without a verifiable MainPID")
            items.append((worker, info))
        return items

    @staticmethod
    def idle(heartbeat):
        empty = heartbeat.get("current_bead", "missing") is None and heartbeat.get("current_task") is None
        # NEEDLE marks admission holds non-idle although they have no task.
        # Stop still requires fresh identity, frozen threads, and no children.
        return empty and ((heartbeat.get("is_idle") is True and heartbeat.get("state") in IDLE_STATES) or heartbeat.get("state") == "ADMISSION_BLOCKED")

    def stop_idle(self, worker, info):
        pid = info["pid"]
        if not pid or not self.idle(self.heartbeat(worker, pid)):
            return False
        generation = self.process_identity(worker, pid)
        # pidfd pins the supervisor identity; it cannot signal a reused PID.
        pidfd = os.pidfd_open(pid)
        frozen = False
        try:
            if self.process_identity(worker, pid) != generation:
                return False
            frozen = True
            signal.pidfd_send_signal(pidfd, signal.SIGSTOP)
            limit = time.monotonic() + 0.3
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
            if not self.idle(self.heartbeat(worker, pid)) or self.status(worker)["pid"] != pid or self.process_identity(worker, pid) != generation:
                return False
            self.command("stop", "--no-block", worker["unit"])
            return True
        finally:
            if frozen:
                try:
                    signal.pidfd_send_signal(pidfd, signal.SIGCONT)
                except ProcessLookupError:
                    pass
            os.close(pidfd)

    def reconcile(self, desired, state):
        items = self.inspect()  # Validate the complete roster before any mutation.
        permitted = [(worker, info) for worker, info in items if not info["masked"]]
        if desired > len(permitted):
            raise ValueError("desired count exceeds unmasked explicit roster")
        # Keep already active workers first; stable roster order determines ties.
        ordered = sorted(permitted, key=lambda item: not bool(item[1]["pid"]))
        keep = {worker["unit"] for worker, _ in ordered[:desired]}
        retirees = [worker["unit"] for worker, info in items if worker["unit"] not in keep]
        state.update(desired=desired, pending_retirees=retirees)
        atomic_json(self.state_path, state)
        changed = False
        for worker, info in items:
            dropin, marker = self.paths(worker)
            if worker["unit"] in keep:
                # Only our exact files were admitted by validate_owned_files().
                for path in (marker, dropin):
                    if path.exists():
                        path.unlink()
                        changed = True
            else:
                if not marker.exists():
                    atomic_text(str(marker), self.identity + "\n")
                if not dropin.exists():
                    atomic_text(str(dropin), self.dropin(worker))
                    changed = True
        # Reload even when a previous interrupted invocation wrote all files
        # but never reached its reload. Idempotent retry must install guards.
        self.command("daemon-reload")
        for worker, info in items:
            if worker["unit"] in keep:
                # Refresh masking immediately before starting; never unmask.
                current = self.status(worker)
                if not current["masked"] and not current["pid"] and current["ActiveState"] in {"inactive", "failed"}:
                    self.command("start", "--no-block", worker["unit"])
            elif info["pid"]:
                self.stop_idle(worker, info)
        state["pending_retirees"] = [worker["unit"] for worker, _ in items if worker["unit"] not in keep and self.status(worker)["pid"]]
        atomic_json(self.state_path, state)

    def load_state(self):
        state = read_json(self.state_path, {"version": 1, "owner": self.roster["owner"], "roster_hash": self.identity, "pending_retirees": []})
        if state.get("version") != 1 or state.get("owner") != self.roster["owner"] or state.get("roster_hash") != self.identity:
            raise ValueError("fleet state ownership/roster conflict")
        return state


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--roster", required=True)
    parser.add_argument("--state", required=True)
    parser.add_argument("--heartbeat-dir", default="~/.needle/state/heartbeats")
    parser.add_argument("--routing-dir", default="~/.config/needle/workers")
    parser.add_argument("--unit-dir", default="~/.config/systemd/user", help="system scope must explicitly pass /etc/systemd/system")
    parser.add_argument("--proc-root", default="/proc", help=argparse.SUPPRESS)
    parser.add_argument("--stale-after", type=float, default=60)
    sub = parser.add_subparsers(dest="mode", required=True)
    sub.add_parser("observe")
    apply = sub.add_parser("actuate")
    apply.add_argument("desired", type=int)
    args = parser.parse_args()
    if args.stale_after <= 0 or (args.mode == "actuate" and args.desired < 0):
        parser.error("invalid freshness/desired count")
    def deadline(_signum, _frame):
        raise TimeoutError("fleet deadline exceeded")
    signal.signal(signal.SIGALRM, deadline)
    signal.alarm(12)
    try:
        roster = roster_read(args.roster)
        fleet = Fleet(args, roster)
        with locked(fleet.state_path):
            state = fleet.load_state()
            if args.mode == "observe":
                # Drainers remain current until their MainPID is actually gone.
                count = sum(bool(info["pid"]) for _, info in fleet.inspect())
                print(json.dumps({"current_workers": count}))
            else:
                fleet.reconcile(args.desired, state)
    except Exception as error:
        print(f"zai-fleet: operation failed ({type(error).__name__})", file=sys.stderr)
        return 1
    finally:
        signal.alarm(0)
    return 0


if __name__ == "__main__":
    sys.exit(main())
