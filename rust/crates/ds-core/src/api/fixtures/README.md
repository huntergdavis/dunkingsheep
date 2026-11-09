# Recovery handler fixtures

These JSON files capture the inputs and Python-observed outcomes, adapter calls,
launch records and persistence flag from the recovery and doctor API differential
tests at Git commit `5c9839aa286dcd070bb938e60e368ae7f126bf05`. The test's Python
oracle executed the archived implementation pinned by `docs/rust/python-daemon-archive.json`
at `1b14a9fb15b7ccf8763b2e15a2ac428824cdc3a8`; the archive restoration tool checked
its manifest hashes before capture. The original test and embedded oracle remain
available in that Git history. `recover` contains 84 cases, `doctor` contains 52.

Capture used the original tests with only a temporary write of the successful
oracle's stdout added before the existing Rust comparison. Both original tests
passed during capture. The numeric JSON tokens are kept as emitted, including
out-of-range exponents, rather than re-encoding through a floating-point parser.
Fixtures are snapshots, so changes in Python behavior will not update them
automatically. Other differential tests still use the Python archive.
