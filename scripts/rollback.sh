#!/usr/bin/env bash
# Put a systemd-managed subgov service into observe-only, optionally handing
# control back to a previous systemd service after the observe-only restart.
set -euo pipefail

usage() {
    cat <<'USAGE'
Usage:
  rollback.sh [--user] observe-only [SUBGOV_UNIT]
  rollback.sh [--user] previous PREVIOUS_CONTROLLER_UNIT [SUBGOV_UNIT]

The default subgov unit is subgov.service. SUBGOV_BIN and SUBGOV_CONFIG may
override /usr/local/bin/subgov and /etc/subgov/governor.yaml. This script
manages system units under /etc/systemd/system by default. With --user, it
uses the current user's manager and ~/.config/systemd/user by default.
USAGE
}

fail() {
    printf 'rollback: %s\n' "$*" >&2
    exit 1
}

valid_service_unit() {
    [[ "$1" =~ ^[-A-Za-z0-9_.@:]+\.service$ ]]
}

valid_trigger_unit() {
    [[ "$1" =~ ^[-A-Za-z0-9_.@:]+\.[-A-Za-z0-9_.@]+$ ]]
}

valid_systemd_word() {
    [[ "$1" =~ ^/[A-Za-z0-9_.+/-]+$ ]]
}

if (($# == 0)); then
    usage >&2
    exit 2
fi

systemd_scope=system
if [[ ${1-} == --user ]]; then
    systemd_scope=user
    shift
fi

if (($# == 0)); then
    usage >&2
    exit 2
fi

mode=$1
shift
default_subgov_unit=${SUBGOV_UNIT:-subgov.service}
previous_unit=
case "$mode" in
    observe-only)
        (($# <= 1)) || {
            usage >&2
            exit 2
        }
        subgov_unit=${1:-$default_subgov_unit}
        ;;
    previous)
        (($# >= 1 && $# <= 2)) || {
            usage >&2
            exit 2
        }
        previous_unit=$1
        subgov_unit=${2:-$default_subgov_unit}
        ;;
    -h|--help)
        usage
        exit 0
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac

valid_service_unit "$subgov_unit" || fail "invalid subgov systemd unit: $subgov_unit"
if [[ $mode == previous ]]; then
    valid_service_unit "$previous_unit" || fail "invalid previous-controller unit: $previous_unit"
    [[ $previous_unit != "$subgov_unit" ]] || fail "the previous controller must use a different unit"
fi

systemctl_bin=${SYSTEMCTL_BIN:-systemctl}
if [[ $systemd_scope == user ]]; then
    systemd_unit_dir=${SYSTEMD_UNIT_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user}
else
    systemd_unit_dir=${SYSTEMD_UNIT_DIR:-/etc/systemd/system}
fi
subgov_bin=${SUBGOV_BIN:-/usr/local/bin/subgov}
subgov_config=${SUBGOV_CONFIG:-/etc/subgov/governor.yaml}

command -v "$systemctl_bin" >/dev/null 2>&1 || fail "systemctl was not found: $systemctl_bin"
valid_systemd_word "$systemd_unit_dir" || fail "SYSTEMD_UNIT_DIR must be an absolute path without whitespace or systemd metacharacters"
valid_systemd_word "$subgov_bin" || fail "SUBGOV_BIN must be an absolute path without whitespace or systemd metacharacters"
valid_systemd_word "$subgov_config" || fail "SUBGOV_CONFIG must be an absolute path without whitespace or systemd metacharacters"
[[ -x $subgov_bin ]] || fail "subgov binary is missing or not executable: $subgov_bin"
[[ -f $subgov_config ]] || fail "configuration file is missing: $subgov_config"

systemctl() {
    if [[ $systemd_scope == user ]]; then
        "$systemctl_bin" --user "$@"
    else
        "$systemctl_bin" "$@"
    fi
}

# Check both units before changing the live service. Suppress unit contents,
# which may include deployment-specific environment values.
systemctl cat "$subgov_unit" >/dev/null 2>&1 || fail "subgov unit is not installed: $subgov_unit"
previous_triggers=()
if [[ $mode == previous ]]; then
    systemctl cat "$previous_unit" >/dev/null 2>&1 || fail "previous-controller unit is not installed: $previous_unit"
    triggered_by=$(systemctl show --property=TriggeredBy --value "$previous_unit") || fail "could not inspect activation units for $previous_unit"
    read -r -a previous_triggers <<<"$triggered_by"
    for trigger in "${previous_triggers[@]}"; do
        valid_trigger_unit "$trigger" || fail "unexpected activation unit for $previous_unit: $trigger"
        systemctl disable --now "$trigger" || fail "could not stop activation unit $trigger before rollback; $subgov_unit was not changed"
    done
    # Clear a previously active old controller before changing subgov. If a
    # later step fails, this leaves a safe no-actuation gap instead of overlap.
    systemctl stop "$previous_unit" || fail "could not stop $previous_unit before the rollback; $subgov_unit was not changed"
fi

dropin_dir="$systemd_unit_dir/$subgov_unit.d"
dropin="$dropin_dir/zzzz-subgov-rollback.conf"
[[ ! -L $dropin ]] || fail "refusing to replace a symlink at $dropin"
install -d -m 0755 -- "$dropin_dir"
temporary_dropin=$(mktemp "$dropin_dir/.rollback.XXXXXX")
trap 'rm -f -- "$temporary_dropin"' EXIT
cat >"$temporary_dropin" <<EOF
[Service]
ExecStart=
ExecStart=$subgov_bin --config $subgov_config run --observe-only
EOF
chmod 0644 "$temporary_dropin"
mv -f -- "$temporary_dropin" "$dropin"
trap - EXIT

systemctl daemon-reload
systemctl restart "$subgov_unit" || fail "could not restart $subgov_unit with observe-only enabled"
systemctl is-active --quiet "$subgov_unit" || fail "$subgov_unit is not active after the observe-only restart"
resolved_exec_start=$(systemctl show --property=ExecStart --value "$subgov_unit") || fail "could not verify the effective ExecStart for $subgov_unit"
[[ $resolved_exec_start =~ (^|[[:space:]])--observe-only([[:space:]]|$) ]] || fail "$subgov_unit's effective ExecStart does not contain --observe-only; no previous controller was started"

if [[ $mode == previous ]]; then
    # Enable only after subgov's effective command has been verified as
    # observe-only. If this fails, subgov remains safe and non-actuating.
    systemctl enable --now "$previous_unit" || fail "$previous_unit could not be enabled and started; $subgov_unit remains observe-only"
    systemctl is-active --quiet "$previous_unit" || fail "$previous_unit is not active; $subgov_unit remains observe-only"
    for trigger in "${previous_triggers[@]}"; do
        systemctl enable --now "$trigger" || fail "$previous_unit is active but its activation unit $trigger could not be restored; $subgov_unit remains observe-only"
    done
    printf 'Rollback complete: %s is observe-only; %s is active.\n' "$subgov_unit" "$previous_unit"
else
    printf 'Rollback complete: %s is active with --observe-only.\n' "$subgov_unit"
fi
