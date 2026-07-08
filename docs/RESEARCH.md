# Research: Dunking Bird → Dunking Sheep (on herdr)

This document records what was studied to build Dunking Sheep: the original
Dunking Bird TUI, and the herdr terminal workspace manager it now targets.

## 1. Dunking Bird (the thing we are porting)

Dunking Bird is a terminal-first automation tool that sends text to windows at
regular intervals — designed to keep coding agents engaged with prompts like
"continue" or "keep going". The reference implementation studied here is the
**TUI** (`dunking_bird_tui.py`), not the optional Tkinter GUI.

### Architecture of the TUI

- **`DunkingBirdTui`** — a `curses` app. Holds a list of dunkers, a selection
  index, a global `send_lock`, and a global status line. Main loop draws a table
  and reads one key at a time (200 ms poll).
- **`DunkerTuiRow`** — one "dunker". Fields: interval (minutes), text to send,
  captured target window (id/name/class/compositor), running flag, status
  string, and its own timer thread.

### Feature inventory (the spec Dunking Sheep must match)

| Feature | How Dunking Bird does it |
| --- | --- |
| Multiple concurrent dunkers | list of `DunkerTuiRow`, each with its own timer thread |
| Per-dunker interval | `i` opens a line-input modal (minutes, > 0) |
| Per-dunker custom text | `e` opens a full-screen `textpad.Textbox` (Ctrl+G save, Esc cancel) |
| Target a specific window | `c` captures the active window after a 2s countdown |
| Test send | `t` sends once after a 2s countdown |
| Start/stop automation | `space`/`s` toggles the timer thread |
| Live countdown | timer thread updates status to `Next: MM:SS` each second |
| Add / remove dunkers | `a` / `d` |
| Navigation | `j`/`k` or arrow keys |
| Quit | `q` / Esc |
| Global status line | environment/health messages, send results |

### How Dunking Bird actually sends text (the part that changes)

1. **Window capture.** On Wayland it queries the active window via `kdotool`
   (KDE), `swaymsg` (Sway) or `hyprctl` (Hyprland). On X11 it uses
   `xdotool selectwindow`. It stores a window id + name + compositor.
2. **Focus.** Before typing it re-focuses the captured window
   (`kdotool windowactivate`, falling back to `ydotool key alt+Tab`).
3. **Type.** It types the text with `ydotool type` and then presses Enter with
   `ydotool key 28:1 28:0`.
4. **Plumbing.** A lot of the code manages the `ydotoold` daemon: finding the
   socket, fixing permissions with `sudo chmod`, restarting the daemon, and
   suppressing the just-typed characters so the TUI doesn't eat them
   (`suppress_typed_input`). A global `send_lock` serializes sends because
   `ydotool` types into whatever window currently has focus.

**Key insight:** almost half of Dunking Bird is ydotool/window-management glue
that exists only because it drives the OS input layer. herdr removes the need
for all of it.

## 2. herdr (the environment we now run inside)

herdr is a "terminal workspace manager for AI coding agents" (studied at
v0.7.3, `~/.local/bin/herdr`). It runs a persistent **server** that owns the
terminals, and a **client** TUI that attaches to it. State lives behind a unix
socket at `~/.config/herdr/herdr.sock`, and the `herdr` CLI exposes that socket
API as subcommands that print JSON.

### Model

herdr organizes terminals as **workspaces → tabs → panes**. Each pane wraps a
terminal and may host a detected **agent** (claude, codex, …) with a live
**status**. `herdr pane list` returns, per pane:

```json
{
  "pane_id": "w1:p3",
  "agent": "claude",
  "agent_status": "working",      // idle | working | blocked | unknown
  "cwd": "/home/hunter/workspace/pspsps-engine",
  "focused": false,
  "tab_id": "w1:t3",
  "workspace_id": "w1",
  "terminal_id": "term_..."
}
```

### CLI surface relevant to Dunking Sheep

| Need | herdr command |
| --- | --- |
| Health check | `herdr status server` |
| List targets | `herdr pane list` / `herdr agent list` |
| Send literal text | `herdr pane send-text <pane_id> <text>` |
| Press a key | `herdr pane send-keys <pane_id> Enter` |
| Text + Enter combined | `herdr pane run <pane_id> <command>` |
| Read pane contents | `herdr wait output <pane_id> --match <text>` |
| Wait for agent state | `herdr agent wait <target> --status idle` |
| Toast notification | `herdr notification show <title> [--body ...]` |
| Full schema / snapshot | `herdr api schema --json` / `herdr api snapshot` |

### Things verified live during research

- `herdr pane send-text` + `herdr pane send-keys <pane> Enter` reliably delivers
  a line to a pane's terminal (confirmed by reading the pane back).
- `Enter` is a valid key name for `send-keys`.
- `herdr pane run <pane> <cmd>` is text + Enter in one call.
- On success these commands exit 0 and print nothing.
- `herdr pane read` printed nothing to a *piped* (non-tty) stdout in testing;
  `herdr wait output ... --match` returns the captured text in `result.read.text`
  and is the reliable way to read a pane from a script.
- The `herdr integration install <name>` system is only for specific agent CLIs
  (claude, codex, copilot, …). It is **not** a general third-party plugin API.

## 3. Design conclusion: plugin vs. process

herdr has no third-party plugin/extension API — `integration` is a fixed list of
agent shims. Therefore Dunking Sheep is built as **a process you run in a herdr
tab** (exactly the option the task allowed), and it drives *other* panes through
the socket API via the `herdr` CLI.

## 4. Dunking Bird → Dunking Sheep mapping

| Dunking Bird | Dunking Sheep |
| --- | --- |
| Capture OS window (`kdotool`/`xdotool`/…) | Pick a herdr **pane** from a live list (`herdr pane list`) |
| Stored window id/name/class/compositor | Stored `pane_id` + agent + cwd |
| Focus window before typing | *Not needed* — sends are addressed to the pane |
| `ydotool type` + `ydotool key 28:1 28:0` | `herdr pane send-text` + `herdr pane send-keys Enter` |
| `ydotoold` socket/permission/daemon management | *Gone* — herdr server owns the terminals |
| `suppress_typed_input` (avoid self-typing) | *Gone* — text never touches the OS input layer |
| `ydotool available?` runtime check | `herdr status server` reachability check |
| Global `send_lock` serializing focus+type | Kept (harmless; avoids herdr CLI thrash) |

Everything the user sees — multiple dunkers, intervals, custom text, test send,
countdown, add/remove, keybindings — is preserved. Only the transport changed,
and the "Window" column became a "Target" column showing the pane's agent.
