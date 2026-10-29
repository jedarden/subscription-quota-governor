#!/usr/bin/env python3
"""Measure gmktec's WP24 host worker ceiling without admitting anything.

Runs one observe-only admission against the host-capacity config: sample
resources, persist the evidence atomically, emit the ceiling event on
stderr. Missing or unreadable evidence publishes ceiling=null and holds
the observed fleet -- it never authorizes a launch or reserves a slot.
The demand publisher runs this after each fresh demand publication so
peer-state carries a measured capacity bound (WP32 actuation gate).
"""
import argparse
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--adapters-dir", required=True,
                        help="directory holding subgov_host_capacity.py")
    parser.add_argument("--config", required=True,
                        help="host-capacity.json config path")
    parser.add_argument("--pool", required=True,
                        help="host pool key inside the config")
    args = parser.parse_args()
    sys.path.insert(0, args.adapters_dir)
    from subgov_host_capacity import admission

    # One admission with no quota-side request: it samples, persists and
    # logs, and the granted target is deliberately unused here.
    with admission({"host_capacity": args.config, "host_pool": args.pool},
                   current=0):
        pass


if __name__ == "__main__":
    main()
