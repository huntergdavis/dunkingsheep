//! Testable TUI event loop, independent of the concrete terminal and RPC adapters.

use std::time::Duration;

use crate::model::{RpcClient, TuiModel, View};

/// The Python TUI uses a curses timeout of 200 ms between redraws.
pub const REFRESH_INTERVAL: Duration = Duration::from_millis(200);

/// Terminal operations needed by the event loop.
pub trait TuiLoopIo<C: RpcClient> {
    type Error;

    fn now(&self) -> f64;
    fn draw(&mut self, model: &TuiModel<C>, now: f64) -> Result<(), Self::Error>;
    fn read_key(&mut self, timeout: Duration) -> Result<Option<char>, Self::Error>;

    /// Forward keys not consumed by the event loop to the command adapter.
    fn handle_key(&mut self, _model: &mut TuiModel<C>, _key: char) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Re-establish the daemon connection after the model's retry threshold.
    fn reconnect(&mut self, _model: &mut TuiModel<C>, _now: f64) {}
}

/// Draw, poll input, refresh RPC-backed state, and clean up on loop exit.
///
/// `q` exits the loop and `v` switches between the dunk and task views. All other
/// input is forwarded to the command adapter. Cleanup also runs when terminal I/O
/// returns an error.
pub fn run<C, I>(model: &mut TuiModel<C>, io: &mut I) -> Result<(), I::Error>
where
    C: RpcClient,
    I: TuiLoopIo<C>,
{
    let result = run_loop(model, io);
    model.shutdown();
    result
}

fn run_loop<C, I>(model: &mut TuiModel<C>, io: &mut I) -> Result<(), I::Error>
where
    C: RpcClient,
    I: TuiLoopIo<C>,
{
    loop {
        let now = io.now();
        io.draw(model, now)?;
        let key = io.read_key(REFRESH_INTERVAL)?;
        let quit = key == Some('q');
        if key == Some('v') {
            model.view = match model.view {
                View::Dunks => View::Tasks,
                View::Tasks => View::Dunks,
            };
        }
        if let Some(key) = key.filter(|key| !matches!(key, 'q' | 'v')) {
            io.handle_key(model, key)?;
        }

        let now = io.now();
        if model.refresh(now) {
            io.reconnect(model, now);
        }
        if quit {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use serde_json::Value;

    use super::*;
    use crate::model::{ClientMode, RpcError};

    #[derive(Default)]
    struct FakeClient {
        close_count: Rc<Cell<usize>>,
        calls: usize,
        offline: bool,
    }

    impl RpcClient for FakeClient {
        fn call(&mut self, command: &str, _: Value) -> Result<Value, RpcError> {
            self.calls += 1;
            if self.offline {
                return Err(RpcError::Transport("offline".to_string()));
            }
            match command {
                "list_dunks" => Ok(serde_json::json!({"dunks":[]})),
                "task_list" => Ok(serde_json::json!({"tasks":[]})),
                other => panic!("unexpected command {other}"),
            }
        }

        fn close_local(&mut self) {
            self.close_count.set(self.close_count.get() + 1);
        }
    }

    struct FakeIo {
        keys: VecDeque<Option<char>>,
        now: f64,
        draws: usize,
        timeouts: Vec<Duration>,
        reconnects: usize,
        handled_keys: Vec<char>,
    }

    impl TuiLoopIo<FakeClient> for FakeIo {
        type Error = &'static str;

        fn now(&self) -> f64 {
            self.now
        }

        fn draw(&mut self, _: &TuiModel<FakeClient>, _: f64) -> Result<(), Self::Error> {
            self.draws += 1;
            Ok(())
        }

        fn read_key(&mut self, timeout: Duration) -> Result<Option<char>, Self::Error> {
            self.timeouts.push(timeout);
            Ok(self.keys.pop_front().unwrap_or(Some('q')))
        }

        fn handle_key(
            &mut self,
            _: &mut TuiModel<FakeClient>,
            key: char,
        ) -> Result<(), Self::Error> {
            self.handled_keys.push(key);
            Ok(())
        }

        fn reconnect(&mut self, _: &mut TuiModel<FakeClient>, _: f64) {
            self.reconnects += 1;
        }
    }

    fn io(keys: impl IntoIterator<Item = Option<char>>) -> FakeIo {
        FakeIo {
            keys: keys.into_iter().collect(),
            now: 10.0,
            draws: 0,
            timeouts: Vec::new(),
            reconnects: 0,
            handled_keys: Vec::new(),
        }
    }

    #[test]
    fn run_draws_polls_refreshes_toggles_view_and_shuts_down_local_client() {
        let client = FakeClient::default();
        let close_count = client.close_count.clone();
        let mut model = TuiModel::local_fallback(client, 0.0, "socket missing");
        let mut io = io([None, Some('v'), Some('q')]);

        run(&mut model, &mut io).unwrap();

        assert_eq!(io.draws, 3);
        assert_eq!(io.timeouts, vec![REFRESH_INTERVAL; 3]);
        assert_eq!(model.view, View::Tasks);
        assert_eq!(model.client.calls, 5); // dunk view once, then dunk + task views
        assert_eq!(close_count.get(), 1);
    }

    #[test]
    fn run_forwards_unhandled_keys_to_io_handler() {
        let mut model = TuiModel::connected(FakeClient::default(), 0.0);
        let mut io = io([Some('a'), Some('v'), Some('q')]);

        run(&mut model, &mut io).unwrap();

        assert_eq!(io.handled_keys, vec!['a']);
        assert_eq!(model.view, View::Tasks);
    }

    #[test]
    fn run_leaves_daemon_and_its_dunks_running_on_quit() {
        let client = FakeClient::default();
        let close_count = client.close_count.clone();
        let mut model = TuiModel::connected(client, 0.0);
        let mut io = io([Some('q')]);

        run(&mut model, &mut io).unwrap();

        assert_eq!(model.mode, ClientMode::Daemon);
        assert_eq!(model.client.calls, 1);
        assert_eq!(close_count.get(), 0);
    }

    #[test]
    fn run_requests_reconnect_after_transport_threshold() {
        let mut model = TuiModel::connected(
            FakeClient {
                offline: true,
                ..FakeClient::default()
            },
            0.0,
        );
        let mut io = io([Some('q')]);
        io.now = 3.0;

        run(&mut model, &mut io).unwrap();

        assert_eq!(io.reconnects, 1);
        assert_eq!(model.message, "Daemon error: offline");
    }

    #[test]
    fn run_closes_local_client_when_terminal_io_fails() {
        struct FailingIo;
        impl TuiLoopIo<FakeClient> for FailingIo {
            type Error = &'static str;
            fn now(&self) -> f64 {
                0.0
            }
            fn draw(&mut self, _: &TuiModel<FakeClient>, _: f64) -> Result<(), Self::Error> {
                Err("terminal failed")
            }
            fn read_key(&mut self, _: Duration) -> Result<Option<char>, Self::Error> {
                Ok(None)
            }
        }

        let client = FakeClient::default();
        let close_count = client.close_count.clone();
        let mut model = TuiModel::local_fallback(client, 0.0, "socket missing");

        assert_eq!(run(&mut model, &mut FailingIo), Err("terminal failed"));
        assert_eq!(close_count.get(), 1);
    }
}
