//! Production backends for the pane activity, Poolrad dashboard, and desk dash routes.
//!
//! These implement the route seams of `web/app.py`: `panes_activity`, `activity`,
//! `poolrad_dashboard_progress`, `refresh_poolrad_dashboard`, `poolrad_dashboard_page`,
//! `poolrad_dashboard_report`, and the built-bundle half of `dash_route`. Every source is an
//! explicit input (the captured daemon client, configured paths, and a clock). A source that
//! cannot be read stays unavailable; it never becomes a zero or a success.
//!
//! Scope (t3213 owner policy update):
//! - P0 core: `DashBackend::index_html`, the static desk bundle, and
//!   `ActivityPoolradBackend::{panes_activity, activity}`. That covers the 4 s pane TTL,
//!   failure caching, per-row read errors, channel resolution, and daemon errors.
//! - P1 optional, tracked as t3239: `ActivityPoolradBackend::poolrad_progress`,
//!   `PoolradDashboardBackend` (page, reports, refresh), and the DS-internal renderer. They
//!   stay honest: refresh is unavailable, and progress without a published checkpoint is
//!   unavailable. Nothing is fabricated.
//! - Core deviations to categorize: the bytes-only seam drops `FileResponse` stat semantics on
//!   the desk page as well, and pane reads are serial.
//!
//! Gaps, reported rather than papered over:
//! - `refresh()` needs `ops_metrics.published_accounting()` (a `gh api` network read) and
//!   `dashboard.py render` (Python). Neither runs in Rust, so refresh reports unavailable and
//!   writes nothing.
//! - The progress view without a published `accounting_snapshots` row needs
//!   `dashboard.py progress()` (Python). It reports unavailable (503).
//! - The traits are synchronous. Daemon RPCs (up to 2 s), `git show` (5 s), and SQLite reads
//!   block the calling thread, so startup integration must run them off async workers.
//! - The route traits return bytes only. Python's `FileResponse` also sends stat-derived
//!   headers and may answer conditional requests (and, depending on the pinned Starlette,
//!   range requests). This seam cannot carry file metadata, so those semantics are not
//!   represented. Byte and stat parity needs the route trait signatures to change, which is
//!   out of scope for this batch.
//! - Pane reads run one at a time behind the single daemon client, where Python fans them
//!   out over four workers. With N slow panes a refresh can block for up to N timeouts of 2 s
//!   each, more if the client retries a timed-out read. Channel activity waits on the same
//!   client until the refresh ends. Startup integration needs a blocking pool and a client
//!   pool to restore the fan-out.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use ds_core::client::{ClientError, FlockClient, Transport};
use rusqlite::OpenFlags;
use serde_json::{json, Map, Value};

use crate::roster::{build_roster, discovered_id, is_lane, match_pane, presence};
use crate::routes::activity_poolrad::{
    ActivityPoolradBackend as ActivityPoolradRoutes, ActivityPoolradError,
};
use crate::routes::dash::DashBackend as DashRoutes;
use crate::routes::poolrad_dashboard::PoolradDashboardBackend as PoolradDashboardRoutes;

/// `web/app.py` TTLs, in whole seconds as the Python JSON encodes them.
const PANES_TTL_SECONDS: u64 = 4;
const PROGRESS_TTL_SECONDS: u64 = 60;
const CAPABILITY_TTL_SECONDS: f64 = 20.0;
/// Pane refresh uses a 2 s client timeout and reads 40 lines per pane.
const PANES_RPC_TIMEOUT: Duration = Duration::from_secs(2);
const PANES_READ_LINES: u64 = 40;
/// The per-pane response bounds: the last 40 lines, at most 32000 characters.
const PANE_TEXT_LINES: usize = 40;
const PANE_TEXT_CHARS: usize = 32_000;
/// `GET /api/activity` reads the daemon's default 80 lines and returns them untrimmed.
const CHANNEL_READ_LINES: u64 = 80;
const GIT_TIMEOUT: Duration = Duration::from_secs(5);

