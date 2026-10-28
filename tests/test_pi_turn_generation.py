"""Execute the exact added producer function from the owner-file patch, without pi."""
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class PiTurnGenerationTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which("node"), "node required to exercise the pi producer patch")
    def test_patch_serializes_tokens_and_never_emits_unbound_ends(self):
        patch = (Path(__file__).resolve().parents[1] /
                 "integrations/pi/turn-generation.patch").read_text()
        after = "".join(line[1:] for line in patch.splitlines(True)
                        if line.startswith((" ", "+")) and not line.startswith("+++"))
        start = after.index("\t// Serialize hook commands")
        end = after.index('\n\tpi.on("agent_start"', start)
        function = after[start:end]
        probe = """
import assert from "node:assert/strict";
function reporter(exec: any) {
  const pi = {exec}, DUNK_BIN = "fake-dunk";
FUNCTION
  return turn;
}
let calls: string[][] = [];
let release: any;
const delayed = new Promise(resolve => { release = resolve; });
const turn = reporter(async (_bin: string, args: string[]) => {
  calls.push(args);
  return args[0] === "--json" ? delayed : {code: 0, stdout: ""};
});
const start = turn("start", "/tmp");
const end = turn("end", "/tmp");
await Promise.resolve();
assert.deepEqual(calls, [["--json", "turn", "start"]]);
release({code: 0, stdout: JSON.stringify({turn_id: "generation-a"})});
await Promise.all([start, end]);
assert.deepEqual(calls[1], ["turn", "end", "--turn-id", "generation-a"]);
await turn("end", "/tmp");
assert.equal(calls.length, 2, "a repeated end cannot reuse a consumed token");

for (const result of [
  {code: 1, stdout: '{"turn_id":"invalid"}'},
  {code: 0, stdout: "not json"},
  {code: 0, stdout: "{}"},
]) {
  const seen: string[][] = [];
  const failed = reporter(async (_bin: string, args: string[]) => {
    seen.push(args); return result;
  });
  await failed("start", "/tmp");
  await failed("end", "/tmp");
  assert.equal(seen.length, 1, "failed/tokenless starts must not emit ends");
}
let generation = 0;
const ordered: string[][] = [];
const serial = reporter(async (_bin: string, args: string[]) => {
  ordered.push(args);
  if (args[0] === "--json") return {code: 0, stdout: JSON.stringify({turn_id: String(++generation)})};
  return {code: 0, stdout: ""};
});
await Promise.all([serial("start", "/tmp"), serial("end", "/tmp"),
                   serial("start", "/tmp"), serial("end", "/tmp")]);
assert.deepEqual(ordered.map(args => args.at(-1)), ["start", "1", "start", "2"]);
console.log("PASS: delayed start, consumed token, three failure responses, ordered generations");
""".replace("FUNCTION", function)
        with tempfile.TemporaryDirectory() as directory:
            script = Path(directory) / "probe.mts"
            script.write_text(probe)
            run = subprocess.run(["node", "--experimental-strip-types", str(script)],
                                 capture_output=True, text=True, timeout=20)
        self.assertEqual(0, run.returncode, run.stdout + run.stderr)
        self.assertIn("PASS:", run.stdout)
