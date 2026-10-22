#!/usr/bin/env python3
"""Publish ready-bead demand and reconcile an explicit owned worker pool.

NEEDLE owns placement and admission. Quota availability never creates work.
"""
import argparse
from datetime import datetime, timezone
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

from subgov_adapter_common import atomic_json, atomic_text, live_children, locked, read_json
from subgov_ready_beads import allowed, native_internal_artifact, policy_ok, observe as ready_demand

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


def settings_read(path):
    value = read_json(path)
    if value.get("version") != 1 or value.get("host") != "codinghome":
        raise ValueError("invalid host or manifest version")
    if value.get("prefix") != "sg-codex-" or not 1 <= value.get("max_workers", 0) <= 12:
        raise ValueError("unexpected worker ownership")
    if not re.fullmatch(r"codex-[A-Za-z0-9_.-]+", value.get("agent", "")):
        raise ValueError("invalid Codex adapter")
    if not isinstance(value.get("demand"), dict):
        raise ValueError("missing ready-bead workspace scope")
    names = set()
    for worker in value.get("adopted_workers", []):
        name, agent = worker.get("worker_id", ""), worker.get("agent", "")
        if (not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", name)
                or name.startswith(value["prefix"]) or name in names
                or not re.fullmatch(r"codex-[A-Za-z0-9_.-]+", agent)
                or not Path(worker.get("workspace", "")).is_absolute()):
            raise ValueError("invalid adopted worker identity")
        if worker.get("unit") not in {None, "needle-worker@" + name + ".service"}:
            raise ValueError("adopted unit identity mismatch")
        names.add(name)
    if len(names) > value["max_workers"]:
        raise ValueError("adopted roster exceeds pool ceiling")
    return value


def adopted(settings, worker):
    return next((entry for entry in settings.get("adopted_workers", [])
                 if entry["worker_id"] == worker["worker_id"]
                 and entry["agent"] == worker.get("model", settings["agent"])), None)


def owned(settings, worker):
    return bool(adopted(settings, worker) or (
        worker["worker_id"].startswith(settings["prefix"])
        and worker.get("model", settings["agent"]) == settings["agent"]))


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
    pattern = "codex-*.json" if settings.get("adopted_workers") else settings["agent"] + "-*.json"
    for path in Path(settings["heartbeat_dir"]).glob(pattern):
        value = read_json(path)
        name = value.get("worker_id", "")
        agent = value.get("model", settings["agent"])
        if value.get("qualified_id") != agent + "-" + name:
            continue
        try:
            age = (current_time() - datetime.fromisoformat(value["last_heartbeat"].replace("Z", "+00:00"))).total_seconds()
            identity(value["pid"], agent, name)
        except FileNotFoundError:
            continue
        if not fresh or 0 <= age < 60:
            result.append(value)
    return result


def unit_info(unit):
    text = run(["systemctl", "--user", "show", unit,
                "--property=Id,MainPID,ActiveState,LoadState,UnitFileState,Restart,EnvironmentFiles", "--no-pager"])
    value = dict(line.split("=", 1) for line in text.splitlines() if "=" in line)
    if value.get("Id") != unit or value.get("LoadState") not in {"loaded", "masked"}:
        raise ValueError("adopted unit missing")
    value["pid"] = int(value.get("MainPID", "0"))
    return value


def check_adopted_routing(settings, entry, pid=0):
    if entry.get("unit"):
        values = {}
        for line in (Path(settings["routing_dir"]) / (entry["worker_id"] + ".env")).read_text().splitlines():
            key, separator, value = line.partition("=")
            if separator and key in {"NEEDLE_AGENT", "NEEDLE_WS"}:
                if key in values:
                    raise ValueError("duplicate worker routing")
                values[key] = value.strip().strip("\"'")
        if values != {"NEEDLE_AGENT": entry["agent"], "NEEDLE_WS": entry["workspace"]}:
            raise ValueError("installed adopted routing differs from reviewed roster")
    if pid:
        args = Path("/proc", str(pid), "cmdline").read_bytes().decode().split("\0")
        if "--workspace" not in args or args[args.index("--workspace") + 1] != entry["workspace"]:
            raise ValueError("adopted startup workspace changed")
        # Inspect only account routing, never retain or print credential values.
        environment = Path("/proc", str(pid), "environ").read_bytes().split(b"\0")
        routing = {}
        forbidden = {b"OPENAI_API_KEY", b"CODEX_API_KEY", b"OPENAI_ACCESS_TOKEN"}
        for item in environment:
            key, separator, value = item.partition(b"=")
            if separator and key in forbidden and value:
                raise ValueError("adopted worker has alternate authentication routing")
            if key in {b"HOME", b"CODEX_HOME"}:
                routing[key] = value
        if routing.get(b"HOME") != b"/home/coding" or routing.get(b"CODEX_HOME", b"/home/coding/.codex") != b"/home/coding/.codex":
            raise ValueError("adopted worker account home differs from quota source")


