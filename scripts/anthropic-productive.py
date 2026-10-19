#!/usr/bin/env python3
"""Anthropic configuration wrapper for the shared productive fleet adapter.

The copied Codex module is shared implementation, not a second controller.
This wrapper never reads Codex settings or quota and changes no other prefix.
"""
import argparse
import importlib.util
import json
from pathlib import Path
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--settings", required=True)
    parser.add_argument("mode", choices=["demand", "actuate", "install-lane", "remove-lane"])
    parser.add_argument("value", nargs="?")
    args = parser.parse_args()
    settings = json.loads(Path(args.settings).read_text())
    if (settings.get("version") != 1 or settings.get("host") != "codinghome"
            or settings.get("agent") != "claude-print"
            or settings.get("prefix") != "sg-claude-anthropic-"
            or settings.get("max_workers") != 2
            or settings.get("lane_label") != "subgov-anthropic-productive"
            or settings.get("adopted_workers")):
        raise ValueError("unexpected Anthropic ownership")
    keys = [(item["workspace"], item["id"]) for item in settings["work"]]
    if len(keys) != len(set(keys)):
        raise ValueError("duplicate reviewed outcome")
    spec = importlib.util.spec_from_file_location("productive", Path(__file__).with_name("codex-productive.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    if args.mode == "demand":
        print(json.dumps(module.demand(settings)))
    elif args.mode == "actuate":
        module.actuate(settings, int(args.value))
    else:
        module.install_lane(settings, args.value, remove=args.mode == "remove-lane")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print("Anthropic productive adapter failed: " + type(error).__name__, file=sys.stderr)
        sys.exit(1)
