//! tmux CONTROL MODE: the line protocol behind the hot path (T23).
//!
//! Today every pane read shells out `tmux list-panes -a -F <14 fields>`: one
//! subprocess per request, ~10-40 ms idle and 3-9 s under load (measured 2026-10-29,
//! t535). T23 keeps ONE `tmux -C` client per server and reads pane state from memory,
//! so `get_pane` becomes a map lookup.
//!
//! The design (RUST_PORT.md 5.1) in three parts, and this file is the two testable
//! ones:
//!
//! * [`Event`] + [`parse_line`] - the protocol parser. Control mode answers a command
//!   with `%begin <time> <num>`, then the command's output lines, then `%end <time>
//!   <num>`, and pushes `%output`, `%layout-change`, `%window-add`, `%session-changed`,
//!   `%exit`. Replies are matched BY THE NUM, out of order if need be.
//! * [`PaneTable`] - the in-memory pane/window/session state, refreshed from
//!   `list-panes -a` and on every window/session/layout event.
//! * [`Client`] - the state machine that ties them together, driven by lines from any
//!   source (a pty in production, a recorded transcript in tests). Nothing here spawns
//!   a process: the transport is deliberately a parameter, which is what makes a
//!   reconnect testable instead of hopeful.

use std::collections::BTreeMap;

use super::tmux::PANE_FORMAT;
use super::{Pane, Tab, Workspace};

/// One parsed control-mode event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// `%begin <time> <num>` - a command reply starts.
    Begin {
        number: u64,
    },
    /// `%end <time> <num> [flags]` - the reply ended. `flags` non-empty means the
    /// command failed (tmux puts the error text in the body).
    End {
        number: u64,
        flags: String,
    },
    /// `%error <time> <num>` - the reply failed.
    Error {
        number: u64,
    },
    /// `%output %<pane> <escaped text>` - pane output; `text` still escaped.
    Output {
        pane_id: String,
        text: String,
    },
    /// `%layout-change @<window> <layout> <visible_layout> <flags>`
    LayoutChange {
        window_id: String,
    },
    /// `%window-add @<window>` / `%window-close @<window>`
    WindowAdd {
        window_id: String,
    },
    WindowClose {
        window_id: String,
    },
    /// `%session-changed $<session> <name>`
    SessionChanged {
        session_id: String,
        name: String,
    },
    /// `%exit [reason]` - the client's server went away.
    Exit {
        reason: String,
    },
    /// Anything else, verbatim, so a new tmux event is visible rather than dropped.
    Unknown(String),
}

/// Parse one line of control-mode output.
pub fn parse_line(line: &str) -> Event {
    let trimmed = line.strip_suffix('\r').unwrap_or(line);
    if let Some(rest) = trimmed.strip_prefix("%begin ") {
        let mut parts = rest.split_whitespace();
        let _time = parts.next();
        let number = parts
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        return Event::Begin { number };
    }
    if let Some(rest) = trimmed.strip_prefix("%end ") {
        let mut parts = rest.split_whitespace();
        let _time = parts.next();
        let number = parts
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        // tmux's `%end <time> <num>` carries no flags: a failed command is announced
        // by `%error <time> <num>` BEFORE this line, with the error text as the body.
        let flags = parts.collect::<Vec<_>>().join(" ");
        return Event::End { number, flags };
    }
    if let Some(rest) = trimmed.strip_prefix("%error ") {
        let mut parts = rest.split_whitespace();
        let _time = parts.next();
        let number = parts
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        return Event::Error { number };
    }
    if let Some(rest) = trimmed.strip_prefix("%output ") {
        let mut parts = rest.splitn(2, ' ');
        let pane_id = parts.next().unwrap_or("").to_string();
        let text = parts.next().unwrap_or("").to_string();
        return Event::Output { pane_id, text };
    }
    if let Some(rest) = trimmed.strip_prefix("%layout-change ") {
        let window_id = rest.split_whitespace().next().unwrap_or("").to_string();
        return Event::LayoutChange { window_id };
    }
    if let Some(rest) = trimmed.strip_prefix("%window-add ") {
        return Event::WindowAdd {
            window_id: rest.trim().to_string(),
        };
    }
    if let Some(rest) = trimmed.strip_prefix("%window-close ") {
        return Event::WindowClose {
            window_id: rest.trim().to_string(),
        };
    }
    if let Some(rest) = trimmed.strip_prefix("%session-changed ") {
        let mut parts = rest.splitn(2, ' ');
        let session_id = parts.next().unwrap_or("").to_string();
        let name = parts.next().unwrap_or("").trim().to_string();
        return Event::SessionChanged { session_id, name };
    }
    if trimmed == "%exit" || trimmed.starts_with("%exit ") {
        return Event::Exit {
            reason: trimmed.strip_prefix("%exit").unwrap().trim().to_string(),
        };
    }
    Event::Unknown(trimmed.to_string())
}

/// One command in flight: the number tmux will answer with, and the lines so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub number: u64,
    pub command: String,
    pub lines: Vec<String>,
    pub failed: bool,
}

/// The reply matcher: many commands may be in flight, and tmux answers by number.
#[derive(Debug, Default)]
pub struct Replies {
    pending: BTreeMap<u64, Pending>,
    next_number: u64,
}