const PROJECT: &str = "poolrad";
const DASHBOARD_SCRIPT: &str = "dashboard.py";
const DASHBOARD_PAGE: &str = "dashboard.html";
const REPLACED_BODIES: &str = "docs/re/replaced_bodies.json";
const ACCOUNTING_SOURCE: &str = "docs/re/function_accounting_checkpoint.json";
const REPORTS: [&str; 2] = [
    "SLOT_A_FIRST_EGA_ROW64.md",
    "SLOT_A_ROW6_COL38_PRINTER_PAGE.md",
];
const REFRESH_UNAVAILABLE: &str =
    "refresh needs dashboard.py render and the published-main network read; neither is ported";

/// Time inputs. Monotonic seconds drive TTL caches; Unix seconds drive emitted timestamps.
pub trait Clock: Send + Sync + 'static {
    /// Seconds on a monotonic timeline (Python `time.monotonic`).
    fn monotonic(&self) -> f64;
    /// Seconds since the Unix epoch (Python `time.time`).
    fn unix_seconds(&self) -> f64;
}

/// The process clock. Monotonic time starts when the clock is created.
#[derive(Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn monotonic(&self) -> f64 {
        self.origin.elapsed().as_secs_f64()
    }

    fn unix_seconds(&self) -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |elapsed| elapsed.as_secs_f64())
    }
}

/// Filesystem inputs, captured once instead of read from the environment per request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DashboardPaths {
    /// `ROOT/ui/dist`: the built desk bundle served by the `/dash` routes.
    pub ui_dist: PathBuf,
    /// `bridges/integrations/poolrad-dashboard`: `dashboard.py`, `dashboard.html`, `state/reports/`.
    pub dashboard_dir: PathBuf,
    /// The Poolrad checkout the progress view reads (`POOLRAD_DASHBOARD_REPO`, else `POOLRAD_REPO`).
    pub dashboard_repo: PathBuf,
    /// The operations database holding `accounting_snapshots` (`VP_OPS_DB`).
    pub operations_db: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RpcFailure {
    /// The daemon socket could not be reached (Python: `_flock()` raised, so 503).
    Unavailable,
    /// The call reached the daemon and failed or returned something unusable.
    Failed,
}

#[derive(Default)]
struct TimedCache {
    expires: f64,
    data: Option<Value>,
}

/// Pane activity, channel activity, and reviewed progress, backed by the captured daemon client.
pub struct ActivityPoolradBackend<T: Transport + Send + 'static> {
    client: Mutex<FlockClient<T>>,
    default_timeout: Duration,
    channels: Vec<Value>,
    paths: DashboardPaths,
    clock: Arc<dyn Clock>,
    panes: Mutex<TimedCache>,
    progress: Mutex<TimedCache>,
    capabilities: Mutex<HashMap<String, (f64, Map<String, Value>)>>,
}

