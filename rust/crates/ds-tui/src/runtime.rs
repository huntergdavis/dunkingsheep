//! Production terminal runtime for the Rust TUI.
//!
//! The caller owns connection policy and passes an already-connected Flock client.
//! This module adapts its RPC contract, owns terminal mode for the duration of the
//! event loop, and restores the terminal after normal exits and I/O errors.

use std::cell::RefCell;
use std::io::{self, Stdout};
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use crossterm::event::{KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ds_core::client::{ClientError, FlockClient, Transport};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::prelude::Frame;
use ratatui::Terminal;
use serde_json::Value;

use crate::app;
use crate::model::{
    LineModal, LineModalProvider, LineModalResult, RpcClient, RpcError, TargetModalProvider,
    TargetPicker, TargetPickerResult, TextModal, TextModalProvider, TextModalResult, TuiModel,
};
use crate::terminal_io::{
    CrosstermInput, ModalProviders, TerminalInput, TerminalLoopIo, TuiTerminal,
};

/// RPC adapter for the already-connected core client.
pub struct FlockRpcClient<T: Transport> {
    client: FlockClient<T>,
}

impl<T: Transport> FlockRpcClient<T> {
    pub fn new(client: FlockClient<T>) -> Self {
        Self { client }
    }

    pub fn into_inner(self) -> FlockClient<T> {
        self.client
    }
}

impl<T: Transport> RpcClient for FlockRpcClient<T> {
    fn call(&mut self, command: &str, args: Value) -> Result<Value, RpcError> {
        let args = args.as_object().cloned().ok_or_else(|| {
            RpcError::Refused("TUI RPC arguments must be a JSON object".to_owned())
        })?;
        self.client
            .call(command, args)
            .map_err(|error| match error {
                ClientError::Dunk(error) => RpcError::Refused(error.to_string()),
                other => RpcError::Transport(other.to_string()),
            })
    }
}

type SharedTerminal = Rc<RefCell<Terminal<CrosstermBackend<Stdout>>>>;

struct SharedTerminalDraw(SharedTerminal);

impl TuiTerminal for SharedTerminalDraw {
    fn draw<C: RpcClient>(&mut self, model: &TuiModel<C>, now: f64) -> io::Result<()> {
        let mut terminal = self
            .0
            .try_borrow_mut()
            .map_err(|_| io::Error::other("terminal is already borrowed"))?;
        TuiTerminal::draw(&mut *terminal, model, now)
    }
}

/// Modal adapters share the terminal with the main renderer. Their public model
/// interfaces use `Option` for cancellation; this side channel preserves terminal
/// I/O errors so `TerminalLoopIo` can return them to the runtime.
struct TerminalModals {
    terminal: SharedTerminal,
    input: CrosstermInput,
    io_error: Option<io::Error>,
}

impl TerminalModals {
    fn new(terminal: SharedTerminal) -> Self {
        Self {
            terminal,
            input: CrosstermInput,
            io_error: None,
        }
    }

    fn draw_modal(&mut self, mut render: impl FnMut(&mut Frame<'_>, Rect)) -> io::Result<()> {
        let mut terminal = self
            .terminal
            .try_borrow_mut()
            .map_err(|_| io::Error::other("terminal is already borrowed"))?;
        terminal
            .draw(|frame| render(frame, frame.area()))
            .map(|_| ())
    }

    fn next_key(&mut self) -> Option<KeyEvent> {
        loop {
            match self.input.read_key(Duration::from_millis(100)) {
                Ok(Some(key)) if key.kind != KeyEventKind::Release => return Some(key),
                Ok(_) => {}
                Err(error) => {
                    self.io_error = Some(error);
                    return None;
                }
            }
        }
    }
}

impl LineModalProvider for TerminalModals {
    fn line_modal(&mut self, label: &str, default: &str) -> Option<String> {
        let mut modal = LineModal::new(label, default);
        loop {
            if let Err(error) = self.draw_modal(|frame, area| modal.render(frame, area)) {
                self.io_error = Some(error);
                return None;
            }
            let Some(key) = self.next_key() else {
                return None;
            };
            match modal.handle_key(key) {
                LineModalResult::Pending => {}
                LineModalResult::Cancelled => return None,
                LineModalResult::Submitted(value) => return Some(value),
            }
        }
    }
}

impl TargetModalProvider for TerminalModals {
    fn target_modal(&mut self, panes: &[Value]) -> Option<Value> {
        let mut picker = TargetPicker::new(panes.to_vec());
        loop {
            if let Err(error) = self.draw_modal(|frame, area| picker.render(frame, area)) {
                self.io_error = Some(error);
                return None;
            }
            let Some(key) = self.next_key() else {
                return None;
            };
            match picker.handle_key(key) {
                TargetPickerResult::Pending => {}
                TargetPickerResult::Cancelled => return None,
                TargetPickerResult::Selected(pane) => return Some(pane),
            }
        }
    }
}

impl TextModalProvider for TerminalModals {
    fn text_modal(&mut self, initial_text: &str) -> Option<String> {
        let mut modal = TextModal::new(initial_text);
        loop {
            let mut viewport_height = 1;
            if let Err(error) = self.draw_modal(|frame, area| {
                viewport_height = usize::from(area.height);
                modal.render(frame, area);
            }) {
                self.io_error = Some(error);
                return None;
            }
            let Some(key) = self.next_key() else {
                return None;
            };
            match modal.handle_key(key, viewport_height) {
                TextModalResult::Pending => {}
                TextModalResult::Cancelled => return None,
                TextModalResult::Submitted(value) => return Some(value),
            }
        }
    }
}

impl ModalProviders for TerminalModals {
    fn take_io_error(&mut self) -> Option<io::Error> {
        self.io_error.take()
    }
}

/// Enter the production TUI using a connection created by the caller.
///
/// This function deliberately does not connect to or start a daemon. The caller
/// keeps that policy, and daemon-owned dunks remain running on q/Escape.
pub fn run_terminal<T: Transport>(client: FlockClient<T>) -> io::Result<()> {
    let mut session = CrosstermSession::enter()?;
    let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let terminal = Rc::new(RefCell::new(terminal));
    let mut io = TerminalLoopIo::new(
        SharedTerminalDraw(Rc::clone(&terminal)),
        CrosstermInput,
        TerminalModals::new(terminal),
    );
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or_default();
    let mut model = TuiModel::connected(FlockRpcClient::new(client), now);
    let result = app::run(&mut model, &mut io);
    finish_session(result, &mut session)
}

trait TerminalRestoration {
    fn restore(&mut self) -> io::Result<()>;
}

fn finish_session(
    run_result: io::Result<()>,
    session: &mut impl TerminalRestoration,
) -> io::Result<()> {
    let restore_result = session.restore();
    run_result.and(restore_result)
}

struct CrosstermSession {
    active: bool,
}

impl CrosstermSession {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(Self { active: true })
    }
}

impl TerminalRestoration for CrosstermSession {
    fn restore(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let screen_result = execute!(io::stdout(), LeaveAlternateScreen);
        let raw_result = disable_raw_mode();
        screen_result.and(raw_result)
    }
}

impl Drop for CrosstermSession {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::TuiLoopIo;
    use crate::model::ClientMode;
    use crossterm::event::{KeyCode, KeyModifiers};
    use serde_json::json;
    use std::cell::Cell;

    struct FakeTransport {
        response: String,
        failure: bool,
        requests: Rc<RefCell<Vec<String>>>,
    }

    impl Transport for FakeTransport {
        fn exchange(&self, request: &str, _: Duration) -> io::Result<Option<String>> {
            self.requests.borrow_mut().push(request.to_owned());
            if self.failure {
                Err(io::Error::other("wire unavailable"))
            } else {
                Ok(Some(self.response.clone()))
            }
        }
    }

    fn flock_client(
        response: &str,
        requests: Rc<RefCell<Vec<String>>>,
    ) -> FlockClient<FakeTransport> {
        FlockClient::new(
            FakeTransport {
                response: response.to_owned(),
                failure: false,
                requests,
            },
            Some("%159".to_owned()),
            "test pane".to_owned(),
            Duration::from_millis(10),
            None,
        )
    }

    #[test]
    fn rpc_adapter_preserves_command_args_and_refusal() {
        let requests = Rc::new(RefCell::new(Vec::new()));
        let mut client = FlockRpcClient::new(flock_client(
            r#"{"ok":true,"result":{"id":"d1"}}"#,
            Rc::clone(&requests),
        ));
        let result = client
            .call("update_dunk", json!({"id": "d1", "name": "next"}))
            .unwrap();
        assert_eq!(result, json!({"id": "d1"}));
        let request: Value = serde_json::from_str(&requests.borrow()[0]).unwrap();
        assert_eq!(request["cmd"], "update_dunk");
        assert_eq!(request["args"], json!({"id": "d1", "name": "next"}));

        let requests = Rc::new(RefCell::new(Vec::new()));
        let mut refused = FlockRpcClient::new(flock_client(
            r#"{"ok":false,"error":"not allowed"}"#,
            requests,
        ));
        assert_eq!(
            refused.call("update_dunk", json!({"id": "d1"})),
            Err(RpcError::Refused("not allowed".to_owned()))
        );
    }

    #[test]
    fn rpc_adapter_maps_transport_failures_separately() {
        let requests = Rc::new(RefCell::new(Vec::new()));
        let mut client = FlockRpcClient::new(FlockClient::new(
            FakeTransport {
                response: String::new(),
                failure: true,
                requests,
            },
            Some("%159".to_owned()),
            "test pane".to_owned(),
            Duration::from_millis(10),
            None,
        ));
        assert_eq!(
            client.call("list_dunks", json!({})),
            Err(RpcError::Transport("wire unavailable".to_owned()))
        );
    }

    #[derive(Default)]
    struct FakeSession {
        restores: usize,
    }

    impl TerminalRestoration for FakeSession {
        fn restore(&mut self) -> io::Result<()> {
            self.restores += 1;
            Ok(())
        }
    }

    #[test]
    fn run_terminal_restores_terminal_after_io_error() {
        let mut session = FakeSession::default();
        let result = finish_session(Err(io::Error::other("draw failed")), &mut session);
        assert_eq!(session.restores, 1);
        assert_eq!(result.unwrap_err().to_string(), "draw failed");
    }

    struct CountingClient {
        closed: Rc<Cell<usize>>,
        calls: Rc<RefCell<Vec<String>>>,
    }

    impl RpcClient for CountingClient {
        fn call(&mut self, command: &str, _: Value) -> Result<Value, RpcError> {
            self.calls.borrow_mut().push(command.to_owned());
            match command {
                "list_dunks" => Ok(json!({"dunks": []})),
                "task_list" => Ok(json!({"tasks": []})),
                "shutdown" => Ok(json!({})),
                _ => Err(RpcError::Refused("unexpected command".to_owned())),
            }
        }

        fn close_local(&mut self) {
            self.closed.set(self.closed.get() + 1);
        }
    }

    struct QuitIo;

    impl TuiLoopIo<CountingClient> for QuitIo {
        type Error = &'static str;

        fn now(&self) -> f64 {
            1.0
        }

        fn draw(&mut self, _: &TuiModel<CountingClient>, _: f64) -> Result<(), Self::Error> {
            Ok(())
        }

        fn read_key(&mut self, _: Duration) -> Result<Option<KeyEvent>, Self::Error> {
            Ok(Some(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)))
        }

        fn handle_key(
            &mut self,
            _: &mut TuiModel<CountingClient>,
            _: KeyEvent,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[test]
    fn quitting_closes_local_fallback_only() {
        let closed = Rc::new(Cell::new(0));
        let calls = Rc::new(RefCell::new(Vec::new()));
        let client = CountingClient {
            closed: Rc::clone(&closed),
            calls: Rc::clone(&calls),
        };
        let mut daemon_model = TuiModel::connected(client, 0.0);
        assert_eq!(daemon_model.mode, ClientMode::Daemon);
        app::run(&mut daemon_model, &mut QuitIo).unwrap();
        assert_eq!(closed.get(), 0);
        assert!(!calls.borrow().iter().any(|command| command == "shutdown"));

        let local_closed = Rc::new(Cell::new(0));
        let local_client = CountingClient {
            closed: Rc::clone(&local_closed),
            calls: Rc::new(RefCell::new(Vec::new())),
        };
        let mut local_model = TuiModel::local_fallback(local_client, 0.0, "offline");
        assert_eq!(local_model.mode, ClientMode::Local);
        app::run(&mut local_model, &mut QuitIo).unwrap();
        assert_eq!(local_closed.get(), 1);
    }
}
