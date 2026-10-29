"""Execute the exact added producer function from the owner-file patch, without pi."""
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class PiTurnGenerationTests(unittest.TestCase):
    def test_t444_patch_adds_prompt_handoff_and_source_to_current_serialized_producer(self):
        patch = (Path(__file__).resolve().parents[1] /
                 "integrations/pi/t444-handoff-source.patch").read_text()
        self.assertIn('--source", "pi-extension', patch)
        self.assertIn('--handoff-id', patch)
        self.assertIn('before_agent_start', patch)
        self.assertIn('const handoff = prompt.match', patch)
        self.assertIn('function turn(kind: "start" | "end", cwd: string, prompt = "")', patch)
        self.assertNotIn('t399 poll-backoff', patch,
                         "the installed poll patch is already present; do not duplicate it")


    @unittest.skipUnless(shutil.which("node"), "node required to exercise the pi producer patch")
    def test_patch_serializes_tokens_and_never_emits_unbound_ends(self):
        patch = (Path(__file__).resolve().parents[1] /
                 "integrations/pi/turn-generation.patch").read_text()
        after = "".join(line[1:] for line in patch.splitlines(True)
                        if line.startswith((" ", "+")) and not line.startswith("+++"))
        start = after.index("\t// Serialize hook commands")
        end = after.index('\n\tpi.on("before_agent_start"', start)
        function = after[start:end]
        # This host's Node has no TypeScript stripping support. Erase only the
        # two known annotations; assert their spelling so a changed patch cannot
        # silently run a different approximation of the function.
        annotations = {
            "let activeTurnId: string | undefined;": "let activeTurnId;",
            'function turn(kind: "start" | "end", cwd: string, prompt = ""): Promise<void>':
                'function turn(kind, cwd, prompt = "")',
        }
        for typed, plain in annotations.items():
            self.assertEqual(1, function.count(typed))
            function = function.replace(typed, plain)
        probe = """
import assert from "node:assert/strict";
function reporter(exec) {
  const pi = {exec}, DUNK_BIN = "fake-dunk";
FUNCTION
  return turn;
}
let calls = [];
let release;
const delayed = new Promise(resolve => { release = resolve; });
const turn = reporter(async (_bin, args) => {
  calls.push(args);
  return args[0] === "--json" ? delayed : {code: 0, stdout: ""};
});
const start = turn("start", "/tmp");
const end = turn("end", "/tmp");
await Promise.resolve();
assert.deepEqual(calls, [["--json", "turn", "start", "--source", "pi-extension"]]);
release({code: 0, stdout: JSON.stringify({turn_id: "generation-a"})});
await Promise.all([start, end]);
assert.deepEqual(calls[1], ["turn", "end", "--turn-id", "generation-a"]);
await turn("end", "/tmp");
assert.equal(calls.length, 2, "a repeated end cannot reuse a consumed token");

const withHandoff = [];
const reporterWithHandoff = reporter(async (_bin, args) => {
  withHandoff.push(args);
  return {code: 0, stdout: '{"turn_id":"handoff-turn"}'};
});
const handoff = "nj-" + "a".repeat(32);
await reporterWithHandoff("start", "/tmp", `[inbox] TASK t2. Handoff ${handoff}.`);
assert.deepEqual(withHandoff[0].slice(0, 5),
                 ["--json", "turn", "start", "--source", "pi-extension"]);
assert.deepEqual(withHandoff[0].slice(-2), ["--handoff-id", handoff]);
await reporterWithHandoff("end", "/tmp");
await reporterWithHandoff("start", "/tmp", "unrelated human prompt");
assert.ok(!withHandoff[2].includes("--handoff-id"));
assert.ok(withHandoff[2].includes("--source"));

for (const result of [
  {code: 1, stdout: '{"turn_id":"invalid"}'},
  {code: 0, stdout: "not json"},
  {code: 0, stdout: "{}"},
]) {
  const seen = [];
  const failed = reporter(async (_bin, args) => {
    seen.push(args); return result;
  });
  await failed("start", "/tmp");
  await failed("end", "/tmp");
  assert.equal(seen.length, 1, "failed/tokenless starts must not emit ends");
}
let generation = 0;
const ordered = [];
const serial = reporter(async (_bin, args) => {
  ordered.push(args);
  if (args[0] === "--json") return {code: 0, stdout: JSON.stringify({turn_id: String(++generation)})};
  return {code: 0, stdout: ""};
});
await Promise.all([serial("start", "/tmp"), serial("end", "/tmp"),
                   serial("start", "/tmp"), serial("end", "/tmp")]);
assert.deepEqual(ordered.map(args => args.at(-1)), ["pi-extension", "1", "pi-extension", "2"]);
console.log("PASS: delayed start, consumed token, three failure responses, ordered generations");
""".replace("FUNCTION", function)
        with tempfile.TemporaryDirectory() as directory:
            script = Path(directory) / "probe.mjs"
            script.write_text(probe)
            run = subprocess.run(["node", str(script)],
                                 capture_output=True, text=True, timeout=20)
        self.assertEqual(0, run.returncode, run.stdout + run.stderr)
        self.assertIn("PASS:", run.stdout)
