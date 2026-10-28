// Exercise the OpenCode plugin with a fake daemon CLI, real plugin code.
//
// Node's test runner is used so the producer's actual spawn/parse/token logic
// runs; only the daemon and OpenCode are faked.
import assert from "node:assert/strict";
import { chmodSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import test from "node:test";

const PLUGIN = join(import.meta.dirname, "..", "integrations", "opencode",
                    "dunkingsheep-turn.js");
let caseNumber = 0;

function fakeCli(directory) {
  const path = join(directory, "dunkingsheep");
  writeFileSync(path, `#!/bin/sh
printf '%s\\n' "$*" >> "$CALLS_FILE"
case " $* " in
  *" --json "*)
    if [ "\${DUNK_FAIL_START:-0}" = "1" ]; then exit 1; fi
    n=$(cat "$COUNTER_FILE"); n=$((n + 1)); printf '%s' "$n" > "$COUNTER_FILE"
    printf '{"turn_id":"gen-%s","pane_id":"%%99"}\\n' "$n"
    ;;
  *)
    printf '{"ended":true,"dispatch":{"trigger":"turn.end"}}\\n'
    ;;
esac
exit 0
`);
  chmodSync(path, 0o755);
  return path;
}

async function setup() {
  const directory = mkdtempSync(join(tmpdir(), "opencode-turn-"));
  const calls = join(directory, "calls");
  const counter = join(directory, "counter");
  writeFileSync(calls, "");
  writeFileSync(counter, "0");
  process.env.DUNKINGSHEEP_BIN = fakeCli(directory);
  process.env.CALLS_FILE = calls;
  process.env.COUNTER_FILE = counter;
  process.env.DUNKINGSHEEP_OPENCODE_TURN_LOG = join(directory, "producer.log");
  process.env.DUNKINGSHEEP_OPENCODE_TURN_TTL_S = "30";
  caseNumber += 1;
  const module = await import(pathToFileURL(PLUGIN).href + `?case=${caseNumber}`);
  const hooks = await module.DunkingsheepTurnPlugin();
  return {
    hooks,
    directory,
    calls: () => readFileSync(calls, "utf8").trim().split("\n").filter(Boolean),
  };
}

async function idle(hooks, sessionID) {
  await hooks.event({ event: { type: "session.idle", properties: { sessionID } } });
}

test("start then token-bound end, with no pane label", async () => {
  const { hooks, calls } = await setup();
  await hooks["chat.message"]({ sessionID: "ses_1" });
  await idle(hooks, "ses_1");
  const lines = calls();
  assert.equal(lines.length, 2);
  assert.match(lines[0], /--json turn start --source opencode-plugin --ttl-s 30$/);
  assert.match(lines[1], /^--json turn end --turn-id gen-1 --reason idle$/);
  assert.ok(!lines.some((line) => line.includes("--pane")));
});

test("duplicate start is suppressed and a consumed token is not reused", async () => {
  const { hooks, calls } = await setup();
  await hooks["chat.message"]({ sessionID: "ses_1" });
  await hooks["chat.message"]({ sessionID: "ses_1" });
  await idle(hooks, "ses_1");
  await idle(hooks, "ses_1");
  const lines = calls();
  assert.equal(lines.length, 2);
  assert.equal(lines.filter((line) => line.includes("turn start")).length, 1);
  assert.equal(lines.filter((line) => line.includes("turn end")).length, 1);
});

test("a failed start never produces an end", async () => {
  const { hooks, calls } = await setup();
  process.env.DUNK_FAIL_START = "1";
  await hooks["chat.message"]({ sessionID: "ses_1" });
  await idle(hooks, "ses_1");
  delete process.env.DUNK_FAIL_START;
  const lines = calls();
  assert.equal(lines.length, 1);
  assert.match(lines[0], /turn start/);
});

test("a child session cannot start or end the pane root", async () => {
  const { hooks, calls } = await setup();
  await hooks.event({ event: { type: "session.created",
    properties: { info: { id: "child-1", parentID: "ses_root" } } } });
  await hooks["chat.message"]({ sessionID: "child-1" });
  await idle(hooks, "child-1");
  assert.deepEqual(calls(), []);
});

test("a second non-child root while active is ambiguous and refused", async () => {
  const { hooks, calls } = await setup();
  await hooks["chat.message"]({ sessionID: "ses_1" });
  await hooks["chat.message"]({ sessionID: "ses_2" });
  await idle(hooks, "ses_1");
  const lines = calls();
  assert.equal(lines.filter((line) => line.includes("turn start")).length, 1);
  assert.equal(lines.filter((line) => line.includes("turn end")).length, 1);
});

test("an idle without a start token is not an end", async () => {
  const { hooks, calls } = await setup();
  await idle(hooks, "ses_1");
  assert.deepEqual(calls(), []);
});

test("session deletion ends the root with a stated reason", async () => {
  const { hooks, calls } = await setup();
  await hooks["chat.message"]({ sessionID: "ses_1" });
  await hooks.event({ event: { type: "session.deleted",
    properties: { sessionID: "ses_1" } } });
  const lines = calls();
  assert.equal(lines.length, 2);
  assert.match(lines[1], /^--json turn end --turn-id gen-1 --reason session_deleted$/);
});

test("only the initiating prompt carries the next-job handoff", async () => {
  const { hooks, calls } = await setup();
  const id = "nj-" + "a".repeat(32);
  await hooks["chat.message"]({ sessionID: "ses_handoff" }, {
    parts: [{type: "text", text: `[inbox] TASK t2. Handoff ${id}.`}],
  });
  assert.ok(calls()[0].endsWith(`--handoff-id ${id}`));
  await idle(hooks, "ses_handoff");
  await hooks["chat.message"]({ sessionID: "ses_handoff" }, {
    parts: [{type: "text", text: "unrelated human prompt"}],
  });
  assert.ok(!calls().at(-1).includes("--handoff-id"));
});
