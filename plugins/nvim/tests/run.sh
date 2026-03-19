#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
SCRIPT_PATH="$ROOT_DIR/plugins/nvim/tests/smoke.lua"

ARGON_REPO_ROOT="$ROOT_DIR" \
  nvim --headless -u NONE \
  +"lua dofile([[${SCRIPT_PATH}]])" \
  +qa
