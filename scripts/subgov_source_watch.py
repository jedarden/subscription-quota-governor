#!/usr/bin/env python3
"""Watch the subgov source publishers; page when one fails or goes stale.

Detection only: each violation prints one line naming the cause and a
non-empty run exits 1, so the unit's OnFailure= hook pages with those lines
as the journal excerpt. This is the alert the 2026-10-23..26 outage lacked
(subgov-28a476df): subgov-zai-source and subgov-codex-source failed every
cycle for days while nothing paged and the publishers logged bare exception
classes.

A source is unhealthy when its systemd unit is failed, or its settings or
snapshot are missing/unreadable, or the snapshot was last written more than
--max-age-seconds ago (three 300s publish cycles by default). A unit the
user manager has never loaded (LoadState=not-found) is skipped, so hosts
deploying a subset of the publishers stay quiet about the rest.
"""
import argparse
import json
from pathlib import Path
import subprocess
import sys
import time

from subgov_adapter_common import describe_error


def unit_state(unit):
    result = subprocess.run(["systemctl", "--user", "show", unit,
                             "--property=LoadState,ActiveState,Result,ExecMainStatus",
                             "--no-pager"], capture_output=True, text=True, timeout=10)
    if result.returncode:
        raise RuntimeError(f"systemctl show {unit} failed")
    return dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)


def violations(unit, settings_path, max_age_seconds, now=None):
    """Return the violation lines for one source; an empty list means healthy."""
    lines = []
    state = unit_state(unit)
    if state.get("LoadState") == "not-found":
        return []
    if state.get("ActiveState") == "failed":
        lines.append(f"{unit} is failed (Result={state.get('Result')}, "
                     f"ExecMainStatus={state.get('ExecMainStatus')}); "
                     f"journalctl --user -u {unit} names the cause")
    try:
        settings = json.loads(Path(settings_path).read_text())
        snapshot = settings["snapshot"]
    except FileNotFoundError:
        lines.append(f"{unit}: settings file missing: {settings_path}")
        return lines
    except (json.JSONDecodeError, KeyError, TypeError) as error:
        lines.append(f"{unit}: settings unusable at {settings_path}: {describe_error(error)}")
        return lines
    path = Path(snapshot)
    if not path.is_file():
        lines.append(f"{unit}: snapshot file missing: {snapshot}")
        return lines
    age = (now if now is not None else time.time()) - path.stat().st_mtime
    if age > max_age_seconds:
        lines.append(f"{unit}: snapshot is stale: {snapshot} last written "
                     f"{int(age // 60)} minutes ago, beyond the {max_age_seconds // 60} "
                     f"minute (three cycle) bound")
    return lines


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--max-age-seconds", type=int, default=900)
    parser.add_argument("--source", action="append", required=True, metavar="UNIT:SETTINGS",
                        help="source unit name and its settings JSON path")
    args = parser.parse_args(argv)
    found = []
    for spec in args.source:
        unit, separator, settings = spec.partition(":")
        if not separator or not unit or not settings:
            print(f"subgov-source-watch: invalid --source {spec!r} (want UNIT:SETTINGS)",
                  file=sys.stderr)
            return 2
        try:
            found.extend(violations(unit, settings, args.max_age_seconds))
        except Exception as error:
            found.append(f"{unit}: watchdog could not inspect source: {describe_error(error)}")
    for line in found:
        print(line, file=sys.stderr)
    if not found:
        print("subgov-source-watch: all watched sources fresh and units healthy")
        return 0
    return 1


if __name__ == "__main__":
    sys.exit(main())
