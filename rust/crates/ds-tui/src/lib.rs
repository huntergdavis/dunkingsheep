//! Dunking Sheep's terminal UI (T61). Ported so far: `text_editor.py` and the row
//! formatting helpers of `dunking_sheep_tui.py`. The app model (`model.rs`) is split
//! out of this PR for separate review; the ratatui app is NOT ported yet.
pub mod model;
pub mod rows;
pub mod text_editor;
