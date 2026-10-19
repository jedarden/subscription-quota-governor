#!/usr/bin/env bash
# Maintained launcher for the read-only Subgov tab in codinghome's metrics space.
set -euo pipefail

exec "${HOME}/.local/lib/subgov/dashboard-current/subgov" dashboard \
  --configs "${HOME}/.config/subgov/claude-anthropic.yaml" \
            "${HOME}/.config/subgov/codex.yaml" \
            "${HOME}/.config/subgov/claude-zai-managed.yaml" \
  "$@"