impl<T: Transport + Send + 'static> ActivityPoolradBackend<T> {
    /// `client` is the explicitly captured daemon client, and `channels` is the validated channel
    /// registry (Python `CHANNELS`). The client's timeout is the default for channel calls.
    pub fn new(
        client: FlockClient<T>,
        channels: Vec<Value>,
        paths: DashboardPaths,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let default_timeout = client.timeout;
        Self {
            client: Mutex::new(client),
            default_timeout,
            channels,
            paths,
            clock,
            panes: Mutex::new(TimedCache::default()),
            progress: Mutex::new(TimedCache::default()),
            capabilities: Mutex::new(HashMap::new()),
        }
    }

    fn channel_config(&self, channel: &str) -> Option<&Value> {
        self.channels
            .iter()
            .find(|config| config.get("id").and_then(Value::as_str) == Some(channel))
    }

    fn list_panes(&self, client: &mut FlockClient<T>) -> Result<Vec<Value>, ActivityPoolradError> {
        let listing = rpc(client, "list_panes", Map::new()).map_err(|failure| match failure {
            RpcFailure::Unavailable => ActivityPoolradError::Http {
                status: StatusCode::SERVICE_UNAVAILABLE,
                detail: "Dunking Sheep unavailable".into(),
            },
            RpcFailure::Failed => ActivityPoolradError::Failure,
        })?;
        listing
            .get("panes")
            .and_then(Value::as_array)
            .cloned()
            .ok_or(ActivityPoolradError::Failure)
    }

    /// Python `_capability`, cached for 20 s by `_can_message`. Non-object replies are not cached.
    fn capability(&self, client: &mut FlockClient<T>, pane_id: &str) -> Map<String, Value> {
        let now = self.clock.unix_seconds();
        let cached = locked(&self.capabilities).get(pane_id).cloned();
        if let Some((fetched_at, capability)) = cached {
            if now - fetched_at < CAPABILITY_TTL_SECONDS {
                return capability;
            }
        }
        let mut args = Map::new();
        args.insert("target".into(), json!(pane_id));
        match rpc(client, "delivery_capability", args) {
            Ok(Value::Object(payload)) => {
                locked(&self.capabilities).insert(
                    pane_id.to_string(),
                    (self.clock.unix_seconds(), payload.clone()),
                );
                payload
            }
            _ => Map::new(),
        }
    }

    /// Python `_can_message`: lanes are messageable, and other panes need a live agent process.
    fn can_message(
        &self,
        client: &mut FlockClient<T>,
        pane: &Value,
    ) -> Result<bool, ActivityPoolradError> {
        let Some(pane_id) = pane
            .get("pane_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return Ok(false);
        };
        if is_lane(Some(pane)).map_err(|_| ActivityPoolradError::Failure)? {
            return Ok(true);
        }
        let capability = self.capability(client, pane_id);
        Ok(capability.get("agent_alive") == Some(&Value::Bool(true)))
    }

    /// Python `_target`: a configured channel, else a discovered agent pane, else a known
    /// channel with no pane (None), else `Unknown channel` (404).
    fn target(
        &self,
        client: &mut FlockClient<T>,
        channel: &str,
        panes: &[Value],
    ) -> Result<Option<Value>, ActivityPoolradError> {
        if let Some(config) = self.channel_config(channel) {
            return match_pane(config, panes).map_err(|_| ActivityPoolradError::Failure);
        }
        for pane in panes {
            let pane_id = pane.get("pane_id").and_then(Value::as_str).unwrap_or("");
            if discovered_id(pane_id) == channel && self.can_message(client, pane)? {
                return Ok(Some(pane.clone()));
            }
        }
        if known_pane_channel(channel) {
            return Ok(None);
        }
        Err(ActivityPoolradError::Http {
            status: StatusCode::NOT_FOUND,
            detail: "Unknown channel".into(),
        })
    }

    /// Python `panes_activity`'s refresh body: any failure means `None`, which is cached.
    fn refresh_panes(&self, client: &mut FlockClient<T>) -> Option<Value> {
        client.timeout = PANES_RPC_TIMEOUT;
        let listing = rpc(client, "list_panes", Map::new()).ok()?;
        let panes = listing.get("panes")?.as_array()?;
        let rows = build_roster(&[], panes, true, None).ok()?;
        let rows: Vec<Value> = rows.iter().map(|row| self.pane_row(client, row)).collect();
        Some(json!({
            "panes": rows,
            "checked_at": utc_stamp(self.clock.unix_seconds()),
            "refresh_after_seconds": PANES_TTL_SECONDS,
        }))
    }

    /// One pane row of the panes snapshot. A failed read keeps the row and marks it.
    fn pane_row(&self, client: &mut FlockClient<T>, row: &Value) -> Value {
        let mut merged = row.as_object().cloned().unwrap_or_default();
        match read_pane_activity(client, row, PANES_READ_LINES) {
            Ok(mut output) => {
                let text = output
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                output.insert("text".into(), json!(trim_pane_text(&text)));
                merged.extend(output);
                merged.insert("error".into(), Value::Null);
            }
            Err(_) => {
                merged.insert("text".into(), json!(""));
                merged.insert("error".into(), json!("Pane output unavailable"));
            }
        }
        Value::Object(merged)
    }

    /// Python `poolrad_dashboard_progress`'s cached body, minus the unported fallback.
    fn progress_view(&self) -> Result<Value, ActivityPoolradError> {
        let checkpoint = published_checkpoint(&self.paths.operations_db)
            .map_err(|_| ActivityPoolradError::Failure)?
            .ok_or(ActivityPoolradError::Failure)?;
        let mut view = compact_view(
            &self.paths.dashboard_repo,
            &checkpoint,
            self.clock.unix_seconds(),
        )
        .map_err(|_| ActivityPoolradError::Failure)?;
        view["observed_at"] = json!(checkpoint.observed_at);
        Ok(view)
    }
}

