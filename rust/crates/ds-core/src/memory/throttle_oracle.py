"""Observe real Python ingestion and native time.sleep using only supplied temporary paths."""
import hashlib
import json
import sqlite3
import sys

from dunk_memory import MemoryStore

path, database, raw, stop_mode, duplicate = sys.argv[1:]
store = MemoryStore(database)
flushes = []
original = store._insert_chunks


def traced(session_id, chunks, **kwargs):
    inserted = original(session_id, chunks, **kwargs)
    flushes.append([len(chunks), kwargs.get("role"), inserted])
    return inserted


store._insert_chunks = traced
if duplicate == "yes":
    store._ingest_jsonl(path, "generic", throttle_s=0.0)
    flushes.clear()
calls = 0


def should_stop():
    global calls
    calls += 1
    return stop_mode != "never" and calls >= int(stop_mode)


error = None
result = None
try:
    result = store._ingest_jsonl(path, "generic", should_stop=should_stop,
                                 throttle_s=float(raw))
except Exception as caught:
    error = [type(caught).__name__, str(caught)]
# Observe committed bytes from a different connection before close can affect transactions.
reader = sqlite3.connect(database)
rows = [[role, ordinal, len(text), hashlib.sha256(text.encode()).hexdigest()]
        for role, ordinal, text in reader.execute(
            "SELECT role, ordinal, text FROM chunks ORDER BY id")]
reader.close()
print(json.dumps({"error": error, "flushes": flushes, "rows": rows,
                  "result": result, "throttle": repr(float(raw))}))
store.close()
