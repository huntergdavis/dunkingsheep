#!/bin/sh
# dunkingsheep gh wrapper: lanes act as their GitHub App; the owner keeps their login.
# Installed by `dunkingsheep github wrappers`; do not edit.
# t756: opening a PR publishes a branch, so the desk's own secrets are refused
# here before gh runs. This path cannot be bypassed by a credential fallback.
case "$1 $2" in
  "pr create")
    if ! dunkingsheep --no-start-daemon secrets-check; then
      echo "REFUSED by the dunkingsheep gh wrapper (t756): fix the paths above." >&2
      exit 1
    fi ;;
esac
tok=$(dunkingsheep --no-start-daemon gh-token --quiet 2>/dev/null)
if [ -n "$tok" ]; then
  GH_TOKEN="$tok" exec __REAL_GH__ "$@"
fi
exec __REAL_GH__ "$@"