impl<T: Transport + Send + 'static> ActivityPoolradRoutes for ActivityPoolradBackend<T> {
    fn panes_activity(&self) -> Result<Option<Value>, ActivityPoolradError> {
        let mut cache = locked(&self.panes);
        if self.clock.monotonic() >= cache.expires {
            let mut client = locked(&self.client);
            cache.data = self.refresh_panes(&mut client);
            cache.expires = self.clock.monotonic() + PANES_TTL_SECONDS as f64;
        }
        Ok(cache.data.clone())
    }

    fn activity(&self, channel: &str) -> Result<Option<Value>, ActivityPoolradError> {
        let mut client = locked(&self.client);
        client.timeout = self.default_timeout;
        let panes = self.list_panes(&mut client)?;
        let Some(pane) = self.target(&mut client, channel, &panes)? else {
            return Ok(None);
        };
        let output = read_pane_activity(&mut client, &pane, CHANNEL_READ_LINES)
            .map_err(|_| ActivityPoolradError::Failure)?;
        let mut body = Map::new();
        body.insert("channel".into(), json!(channel));
        body.extend(output);
        Ok(Some(Value::Object(body)))
    }

    fn poolrad_progress(&self) -> Result<Option<Value>, ActivityPoolradError> {
        if !self.paths.dashboard_dir.join(DASHBOARD_SCRIPT).is_file() {
            return Ok(None);
        }
        let mut cache = locked(&self.progress);
        let now = self.clock.monotonic();
        if now >= cache.expires {
            let mut view = self.progress_view()?;
            let commit = view
                .get("commit")
                .and_then(Value::as_str)
                .map(str::to_owned);
            view["replacements"] = replacement_count(
                &self.paths.dashboard_repo,
                commit.as_deref(),
                self.clock.unix_seconds(),
            );
            cache.data = Some(view);
            cache.expires = now + PROGRESS_TTL_SECONDS as f64;
        }
        let mut view = cache.data.clone().unwrap_or_else(|| json!({}));
        view["refresh_after_seconds"] = json!(PROGRESS_TTL_SECONDS);
        Ok(Some(view))
    }
}

/// Filesystem seam for the Poolrad dashboard page and its reports (`web/app.py`).
pub struct PoolradDashboardBackend {
    paths: DashboardPaths,
}

impl PoolradDashboardBackend {
    pub fn new(paths: DashboardPaths) -> Self {
        Self { paths }
    }
}

impl PoolradDashboardRoutes for PoolradDashboardBackend {
    fn dashboard_available(&self) -> bool {
        self.paths.dashboard_dir.join(DASHBOARD_SCRIPT).is_file()
    }

    fn refresh(&self) -> Result<Option<String>, String> {
        Err(REFRESH_UNAVAILABLE.to_string())
    }

    fn dashboard_html(&self) -> Option<Vec<u8>> {
        read_regular_file(&self.paths.dashboard_dir.join(DASHBOARD_PAGE))
    }

    fn report(&self, filename: &str) -> Option<Vec<u8>> {
        if !REPORTS.contains(&filename) {
            return None;
        }
        read_regular_file(
            &self
                .paths
                .dashboard_dir
                .join("state")
                .join("reports")
                .join(filename),
        )
    }
}

/// Serves the built desk bundle for the `/dash` routes (`DIST / "index.html"`).
pub struct DashBackend {
    ui_dist: PathBuf,
}

impl DashBackend {
    pub fn new(ui_dist: PathBuf) -> Self {
        Self { ui_dist }
    }
}

impl DashRoutes for DashBackend {
    fn index_html(&self) -> Option<Vec<u8>> {
        read_regular_file(&self.ui_dist.join("index.html"))
    }
}

/// Python `percent`: an upper bound is floored to one decimal, an exact count is rounded.
pub fn percent(count: i64, ceiling: i64, bound_kind: &str) -> f64 {
    if ceiling <= 0 {
        return 0.0;
    }
    if bound_kind == "upper_bound" {
        let tenths_of_percent = (i128::from(count) * 1000).div_euclid(i128::from(ceiling));
        return tenths_of_percent as f64 / 10.0;
    }
    let exact = (i128::from(count) * 100) as f64 / i128::from(ceiling) as f64;
    round_one_decimal(exact)
}