def retirement_paths(settings, entry):
    unit = entry["unit"]
    return (Path(settings["unit_dir"]) / (unit + ".d") / "zzzz-subgov-codex.conf",
            Path(settings["actuator_state"] + ".retired") / unit)


def retirement_guard(settings, entry):
    _, marker = retirement_paths(settings, entry)
    if re.search(r"[\s%\\]", str(marker)):
        raise ValueError("unsupported retirement marker path")
    return ("# subgov-codex explicit adoption; owner=subgov-4c4a3d24\n[Unit]\n"
            f"ConditionPathExists=!{marker}\n[Service]\nRestart=no\n"
            f"EnvironmentFile={settings['adoption_env']}\n")


def inspect_fleet(settings):
    if settings.get("adoption_env") and Path(settings["adoption_env"]).read_text() != "NEEDLE_STRANDS__GENERATION__ENABLED=false\n":
        raise ValueError("adopted launch policy changed")
    live = workers(settings, fresh=False)
    for worker in live:
        age = (current_time() - datetime.fromisoformat(worker["last_heartbeat"].replace("Z", "+00:00"))).total_seconds()
        if not 0 <= age < 60:
            raise ValueError("live Codex supervisor has a stale heartbeat")
    for entry in settings.get("adopted_workers", []):
        if not entry.get("unit"):
            current = next((worker for worker in live if adopted(settings, worker) == entry), None)
            if current:
                check_adopted_routing(settings, entry, current["pid"])
            continue
        info = unit_info(entry["unit"])
        check_adopted_routing(settings, entry, info["pid"])
        dropin, marker = retirement_paths(settings, entry)
        if not dropin.exists() or dropin.read_text() != retirement_guard(settings, entry):
            raise ValueError("adopted unit lacks its sole-owner restart guard")
        if info["UnitFileState"] not in {"disabled", "masked", "masked-runtime"} or (
                info["LoadState"] != "masked" and info["Restart"] != "no"):
            raise ValueError("effective systemd restart or boot ownership conflicts")
        if info["LoadState"] != "masked" and info.get("EnvironmentFiles") != settings["adoption_env"] + " (ignore_errors=no)":
            raise ValueError("adopted generation policy is not the final environment file")
        if marker.exists() and marker.read_text() != "subgov-codex\n":
            raise ValueError("retirement marker ownership conflict")
        if info["pid"]:
            if not any(value["pid"] == info["pid"] and adopted(settings, value) == entry for value in live):
                raise ValueError("unit has no fresh identity-verified heartbeat")
        elif info["ActiveState"] not in {"inactive", "failed"}:
            raise ValueError("adopted unit is transitioning")
    if settings.get("adopted_workers") and any(not owned(settings, value) for value in live):
        # Command observers cannot report an unowned floor. An unexpected
        # account worker must hold the pool until explicitly reviewed.
        raise ValueError("unreviewed Codex worker outside explicit ownership")
    return live


def install_adoption(settings):
    entries = [entry for entry in settings.get("adopted_workers", []) if entry.get("unit")]
    # Check all conflicts and identity before writing the first guard.
    live = workers(settings)
    for entry in entries:
        info = unit_info(entry["unit"])
        check_adopted_routing(settings, entry, info["pid"])
        dropin, marker = retirement_paths(settings, entry)
        if dropin.exists() and dropin.read_text() != retirement_guard(settings, entry):
            raise ValueError("conflicting unit ownership")
        if marker.exists():
            raise ValueError("review existing retirement before adoption installation")
        if info["pid"] and not any(value["pid"] == info["pid"] and adopted(settings, value) == entry for value in live):
            raise ValueError("adoption identity mismatch")
    for entry in entries:
        dropin, _ = retirement_paths(settings, entry)
        atomic_text(str(dropin), retirement_guard(settings, entry))
        # Disable boot ownership without stopping the live worker or its task.
        run(["systemctl", "--user", "disable", entry["unit"]])
    run(["systemctl", "--user", "daemon-reload"])


def demand(settings):
    owned_busy, unowned = [], 0
    for worker in inspect_fleet(settings) if settings.get("adopted_workers") else workers(settings):
        if not owned(settings, worker):
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
    return ready_demand(settings, owned_busy, unowned)


def refresh(settings):
    with locked(settings["snapshot"]):
        quota = document(run([settings["quota_binary"], "--config", settings["quota_config"],
                              "snapshot", settings.get("quota_account", "codex")], timeout=35))
        quota.pop("eligible_backlog_capacity", None)
        try:
            measured = demand(settings)
            quota["eligible_backlog_capacity"] = measured["eligible_backlog_capacity"]
        except Exception as exc:
            # Unknown demand must hold existing workers, never manufacture zero
            # or grant capacity. Do not forward CLI output or credentials.
            quota["fresh"] = False
            measured = {"observed_at": current_time().isoformat(), "unknown": type(exc).__name__}
        atomic_json(settings["demand_state"], measured)
        atomic_json(settings["snapshot"], quota)


def idle(worker):
    return (worker.get("state") in IDLE and worker.get("current_bead", "missing") is None
            and worker.get("current_task") is None
            and (worker.get("is_idle") is True or worker.get("state") == "ADMISSION_BLOCKED"))


