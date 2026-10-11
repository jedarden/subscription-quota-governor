#!/usr/bin/env bash
# Security-advisory audit for Cargo.lock (plan.md §16 WP7). Required by the
# subscription-quota-governor-ci Argo WorkflowTemplate; also run locally after
# dependency changes and before releases.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

cargo audit
