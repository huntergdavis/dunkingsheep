#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."   # repo root
if [[ ! -x .venv/bin/uvicorn ]]; then
  python3 -m venv .venv
  .venv/bin/pip install -r requirements.txt
fi
if [[ ! -f ui/dist/index.html ]]; then
  npm install --prefix ui
  npm run build --prefix ui
fi
exec .venv/bin/uvicorn web.app:app --host "${VP_BIND:-0.0.0.0}" --port "${VP_PORT:-8787}" --workers 1
