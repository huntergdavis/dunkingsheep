# Python daemon source retirement

The root Python daemon, CLI and TUI modules were removed after Rust cutover. The exact removed 51 files are identified by path, Git blob, SHA-256, size and mode in `python-daemon-archive.json`. The independently verified, pushed annotated tag `python-pre-retirement-1b14a9fb` points to commit `1b14a9fb15b7ccf8763b2e15a2ac428824cdc3a8`. The older `python-final` tag predates changes to six of these files and is retained as historical evidence.

For example, retrieve one file with:

```sh
git fetch origin refs/tags/python-pre-retirement-1b14a9fb
git show '1b14a9fb15b7ccf8763b2e15a2ac428824cdc3a8:dunk_tasks.py' > /tmp/dunk_tasks.py
```

Existing Rust differential tests, Python tests and CLI generators still require the Python reference. In a clean checkout, `python3 tools/restore_python_oracle.py` verifies every pinned file and materializes untracked root files for those tests. It refuses to overwrite any existing path. CI runs it before the unchanged oracle-dependent checks. The restore is a test-only operation: it does not install or start the Python daemon. A shallow checkout fetches the named archive tag from `origin` and checks its peeled commit before restoring. For offline use, fetch the tag before disconnecting.

This change does not remove `web/`, `bridges/`, `tools/`, or Python-based test/guard fixtures. It does not remove installed release directories, the current symlink, Python virtualenv, or runtime state. In particular, installed Telegram and web rollback units can still need the retained Python release, and the installed `dunk-poll` wrapper still runs a Python helper. Their retirement requires separate verified replacements and a coordinated rollback decision.

The tool checks the whole manifest and refuses conflicts before writing, but filesystem errors or concurrent creation can interrupt publication. It reports the names it created and never deletes them automatically. If that happens, stop concurrent writers and inspect each reported path against the manifest and the pinned Git blob; preserve anything that differs or is not attributable to the failed invocation. Remove only verified, unneeded partial files before retrying. Never use a wildcard cleanup against a working tree. When the pinned commit is already in the local Git object database, restoration makes no network request; a missing or corrupt object fails without publishing source files.
