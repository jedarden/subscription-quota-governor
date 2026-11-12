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

from subgov_adapter_common import atomic_json, describe_error, locked, read_json
from subgov_ready_beads import policy_ok, observe as ready_demand

# Same freshness window the fleet observer and seed occupancy apply: a
# heartbeat older than this is not evidence of a running worker.
HEARTBEAT_FRESH_SECONDS = 60


def _matches_worker_prefix(prefix, worker_id):
    """Mirror subgov's ownership rule (src/ownership.rs): a pool prefix owns
    `<prefix><digits>` exactly, so a peer host's `sg-zai-gmktec-3` heartbeat
    never counts as this pool's elastic worker."""
    suffix = worker_id[len(prefix):] if worker_id.startswith(prefix) else None
    return suffix is not None and suffix.isdigit()


def _live_process_is_worker(pid, worker):
    """True when the heartbeat's pid is that worker's own live process.

    Roster workers run under a systemd unit, so the unit's MainPID must be
    the heartbeat writer. Elastic workers have no unit: identity is the
    `--identifier <worker_id>` in the live process's own argv, so a dead or
    recycled pid can never pass as a busy worker.
    """
    if unit := worker.get("unit"):
        status = subprocess.run(["systemctl", "--user", "show", unit,
                                 "--property=MainPID", "--value"], capture_output=True,
                                text=True, timeout=2, check=True)
        if int(status.stdout.strip()) != pid:
            raise ValueError("busy heartbeat identity differs from live worker")
        return True
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as stream:
            argv = [part.decode("utf-8", "replace")
                    for part in stream.read().split(b"\0") if part]
    except (OSError, TypeError):
        # The worker's process is gone; whatever claim it held is stale.
        return False
    return any(flag == "--identifier" and value == worker["worker_id"]
               for flag, value in zip(argv, argv[1:]))


def _busy_workers(settings):
    """Every worker this pool may count: the §25 legacy roster plus every
    elastic heartbeat whose worker id matches the configured prefix."""
    workers = [{"worker_id": worker["worker_id"], "agent": worker["agent"],
                "unit": worker.get("unit")}
               for worker in read_json(settings["roster"])["workers"]]
    agent = settings.get("agent")
    prefix = settings.get("identifier_prefix")
    if agent and prefix:
        directory = Path(settings["heartbeat_dir"])
        for path in sorted(directory.glob(f"{agent}-*.json")):
            worker_id = path.stem[len(agent) + 1:]
            if _matches_worker_prefix(prefix, worker_id):
                workers.append({"worker_id": worker_id, "agent": agent, "unit": None})
    return workers


def backlog(settings):
    """Validated in-flight claims across the legacy roster and elastic pool.

    Both populations pass the same gates before their (workspace, bead)
    pair occupies a slot: qualified identity, a live process, a fresh
    heartbeat, and a native in_progress claim assigned to that worker.
    Without the elastic scan a live sg-zai-4 claim reported busy_count=0
    and its workspace looked free (subgov-a5c14e89). Stale or dead workers
    are simply not busy; an identity mismatch fails the whole observation
    so capacity is held instead of guessed.
    """
    busy = set()
    heartbeat_dir = Path(settings["heartbeat_dir"])
    for worker in _busy_workers(settings):
        qualified_id = worker["agent"] + "-" + worker["worker_id"]
        hb = read_json(heartbeat_dir / (qualified_id + ".json"), {})
        if not hb.get("current_bead"):
            continue
        if hb.get("qualified_id", qualified_id) != qualified_id:
            raise ValueError("busy heartbeat identity differs from worker")
        stamp = datetime.fromisoformat(str(hb["last_heartbeat"]).replace("Z", "+00:00"))
        if not 0 <= (datetime.now(timezone.utc) - stamp).total_seconds() < HEARTBEAT_FRESH_SECONDS:
            continue
        pid = hb.get("pid")
        if not isinstance(pid, int) or isinstance(pid, bool):
            raise ValueError("busy heartbeat has no live pid")
        if not _live_process_is_worker(pid, worker):
            continue
        workspace = str(Path(hb["workspace"]).resolve())
        if not policy_ok(workspace, settings["host"]):
            raise ValueError("busy route ownership unknown")
        claim = subprocess.run(["bead", "show", hb["current_bead"], "--json"],
                               cwd=workspace, capture_output=True, text=True,
                               timeout=8, check=True)
        issue = json.loads(claim.stdout)
        if isinstance(issue, list) and len(issue) == 1:
            issue = issue[0]
        if issue.get("status") != "in_progress" or issue.get("assignee") != qualified_id:
            raise ValueError("busy claim identity mismatch")
        # Two live workers can hold one claim (the cg-l0v0kc shape): the
        # pair still occupies exactly one slot, never two and never zero.
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
            # Name the cause in the journal; the published state only records
            # that demand is unknown, so existing capacity is held, never zero.
            print(f"zai-source-refresh: demand unavailable "
                  f"({type(error).__name__}: {describe_error(error)})", file=sys.stderr)
            snapshot["fresh"] = False
            demand = {"observed_at": datetime.now(timezone.utc).isoformat(),
                      "backlog_unavailable": type(error).__name__}
            if settings.get("demand_state"):
                atomic_json(settings["demand_state"], demand)
        atomic_json(settings["snapshot"], snapshot)
        print(json.dumps({"event": "zai_source_refresh", "observed_at": snapshot["observed_at"],
                          "fresh": snapshot.get("fresh"), "saturation": snapshot.get("saturation"),
                          **{key: value for key, value in demand.items()
                             if key in ("eligible_backlog_capacity", "ready_bead_count",
                                        "busy_count", "backlog_unavailable")}}))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--settings", required=True)
    args = parser.parse_args(argv)
    try:
        refresh(read_json(args.settings))
    except Exception as error:
        # Keep the prior snapshot unchanged and allow its freshness to expire.
        print(f"zai-source-refresh: refresh failed "
              f"({type(error).__name__}: {describe_error(error)})", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