def retention_order(worker):
    if not idle(worker):
        return (0, worker["worker_id"])
    try:
        tasks = list(Path("/proc", str(worker["pid"]), "task").iterdir())
        child_bearing = not tasks or any(live_children(task) for task in tasks)
    except (OSError, KeyError):
        child_bearing = True
    # Prefer keeping an idle supervisor with children over a child-free idle
    # slot. This is only selection: stop_idle still freezes and rechecks.
    return (1 if child_bearing else 2, worker["worker_id"])


def stop_idle(settings, worker):
    if not idle(worker):
        return False
    pid, name = worker["pid"], worker["worker_id"]
    agent = worker.get("model", settings["agent"])
    generation = identity(pid, agent, name)
    fd = os.pidfd_open(pid)
    frozen = False
    try:
        if identity(pid, agent, name) != generation:
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
        if any(live_children(task) for task in tasks):
            return False
        current = next((value for value in workers(settings) if value["pid"] == pid), None)
        if not current or not idle(current) or identity(pid, agent, name) != generation:
            return False
        entry = adopted(settings, worker)
        if entry and entry.get("unit"):
            if unit_info(entry["unit"])["pid"] != pid:
                return False
            # The permanent Restart=no guard and retirement marker already
            # prevent systemd from recreating this supervisor.
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
        observed = inspect_fleet(settings) if settings.get("adopted_workers") else workers(settings, fresh=False)
        live = [worker for worker in observed if owned(settings, worker)]
        pending = []
        # Preserve busy workers first. A retirement marker must never remain on
        # a slot selected to stay merely because an earlier stop was deferred.
        keep = {worker["worker_id"] for worker in sorted(live, key=retention_order)[:desired]}
        for worker in live:
            entry = adopted(settings, worker)
            if entry and entry.get("unit") and worker["worker_id"] in keep:
                _, marker = retirement_paths(settings, entry)
                marker.unlink(missing_ok=True)
        if len(live) > desired:
            for worker in sorted(live, key=lambda value: value["worker_id"]):
                if worker["worker_id"] in keep:
                    continue
                entry = adopted(settings, worker)
                if entry and entry.get("unit"):
                    _, marker = retirement_paths(settings, entry)
                    atomic_text(str(marker), "subgov-codex\n")
                if not stop_idle(settings, worker):
                    pending.append(worker["worker_id"])
        elif len(live) < desired:
            measured = demand(settings)  # Revalidate actual work immediately before launch.
            available = measured["ready"][:desired - len(live)]
            taken = {worker["worker_id"] for worker in live}
            candidates = []
            for entry in settings.get("adopted_workers", []):
                if entry["worker_id"] in taken:
                    continue
                info = unit_info(entry["unit"]) if entry.get("unit") else None
                if info and (info["LoadState"] == "masked" or info["UnitFileState"].startswith("masked")):
                    continue
                candidates.append(entry)
            for job in available:
                # Restore an adopted slot first, retaining its model and home
                # routing. New elastic slots use the configured Luna adapter.
                entry = candidates.pop(0) if candidates else None
                if entry and entry.get("unit"):
                    info = unit_info(entry["unit"])
                    if info["LoadState"] == "masked" or info["UnitFileState"].startswith("masked"):
                        continue
                    _, marker = retirement_paths(settings, entry)
                    marker.unlink(missing_ok=True)
                    run(["systemctl", "--user", "start", "--no-block", entry["unit"]])
                    taken.add(entry["worker_id"])
                    continue
                number = next(i for i in range(1, settings["max_workers"] + 1)
                              if settings["prefix"] + str(i) not in taken)
                name = entry["worker_id"] if entry else settings["prefix"] + str(number)
                # Reuse NEEDLE's inner tmux entry point, with non-secret,
                # per-worker environment overrides. This does not alter the
                # tmux server environment or other workers. Disable Weave's
                # generation gate so spare quota cannot generate make-work.
                binary = shutil.which("needle")
                if not binary:
                    raise ValueError("NEEDLE not installed")
                agent = entry["agent"] if entry else settings["agent"]
                route = entry["workspace"] if entry else job["workspace"]
                argv = [binary, "run", "--agent", agent, "--workspace", route,
                        "--identifier", name, "--count", "1"]
                session = "needle-" + agent.replace(".", "_") + "-" + name
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
    names += [entry["worker_id"] for entry in settings.get("adopted_workers", [])]
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
    sub.add_parser("observe")
    sub.add_parser("install-adoption")
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
    elif args.mode == "observe":
        print(json.dumps({"current_workers": sum(owned(settings, value) for value in inspect_fleet(settings))}))
    elif args.mode == "install-adoption":
        install_adoption(settings)
    else:
        install_lane(settings, args.config, remove=args.mode == "remove-lane")


if __name__ == "__main__":
    try:
        main()
    except Exception as exc:
        print("Codex productive adapter failed: " + type(exc).__name__, file=sys.stderr)
        sys.exit(1)
