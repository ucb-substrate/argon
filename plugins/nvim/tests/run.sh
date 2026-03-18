#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
nvim --headless -u NONE -c "luafile ${script_dir}/smoke.lua" -c "qa!"
