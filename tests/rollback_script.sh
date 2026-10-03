#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
test_dir=$(mktemp -d "$repo_root/.rollback-script-test.XXXXXX")
trap 'rm -rf -- "$test_dir"' EXIT

mkdir -p "$test_dir/bin" "$test_dir/systemd"
cat >"$test_dir/bin/systemctl" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$SYSTEMCTL_LOG"
if [[ " $* " == *' --property=TriggeredBy '* ]]; then
    printf '%s\n' "${MOCK_TRIGGERED_BY:-}"
elif [[ " $* " == *' --property=ExecStart '* ]]; then
    printf '%s\n' "${MOCK_EXEC_START:-argv[]=/usr/local/bin/subgov --config /etc/subgov/governor.yaml run --observe-only}"
fi
if [[ ${SYSTEMCTL_FAIL_ON:-} == "${1-} ${2-}" ]]; then
    exit 1
fi
MOCK
chmod +x "$test_dir/bin/systemctl"

subgov_bin="$test_dir/bin/subgov"
subgov_config="$test_dir/governor.yaml"
touch "$subgov_bin" "$subgov_config"
chmod +x "$subgov_bin"
export SYSTEMCTL_LOG="$test_dir/systemctl.log"
export SYSTEMCTL_BIN="$test_dir/bin/systemctl"
export SYSTEMD_UNIT_DIR="$test_dir/systemd"
export SUBGOV_BIN="$subgov_bin"
export SUBGOV_CONFIG="$subgov_config"
script="$repo_root/scripts/rollback.sh"

assert_logged() {
    grep -Fxq -- "$1" "$SYSTEMCTL_LOG" || {
        printf 'expected systemctl call missing: %s\n' "$1" >&2
        exit 1
    }
}

"$script" observe-only >/dev/null
dropin="$SYSTEMD_UNIT_DIR/subgov.service.d/zzzz-subgov-rollback.conf"
grep -Fxq '[Service]' "$dropin"
grep -Fxq 'ExecStart=' "$dropin"
grep -Fxq "ExecStart=$SUBGOV_BIN --config $SUBGOV_CONFIG run --observe-only" "$dropin"
assert_logged 'daemon-reload'
assert_logged 'restart subgov.service'
assert_logged 'is-active --quiet subgov.service'

: >"$SYSTEMCTL_LOG"
MOCK_TRIGGERED_BY=cgov-watch.timer "$script" previous cgov.service >/dev/null
assert_logged 'restart subgov.service'
assert_logged 'show --property=ExecStart --value subgov.service'
assert_logged 'enable --now cgov.service'
assert_logged 'disable --now cgov-watch.timer'
assert_logged 'enable --now cgov-watch.timer'
stop_trigger_line=$(grep -nFx 'disable --now cgov-watch.timer' "$SYSTEMCTL_LOG" | cut -d: -f1)
stop_line=$(grep -nFx 'stop cgov.service' "$SYSTEMCTL_LOG" | cut -d: -f1)
restart_line=$(grep -nFx 'restart subgov.service' "$SYSTEMCTL_LOG" | cut -d: -f1)
enable_line=$(grep -nFx 'enable --now cgov.service' "$SYSTEMCTL_LOG" | cut -d: -f1)
enable_trigger_line=$(grep -nFx 'enable --now cgov-watch.timer' "$SYSTEMCTL_LOG" | cut -d: -f1)
((stop_trigger_line < stop_line))
((stop_line < restart_line))
((restart_line < enable_line))
((enable_line < enable_trigger_line))

: >"$SYSTEMCTL_LOG"
if SYSTEMCTL_FAIL_ON='restart subgov.service' "$script" previous cgov.service >/dev/null 2>&1; then
    printf 'rollback unexpectedly continued after the observe-only restart failed\n' >&2
    exit 1
fi
if grep -Fxq 'enable --now cgov.service' "$SYSTEMCTL_LOG"; then
    printf 'previous controller started before observe-only was established\n' >&2
    exit 1
fi

: >"$SYSTEMCTL_LOG"
if MOCK_EXEC_START='argv[]=/usr/local/bin/subgov run' "$script" previous cgov.service >/dev/null 2>&1; then
    printf 'rollback unexpectedly accepted an ExecStart without --observe-only\n' >&2
    exit 1
fi
if grep -Fxq 'enable --now cgov.service' "$SYSTEMCTL_LOG"; then
    printf 'previous controller started when effective observe-only was not verified\n' >&2
    exit 1
fi

 : >"$SYSTEMCTL_LOG"
"$script" --user observe-only >/dev/null
assert_logged '--user daemon-reload'
assert_logged '--user restart subgov.service'

printf 'rollback script checks passed\n'
