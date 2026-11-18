#!/usr/bin/env bash
# Runs the docs checks that CI runs (.github/workflows/docs.yml): the generated
# quick reference, the guide against its example, and a production build, which
# fails on any broken internal link or anchor. With --external, it then checks
# the built site's external links too, which CI leaves out. Run from anywhere.
set -euo pipefail
cd "$(dirname "$0")/.."

[ -d node_modules ] || npm ci --no-audit --no-fund
node scripts/reference.mjs --check
node scripts/guide.mjs
npm run build
if [ "${1:-}" = "--external" ]; then
  node scripts/external-links.mjs
fi
