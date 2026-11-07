"""The Python roster over a TmuxBackend whose tmux cannot reach a server (t3464).

Run from the repository root with DUNKINGSHEEP_DIR, DUNKINGSHEEP_SOCKET and HOME pointing at
a scratch directory, with the fake tmux path as the only argument. Prints one JSON object:
the roster view and the resolved lanes, which the Rust side compares with its own answer.
"""
import json
import os
import sys

sys.path.insert(0, os.getcwd())
fake_tmux = sys.argv[1]
scratch = os.path.dirname(fake_tmux)
import dunk_terminal  # noqa: E402
import dunk_views  # noqa: E402
from dunk_core import Flock  # noqa: E402

backend = dunk_terminal.TmuxBackend(
    socket_name="no-such-server",
    registry_path=os.path.join(scratch, "py-tmux-panes.json"),
    tmux_bin=fake_tmux,
    guard_dir=os.path.join(scratch, "no-guard"),
)
flock = Flock(herdr=backend, persist=False, restore=False)
print(json.dumps({
    "list_panes": backend.list_panes(),
    "lanes": flock.resolve_lanes(),
    "roster": dunk_views.roster(flock),
}))
