#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."   # repo root
if [[ ! -x .venv/bin/uvicorn ]]; then
  python3 -m venv .venv
  # The desk's dependencies live in bridges/requirements.txt; the repo-root
  # requirements.txt is deliberately stdlib-only ("Dunking Sheep has no
  # third-party Python dependencies"), so installing from it left no uvicorn and
  # the exec below failed with ENOENT - every restart re-entered this branch, a
  # crash loop under Restart=on-failure, and the guard tested a binary that could
  # never appear (t622/t588).
  .venv/bin/pip install -r bridges/requirements.txt
fi
if [[ ! -f ui/dist/index.html ]]; then
  npm install --prefix ui
  npm run build --prefix ui
fi
exec .venv/bin/uvicorn web.app:app --host "${VP_BIND:-0.0.0.0}" --port "${VP_PORT:-8787}" --workers 1
