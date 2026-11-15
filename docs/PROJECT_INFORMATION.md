# Project information browser (t3250)

The desk's Projects view has an **Information** panel: a read-only browser of each project's
context store (`<project dir>/.dunkingsheep/`) and of the global store
(`$DUNKINGSHEEP_DIR/context`). Ground truths are listed first, apart from facts, provisional
entries and history; each entry shows its type, authority, source, when it was stated, and
whether it is stale, superseded or retired.

## Which store belongs to which project

Only an owner-written map says so: `$DUNKINGSHEEP_DIR/project-stores.json`, a JSON object of
project name to project directory. See `docs/examples/project-stores.example.json`.

```json
{
  "example-project": "/home/example/workspace/example-project"
}
```

The desk uses the map only when it is a regular file (not a link), owned by the desk's user and
not writable by group or others. Each directory must be absolute and, once resolved, contain a
`.dunkingsheep` directory. A bad map or a bad entry is shown as "mapping invalid" with the
reason; a project that is not in the map shows "no information store mapped". Nothing is
inferred from where lanes run.

## Read-only

Viewing never writes: no store, cache or index file is created or changed (search is a
substring match over the entries read for the request). A file in a store that is a link, or
that resolves outside the store, is skipped and counted. Entry text is shown as text; long
bodies are truncated with a marker, and at most 500 entries are returned per request.

## Who

The owner's signed-in session only. Routes: `GET /api/project-context` (the map and the global
store), `GET /api/project-context/project/{name}?q=`, `GET /api/project-context/global?q=`.
