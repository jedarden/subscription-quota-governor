#!/usr/bin/env bash
set -euo pipefail

state_dir=/home/coding/.needle/subgov
stage=$(mktemp "$state_dir/.codex-ready-demand.XXXXXX")
trap 'rm -f -- "$stage"' EXIT
python3 -B "$state_dir/adapters/subgov_ready_beads.py" \
  --settings "$state_dir/codex-demand-settings.json" > "$stage"
chmod 0600 "$stage"
mv -- "$stage" "$state_dir/codex-ready-demand.json"
# Measure after publishing demand so the ceiling evidence weights a fresh
# ready count. Observe-only: this persists evidence, never a reservation.
python3 -B "$state_dir/measure-host-capacity.py" \
  --adapters-dir "$state_dir/adapters" \
  --config "$state_dir/host-capacity.json" \
  --pool codex 1>&2
