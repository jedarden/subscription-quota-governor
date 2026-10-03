#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 <vMAJOR.MINOR.PATCH[-prerelease]> <output-directory> [--publish]" >&2
  exit 2
}

[[ $# -ge 2 && $# -le 3 ]] || usage
TAG=$1
OUTPUT_DIR=$2
PUBLISH=${3:-}
[[ -z "$PUBLISH" || "$PUBLISH" == "--publish" ]] || usage

if [[ ! "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]]; then
  echo "release tag must be a v-prefixed semantic version: $TAG" >&2
  exit 2
fi

case "$(uname -s)/$(uname -m)" in
  Linux/x86_64) ;;
  *) echo "release build requires Linux x86_64" >&2; exit 2 ;;
esac

REPO_ROOT=$(git rev-parse --show-toplevel)
cd "$REPO_ROOT"
TAG_COMMIT=$(git rev-parse --verify "refs/tags/${TAG}^{commit}")
HEAD_COMMIT=$(git rev-parse HEAD)
if [[ "$TAG_COMMIT" != "$HEAD_COMMIT" ]]; then
  echo "HEAD ($HEAD_COMMIT) does not match release tag $TAG ($TAG_COMMIT)" >&2
  exit 1
fi
if [[ -n "$(git status --porcelain)" ]]; then
  echo "release source tree is not clean" >&2
  exit 1
fi

VERSION=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["package"]["version"])')
TAG_VERSION=${TAG#v}
if [[ "$TAG_VERSION" != "$VERSION" ]]; then
  echo "tag version $TAG_VERSION does not match Cargo.toml version $VERSION" >&2
  exit 1
fi

OUTPUT_DIR=$(realpath -m "$OUTPUT_DIR")
case "$OUTPUT_DIR/" in
  "$REPO_ROOT/"*) echo "output directory must be outside the source checkout" >&2; exit 2 ;;
esac
mkdir -p "$OUTPUT_DIR"
if [[ -n "$(find "$OUTPUT_DIR" -mindepth 1 -maxdepth 1 -print -quit)" ]]; then
  echo "output directory must be empty: $OUTPUT_DIR" >&2
  exit 1
fi

TARGET=x86_64-unknown-linux-gnu
ARTIFACT="subgov-${TAG}-linux-x86_64"
STARTED_ON=$(date -u +%Y-%m-%dT%H:%M:%SZ)
SOURCE_DATE_EPOCH=$(git show -s --format=%ct "$TAG_COMMIT")
RUSTC_VERSION=$(rustc --version)
export SOURCE_DATE_EPOCH
# Normalize source paths so the same pinned toolchain can reproduce the binary
# from a different checkout path. Do not inherit arbitrary caller rustc flags.
export CARGO_ENCODED_RUSTFLAGS="--remap-path-prefix=${REPO_ROOT}=/workspace"

echo "Building $ARTIFACT from $TAG_COMMIT with $RUSTC_VERSION"
TARGET_DIR=$(cargo metadata --no-deps --format-version 1 | \
  python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
cargo build --locked --release --target "$TARGET" --bin subgov
cp "${TARGET_DIR}/${TARGET}/release/subgov" "$OUTPUT_DIR/$ARTIFACT"
chmod 0755 "$OUTPUT_DIR/$ARTIFACT"

LOCK_SHA256=$(sha256sum Cargo.lock | cut -d' ' -f1)
SUBGOV_BUILD_TAG="$TAG" \
SUBGOV_BUILD_VERSION="$VERSION" \
SUBGOV_BUILD_COMMIT="$TAG_COMMIT" \
SUBGOV_BUILD_TARGET="$TARGET" \
SUBGOV_BUILD_ARTIFACT="$ARTIFACT" \
SUBGOV_BUILD_LOCK_SHA256="$LOCK_SHA256" \
SUBGOV_BUILD_STARTED_ON="$STARTED_ON" \
SUBGOV_BUILD_RUSTC_VERSION="$RUSTC_VERSION" \
SUBGOV_BUILD_WORKFLOW_UID="${SUBGOV_BUILD_WORKFLOW_UID:-local-${TAG_COMMIT}}" \
SUBGOV_BUILDER_ID="${SUBGOV_BUILDER_ID:-local://scripts/package-linux-release.sh}" \
SUBGOV_BINARY_PATH="$OUTPUT_DIR/$ARTIFACT" \
SUBGOV_PROVENANCE_PATH="$OUTPUT_DIR/$ARTIFACT.intoto.jsonl" \
python3 - <<'PY'
import hashlib
import json
import os
from datetime import datetime, timezone


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


finished_on = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
source = "https://git.ardenone.com/jedarden/subscription-quota-governor.git"
commit = os.environ["SUBGOV_BUILD_COMMIT"]
artifact = os.environ["SUBGOV_BUILD_ARTIFACT"]
statement = {
    "_type": "https://in-toto.io/Statement/v1",
    "subject": [{"name": artifact, "digest": {"sha256": sha256(os.environ["SUBGOV_BINARY_PATH"])}}],
    "predicateType": "https://slsa.dev/provenance/v1",
    "predicate": {
        "buildDefinition": {
            "buildType": "https://argoproj.io/argo-workflows/v1",
            "externalParameters": {
                "repository": source,
                "ref": f"refs/tags/{os.environ['SUBGOV_BUILD_TAG']}",
                "version": os.environ["SUBGOV_BUILD_VERSION"],
                "target": os.environ["SUBGOV_BUILD_TARGET"],
            },
            "internalParameters": {
                "workflow": "subscription-quota-governor-ci",
                "command": "cargo build --locked --release --target x86_64-unknown-linux-gnu --bin subgov",
                "rustc": os.environ["SUBGOV_BUILD_RUSTC_VERSION"],
                "rustflags": "--remap-path-prefix=<checkout>=/workspace",
                "sourceDateEpoch": os.environ.get("SOURCE_DATE_EPOCH", ""),
            },
            "resolvedDependencies": [
                {"uri": f"git+{source}@{commit}", "digest": {"gitCommit": commit}},
                {"uri": "Cargo.lock", "digest": {"sha256": os.environ["SUBGOV_BUILD_LOCK_SHA256"]}},
            ],
        },
        "runDetails": {
            "builder": {"id": os.environ["SUBGOV_BUILDER_ID"]},
            "metadata": {
                "invocationId": os.environ["SUBGOV_BUILD_WORKFLOW_UID"],
                "startedOn": os.environ["SUBGOV_BUILD_STARTED_ON"],
                "finishedOn": finished_on,
            },
            "byproducts": [],
        },
    },
}
with open(os.environ["SUBGOV_PROVENANCE_PATH"], "w", encoding="utf-8") as output:
    output.write(json.dumps(statement, sort_keys=True, separators=(",", ":")) + "\n")
PY

(
  cd "$OUTPUT_DIR"
  sha256sum "$ARTIFACT" "$ARTIFACT.intoto.jsonl" > SHA256SUMS
  sha256sum --check SHA256SUMS
)

if [[ "$PUBLISH" == "--publish" ]]; then
  : "${FORGEJO_TOKEN:?FORGEJO_TOKEN is required when --publish is used}"
  PACKAGE_VERSION=${VERSION}
  PACKAGE_URL="https://git.ardenone.com/api/packages/jedarden/generic/subgov/${PACKAGE_VERSION}"
  NETRC=$(mktemp)
  DOWNLOAD=$(mktemp -d)
  chmod 0600 "$NETRC"
  printf 'machine git.ardenone.com\nlogin jedarden\npassword %s\n' "$FORGEJO_TOKEN" > "$NETRC"
  cleanup_publish() { rm -f "$NETRC"; rm -rf "$DOWNLOAD"; }
  trap cleanup_publish EXIT

  # Preflight every existing name before uploading any missing asset. A
  # published version is immutable: a name collision with different bytes
  # fails closed instead of replacing the release contents.
  for file in "$ARTIFACT" "$ARTIFACT.intoto.jsonl" SHA256SUMS; do
    code=$(curl --silent --show-error --netrc-file "$NETRC" \
      --output "$DOWNLOAD/$file" --write-out '%{http_code}' "$PACKAGE_URL/$file")
    case "$code" in
      200)
        if ! cmp -s "$OUTPUT_DIR/$file" "$DOWNLOAD/$file"; then
          echo "published asset differs from this build: $file" >&2
          exit 1
        fi
        ;;
      404) rm -f "$DOWNLOAD/$file" ;;
      *) echo "registry preflight failed for $file (HTTP $code)" >&2; exit 1 ;;
    esac
  done

  for file in "$ARTIFACT" "$ARTIFACT.intoto.jsonl" SHA256SUMS; do
    if [[ -f "$DOWNLOAD/$file" ]]; then
      echo "Already published with matching bytes: $file"
      continue
    fi
    curl --fail --silent --show-error --netrc-file "$NETRC" \
      --upload-file "$OUTPUT_DIR/$file" "$PACKAGE_URL/$file"
    echo "Published $file"
  done
fi

echo "Release artifacts are in $OUTPUT_DIR"
