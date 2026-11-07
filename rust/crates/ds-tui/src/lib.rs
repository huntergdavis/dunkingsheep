//! Dunking Sheep's terminal UI (T61): a ratatui port of `dunking_sheep_tui.py` and
//! `text_editor.py`. The dunk and task list screens, the line, text and target modals,
//! and the event loop are ported; `docs/rust/DS_TUI_PARITY.md` lists each Python
//! function against its Rust counterpart and says what is not ported.
pub mod app;
pub mod model;
pub mod rows;
pub mod runtime;
pub mod terminal_io;
pub mod text_editor;
pub mod view;

#[cfg(test)]
mod main_screen_oracle_tests;

#[cfg(test)]
mod target_picker_oracle_tests;

#[cfg(test)]
mod modal_text_line_oracle_tests;
