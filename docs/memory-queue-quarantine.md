# Failed memory-ingest entries

An owner can stop retries for a single failed queue entry without deleting the source or indexed data:

```
dunkingsheep memory queue quarantine QUEUE_ID --path /exact/source/path --reason 'reason'
```

The operation requires the current queue ID, exact source path and a reason. It refuses leased, queued, finished, changed, or already quarantined entries. It records the operator and reason in memory events. Restore the same entry to the failed retry queue with `memory queue restore QUEUE_ID --path /exact/source/path --reason 'reason'`. The source record, previous error, attempt count and existing corpus are retained. A nonzero quarantined count is exposed in memory status; Status remains degraded while entries are quarantined; quarantine acknowledges a failure and stops retries but does not claim ingestion succeeded. Restoring an unchanged unsupported input will cause failure again.

A pane-less authenticated owner service token may perform the same operation. Its audit actor is `owner-token:<label>`, a non-secret, operator-chosen label; labels need not uniquely identify a credential or person. Token validation and the queue transaction use separate stores, so revocation is checked at request time and is not atomic with the queue commit. Bound owner panes continue to use their pane ID.
