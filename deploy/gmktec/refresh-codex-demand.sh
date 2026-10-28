#!/usr/bin/env bash
set -euo pipefail

state_dir=/home/coding/.needle/subgov
stage=$(mktemp "$state_dir/.codex-ready-demand.XXXXXX")
trap 'rm -f -- "$stage"' EXIT
python3 -B "$state_dir/adapters/subgov_ready_beads.py" \
  --settings "$state_dir/codex-ready-demand.json" > "$stage"
chmod 0600 "$stage"
mv -- "$stage" "$state_dir/codex-ready-demand.json"
