#!/bin/sh
# Claude turn hook entry point for Dunking Sheep.
#
# Claude Code invokes this from ~/.claude/settings.json:
#   UserPromptSubmit -> .../dunkingsheep-turn.sh start
#   Stop             -> .../dunkingsheep-turn.sh end
#   SubagentStop     -> .../dunkingsheep-turn.sh noop
#
# SubagentStop must NEVER end the parent turn: a subagent finishing is not the
# agent going idle.  It is wired to `noop` here, and the producer's `end`
# additionally refuses events that are not Stop-shaped.
#
# Keep this wrapper tiny and always exit 0.  Claude treats a nonzero
# UserPromptSubmit exit as "block this prompt"; a daemon refusal must be logged
# by the Python producer, never turned into a blocked agent turn.
set -u

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd 2>/dev/null) || exit 0
for candidate in "$script_dir/dunkingsheep-turn.py" "$script_dir/turn-generation.py"; do
    if [ -f "$candidate" ]; then
        python3 "$candidate" "$@" || true
        exit 0
    fi
done
exit 0
