#!/usr/bin/env bash
# Install and verify the Python adapters from the repository's committed HEAD.
#
# The three adapter directories intentionally have different manifests because
# each provider's launch paths import a different subset of the shared scripts.
# Keep this manifest as the single source of truth for deployment.
set -euo pipefail

usage() {
    cat >&2 <<'USAGE'
Usage:
  install-adapter-scripts.sh [--root DIRECTORY] [--verify]
  install-adapter-scripts.sh [--root DIRECTORY] [install|verify]

Without --verify (or the verify subcommand), install the committed adapter
scripts into DIRECTORY/subgov-{codex,zai,anthropic}. The default DIRECTORY is
$HOME/.local/lib. The source files are read from git HEAD, never from a dirty
working tree. --verify only compares deployed bytes and never writes files.
--source-root is only for a git-archive export that has no .git directory;
normal deployments should omit it so HEAD is enforced.
USAGE
    exit 2
}

fail() {
    printf 'adapter scripts: %s\n' "$*" >&2
    exit 1
}

mode=install
destination_root=${SUBGOV_ADAPTER_ROOT:-${HOME:?HOME is not set}/.local/lib}
source_root=

while (($# > 0)); do
    case $1 in
        --root|--destination-root)
            (($# >= 2)) || usage
            destination_root=$2
            shift 2
            ;;
        --source-root)
            (($# >= 2)) || usage
            source_root=$2
            shift 2
            ;;
        --verify|verify)
            mode=verify
            shift
            ;;
        --install|install)
            mode=install
            shift
            ;;
        -h|--help)
            usage
            ;;
        *)
            usage
            ;;
    esac
done

[[ $destination_root == /* ]] || fail "destination root must be an absolute path: $destination_root"
if [[ -n $source_root ]]; then
    [[ $source_root == /* ]] || fail "source root must be an absolute path: $source_root"
    source_root=$(cd -- "$source_root" && pwd -P) || fail "source root is not accessible: $source_root"
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
if [[ -n $source_root ]]; then
    head_commit='explicit source root (git-archive export)'
else
    repo_root=$(git -C "$script_dir/.." rev-parse --show-toplevel 2>/dev/null) || \
        fail "run from a git checkout so deployed files can be read from HEAD"
    head_commit=$(git -C "$repo_root" rev-parse --verify HEAD^{commit}) || \
        fail "could not resolve repository HEAD"
fi

adapter_dirs=(subgov-codex subgov-zai subgov-anthropic)
declare -A adapter_files=(
    [subgov-codex]='codex-productive.py subgov_adapter_common.py subgov_ready_beads.py zai-source-refresh.py'
    [subgov-zai]='subgov_adapter_common.py subgov_ready_beads.py zai-collector.py zai-fleet.py zai-source-refresh.py'
    [subgov-anthropic]='anthropic-productive.py codex-productive.py subgov_adapter_common.py subgov_ready_beads.py zai-source-refresh.py'
)

stage=$(mktemp -d "${TMPDIR:-/tmp}/subgov-adapter-scripts.XXXXXX")
cleanup() {
    rm -rf -- "$stage"
}
trap cleanup EXIT

sources=()
for adapter_dir in "${adapter_dirs[@]}"; do
    for file in ${adapter_files[$adapter_dir]}; do
        sources+=("scripts/$file")
    done
done

# A single archive gives every destination the same immutable source snapshot,
# even if another process edits the checkout while this command is running.
# An explicit source root exists only so the definition-of-done can exercise a
# committed git-archive export, which necessarily has no .git directory.
if [[ -n $source_root ]]; then
    for source in "${sources[@]}"; do
        [[ -f $source_root/$source ]] || fail "source root is missing $source"
        install -D -m "$(stat -c '%a' -- "$source_root/$source")" -- \
            "$source_root/$source" "$stage/$source"
    done
else
    git -C "$repo_root" archive --format=tar HEAD -- "${sources[@]}" |
        tar -xf - -C "$stage" || fail "could not extract adapter scripts from HEAD"
fi

check_one() {
    local adapter_dir=$1
    local file=$2
    local source="$stage/scripts/$file"
    local destination="$destination_root/$adapter_dir/$file"

    [[ -f $source ]] || fail "HEAD is missing scripts/$file"
    if [[ ! -f $destination ]]; then
        printf 'FAIL missing %s\n' "$destination" >&2
        return 1
    fi
    if ! cmp -s -- "$source" "$destination"; then
        printf 'FAIL differs from HEAD: %s\n' "$destination" >&2
        return 1
    fi
    printf 'PASS %s\n' "$destination"
}

install_one() {
    local adapter_dir=$1
    local file=$2
    local source="$stage/scripts/$file"
    local destination_dir="$destination_root/$adapter_dir"
    local destination="$destination_dir/$file"
    local temporary
    local mode_bits

    [[ -f $source ]] || fail "HEAD is missing scripts/$file"
    mkdir -p -- "$destination_dir"
    mode_bits=$(stat -c '%a' -- "$source") || fail "could not read mode for scripts/$file"
    temporary=$(mktemp "$destination_dir/.${file}.$$XXXXXX") || \
        fail "could not create temporary file for $destination"
    if ! install -m "$mode_bits" -- "$source" "$temporary"; then
        rm -f -- "$temporary"
        fail "could not stage $destination"
    fi
    mv -f -- "$temporary" "$destination"
    printf 'INSTALLED %s\n' "$destination"
}

printf 'Using %s\n' "$head_commit"
failures=0
for adapter_dir in "${adapter_dirs[@]}"; do
    for file in ${adapter_files[$adapter_dir]}; do
        if [[ $mode == install ]]; then
            install_one "$adapter_dir" "$file"
        else
            check_one "$adapter_dir" "$file" || failures=$((failures + 1))
        fi
    done
done

if [[ $mode == install ]]; then
    printf 'Verifying deployed adapter scripts...\n'
    for adapter_dir in "${adapter_dirs[@]}"; do
        for file in ${adapter_files[$adapter_dir]}; do
            check_one "$adapter_dir" "$file" || failures=$((failures + 1))
        done
    done
fi

if ((failures > 0)); then
    fail "$failures deployed adapter script(s) are missing or differ from HEAD"
fi

if [[ $mode == verify ]]; then
    printf 'All deployed adapter scripts match HEAD.\n'
else
    printf 'All adapter scripts installed and verified.\n'
fi
