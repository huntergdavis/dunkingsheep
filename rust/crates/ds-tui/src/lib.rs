//! Dunking Sheep's terminal UI (T61). Ported so far: `text_editor.py` and the row
//! formatting helpers of `dunking_sheep_tui.py`. Dunk-list and task-list rendering
//! use ratatui, with a testable event-loop adapter.
pub mod app;
pub mod model;
pub mod rows;
pub mod text_editor;
pub mod view;
