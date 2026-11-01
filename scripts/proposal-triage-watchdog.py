#!/usr/bin/env python3
"""Detect a silently stopped proposal-triage timer or service.

This runs on its own OnCalendar timer so a broken primary timer cannot also
silence the check. It reports one bounded line to the journal and exits nonzero
for the existing user-unit failure alert hook.
"""
import subprocess
import sys
import time


TIMER_UNIT = "subgov-proposal-triage.timer"
SERVICE_UNIT = "subgov-proposal-triage.service"
STALE_AFTER_SECONDS = 25 * 60


def show(unit, prop):
    result = subprocess.run(
        ["systemctl", "--user", "show", unit, "--property", prop, "--value"],
        capture_output=True,
        text=True,
        timeout=15,
    )
    return result.stdout.strip()


def parse_timestamp(value):
    if not value or value in ("n/a", "0"):
        return None
    parts = value.rsplit(" ", 1)
    candidate = parts[0] if len(parts) == 2 else value
    for fmt in ("%a %Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M:%S"):
        try:
            return time.mktime(time.strptime(candidate, fmt))
        except ValueError:
            continue
    return None


def main():
    problems = []
    enabled = show(TIMER_UNIT, "UnitFileState")
    active = show(TIMER_UNIT, "ActiveState")
    next_run = show(TIMER_UNIT, "NextElapseUSecRealtime")
    last_start_raw = show(SERVICE_UNIT, "ExecMainStartTimestamp")

    if enabled != "enabled":
        problems.append(f"timer UnitFileState={enabled!r}")
    if active != "active":
        problems.append(f"timer ActiveState={active!r}")
    if not next_run or next_run == "infinity":
        problems.append("timer has no scheduled next run")

    last_start = parse_timestamp(last_start_raw)
    if last_start is not None and time.time() - last_start > STALE_AFTER_SECONDS:
        problems.append("triage service has not run within 25 minutes")

    if problems:
        print("proposal-triage-watchdog failed: " + "; ".join(problems), flush=True)
        return 1
    print("proposal-triage-watchdog ok", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
