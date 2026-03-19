#!/usr/bin/env bash

set -eufxo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
ANTLR4_DIR="$ROOT_DIR/antlr4"

usage() {
    cat <<EOF
Usage: $(basename "$0")

Initializes the checked-in antlr4 submodule tree and builds the local Rust-target ANTLR jar:
  $ANTLR4_DIR/tool/target/antlr4-4.8-2-SNAPSHOT-complete.jar
EOF
}

if [[ ${1:-} == "-h" || ${1:-} == "--help" ]]; then
    usage
    exit 0
fi

if [[ $# -ne 0 ]]; then
    usage >&2
    exit 1
fi

require_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "missing required command: $1" >&2
        exit 1
    fi
}

require_cmd git
require_cmd java
require_cmd mvn

echo "[1/2] syncing ANTLR submodules"
git -C "$ROOT_DIR" submodule sync --recursive
git -C "$ROOT_DIR" submodule update --init --recursive

if [[ ! -d "$ANTLR4_DIR/tool" || ! -d "$ANTLR4_DIR/runtime/Rust" ]]; then
    echo "missing required antlr4 submodule contents under $ANTLR4_DIR" >&2
    exit 1
fi

echo "[2/2] building ANTLR tool jar"
(
    cd "$ANTLR4_DIR"
    mvn -pl tool -am -DskipTests package
)

echo
echo "ANTLR bootstrap complete."
echo "ANTLR dir: $ANTLR4_DIR"
echo "Tool jar: $ANTLR4_DIR/tool/target/antlr4-4.8-2-SNAPSHOT-complete.jar"
echo
echo "You can now run:"
echo "  ./scripts/bootstrap.sh --release"
echo "or:"
echo "  cargo build --workspace --release"