/// Python `round(value, 1)`: correctly rounded, ties to even.
fn round_one_decimal(value: f64) -> f64 {
    format!("{value:.1}").parse().unwrap_or(value)
}

/// Python `str.splitlines()`: universal line boundaries, with no trailing empty line.
pub fn py_splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        let end = match ch {
            '\r' => match chars.peek() {
                Some(&(next, '\n')) => {
                    chars.next();
                    next + 1
                }
                _ => index + 1,
            },
            '\n' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}'
            | '\u{2029}' => index + ch.len_utf8(),
            _ => continue,
        };
        lines.push(&text[start..index]);
        start = end;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// The pane snapshot's text bound: `"\n".join(text.splitlines()[-40:])[-32000:]`.
pub fn trim_pane_text(text: &str) -> String {
    let lines = py_splitlines(text);
    let first = lines.len().saturating_sub(PANE_TEXT_LINES);
    let joined = lines[first..].join("\n");
    last_chars(&joined, PANE_TEXT_CHARS).to_string()
}

/// Python `text[-count:]`, counted in characters rather than bytes.
fn last_chars(text: &str, count: usize) -> &str {
    let total = text.chars().count();
    if total <= count {
        return text;
    }
    let start = text
        .char_indices()
        .nth(total - count)
        .map_or(text.len(), |(index, _)| index);
    &text[start..]
}

/// `time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(seconds))`.
pub fn utc_stamp(unix_seconds: f64) -> String {
    let seconds = unix_seconds.floor() as i64;
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let time_of_day = seconds.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time_of_day / 3600,
        time_of_day % 3600 / 60,
        time_of_day % 60,
    )
}

/// `datetime.fromtimestamp(t, ZoneInfo("America/Los_Angeles")).isoformat(timespec="seconds")`.
pub fn pacific_iso(unix_seconds: i64) -> String {
    let offset = pacific_offset(unix_seconds);
    let local = unix_seconds + offset;
    let (year, month, day) = civil_from_days(local.div_euclid(86_400));
    let time_of_day = local.rem_euclid(86_400);
    let sign = if offset < 0 { '-' } else { '+' };
    let offset_minutes = offset.abs() / 60;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}{sign}{:02}:{:02}",
        time_of_day / 3600,
        time_of_day % 3600 / 60,
        time_of_day % 60,
        offset_minutes / 60,
        offset_minutes % 60,
    )
}

/// US Pacific offset: PDT (-7 h) from 2 a.m. on the second Sunday of March to 2 a.m. on the first
/// Sunday of November, both local. Those instants are 10:00 and 09:00 UTC.
fn pacific_offset(unix_seconds: i64) -> i64 {
    let (year, _, _) = civil_from_days(unix_seconds.div_euclid(86_400));
    let dst_start = nth_sunday(year, 3, 2) * 86_400 + 10 * 3_600;
    let dst_end = nth_sunday(year, 11, 1) * 86_400 + 9 * 3_600;
    if (dst_start..dst_end).contains(&unix_seconds) {
        -7 * 3_600
    } else {
        -8 * 3_600
    }
}

fn nth_sunday(year: i64, month: i64, nth: i64) -> i64 {
    let first = days_from_civil(year, month, 1);
    let to_sunday = (7 - weekday_sunday_zero(first)) % 7;
    first + to_sunday + 7 * (nth - 1)
}

/// 1970-01-01 was a Thursday; Sunday is 0.
fn weekday_sunday_zero(days: i64) -> i64 {
    (days + 4).rem_euclid(7)
}

/// Howard Hinnant's `civil_from_days`, proleptic Gregorian, days since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month as u32, day as u32)
}

/// Howard Hinnant's `days_from_civil`, the inverse of [`civil_from_days`].
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let shifted_year = if month <= 2 { year - 1 } else { year };
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year.rem_euclid(400);
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The reviewed accounting row for the `poolrad` project (`_published_accounting`).
#[derive(Debug, PartialEq, Eq)]
struct Checkpoint {
    commit_oid: String,
    strict_full_c: i64,
    verified_identities: i64,
    ceiling: i64,
    bound_kind: String,
    observed_at: String,
    evidence_ref: String,
}

