//! Required raw inputs for report collection.
//!
//! Production owns the adapter in the daemon integration task. This boundary exposes store
//! measurements, cache location and the report transport; it cannot hand the collector a
//! prebuilt section or silently substitute an unavailable value.

use std::path::PathBuf;

use serde_json::Value;

use super::ReportPaths;

/// The concrete measurements and I/O used to build and publish one report.
pub trait ReportSources {
    /// Wall-clock time for deterministic age and report timestamps.
    fn now_s(&self) -> f64;

    /// Output directory and home directory used by report file operations.
    fn report_paths(&self) -> ReportPaths;

    /// Configuration directory containing `usage.json` and `watchdog_state.json`.
    fn config_dir(&self) -> PathBuf;

    /// Raw `Tasks.metrics(window_s=...)` result.
    fn task_metrics(&self, window_s: f64) -> Result<Value, String>;

    /// Raw live pane records from the terminal backend.
    fn list_panes(&self) -> Result<Vec<Value>, String>;

    /// Raw open task rows with their leases, used to distinguish working from idle lanes.
    fn list_tasks(&self, limit: usize) -> Result<Vec<Value>, String>;

    /// Raw task-list response filtered to needs-owner questions.
    fn needs_owner_tasks(&self, limit: usize) -> Result<Value, String>;

    /// Raw registered lane map from the roles store.
    fn lane_registry(&self) -> Result<Value, String>;

    /// Raw review-list response.
    fn review_list(&self) -> Result<Value, String>;

    /// Raw recorded Poolrad progress.
    fn poolrad_progress(&self) -> Result<Value, String>;

    /// Post a report through the existing owner report transport.
    fn post_report(&self, report: &Value, text: &str) -> Result<Value, String>;
}
