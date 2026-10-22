#!/usr/bin/env python3
"""Publish Z.AI telemetry and an explicitly measured accepted-work frontier.

Run asynchronously; subgov reads the atomic snapshot with normalized_file.
"""
import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import subprocess
import sys

from subgov_adapter_common import atomic_json, locked, read_json
from subgov_ready_beads import policy_ok, observe as ready_demand


def backlog(settings):
    roster = read_json(settings["roster"])
    busy = set()
    for worker in roster["workers"]:
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
        if not policy_ok(workspace, settings["host"]):
            raise ValueError("busy route ownership unknown")
        claim = subprocess.run(["bead", "show", hb["current_bead"], "--json"],
                               cwd=workspace, capture_output=True, text=True,
                               timeout=8, check=True)
        issue = json.loads(claim.stdout)
        if isinstance(issue, list) and len(issue) == 1:
            issue = issue[0]
        if (issue.get("status") != "in_progress"
                or issue.get("assignee") != hb.get("qualified_id", worker["agent"] + "-" + worker["worker_id"])):
            raise ValueError("busy claim identity mismatch")
        busy.add((workspace, issue["id"]))
    measured = ready_demand(settings, [{'workspace': workspace, 'id': issue}
                                       for workspace, issue in sorted(busy)])
    return measured


def refresh(settings):
    with locked(settings["snapshot"]):
        result = subprocess.run([sys.executable, "-B", settings["collector"],
                                 *settings["collector_args"]], capture_output=True,
                                text=True, timeout=125, check=True)
        snapshot = json.loads(result.stdout)
        # Missing/failed demand evidence holds existing capacity; it never
        # manufactures zero work or publishes a guessed backlog.
        snapshot.pop("eligible_backlog_capacity", None)
        try:
            demand = backlog(settings)
            snapshot["eligible_backlog_capacity"] = demand["eligible_backlog_capacity"]
            if settings.get("demand_state"):
                atomic_json(settings["demand_state"], demand)
        except Exception as error:
            snapshot["fresh"] = False
            demand = {"observed_at": datetime.now(timezone.utc).isoformat(),
                      "backlog_unavailable": type(error).__name__}
            if settings.get("demand_state"):
                atomic_json(settings["demand_state"], demand)
        atomic_json(settings["snapshot"], snapshot)
        print(json.dumps({"event": "zai_source_refresh", "observed_at": snapshot["observed_at"],
                          "fresh": snapshot.get("fresh"), "saturation": snapshot.get("saturation"),
                          **{key: value for key, value in demand.items()
                             if key in ("eligible_backlog_capacity", "ready_bead_count", "backlog_unavailable")}}))


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
