#!/usr/bin/env python3
"""Publish reviewed Codex demand and reconcile only the governed worker prefix.

Admission is explicit in a reviewed manifest, enforced by NEEDLE's existing
positive-label lane. Quota availability never creates or admits work.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import subprocess
import sys
import time

from subgov_adapter_common import atomic_json, atomic_text, locked, read_json

IDLE = {"SELECTING", "IDLE", "WAITING", "BACKOFF", "SLEEPING", "EXHAUSTED", "ADMISSION_BLOCKED"}


def run(argv, cwd=None, timeout=5):
    result = subprocess.run(argv, cwd=cwd, capture_output=True, text=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError("read-only command failed" if argv[0] == "bead" else "adapter command failed")
    return result.stdout


def document(stdout):
    value = json.loads(stdout)
    # Support current bead-rs objects/arrays and old singleton arrays without
    # confusing a JSON array with one JSONL issue.
    return value


def single(stdout):
    value = document(stdout)
    if isinstance(value, list):
        if len(value) != 1:
            raise ValueError("expected one bead")
        value = value[0]
    if not isinstance(value, dict):
        raise ValueError("invalid bead document")
    return value


def current_time():
    return datetime.now(timezone.utc)


def allowed(issue, now):
    if issue.get("status") != "open" or issue.get("assignee") or issue.get("manual_blocked"):
        return False
    for raw in issue.get("labels", []):
        label = raw.lower()
        if label in {"human", "human-owned", "owner:human", "proposal-pending", "deferred",
                     "blocked", "manual_blocked", "manual-blocked", "blocked:manual"} or label.startswith("human:"):
            return False
        if label.startswith(("deferred:", "quarantine-until:", "retry-cooldown-until:")):
            try:
                until = datetime.fromisoformat(raw.split(":", 1)[1].replace("Z", "+00:00"))
                if until > now:
                    return False
            except (ValueError, TypeError):
                return False
    return True


def settings_read(path):
    value = read_json(path)
    if value.get("version") != 1 or value.get("host") != "codinghome":
        raise ValueError("invalid host or manifest version")
    if value.get("prefix") != "sg-codex-" or not 1 <= value.get("max_workers", 0) <= 12:
        raise ValueError("unexpected worker ownership")
    if not re.fullmatch(r"codex-[A-Za-z0-9_.-]+", value.get("agent", "")):
        raise ValueError("invalid Codex adapter")
    keys = [(entry["workspace"], entry["id"]) for entry in value["work"]]
    if len(keys) != len(set(keys)):
        raise ValueError("duplicate reviewed outcome")
    return value


def policy_ok(route, host):
    # Reuse the deployed publisher's conservative backend/queue policy parser.
    import importlib.util
    spec = importlib.util.spec_from_file_location("subgov_policy", Path(__file__).with_name("zai-source-refresh.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    policy = module.workspace_policy(route / ".needle.yaml")
    return ((route / ".beads/config.json").is_file()
            and policy["bead_cli"].get("backend") == "bead-rs"
            and policy["queue"].get("owner_host", host) == host)


def gate_degraded(settings, route):
    key = hashlib.sha256(str(route.resolve()).encode()).hexdigest()[:12]
    value = read_json(Path(settings["gate_health_dir"]) / (key + ".json"), {})
    return value.get("degraded", False) is not False


def identity(pid, agent, identifier):
    path = Path("/proc") / str(pid)
    args = path.joinpath("cmdline").read_bytes().decode().split("\0")
    if Path(args[0]).name not in {"needle", "needle-stable"} or "run" not in args:
        raise ValueError("not a NEEDLE supervisor")
    for flag, expected in [("--agent", agent), ("--identifier", identifier)]:
        if flag not in args or args[args.index(flag) + 1] != expected:
            raise ValueError("worker identity mismatch")
    return path.joinpath("stat").read_text().rsplit(")", 1)[1].split()[19]


def workers(settings, fresh=True):
    result = []
    for path in Path(settings["heartbeat_dir"]).glob(settings["agent"] + "-*.json"):
        value = read_json(path)
        name = value.get("worker_id", "")
        if value.get("qualified_id") != settings["agent"] + "-" + name:
            continue
        try:
            age = (current_time() - datetime.fromisoformat(value["last_heartbeat"].replace("Z", "+00:00"))).total_seconds()
            identity(value["pid"], settings["agent"], name)
        except FileNotFoundError:
            continue
        if not fresh or 0 <= age < 60:
            result.append(value)
    return result


def demand(settings):
    now, ready, blocked = current_time(), [], []
    frontiers = {}
    for entry in settings["work"]:
        route = Path(entry["workspace"]).resolve()
        reason = None
        if not entry.get("admission_evidence"):
            raise ValueError("missing admission evidence")
        if entry.get("external_hold"):
            reason = "recorded_external_hold"
        elif not policy_ok(route, settings["host"]):
            reason = "backend_or_queue_owner"
        elif gate_degraded(settings, route):
            reason = "workspace_gate_degraded"
        else:
            if str(route) not in frontiers:
                values = document(run(["bead", "list", "--ready", "--json", "--limit", "10000"], cwd=route))
                if not isinstance(values, list):
                    raise ValueError("invalid native ready array")
                frontiers[str(route)] = {item["id"] for item in values}
            issue = single(run(["bead", "show", entry["id"], "--json"], cwd=route))
            if issue.get("revision") != entry["reviewed_revision"]:
                reason = "changed_since_review"
            elif settings["lane_label"] not in issue.get("labels", []):
                reason = "missing_dispatch_lane"
            elif issue["id"] not in frontiers[str(route)] or not allowed(issue, now):
                reason = "not_currently_claimable"
            else:
                ready.append({"id": issue["id"], "workspace": str(route)})
        if reason:
            blocked.append({"id": entry["id"], "reason": reason})
    owned_busy, unowned = [], 0
    for worker in workers(settings):
        if not worker["worker_id"].startswith(settings["prefix"]):
            unowned += 1
            continue
        task = worker.get("current_bead") or worker.get("current_task")
        if not task:
            continue
        route = Path(worker["workspace"]).resolve()
        if not policy_ok(route, settings["host"]):
            raise ValueError("busy worker route ownership unknown")
        issue = single(run(["bead", "show", task, "--json"], cwd=route))
        if issue.get("status") != "in_progress" or issue.get("assignee") != worker["qualified_id"]:
            raise ValueError("busy worker claim identity mismatch")
        owned_busy.append({"id": task, "workspace": str(route)})
    jobs = {(item["workspace"], item["id"]) for item in ready + owned_busy}
    return {"observed_at": now.isoformat(), "ready": ready, "busy": owned_busy,
            "blocked": blocked, "unowned_floor": unowned,
            "eligible_backlog_capacity": len(jobs) + unowned}


def refresh(settings):
    with locked(settings["snapshot"]):
        quota = document(run([settings["quota_binary"], "--config", settings["quota_config"],
                              "snapshot", "codex"], timeout=35))
        quota.pop("eligible_backlog_capacity", None)
        try:
            measured = demand(settings)
            quota["eligible_backlog_capacity"] = measured["eligible_backlog_capacity"]
        except Exception as exc:
            # Unknown demand must hold existing workers, never manufacture zero
            # or grant capacity. Do not forward CLI output or credentials.
            measured = {"observed_at": current_time().isoformat(), "unknown": type(exc).__name__}
        atomic_json(settings["demand_state"], measured)
        atomic_json(settings["snapshot"], quota)


def idle(worker):
    return (worker.get("state") in IDLE and worker.get("current_bead", "missing") is None
            and worker.get("current_task") is None
            and (worker.get("is_idle") is True or worker.get("state") == "ADMISSION_BLOCKED"))


def stop_idle(settings, worker):
    if not idle(worker):
        return False
    pid, name = worker["pid"], worker["worker_id"]
    generation = identity(pid, settings["agent"], name)
    fd = os.pidfd_open(pid)
    frozen = False
    try:
        if identity(pid, settings["agent"], name) != generation:
            return False
        signal.pidfd_send_signal(fd, signal.SIGSTOP)
        frozen = True
        limit = time.monotonic() + .5
        while True:
            tasks = list(Path("/proc", str(pid), "task").iterdir())
            if tasks and all(re.search(r"^State:\s+[Tt]", (task / "status").read_text(), re.M) for task in tasks):
                break
            if time.monotonic() > limit:
                return False
            time.sleep(.01)
        if any((task / "children").read_text().strip() for task in tasks):
            return False
        current = next((value for value in workers(settings) if value["pid"] == pid), None)
        if not current or not idle(current) or identity(pid, settings["agent"], name) != generation:
            return False
        signal.pidfd_send_signal(fd, signal.SIGTERM)
        return True
    finally:
        if frozen:
            try:
                signal.pidfd_send_signal(fd, signal.SIGCONT)
            except ProcessLookupError:
                pass
        os.close(fd)


def actuate(settings, desired):
    if not 0 <= desired <= settings["max_workers"]:
        raise ValueError("desired outside owned ceiling")
    with locked(settings["actuator_state"]):
        live = [worker for worker in workers(settings, fresh=False)
                if worker["worker_id"].startswith(settings["prefix"])]
        pending = []
        if len(live) > desired:
            remaining = len(live) - desired
            for worker in sorted(live, key=lambda value: value["worker_id"], reverse=True):
                if not remaining:
                    break
                if stop_idle(settings, worker):
                    remaining -= 1
                else:
                    pending.append(worker["worker_id"])
            if not remaining:
                pending = []
        elif len(live) < desired:
            measured = demand(settings)  # Revalidate actual work immediately before launch.
            available = measured["ready"][:desired - len(live)]
            taken = {worker["worker_id"] for worker in live}
            for job in available:
                number = next(i for i in range(1, settings["max_workers"] + 1)
                              if settings["prefix"] + str(i) not in taken)
                name = settings["prefix"] + str(number)
                # Reuse NEEDLE's inner tmux entry point, with non-secret,
                # per-worker environment overrides. This does not alter the
                # tmux server environment or other workers. Disable Weave's
                # generation gate so spare quota cannot generate make-work.
                binary = shutil.which("needle")
                if not binary:
                    raise ValueError("NEEDLE not installed")
                argv = [binary, "run", "--agent", settings["agent"], "--workspace", job["workspace"],
                        "--identifier", name, "--count", "1"]
                session = "needle-" + settings["agent"].replace(".", "_") + "-" + name
                run(["tmux", "new-session", "-d", "-s", session,
                     "-e", "NEEDLE_INNER=1", "-e", "NEEDLE_STRANDS__GENERATION__ENABLED=false",
                     shlex.join(argv)], timeout=5)
                taken.add(name)
        atomic_json(settings["actuator_state"], {"desired": desired, "pending_retirees": pending,
                                                "observed_at": current_time().isoformat()})


def install_lane(settings, config, remove=False):
    path = Path(config)
    if path.is_symlink():
        raise ValueError("resolve maintained configuration source first")
    original = path.read_text()
    label = settings["lane_label"]
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.:-]*", label):
        raise ValueError("invalid dispatch label")
    names = [settings["prefix"] + str(i) for i in range(1, settings["max_workers"] + 1)]
    block = "    - label: " + label + "\n      workers:\n"
    block += "".join("      - " + name + "\n" for name in names)
    block += "      when_empty: idle\n"
    # Recognize only the existing simple, indentless lane sequence inside
    # strands.pluck. Refuse flow YAML, anchors, or unfamiliar layouts instead
    # of guessing. Every unrelated byte is retained, including comments.
    strands = re.search(r"(?m)^strands:\s*\n", original)
    if not strands:
        raise ValueError("missing strands configuration")
    tail = original[strands.end():]
    outer_end = re.search(r"(?m)^\S", tail)
    body = tail[:outer_end.start()] if outer_end else tail
    pluck = re.search(r"(?m)^  pluck:\s*\n", body)
    if not pluck:
        raise ValueError("missing pluck configuration")
    tail = body[pluck.end():]
    pluck_end = re.search(r"(?m)^  [A-Za-z_]", tail)
    body = tail[:pluck_end.start()] if pluck_end else tail
    lanes = re.search(r"(?m)^    lanes:\s*\n(?P<rows>(?:(?:    -|      ).*\n)*)", body)
    if not lanes:
        raise ValueError("unrecognized local lane layout")
    rows = lanes.group("rows")
    pattern = re.compile(r"    - label: ([A-Za-z0-9_.:-]+)\n      workers:\n((?:      - [A-Za-z0-9_.-]+\n)+)      when_empty: (idle|normal)\n")
    entries = list(pattern.finditer(rows))
    if "".join(item.group(0) for item in entries) != rows:
        raise ValueError("complex or malformed lane layout")
    own = [item for item in entries if item.group(1) == label]
    if own:
        if len(own) != 1 or own[0].group(0) != block:
            raise ValueError("existing owned lane differs; review required")
        if not remove:
            return
    elif remove:
        return
    if not own and any(set(re.findall(r"      - ([A-Za-z0-9_.-]+)", item.group(2))) & set(names)
                       for item in entries):
        raise ValueError("worker already bound to another lane")
    offset = strands.end() + pluck.end() + lanes.start("rows")
    if remove:
        start = offset + own[0].start()
        updated = original[:start] + original[start + len(block):]
    else:
        start = offset + len(rows)
        updated = original[:start] + block + original[start:]
    atomic_text(str(path), updated)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--settings", required=True)
    sub = parser.add_subparsers(dest="mode", required=True)
    sub.add_parser("refresh")
    sub.add_parser("demand")
    apply = sub.add_parser("actuate")
    apply.add_argument("desired", type=int)
    lane = sub.add_parser("install-lane")
    lane.add_argument("config")
    lane_remove = sub.add_parser("remove-lane")
    lane_remove.add_argument("config")
    args = parser.parse_args()
    settings = settings_read(args.settings)
    if args.mode == "refresh":
        refresh(settings)
    elif args.mode == "demand":
        print(json.dumps(demand(settings)))
    elif args.mode == "actuate":
        actuate(settings, args.desired)
    else:
        install_lane(settings, args.config, remove=args.mode == "remove-lane")


if __name__ == "__main__":
    try:
        main()
    except Exception as exc:
        print("Codex productive adapter failed: " + type(exc).__name__, file=sys.stderr)
        sys.exit(1)
