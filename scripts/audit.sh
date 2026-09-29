#!/usr/bin/env bash
# Security-advisory audit for Cargo.lock (plan.md §16 WP7). Wired into the
# Argo CI workflow by the pipeline work in the same work package; run it by
# hand until then.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

cargo audit