/// Newest published checkpoint, read-only. A missing database or a missing row is `None`.
fn published_checkpoint(path: &Path) -> Result<Option<Checkpoint>, rusqlite::Error> {
    if !path.is_file() {
        return Ok(None);
    }
    let connection = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_secs(10))?;
    let mut statement = connection.prepare(
        "SELECT commit_oid,strict_full_c,verified_game_identities,ceiling,bound_kind,\
         observed_at,evidence_ref FROM accounting_snapshots WHERE project=?1 \
         ORDER BY observed_at DESC,id DESC LIMIT 1",
    )?;
    let row = statement.query_row([PROJECT], |row| {
        Ok(Checkpoint {
            commit_oid: row.get(0)?,
            strict_full_c: row.get(1)?,
            verified_identities: row.get(2)?,
            ceiling: row.get(3)?,
            bound_kind: row.get(4)?,
            observed_at: row.get(5)?,
            evidence_ref: row.get(6)?,
        })
    });
    match row {
        Ok(checkpoint) => Ok(Some(checkpoint)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Python `compact_view` for the published-main source.
fn compact_view(repo: &Path, checkpoint: &Checkpoint, now: f64) -> Result<Value, ()> {
    let strict = checkpoint.strict_full_c;
    let identities = checkpoint.verified_identities;
    let ceiling = checkpoint.ceiling;
    let bound = checkpoint.bound_kind.as_str();
    let comparisons = cited_evidence(repo)?;
    let evidence: Vec<String> = comparisons
        .iter()
        .filter_map(|item| {
            let matched = item.get("matched")?.as_i64()?;
            let compared = item.get("compared")?.as_i64()?;
            Some(format!("{matched}/{compared} px"))
        })
        .collect();
    let commit8: String = checkpoint.commit_oid.chars().take(8).collect();
    let mut line = format!(
        "PM-reviewed checkpoint {commit8} · {strict} strict / {identities} identities · \
         ceiling {ceiling} ({}) · {} published scoped comparison(s)",
        bound.replace('_', " "),
        comparisons.len(),
    );
    if !evidence.is_empty() {
        line.push_str(&format!(" ({})", evidence.join(", ")));
    }
    line.push_str(" · not whole-game completion");
    Ok(json!({
        "generated": pacific_iso(now.floor() as i64),
        "commit": checkpoint.commit_oid,
        "source": "published main",
        "status": "reviewed",
        "strict_full_c": strict,
        "identities": identities,
        "ceiling": ceiling,
        "bound_kind": bound,
        "strict_percent": percent(strict, ceiling, bound),
        "identity_percent": percent(identities, ceiling, bound),
        "remaining": ceiling - strict,
        "accounting_source": ACCOUNTING_SOURCE,
        "evidence_doc": checkpoint.evidence_ref,
        "evidence_line": line,
        "comparisons": comparisons,
        "stale": false,
    }))
}

/// Python `cited_evidence`: literal claims present in two published reports, never rerun here.
fn cited_evidence(repo: &Path) -> Result<Vec<Value>, ()> {
    const CASES: [(&str, &str, &str, i64, &str); 2] = [
        (
            "EGA first-view row 64",
            "docs/re/SLOT_A_FIRST_EGA_ROW64.md",
            "**320 indexed pixels match all 320**",
            320,
            "one 320-pixel row at the saved first-view return",
        ),
        (
            "Printer page, row 6 column 38",
            "docs/re/SLOT_A_ROW6_COL38_PRINTER_PAGE.md",
            "**28,160/28,160** pixel indices equal",
            28_160,
            "one reached printer call; 320×88 diagnostic crop",
        ),
    ];
    let mut output = Vec::with_capacity(CASES.len());
    for (label, source, proof, count, scope) in CASES {
        let body = fs::read_to_string(repo.join(source)).map_err(|_| ())?;
        let found = body.contains(proof);
        output.push(json!({
            "label": label,
            "source": source,
            "scope": scope,
            "matched": if found { json!(count) } else { Value::Null },
            "compared": if found { json!(count) } else { Value::Null },
            "status": if found { "reported in source" } else { "source claim changed; re-review" },
            "method": "published scoped report; not rerun by dashboard",
        }));
    }
    Ok(output)
}

/// Python `_replacement_count`: recorded replacements at the exact checkpoint commit.
fn replacement_count(repo: &Path, commit: Option<&str>, now: f64) -> Value {
    let mut result = json!({
        "state": "unavailable",
        "count": null,
        "commit": commit,
        "source": REPLACED_BODIES,
        "observed_at": utc_stamp(now),
    });
    let Some(commit) = commit.filter(|value| is_sha40(value)) else {
        return result;
    };
    let Some(text) = git_show(repo, commit, REPLACED_BODIES) else {
        return result;
    };
    if let Some((count, truth)) = validate_replacements(&text) {
        result["state"] = json!("ok");
        result["count"] = json!(count);
        result["truth"] = truth;
    }
    result
}

fn is_sha40(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// The `replaced/1` schema: bodies with unique string ids and truthy proof fields.
fn validate_replacements(text: &str) -> Option<(usize, Value)> {
    let record: Value = serde_json::from_str(text).ok()?;
    let record = record.as_object()?;
    let bodies = record.get("bodies")?.as_array()?;
    if record.get("schema") != Some(&json!("replaced/1")) {
        return None;
    }
    let mut ids = HashSet::new();
    for row in bodies {
        let row = row.as_object()?;
        let id = row.get("id")?.as_str()?;
        if !truthy(row.get("proven_at")) || !truthy(row.get("evidence")) {
            return None;
        }
        ids.insert(id);
    }
    if ids.len() != bodies.len() {
        return None;
    }
    Some((
        bodies.len(),
        record.get("truth").cloned().unwrap_or(Value::Null),
    ))
}

/// Python truthiness for a JSON value; a missing key is falsy.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

/// `git -C repo show <commit>:<path>` with no shell, bounded by [`GIT_TIMEOUT`].
fn git_show(repo: &Path, commit: &str, path: &str) -> Option<String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("show")
        .arg(format!("{commit}:{path}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + GIT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let bytes = reader.join().ok()?.ok()?;
    if !status.success() {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Python `known channel` fallback: `pane-` followed by one or more of `[a-z0-9-]`.
fn known_pane_channel(channel: &str) -> bool {
    channel.strip_prefix("pane-").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    })
}

/// One request/reply exchange with the daemon, split the way the Python route surfaces it.
fn rpc<T: Transport>(
    client: &mut FlockClient<T>,
    method: &str,
    args: Map<String, Value>,
) -> Result<Value, RpcFailure> {
    client.call(method, args).map_err(|error| match error {
        ClientError::Io(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            RpcFailure::Unavailable
        }
        _ => RpcFailure::Failed,
    })
}

/// Python `_read_pane_activity`: `{pane_id, status, text}` from one `read_pane` call.
fn read_pane_activity<T: Transport>(
    client: &mut FlockClient<T>,
    pane: &Value,
    lines: u64,
) -> Result<Map<String, Value>, RpcFailure> {
    let pane_id = pane.get("pane_id").cloned().ok_or(RpcFailure::Failed)?;
    let mut args = Map::new();
    args.insert("target".into(), pane_id.clone());
    args.insert("lines".into(), json!(lines));
    let reply = rpc(client, "read_pane", args)?;
    let reply = reply.as_object().ok_or(RpcFailure::Failed)?;
    let status = if let Some(raw) = reply.get("agent_status") {
        json!(presence(Some(raw)).map_err(|_| RpcFailure::Failed)?)
    } else {
        let fallback = presence(pane.get("agent_status")).map_err(|_| RpcFailure::Failed)?;
        pane.get("status")
            .cloned()
            .unwrap_or_else(|| json!(fallback))
    };
    let text = match reply.get("text") {
        None => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(_) => return Err(RpcFailure::Failed),
    };
    let mut output = Map::new();
    output.insert("pane_id".into(), pane_id);
    output.insert("status".into(), status);
    output.insert("text".into(), json!(text));
    Ok(output)
}

fn read_regular_file(path: &Path) -> Option<Vec<u8>> {
    path.is_file().then(|| fs::read(path).ok()).flatten()
}

fn locked<V>(mutex: &Mutex<V>) -> MutexGuard<'_, V> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
