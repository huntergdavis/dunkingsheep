"""t399: execute the exact poller from the pi patch with fake executors."""
import json
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PATCH = ROOT / "integrations" / "pi" / "poll-backoff.patch"


class PiPollBackoffTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which("node"), "node required for the pi poller test")
    def test_poller_backoff_and_fallback(self):
        patch = PATCH.read_text()
        # Reconstruct the new side of the diff: + added, space context, drop -.
        lines = []
        for line in patch.splitlines():
            if line.startswith(("+++", "---", "@@", "\\")):
                continue
            if line.startswith("+"):
                lines.append(line[1:])
            elif line.startswith(" "):
                lines.append(line[1:])
        added = "\n".join(lines)
        start = added.index("// --- t399 poll-backoff:start ---")
        end = added.index("// --- t399 poll-backoff:end ---")
        functions = added[start:end]
        probe = """
import assert from "node:assert/strict";
FUNCTIONS
const snap = () => ({unread:1,total:2,dunks:[],recent:[]});
const OK = {code:0,stdout:JSON.stringify({status:{},inbox_count:{},dunks:[],panes:{}})};
function harness(execImpl, runImpl, maxMs=4000, baseMs=1000) {
  let t = 0; const emitted = []; let execCalls = 0; const cli = [];
  const poller = makePoller({
    exec: async (...args) => { execCalls += 1; return execImpl(...args); },
    pollBin: "dunk-poll",
    runCli: async (args, cwd) => { cli.push(args); return runImpl(args, cwd); },
    buildSnapshot: () => snap(),
    emit: (s) => emitted.push(s),
    now: () => t,
    baseMs, maxMs,
  });
  return {poller, emitted, execCalls: () => execCalls, cli: () => cli,
          advance: (ms) => { t += ms; }, state: () => poller.state()};
}

// success: helper only, no CLI
let h = harness(async () => OK, async () => { throw new Error("CLI must not run"); });
let r = await h.poller.poll("/tmp");
assert.equal(r.snapshot.stale, undefined);
assert.equal(r.snapshot.source, "dunk-poll");
assert.deepEqual(h.cli(), []); assert.equal(h.execCalls(), 1);

// timeout after a good snapshot: no CLI fallback, stale marked, backoff, no spawn inside window
let mode = {kind:"ok"};
h = harness(async () => mode.kind === "timeout" ? {code:124,stderr:"timed out"} : OK,
            async () => { throw new Error("CLI must not run on timeout"); });
await h.poller.poll("/tmp"); mode.kind = "timeout";
r = await h.poller.poll("/tmp");
assert.equal(r.snapshot.stale, true);
assert.match(r.snapshot.stale_reason, /timeout|timed out/i);
assert.deepEqual(h.cli(), []);
assert.ok(h.state().backoffMs >= 1000);
const callsAfterTimeout = h.execCalls();
r = await h.poller.poll("/tmp");
assert.equal(h.execCalls(), callsAfterTimeout, "inside backoff must not spawn");
assert.equal(r.snapshot.stale, true);
h.advance(h.state().backoffMs); mode.kind = "ok";
r = await h.poller.poll("/tmp");
assert.equal(r.snapshot.stale, undefined);
assert.equal(h.state().backoffMs, 0, "success resets backoff");

// daemon unavailable / EAGAIN: no CLI fallback
h = harness(async () => ({code:1,stderr:"EAGAIN daemon unavailable"}),
            async () => { throw new Error("CLI must not run on unavailable"); });
r = await h.poller.poll("/tmp");
assert.equal(r, undefined, "no synthetic healthy snapshot");
assert.deepEqual(h.cli(), []);
assert.ok(h.state().backoffMs >= 1000);

// missing helper is the ONLY fallback: three CLI reads, source cli-fallback
h = harness(async () => ({code:127,stderr:"dunk-poll: not found"}),
            async (args) => ({ok:true,args}));
r = await h.poller.poll("/tmp");
assert.equal(r.snapshot.source, "cli-fallback");
assert.equal(r.snapshot.stale, undefined);
assert.deepEqual(h.cli().map(a => a[0]).sort(), ["inbox","list","status"]);
assert.equal(h.execCalls(), 1);
assert.ok(h.state().backoffMs >= 1000, "fallback success must still back off");
const fallbackExecs = h.execCalls();
r = await h.poller.poll("/tmp");
assert.equal(h.execCalls(), fallbackExecs, "fallback is also backoff-gated");

// missing helper + fallback failure: no fake snapshot, bounded retry
h = harness(async () => ({code:127,stderr:"ENOENT: no such file"}),
            async () => { throw new Error("cli down"); });
r = await h.poller.poll("/tmp");
assert.equal(r, undefined);
assert.ok(h.state().backoffMs >= 1000);
const failedCalls = h.execCalls();
r = await h.poller.poll("/tmp");
assert.equal(h.execCalls(), failedCalls, "failed fallback is backoff-gated");

// one inflight: concurrent poll returns undefined and spawns once
let release; const pending = new Promise(res => { release = res; });
h = harness(async () => pending, async () => ({ok:true}));
const first = h.poller.poll("/tmp");
const second = await h.poller.poll("/tmp");
assert.equal(second, undefined);
release(OK);
await first;
assert.equal(h.execCalls(), 1);

// backoff doubles and caps
h = harness(async () => ({code:124,stderr:"timeout"}), async () => ({ok:true}), 4000, 1000);
await h.poller.poll("/tmp");
for (let i = 0; i < 8; i++) {
  h.advance(h.state().backoffMs);
  await h.poller.poll("/tmp");
}
assert.ok(h.state().backoffMs <= 4000, `backoff ${h.state().backoffMs} must cap at 4000`);

// stale UI line is not a healthy count
const line = staleLine({unread:1,total:2,stale:true,stale_reason:"timed out",last_good_at:1700000000000});
assert.match(line, /STALE/); assert.match(line, /timed out/); assert.match(line, /1 unread of 2/);
console.log("PASS: poller success/timeout/unavailable/missing/backoff/inflight/stale");
""".replace("FUNCTIONS", functions)
        with tempfile.TemporaryDirectory() as directory:
            script = Path(directory) / "probe.mjs"
            script.write_text(probe)
            run = subprocess.run(["node", str(script)], capture_output=True,
                                 text=True, timeout=30)
        self.assertEqual(0, run.returncode, run.stdout + run.stderr)
        self.assertIn("PASS:", run.stdout)


if __name__ == "__main__":
    unittest.main()
