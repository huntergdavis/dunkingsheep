"""Regenerate python_home_3_14_4.json from the repository root with Python 3.14.4.

Runs each case in an isolated child environment. Only the missing-passwd case
replaces pwd.getpwuid; the absent-HOME case records the actual current account.
"""

import json
import os
from pathlib import Path
import subprocess
import sys

assert sys.version_info[:3] == (3, 14, 4), sys.version
probe = r'''
import importlib, json, os, pathlib, pwd, sys
missing = sys.argv[1] == "missing-passwd"
if missing:
    def missing_user(uid):
        raise KeyError(uid)
    pwd.getpwuid = missing_user

def outcome(fn):
    try:
        return {"ok": str(fn())}
    except Exception as error:
        return {"error": str(error), "exception": type(error).__name__}

account = None
if "HOME" not in os.environ and not missing:
    account = pwd.getpwuid(os.getuid()).pw_dir
result = {"home": os.environ.get("HOME"), "account_home": account,
          "expanduser": os.path.expanduser("~"), "path": outcome(pathlib.Path.home)}
for setting in [None, "/explicit"]:
    sys.modules.pop("dunk_poolrad", None)
    if setting is None:
        os.environ.pop("DUNKINGSHEEP_DIR", None)
    else:
        os.environ["DUNKINGSHEEP_DIR"] = setting
    result["config_default" if setting is None else "config_explicit"] = outcome(
        lambda: importlib.import_module("dunk_poolrad").CONFIG_DIR)
print(json.dumps(result))
'''
cases = []
for name, home in [("exact-tilde", "~"), ("tilde-name", "~unknown"),
                   ("tilde-path", "~/a"), ("empty", ""), ("all-slashes", "////"),
                   ("absent", None), ("missing-passwd", None)]:
    env = dict(os.environ, PYTHONDONTWRITEBYTECODE="1")
    if home is None:
        env.pop("HOME", None)
    else:
        env["HOME"] = home
    result = subprocess.run([sys.executable, "-c", probe, name], env=env,
                            check=True, capture_output=True, text=True)
    cases.append({"name": name, **json.loads(result.stdout)})
data = {"python_version": list(sys.version_info[:3]),
        "oracle": "Python 3.14.4 pathlib.Path.home and exact-head dunk_poolrad.CONFIG_DIR imports; absent uses actual getpwuid(real uid), missing-passwd injects KeyError only",
        "cases": cases}
path = Path("rust/crates/ds-core/src/server_runtime/poolrad/python_home_3_14_4.json")
path.write_text(json.dumps(data, indent=2) + "\n")
print(json.dumps(data, indent=2))
