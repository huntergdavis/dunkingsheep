// In-pane OpenCode turn producer for Dunking Sheep.
//
// OpenCode plugins run inside the OpenCode server process.  When that server
// belongs to the pane (the default TUI and `opencode run`), the daemon's turn
// RPC authenticates the spawned CLI by its kernel ancestry under the pane root.
// A shared/detached server fails that check and the daemon refuses the event;
// this plugin never falls back to a pane label, cwd or session guess.
//
// Contract (same as integrations/pi/turn-generation.patch):
//   * `chat.message` starts a turn for its exact session id;
//   * `session.idle` ends only the session that started, with the token returned
//     by `--json turn start`;
//   * no token means no end (never manufacture an idle claim);
//   * child sessions cannot replace the pane's root session;
//   * an ambiguous second root session is refused;
//   * a reconnect/restart loses in-memory tokens, and the daemon TTL is the
//     stated backstop (disconnect behaviour is not inferred as idle).
//
// The pane is never passed as a label.  The CLI resolves the caller's terminal
// identity and the daemon re-checks process ancestry.

import { spawnSync } from "node:child_process";
import { appendFileSync, mkdirSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const DUNK_BIN = process.env.DUNKINGSHEEP_BIN || "dunkingsheep";
const TTL_S = Number(process.env.DUNKINGSHEEP_OPENCODE_TURN_TTL_S || 1800);
const CLI_TIMEOUT_MS = Number(process.env.DUNKINGSHEEP_TURN_CLI_TIMEOUT_MS || 5_000);
const LOG_PATH = process.env.DUNKINGSHEEP_OPENCODE_TURN_LOG ||
  join(process.env.XDG_STATE_HOME || join(homedir(), ".local", "state"),
       "dunkingsheep", "opencode-turn.log");

const active = new Map();       // sessionID -> {turnId, startedAt, expiresAt, cwd, paneId}
const childSessions = new Map(); // child id -> parent id
let rootSessionID;

function log(message) {
  try {
    mkdirSync(join(LOG_PATH, ".."), { recursive: true });
    appendFileSync(LOG_PATH, `${new Date().toISOString()} ${message}\n`);
  } catch {
    // A producer must never break the agent; the daemon TTL is the backstop.
  }
}

function sessionIDFrom(properties) {
  return typeof properties?.sessionID === "string" && properties.sessionID
    ? properties.sessionID
    : undefined;
}

function short(id) {
  return typeof id === "string" ? id.slice(0, 12) : String(id);
}

function runCli(args, cwd) {
  // Synchronous on purpose.  `opencode run` exits immediately after
  // `session.idle`; an awaited spawn can be abandoned mid-flight, leaving a
  // working turn until the TTL.  A short blocking call guarantees the token is
  // either consumed or never created before the process goes away.
  try {
    const result = spawnSync(DUNK_BIN, args, {
      cwd: cwd || undefined,
      env: process.env,
      encoding: "utf8",
      timeout: CLI_TIMEOUT_MS,
      maxBuffer: 1024 * 1024,
    });
    return {
      code: typeof result.status === "number" ? result.status : 1,
      stdout: result.stdout || "",
      stderr: String(result.stderr || result.error || "").slice(0, 300),
    };
  } catch (error) {
    return { code: 1, stdout: "", stderr: String(error).slice(0, 300) };
  }
}

function parseTurnID(stdout) {
  try {
    const value = JSON.parse(stdout);
    return typeof value?.turn_id === "string" && value.turn_id
      ? { turnId: value.turn_id, paneId: value.pane_id }
      : null;
  } catch {
    return null;
  }
}

function startTurn(sessionID, cwd, prompt = "") {
  if (!sessionID || active.has(sessionID)) return;
  const args = [
    "--json", "turn", "start", "--source", "opencode-plugin",
    "--ttl-s", String(Math.trunc(TTL_S)),
  ];
  const handoff = prompt.match(/\bHandoff (nj-[0-9a-f]{32})\./);
  if (handoff) args.push("--handoff-id", handoff[1]);
  const result = runCli(args, cwd);
  const token = result.code === 0 ? parseTurnID(result.stdout) : null;
  if (!token) {
    log(`start failed session=${short(sessionID)} rc=${result.code} ` +
        `error=${JSON.stringify(result.stderr)}`);
    return;
  }
  const now = Date.now();
  active.set(sessionID, {
    turnId: token.turnId,
    paneId: token.paneId,
    cwd,
    startedAt: now,
    expiresAt: now + TTL_S * 1000,
  });
  log(`start ok session=${short(sessionID)} pane=${token.paneId}`);
}

function endTurn(sessionID, reason) {
  const state = active.get(sessionID);
  if (!state) return;
  active.delete(sessionID); // consume before the RPC: a replay cannot send it twice
  if (Date.now() >= state.expiresAt) {
    log(`end refused reason=expired_turn session=${short(sessionID)}`);
    return;
  }
  const result = runCli([
    "--json", "turn", "end", "--turn-id", state.turnId, "--reason", reason,
  ], state.cwd);
  let ended = false;
  try { ended = JSON.parse(result.stdout)?.ended === true; } catch { /* refusal */ }
  log(`end session=${short(sessionID)} reason=${reason} rc=${result.code} ` +
      `ended=${ended} error=${JSON.stringify(result.stderr)}`);
}

export const DunkingsheepTurnPlugin = async () => ({
  // `chat.message` is the user's prompt entering this server: the start edge.
  "chat.message": ({ sessionID }, output) => {
    if (!sessionID || childSessions.has(sessionID)) return;
    if (rootSessionID && rootSessionID !== sessionID) {
      if (active.size > 0) {
        log(`start refused reason=ambiguous_session root=${short(rootSessionID)} ` +
            `other=${short(sessionID)}`);
        return;
      }
      rootSessionID = sessionID;
    }
    rootSessionID ||= sessionID;
    const prompt = (output?.parts ?? []).filter(p => p.type === "text")
      .map(p => p.text ?? "").join("\n");
    startTurn(sessionID, process.cwd(), prompt);
  },

  event: ({ event }) => {
    const type = event?.type;
    const properties = event?.properties ?? {};
    const info = properties.info;
    if (info?.id && info.parentID) childSessions.set(info.id, info.parentID);
    const sessionID = sessionIDFrom(properties);
    if (!sessionID) return;
    if (childSessions.has(sessionID)) return;

    switch (type) {
      case "session.idle":
        if (sessionID === rootSessionID) endTurn(sessionID, "idle");
        else if (active.has(sessionID)) endTurn(sessionID, "idle");
        else log(`end skipped reason=not_root_session session=${short(sessionID)}`);
        break;
      case "session.deleted":
        childSessions.delete(sessionID);
        if (sessionID === rootSessionID) {
          endTurn(sessionID, "session_deleted");
          rootSessionID = undefined;
        }
        break;
      case "session.error":
        // An error is not an end: the session may retry. The TTL is the backstop.
        if (active.has(sessionID)) {
          log(`session.error while active session=${short(sessionID)} ` +
              "state=working_until_idle_or_ttl");
        }
        break;
      default:
        break;
    }
  },
});

export default {
  id: "dunkingsheep.opencode.turn-generation",
  server: DunkingsheepTurnPlugin,
  setup() {},
};