/// A finished reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub number: u64,
    pub command: String,
    pub lines: Vec<String>,
    pub failed: bool,
}

impl Reply {
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }
}

impl Replies {
    /// Register a command and return the number tmux will answer with.
    pub fn start(&mut self, command: &str) -> u64 {
        self.next_number += 1;
        let number = self.next_number;
        self.pending.insert(
            number,
            Pending {
                number,
                command: command.to_string(),
                lines: Vec::new(),
                failed: false,
            },
        );
        number
    }

    /// Feed one event. Returns a [`Reply`] when this event completed one.
    pub fn feed(&mut self, event: &Event) -> Option<Reply> {
        match event {
            Event::Begin { number } => {
                if let Some(pending) = self.pending.get_mut(number) {
                    pending.lines.clear();
                }
                None
            }
            Event::Error { number } => {
                // `%error` announces the failure and the body follows; `%end` still
                // closes the reply, so the pending entry must stay.
                if let Some(pending) = self.pending.get_mut(number) {
                    pending.failed = true;
                }
                None
            }
            Event::End { number, flags } => {
                let pending = self.pending.remove(number)?;
                let _ = flags;
                Some(Reply {
                    number: pending.number,
                    command: pending.command,
                    lines: pending.lines,
                    failed: pending.failed,
                })
            }
            _ => {
                // A body line belongs to whichever reply is open; tmux does not nest,
                // so the lowest open number is the right one and an out-of-order
                // %end still lands on its own reply.
                if let Some(open) = self.pending.values_mut().next() {
                    if let Event::Unknown(text) = event {
                        open.lines.push(text.clone());
                    }
                }
                None
            }
        }
    }

    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }

    /// Drop every in-flight command: called on reconnect, where the numbers a dead
    /// server would have answered no longer mean anything.
    pub fn reset(&mut self) -> usize {
        let dropped = self.pending.len();
        self.pending.clear();
        dropped
    }
}

/// The in-memory pane/window/session state (the Python's PaneTable equivalent).
#[derive(Debug, Default, Clone)]
pub struct PaneTable {
    pub panes: BTreeMap<String, Pane>,
    pub tabs: BTreeMap<String, Tab>,
    pub workspaces: BTreeMap<String, Workspace>,
    /// Set when an event says the world moved and a `list-panes -a` refresh is owed.
    pub stale: bool,
}

impl PaneTable {
    /// Replace the pane set from a `list-panes -a -F PANE_FORMAT` reply.
    pub fn load_panes(&mut self, output: &str) -> usize {
        let parsed = super::parse::parse_panes(output, &|_| None);
        self.panes = parsed
            .into_iter()
            .map(|pane| (pane.pane_id.clone(), pane))
            .collect();
        self.stale = false;
        self.panes.len()
    }

    /// `get_pane` is a map lookup: this is the whole point of the task (RUST_PORT 5.1).
    pub fn get_pane(&self, pane_id: &str) -> Option<&Pane> {
        self.panes.get(pane_id)
    }

    /// Apply an event that changes the world. Returns true when a refresh is owed.
    pub fn apply(&mut self, event: &Event) -> bool {
        match event {
            Event::LayoutChange { .. }
            | Event::WindowAdd { .. }
            | Event::WindowClose { .. }
            | Event::SessionChanged { .. } => {
                self.stale = true;
                true
            }
            _ => false,
        }
    }

    pub fn list_panes(&self) -> Vec<Pane> {
        self.panes.values().cloned().collect()
    }
}

/// The control-mode client state machine.
///
/// The transport is NOT here: `handle_line` takes one line and answers with the
/// replies it completed. A production driver reads a pty and feeds lines; a test
/// feeds a recorded transcript. That split is what makes reconnect behaviour testable.
#[derive(Debug, Default)]
pub struct Client {
    pub replies: Replies,
    pub table: PaneTable,
    pub connected: bool,
    /// Events the client did not act on yet, in order, for a test or a log.
    pub observed: Vec<Event>,
}

impl Client {
    pub fn new() -> Self {
        Client {
            connected: true,
            ..Client::default()
        }
    }

    /// Send a command: returns the number the reply will carry.
    pub fn send(&mut self, command: &str) -> u64 {
        self.replies.start(command)
    }

    /// The argv a caller writes to the pty, newline-terminated.
    pub fn line_for(command: &str) -> String {
        format!("{command}\n")
    }

    /// Feed one line from the transport.
    pub fn handle_line(&mut self, line: &str) -> Option<Reply> {
        let event = parse_line(line);
        let was_exit = matches!(event, Event::Exit { .. });
        self.table.apply(&event);
        let reply = self.replies.feed(&event);
        if was_exit {
            // The server went away: the world is unknown, not empty, and any command
            // still in flight will never be answered.
            self.connected = false;
            self.table.stale = true;
            self.replies.reset();
        }
        self.observed.push(event);
        reply
    }

    /// Reconnect: a fresh server means the numbers start over and the table is stale.
    pub fn on_reconnect(&mut self) {
        self.connected = true;
        self.table.stale = true;
        self.replies.reset();
    }

    /// The command a refresh sends (the Python's `list-panes -a -F PANE_FORMAT`).
    pub fn refresh_command() -> String {
        format!("list-panes -a -F {PANE_FORMAT}")
    }
}
