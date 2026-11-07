# Web framework pins (t3331)

The Python desk that the Rust `ds-web` port is checked against is pinned here, so the
parity evidence names the exact versions that produced it.

| Package    | Pinned     | Where                                   |
|------------|------------|-----------------------------------------|
| fastapi    | 0.162.0    | `bridges/requirements.txt`              |
| starlette  | 1.10.0     | `bridges/requirements.txt`              |
| pydantic   | 2.14.2     | pulled by fastapi 0.162.0 (not pinned directly) |
| Python     | 3.14.4     | the served release venv                 |

## Host version behind the 405 Allow

The 405 answers recorded here came from the served release venv:

    releases/web-9c8f919-client499-fd1d498/.venv   fastapi 0.162.0, starlette 1.10.0, pydantic 2.14.2

- The upload route's HEAD answer (`405`, `Allow: GET`) is recorded in
  `rust/crates/ds-web/tests/fixtures/uploads_range_oracle.json` with the same versions.
- The static mount's 405 `Allow` (`GET, HEAD`) was NOT re-recorded for this pin. It is
  not verified here, and it is a separate check against the same venv.

## Keeping them in step

`tests/test_web_production_pins.py` fails if `bridges/requirements.txt`, this document, or the
uploads oracle's recorded versions disagree.
