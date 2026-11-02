//! Concrete terminal input/rendering adapters for the generic TUI event loop.
//!
//! Raw mode and alternate-screen lifecycle are owned by the runtime entry point,
//! so this module can be exercised with injected terminal, input, and modal fakes.

use std::io;
use std::time::{Duration, SystemTime};

use crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use ratatui::backend::Backend;
use ratatui::Terminal;

use crate::app::TuiLoopIo;
use crate::model::{
    KeyDispatch, LineModalProvider, RpcClient, TargetModalProvider, TextModalProvider, TuiModel,
    View,
};

/// Rendering surface used by [`TerminalLoopIo`].
pub trait TuiTerminal {
    fn draw<C: RpcClient>(&mut self, model: &TuiModel<C>, now: f64) -> io::Result<()>;
}

impl<B: Backend<Error = io::Error>> TuiTerminal for Terminal<B> {
    fn draw<C: RpcClient>(&mut self, model: &TuiModel<C>, now: f64) -> io::Result<()> {
        self.draw(|frame| match model.view {
            View::Dunks => crate::view::draw_dunks(frame, model, now),
            View::Tasks => crate::view::draw_tasks(frame, model, now),
        })
        .map(|_| ())
    }
}

/// Injectable source of key events. Implementations return the complete crossterm
/// event so arrows, modifiers, and event kinds survive the adapter.
pub trait TerminalInput {
    fn read_key(&mut self, timeout: Duration) -> io::Result<Option<KeyEvent>>;
}

/// Production input adapter; non-key terminal events are ignored by the loop.
#[derive(Debug, Default, Clone, Copy)]
pub struct CrosstermInput;

impl TerminalInput for CrosstermInput {
    fn read_key(&mut self, timeout: Duration) -> io::Result<Option<KeyEvent>> {
        if !event::poll(timeout)? {
            return Ok(None);
        }
        match event::read()? {
            Event::Key(key) => Ok(Some(key)),
            _ => Ok(None),
        }
    }
}

/// The modal provider supplied by the runtime adapter must handle all existing
/// prompts without teaching the model about a concrete terminal.
pub trait ModalProviders: LineModalProvider + TargetModalProvider + TextModalProvider {}

impl<T> ModalProviders for T where T: LineModalProvider + TargetModalProvider + TextModalProvider {}

/// Ratatui drawing, crossterm key input, and existing model action dispatch.
pub struct TerminalLoopIo<T, I, M> {
    terminal: T,
    input: I,
    modals: M,
    clock: fn() -> f64,
}

impl<T, I, M> TerminalLoopIo<T, I, M> {
    pub fn new(terminal: T, input: I, modals: M) -> Self {
        Self {
            terminal,
            input,
            modals,
            clock: unix_now,
        }
    }

    /// Inject a deterministic clock for focused tests.
    pub fn with_clock(mut self, clock: fn() -> f64) -> Self {
        self.clock = clock;
        self
    }

    pub fn into_parts(self) -> (T, I, M) {
        (self.terminal, self.input, self.modals)
    }
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or_default()
}

