#!/usr/bin/env bash
#
# Launch Dunking Sheep TUI. Meant to be run inside a herdr tab (the whole point
# is to drive *other* herdr panes), but it works from any terminal as long as a
# herdr server is running and the `herdr` binary is on PATH.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Make sure the usual herdr install location is reachable.
export PATH="$HOME/.local/bin:$PATH"

if ! command -v herdr >/dev/null 2>&1; then
    echo "warning: 'herdr' not found on PATH (~/.local/bin). Dunking Sheep needs" >&2
    echo "         a running herdr server to send text. See https://herdr.dev" >&2
fi

PYTHON="${PYTHON:-python3}"
exec "$PYTHON" "$SCRIPT_DIR/dunking_sheep_tui.py" "$@"
