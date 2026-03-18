#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
TOOLCHAIN_DIR="${ARGON_TOOLCHAIN_DIR:-$ROOT_DIR/.argon-toolchain}"
ANTLR4_TOOL_DIR="$TOOLCHAIN_DIR/antlr4-tool"

ANTLR4_TOOL_REPO="${ANTLR4_TOOL_REPO:-https://github.com/rrevenantt/antlr4.git}"
ANTLR4_TOOL_BRANCH="${ANTLR4_TOOL_BRANCH:-rust-target}"
ANTLR4_TOOL_COMMIT="${ANTLR4_TOOL_COMMIT:-e157622876d58b5858147a31ee88aca394a07af8}"

usage() {
    cat <<EOF
Usage: $(basename "$0")

Bootstraps the pinned antlr4rust runtime and Rust-target ANTLR tool into:
  $TOOLCHAIN_DIR

Overrides:
  ARGON_TOOLCHAIN_DIR  Change the toolchain directory location.
  ANTLR4RUST_COMMIT    Override the antlr4rust runtime commit.
  ANTLR4_TOOL_REPO     Override the ANTLR tool repository.
  ANTLR4_TOOL_BRANCH   Override the ANTLR tool branch.
  ANTLR4_TOOL_COMMIT   Override the ANTLR tool commit.
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

ensure_repo() {
    local repo_url=$1
    local repo_dir=$2
    local repo_ref=$3
    shift 3

    if [[ ! -d "$repo_dir/.git" ]]; then
        git clone "$@" "$repo_url" "$repo_dir"
    fi

    git -C "$repo_dir" fetch --all --tags
    git -C "$repo_dir" checkout "$repo_ref"
}

patch_file() {
    local file=$1
    local search=$2
    local replace=$3

    SEARCH="$search" REPLACE="$replace" \
        perl -0pi -e 's/\Q$ENV{SEARCH}\E/$ENV{REPLACE}/g' "$file"
}

require_cmd git
require_cmd java
require_cmd mvn
require_cmd perl
require_cmd install

install -d "$TOOLCHAIN_DIR"

echo "[1/4] syncing antlr4 rust-target tool"
ensure_repo "$ANTLR4_TOOL_REPO" "$ANTLR4_TOOL_DIR" "$ANTLR4_TOOL_COMMIT" --branch "$ANTLR4_TOOL_BRANCH" --recurse-submodules
git -C "$ANTLR4_TOOL_DIR" submodule update --init --recursive

echo "[2/4] applying compatibility patches"
patch_file \
    "$ANTLR4_TOOL_DIR/pom.xml" \
    "<maven.compiler.source>1.7</maven.compiler.source>" \
    "<maven.compiler.source>1.8</maven.compiler.source>"
patch_file \
    "$ANTLR4_TOOL_DIR/pom.xml" \
    "<maven.compiler.target>1.7</maven.compiler.target>" \
    "<maven.compiler.target>1.8</maven.compiler.target>"
patch_file \
    "$ANTLR4_TOOL_DIR/runtime/Java/pom.xml" \
    "<javadocVersion>1.7</javadocVersion>" \
    "<javadocVersion>1.8</javadocVersion>"
patch_file \
    "$ANTLR4_TOOL_DIR/tool/pom.xml" \
    "<javadocVersion>1.7</javadocVersion>" \
    "<javadocVersion>1.8</javadocVersion>"
patch_file \
    "$ANTLR4_TOOL_DIR/tool/pom.xml" \
    "<sourceVersion>1.7</sourceVersion>" \
    "<sourceVersion>1.8</sourceVersion>"
patch_file \
    "$ANTLR4_TOOL_DIR/tool/pom.xml" \
    "<targetVersion>1.7</targetVersion>" \
    "<targetVersion>1.8</targetVersion>"

echo "[3/4] syncing Rust target templates"
install -d "$ANTLR4_TOOL_DIR/runtime/Rust/templates"
install -d "$ANTLR4_TOOL_DIR/tool/resources/org/antlr/v4/tool/templates/codegen/Rust"

echo "[4/4] building ANTLR tool jar"
(
    cd "$ANTLR4_TOOL_DIR"
    mvn -pl tool -am -DskipTests package
)

echo
echo "ANTLR bootstrap complete."
echo "Tool jar: $ANTLR4_TOOL_DIR/tool/target/antlr4-4.8-2-SNAPSHOT-complete.jar"
echo
echo "You can now run:"
echo "  ./scripts/build-compiler.sh --release"
echo "or:"
echo "  cargo build --workspace --release"
