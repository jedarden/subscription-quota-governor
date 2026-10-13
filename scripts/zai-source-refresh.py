#!/usr/bin/env python3
"""Publish Z.AI telemetry and an explicitly measured accepted-work frontier.

Run asynchronously; subgov reads the atomic snapshot with normalized_file.
"""
import argparse
import concurrent.futures
from datetime import datetime, timezone
import json
from pathlib import Path
import re
import subprocess
import sys

from subgov_adapter_common import atomic_json, locked, read_json


def eligible(issue):
    if issue.get("status") != "open" or issue.get("assignee") or issue.get("manual_blocked"):
        return False
    exact = {"human", "human-owned", "owner:human", "proposal-pending", "deferred",
             "blocked", "manual_blocked", "manual-blocked", "blocked:manual"}
    return not any(label.lower() in exact or label.lower().startswith(
        ("human:", "deferred:", "quarantine-until:", "retry-cooldown-until:")) for label in issue.get("labels", []))


def workspace_policy(path):
    """Read only simple backend/queue-owner scalars, refusing ambiguous YAML.

    This intentionally fails closed on flow maps, anchors, duplicate policy
    keys and complex policy scalars rather than guessing ownership.
    """
    config = {"bead_cli": {}, "queue": {}}
    section, child_indent, seen = None, None, set()
    for raw in path.read_text().splitlines():
        if not raw.strip() or raw.lstrip().startswith("#"):
            continue
        if "\t" in raw[:len(raw) - len(raw.lstrip())]:
            raise ValueError("tab-indented policy")
        indent = len(raw) - len(raw.lstrip(" "))
        if indent == 0:
            section, child_indent = None, None
            for key in config:
                if raw.startswith(key + ":"):
                    if key in seen or not re.fullmatch(key + r":\s*(?:#.*)?", raw):
                        raise ValueError("complex or duplicate policy section")
                    seen.add(key)
                    section = key
            continue
        if section is None:
            continue
        child_indent = indent if child_indent is None else child_indent
        if indent != child_indent:
            continue
        key, separator, value = raw.strip().partition(":")
        if not separator or key not in {"backend", "owner_host"}:
            continue
        if key in config[section]:
            raise ValueError("duplicate policy key")
        value = value.strip()
        if value.startswith('"'):
            value = json.loads(value)
        elif value.startswith("'"):
            if not value.endswith("'"):
                raise ValueError("complex quoted policy scalar")
            value = value[1:-1].replace("''", "'")
        else:
            value = value.split("#", 1)[0].strip()
            if value and not re.fullmatch(r"[A-Za-z0-9_.-]+", value):
                raise ValueError("complex policy scalar")
        config[section][key] = value
    return config


def ready(workspace, host):
    path = Path(workspace)
    if not (path / ".beads/config.json").is_file():
        raise ValueError("route is not a bead-rs workspace")
    config = workspace_policy(path / ".needle.yaml")
    if config.get("bead_cli", {}).get("backend") != "bead-rs":
        raise ValueError("route backend is not bead-rs")
    queue = config.get("queue", {})
    if "owner_host" in queue and queue["owner_host"] != host:
        return set()
    result = subprocess.run(["bead", "list", "--ready", "--json", "--limit", "10000"],
                            cwd=path, text=True, capture_output=True, timeout=8, check=True)
    issues = [json.loads(line) for line in result.stdout.splitlines() if line.strip()]
    return {(str(path.resolve()), issue["id"]) for issue in issues if eligible(issue)}


def backlog(settings):
    roster = read_json(settings["roster"])
    routes, busy = set(), set()
    for worker in roster["workers"]:
        route = Path(settings["routing_dir"]) / (worker["worker_id"] + ".env")
        # Retain only the non-secret home route from this environment file.
        homes = [line.split("=", 1)[1].strip().strip("\"'") for line in route.read_text().splitlines()
                 if line.startswith("NEEDLE_WS=")]
        if len(homes) != 1:
            raise ValueError("worker has no unique home route")
        routes.add(homes[0])
        hb = read_json(Path(settings["heartbeat_dir"]) /
                       (worker["agent"] + "-" + worker["worker_id"] + ".json"), {})
        if not hb.get("current_bead"):
            continue
        stamp = datetime.fromisoformat(hb["last_heartbeat"].replace("Z", "+00:00"))
        if not 0 <= (datetime.now(timezone.utc) - stamp).total_seconds() < 60:
            continue
        status = subprocess.run(["systemctl", "--user", "show", worker["unit"],
                                 "--property=MainPID", "--value"], capture_output=True,
                                text=True, timeout=2, check=True)
        if int(status.stdout.strip()) != hb.get("pid"):
            raise ValueError("busy heartbeat identity differs from live worker")
        workspace = str(Path(hb["workspace"]).resolve())
        policy = workspace_policy(Path(workspace) / ".needle.yaml")
        if not (Path(workspace) / ".beads/config.json").is_file() or policy["bead_cli"].get("backend") != "bead-rs":
            raise ValueError("busy route is not bead-rs")
        if "owner_host" in policy["queue"] and policy["queue"]["owner_host"] != settings["host"]:
            raise ValueError("busy route belongs to another host")
        routes.add(workspace)
        claim = subprocess.run(["bead", "show", hb["current_bead"], "--json"],
                               cwd=workspace, capture_output=True, text=True,
                               timeout=8, check=True)
        issue = json.loads(claim.stdout)[0]
        if issue.get("status") == "in_progress":
            busy.add((workspace, issue["id"]))
    jobs = set(busy)
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        for result in pool.map(lambda route: ready(route, settings["host"]), sorted(routes)):
            jobs.update(result)
    return len(jobs), len(routes), len(busy)


def refresh(settings):
    with locked(settings["snapshot"]):
        result = subprocess.run([sys.executable, "-B", settings["collector"],
                                 *settings["collector_args"]], capture_output=True,
                                text=True, timeout=125, check=True)
        snapshot = json.loads(result.stdout)
        # Missing/failed demand evidence holds existing capacity; it never
        # manufactures zero work or publishes a guessed backlog.
        try:
            capacity, routes, busy = backlog(settings)
            snapshot["eligible_backlog_capacity"] = capacity
            demand = {"eligible_backlog_capacity": capacity, "routes": routes, "busy_claims": busy}
        except Exception as error:
            demand = {"backlog_unavailable": type(error).__name__}
        atomic_json(settings["snapshot"], snapshot)
        print(json.dumps({"event": "zai_source_refresh", "observed_at": snapshot["observed_at"],
                          "fresh": snapshot.get("fresh"), "saturation": snapshot.get("saturation"), **demand}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--settings", required=True)
    args = parser.parse_args()
    try:
        refresh(read_json(args.settings))
    except Exception as error:
        # Keep the prior snapshot unchanged and allow its freshness to expire.
        print(f"zai-source-refresh: refresh failed ({type(error).__name__})", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
