#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

"$ROOT_DIR/scripts/bootstrap-antlr.sh"

cd "$ROOT_DIR"
cargo build -p compiler "$@"
