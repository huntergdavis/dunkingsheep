"""Where the desk tools keep their state: under the config dir, never the repo (t1031).

Same rule as web/app.py, bridges/ops_metrics.py and bridges/host_watch.py: VP_DATA_DIR, else
<DUNKINGSHEEP_DIR or ~/.config/dunkingsheep>/desk. `or`, NOT os.getenv's default: an EMPTY
value would otherwise be Path("") - the current directory, i.e. the checkout, which is how
three credential files reached a commit on 2026-10-29. Nothing here creates the directory;
callers mkdir what they write.
"""

import os
from pathlib import Path


def data_dir() -> Path:
    return Path(os.environ.get("VP_DATA_DIR")
                or (Path(os.environ.get("DUNKINGSHEEP_DIR")
                         or str(Path.home() / ".config" / "dunkingsheep")) / "desk")).expanduser()