impl<C, T, I, M> TuiLoopIo<C> for TerminalLoopIo<T, I, M>
where
    C: RpcClient,
    T: TuiTerminal,
    I: TerminalInput,
    M: ModalProviders,
{
    type Error = io::Error;

    fn now(&self) -> f64 {
        (self.clock)()
    }

    fn draw(&mut self, model: &TuiModel<C>, now: f64) -> Result<(), Self::Error> {
        self.terminal.draw(model, now)
    }

    fn read_key(&mut self, timeout: Duration) -> Result<Option<KeyEvent>, Self::Error> {
        self.input.read_key(timeout)
    }

    fn handle_key(&mut self, model: &mut TuiModel<C>, key: KeyEvent) -> Result<(), Self::Error> {
        if key.kind == KeyEventKind::Release {
            return Ok(());
        }
        let now = (self.clock)();
        match model.handle_key(key, &mut self.modals, now) {
            KeyDispatch::Handled
            | KeyDispatch::Ignored
            | KeyDispatch::Quit
            | KeyDispatch::QuitAndShutdown => {}
            KeyDispatch::TaskView(task_key) => {
                model.handle_task_key(task_key, &mut self.modals, now);
            }
            KeyDispatch::PickTarget => {
                model.pick_target(&mut self.modals, now);
            }
            KeyDispatch::EditInterval => {
                model.edit_interval(&mut self.modals, now);
            }
            KeyDispatch::EditText => {
                model.edit_text(&mut self.modals, now);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::{json, Value};
    use std::collections::VecDeque;

    use crate::model::{RpcError, View};

    #[derive(Default)]
    struct FakeClient {
        calls: Vec<(String, Value)>,
    }

    impl RpcClient for FakeClient {
        fn call(&mut self, command: &str, args: Value) -> Result<Value, RpcError> {
            self.calls.push((command.to_owned(), args));
            match command {
                "list_dunks" => Ok(json!({"dunks": [{"id": "d1", "name": "old"}]})),
                "task_list" => Ok(json!({"tasks": []})),
                "update_dunk" => Ok(json!({})),
                other => Err(RpcError::Refused(format!("unexpected command {other}"))),
            }
        }
    }

    struct FakeInput {
        events: VecDeque<Option<KeyEvent>>,
    }

    impl TerminalInput for FakeInput {
        fn read_key(&mut self, _: Duration) -> io::Result<Option<KeyEvent>> {
            Ok(self.events.pop_front().unwrap_or(None))
        }
    }

    #[derive(Default)]
    struct FakeModals {
        line_result: Option<String>,
        line_calls: Vec<(String, String)>,
        target_calls: usize,
        text_calls: usize,
    }

    impl LineModalProvider for FakeModals {
        fn line_modal(&mut self, label: &str, default: &str) -> Option<String> {
            self.line_calls.push((label.to_owned(), default.to_owned()));
            self.line_result.clone()
        }
    }

    impl TargetModalProvider for FakeModals {
        fn target_modal(&mut self, _: &[Value]) -> Option<Value> {
            self.target_calls += 1;
            None
        }
    }

    impl TextModalProvider for FakeModals {
        fn text_modal(&mut self, _: &str) -> Option<String> {
            self.text_calls += 1;
            None
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn model(client: FakeClient) -> TuiModel<FakeClient> {
        TuiModel::connected(client, 0.0)
    }

    #[test]
    fn draw_selects_the_active_view_renderer() {
        use ratatui::backend::TestBackend;

        for (view, expected_title) in [
            (View::Dunks, "Dunking Sheep TUI"),
            (View::Tasks, "Dunking Sheep TUI - tasks"),
        ] {
            let terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
            let mut io = TerminalLoopIo::new(
                terminal,
                FakeInput {
                    events: VecDeque::new(),
                },
                FakeModals::default(),
            )
            .with_clock(|| 7.0);
            let mut model = model(FakeClient::default());
            model.view = view;

            TuiLoopIo::draw(&mut io, &model, 7.0).unwrap();

            let (terminal, _, _) = io.into_parts();
            let text = (0..30)
                .map(|y| {
                    (0..100)
                        .map(|x| terminal.backend().buffer().cell((x, y)).unwrap().symbol())
                        .collect::<String>()
                })
                .collect::<String>();
            assert!(text.contains(expected_title), "view {view:?}: {text}");
        }
    }

    #[test]
    fn read_key_preserves_arrows_modifiers_and_escape() {
        let left = key(KeyCode::Left, KeyModifiers::SHIFT);
        let escape = key(KeyCode::Esc, KeyModifiers::NONE);
        let mut io = TerminalLoopIo::new(
            Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap(),
            FakeInput {
                events: [Some(left), Some(escape)].into(),
            },
            FakeModals::default(),
        );

        assert_eq!(
            TuiLoopIo::<FakeClient>::read_key(&mut io, Duration::ZERO).unwrap(),
            Some(left)
        );
        assert_eq!(
            TuiLoopIo::<FakeClient>::read_key(&mut io, Duration::ZERO).unwrap(),
            Some(escape)
        );
    }

    #[test]
    fn handle_key_uses_existing_model_actions_and_modal_results() {
        let terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        let modals = FakeModals {
            line_result: Some("new name".to_owned()),
            ..FakeModals::default()
        };
        let mut io = TerminalLoopIo::new(
            terminal,
            FakeInput {
                events: VecDeque::new(),
            },
            modals,
        );
        let mut model = model(FakeClient::default());
        model.refresh(0.0);

        TuiLoopIo::handle_key(
            &mut io,
            &mut model,
            key(KeyCode::Char('n'), KeyModifiers::NONE),
        )
        .unwrap();

        let (_, _, modals) = io.into_parts();
        assert_eq!(
            modals.line_calls,
            [("Dunker name".to_owned(), "old".to_owned())]
        );
        assert!(model.client.calls.iter().any(|(command, args)| {
            command == "update_dunk" && args == &json!({"id": "d1", "name": "new name"})
        }));
    }
}
