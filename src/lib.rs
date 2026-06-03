use std::collections::{HashMap, HashSet, hash_map::Entry};
use std::env;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Datelike, Local, LocalResult, NaiveDate, TimeDelta, TimeZone};
use csv::Writer;
use rusqlite::{Connection, Row, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_INTERVAL_SECONDS: u64 = 2;
pub const DEFAULT_IDLE_THRESHOLD_SECONDS: u64 = 300;
pub const DEFAULT_PROBE_MISS_TOLERANCE: u8 = 3;
pub const DEFAULT_RECENT_CHECKPOINT_SECONDS: u64 = 30;
pub const DEFAULT_AUDIT_GAP_THRESHOLD_SECONDS: f64 = 30.0;
pub const DEFAULT_HEALTH_STALE_THRESHOLD_SECONDS: u64 = 60;
pub const MAX_AUDIT_QUALITY_ISSUES: usize = 50;
pub const SERVICE_LABEL: &str = "com.local.activity-tracker";
pub const IDLE_BUNDLE_ID: &str = "local.activity_tracker.idle";
pub const UNTRACKED_BUNDLE_ID: &str = "local.activity_tracker.untracked";

#[derive(Debug, Error)]
pub enum TrackerError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("CSV error: {0}")]
    Csv(#[from] csv::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("invalid JSONL record in {path} at line {line}: {source}")]
    JsonLine {
        path: PathBuf,
        line: usize,
        source: serde_json::Error,
    },
    #[error("invalid date `{0}`; expected YYYY-MM-DD")]
    InvalidDate(String),
    #[error("invalid timestamp `{0}`; expected RFC3339")]
    InvalidTimestamp(String),
    #[error("invalid date range `{from}`..`{to}`; --to must be same day or after --from")]
    InvalidDateRange { from: String, to: String },
    #[error(
        "invalid time range `{since}`..`{until}`; --until must be same instant or after --since"
    )]
    InvalidTimeRange { since: String, until: String },
    #[error("invalid duration `{0}`; expected positive whole minutes")]
    InvalidDuration(String),
    #[error("conflicting query window arguments: {0}")]
    ConflictingQueryWindowArgs(&'static str),
    #[error("CSV is missing required column `{0}`")]
    MissingCsvColumn(&'static str),
    #[error("invalid activity type `{0}`")]
    InvalidActivityType(String),
    #[error("could not resolve local midnight for {0}")]
    InvalidLocalDay(NaiveDate),
    #[error("AppleScript failed: {0}")]
    AppleScript(String),
    #[error("command `{command}` failed: {stderr}")]
    Command { command: String, stderr: String },
    #[error("command `{0}` timed out")]
    CommandTimeout(String),
    #[error("home directory not found")]
    HomeNotFound,
    #[error("data directory not found")]
    DataDirNotFound,
    #[error("Ctrl-C handler failed: {0}")]
    CtrlC(String),
}

pub type Result<T> = std::result::Result<T, TrackerError>;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityType {
    #[default]
    Active,
    Idle,
    Untracked,
}

impl ActivityType {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Idle => "idle",
            Self::Untracked => "untracked",
        }
    }
}

impl std::str::FromStr for ActivityType {
    type Err = TrackerError;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_lowercase().as_str() {
            "" | "active" => Ok(Self::Active),
            "idle" => Ok(Self::Idle),
            "untracked" => Ok(Self::Untracked),
            other => Err(TrackerError::InvalidActivityType(other.to_string())),
        }
    }
}

impl fmt::Display for ActivityType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActiveEntity {
    pub bundle_id: String,
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    pub url: Option<String>,
    pub category: String,
    #[serde(default)]
    pub activity_type: ActivityType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSession {
    pub start_time: DateTime<Local>,
    pub end_time: DateTime<Local>,
    pub duration_seconds: f64,
    pub app_name: String,
    pub bundle_id: String,
    #[serde(default)]
    pub title: Option<String>,
    pub category: String,
    pub url: Option<String>,
    #[serde(default)]
    pub activity_type: ActivityType,
}

impl UsageSession {
    #[must_use]
    pub fn from_entity(
        entity: &ActiveEntity,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Option<Self> {
        seconds_between(start_time, end_time).map(|duration_seconds| Self {
            start_time,
            end_time,
            duration_seconds,
            app_name: entity.name.clone(),
            bundle_id: entity.bundle_id.clone(),
            title: entity.title.clone(),
            category: entity.category.clone(),
            url: entity.url.clone(),
            activity_type: entity.activity_type,
        })
    }

    #[must_use]
    pub fn seconds_within(
        &self,
        window_start: DateTime<Local>,
        window_end: DateTime<Local>,
    ) -> f64 {
        let start = if self.start_time > window_start {
            self.start_time
        } else {
            window_start
        };
        let end = if self.end_time < window_end {
            self.end_time
        } else {
            window_end
        };
        seconds_between(start, end).unwrap_or(0.0)
    }

    #[must_use]
    pub fn overlaps(&self, window_start: DateTime<Local>, window_end: DateTime<Local>) -> bool {
        self.start_time < window_end && self.end_time > window_start
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SummaryRow {
    pub name: String,
    pub seconds: f64,
    pub percentage: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActivitySummary {
    pub session_count: usize,
    pub total_seconds: f64,
    pub by_activity_type: Vec<SummaryRow>,
    pub by_category: Vec<SummaryRow>,
    pub by_app: Vec<SummaryRow>,
    pub by_domain: Vec<SummaryRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TimelineBlock {
    pub start_time: DateTime<Local>,
    pub end_time: DateTime<Local>,
    pub duration_seconds: f64,
    pub activity_type: ActivityType,
    pub category: String,
    pub app_name: String,
    pub bundle_id: String,
    pub domain: Option<String>,
    pub title: Option<String>,
    pub url: Option<String>,
    pub session_count: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct QueryTimeWindowInput<'a> {
    pub from: Option<&'a str>,
    pub to: Option<&'a str>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub last_minutes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryTimeWindow {
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    pub since: Option<DateTime<Local>>,
    pub until: Option<DateTime<Local>>,
    pub last_minutes: Option<u64>,
    pub start: Option<DateTime<Local>>,
    pub end: Option<DateTime<Local>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActivityAudit {
    pub session_count: usize,
    pub gap_count: usize,
    pub overlap_count: usize,
    pub invalid_session_count: usize,
    pub active_session_count: usize,
    pub idle_session_count: usize,
    pub untracked_session_count: usize,
    pub missing_title_count: usize,
    pub browser_session_count: usize,
    pub browser_missing_url_count: usize,
    pub browser_blank_tab_count: usize,
    pub uncategorized_session_count: usize,
    pub missing_title_by_app: Vec<AuditQualityRow>,
    pub browser_missing_url_by_app: Vec<AuditQualityRow>,
    pub browser_missing_url_by_title: Vec<AuditQualityRow>,
    pub browser_blank_tab_by_app: Vec<AuditQualityRow>,
    pub uncategorized_by_app: Vec<AuditQualityRow>,
    pub quality_issues: Vec<AuditQualityIssue>,
    pub total_gap_seconds: f64,
    pub longest_gap_seconds: f64,
    pub gaps: Vec<AuditGap>,
    pub overlaps: Vec<AuditOverlap>,
    pub invalid_sessions: Vec<AuditInvalidSession>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditQualityRow {
    pub name: String,
    pub count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditQualityIssueKind {
    MissingTitle,
    BrowserMissingUrl,
    Uncategorized,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditQualityIssue {
    pub kind: AuditQualityIssueKind,
    pub start_time: DateTime<Local>,
    pub end_time: DateTime<Local>,
    pub duration_seconds: f64,
    pub app_name: String,
    pub bundle_id: String,
    pub title: Option<String>,
    pub category: String,
    pub url: Option<String>,
    pub activity_type: ActivityType,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditGap {
    pub start_time: DateTime<Local>,
    pub end_time: DateTime<Local>,
    pub duration_seconds: f64,
    pub previous_app: String,
    pub next_app: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditOverlap {
    pub start_time: DateTime<Local>,
    pub end_time: DateTime<Local>,
    pub duration_seconds: f64,
    pub first_app: String,
    pub second_app: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditInvalidSession {
    pub start_time: DateTime<Local>,
    pub end_time: DateTime<Local>,
    pub duration_seconds: f64,
    pub app_name: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceStatusReport {
    pub label: String,
    pub loaded: bool,
    pub running: bool,
    pub pid: Option<u32>,
    pub raw: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StorageHealth {
    pub session_count: u64,
    pub last_completed_session: Option<UsageSession>,
    pub open_session: Option<SessionCheckpoint>,
    pub latest_observed_at: Option<DateTime<Local>>,
    pub latest_observed_age_seconds: Option<f64>,
    pub stale_threshold_seconds: u64,
    pub fresh: bool,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct ImportReport {
    pub scanned: usize,
    pub imported: usize,
    pub skipped_duplicates: usize,
    pub dry_run: bool,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct ReclassifyReport {
    pub scanned: usize,
    pub changed: usize,
    pub dry_run: bool,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct RepairGapsReport {
    pub scanned: usize,
    pub gaps_found: usize,
    pub repaired: usize,
    pub dry_run: bool,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct RepairTitlesReport {
    pub scanned: usize,
    pub repaired: usize,
    pub native_repaired: usize,
    pub browser_repaired: usize,
    pub dry_run: bool,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct RepairUrlsReport {
    pub scanned: usize,
    pub repaired: usize,
    pub blank_tab_urls: usize,
    pub blank_tab_context_urls: usize,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionCheckpoint {
    pub start_time: DateTime<Local>,
    pub last_seen_at: DateTime<Local>,
    pub entity: ActiveEntity,
}

#[derive(Debug, Clone)]
pub struct ProbeMissStabilizer {
    max_consecutive_misses: u8,
    consecutive_misses: u8,
}

impl ProbeMissStabilizer {
    #[must_use]
    pub const fn new(max_consecutive_misses: u8) -> Self {
        Self {
            max_consecutive_misses,
            consecutive_misses: 0,
        }
    }

    #[must_use]
    pub fn stabilize(
        &mut self,
        observed: Option<ActiveEntity>,
        current: Option<&ActiveEntity>,
    ) -> Option<ActiveEntity> {
        if let Some(entity) = observed {
            self.consecutive_misses = 0;
            return Some(entity);
        }

        self.consecutive_misses = self.consecutive_misses.saturating_add(1);
        if self.consecutive_misses <= self.max_consecutive_misses {
            current.cloned()
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
pub struct TrackerState {
    current_entity: Option<ActiveEntity>,
    session_start: DateTime<Local>,
    idle_threshold_seconds: u64,
}

impl TrackerState {
    #[must_use]
    pub fn new(
        current_entity: Option<ActiveEntity>,
        session_start: DateTime<Local>,
        idle_threshold_seconds: u64,
    ) -> Self {
        Self {
            current_entity,
            session_start,
            idle_threshold_seconds,
        }
    }

    #[must_use]
    pub fn current_entity(&self) -> Option<&ActiveEntity> {
        self.current_entity.as_ref()
    }

    #[must_use]
    pub const fn session_start(&self) -> DateTime<Local> {
        self.session_start
    }

    #[must_use]
    pub fn apply_sample(
        &mut self,
        entity: Option<ActiveEntity>,
        idle_seconds: Option<f64>,
        observed_at: DateTime<Local>,
    ) -> Option<UsageSession> {
        let idle_started_at = idle_start(observed_at, idle_seconds, self.idle_threshold_seconds);
        let desired_entity = if idle_started_at.is_some() {
            Some(idle_entity())
        } else {
            entity
        };

        if desired_entity == self.current_entity {
            return None;
        }

        let previous = self.current_entity.as_ref();
        let desired_is_idle = desired_entity
            .as_ref()
            .is_some_and(|entity| entity.activity_type == ActivityType::Idle)
            && previous.is_none_or(|entity| entity.activity_type != ActivityType::Idle);
        let end_time = if desired_is_idle {
            max_datetime(self.session_start, idle_started_at.unwrap_or(observed_at))
        } else {
            observed_at
        };

        let completed = if let Some(entity) = previous {
            UsageSession::from_entity(entity, self.session_start, end_time)
        } else if desired_entity.is_some() {
            untracked_session(self.session_start, end_time)
        } else {
            None
        };
        self.current_entity = desired_entity;
        self.session_start = if completed.is_some() || desired_is_idle {
            end_time
        } else {
            observed_at
        };
        completed
    }

    #[must_use]
    pub fn finish(&self, end_time: DateTime<Local>) -> Option<UsageSession> {
        self.current_entity
            .as_ref()
            .and_then(|entity| UsageSession::from_entity(entity, self.session_start, end_time))
    }
}

#[derive(Debug, Clone)]
pub struct LogStore {
    root: PathBuf,
}

impl LogStore {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn from_env() -> Result<Self> {
        let root = if let Ok(path) = env::var("ACTIVITY_TRACKER_HOME") {
            PathBuf::from(path)
        } else {
            default_data_dir()?
        };
        Ok(Self::new(root))
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn db_path(&self) -> PathBuf {
        self.root.join("activity.db")
    }

    #[must_use]
    pub fn sessions_path(&self) -> PathBuf {
        self.root.join("sessions.jsonl")
    }

    #[must_use]
    pub fn csv_path(&self) -> PathBuf {
        self.root.join("usage_stats.csv")
    }

    #[must_use]
    pub fn exports_dir(&self) -> PathBuf {
        self.root.join("exports")
    }

    #[must_use]
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        self.ensure_database_ready()?;
        if self.migrate_jsonl_to_db_if_needed()? {
            self.rewrite_jsonl_mirror_from_db()?;
        } else {
            self.ensure_jsonl_mirror_exists()?;
        }
        Ok(())
    }

    fn ensure_database_ready(&self) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        fs::create_dir_all(self.exports_dir())?;
        fs::create_dir_all(self.logs_dir())?;
        self.init_database()?;
        Ok(())
    }

    pub fn append_session(&self, session: &UsageSession) -> Result<()> {
        let _inserted = self.append_session_if_new(session)?;
        Ok(())
    }

    fn append_session_if_new(&self, session: &UsageSession) -> Result<bool> {
        self.ensure_dirs()?;
        let inserted = self.insert_session_db(session)?;
        if inserted {
            self.append_session_jsonl(session)?;
        }
        Ok(inserted)
    }

    pub fn load_sessions(&self) -> Result<Vec<UsageSession>> {
        if self.db_path().exists() {
            self.ensure_database_ready()?;
            return self.load_sessions_from_db();
        }
        if self.sessions_path().exists() {
            return self.load_sessions_from_jsonl();
        }
        if self.is_default_root()
            && let Some(path) = legacy_sessions_path()
            && path.exists()
        {
            return load_jsonl_sessions_from_path(&path);
        }
        Ok(Vec::new())
    }

    pub fn session_count(&self) -> Result<u64> {
        if self.db_path().exists() {
            self.ensure_database_ready()?;
            return self.db_session_count();
        }

        Ok(self.load_sessions()?.len() as u64)
    }

    pub fn last_completed_session(&self) -> Result<Option<UsageSession>> {
        if self.db_path().exists() {
            self.ensure_database_ready()?;
            return self.load_last_session_from_db();
        }

        let mut sessions = self.load_sessions()?;
        sessions.sort_by_key(|session| (session.end_time, session.start_time));
        Ok(sessions.pop())
    }

    pub fn storage_health(
        &self,
        now: DateTime<Local>,
        stale_threshold_seconds: u64,
    ) -> Result<StorageHealth> {
        self.ensure_dirs()?;
        let session_count = self.session_count()?;
        let last_completed_session = self.last_completed_session()?;
        let open_session = self.open_session_checkpoint()?;
        let latest_observed_at =
            latest_observed_at(last_completed_session.as_ref(), open_session.as_ref());
        let latest_observed_age_seconds =
            latest_observed_at.map(|observed_at| age_seconds(observed_at, now));
        let fresh = latest_observed_age_seconds
            .is_some_and(|seconds| seconds <= stale_threshold_seconds as f64);

        Ok(StorageHealth {
            session_count,
            last_completed_session,
            open_session,
            latest_observed_at,
            latest_observed_age_seconds,
            stale_threshold_seconds,
            fresh,
        })
    }

    pub fn sessions_in_window(
        &self,
        window_start: Option<DateTime<Local>>,
        window_end: Option<DateTime<Local>>,
    ) -> Result<Vec<UsageSession>> {
        if self.db_path().exists() {
            self.ensure_database_ready()?;
            return self.load_sessions_from_db_window(window_start, window_end);
        }

        Ok(filter_sessions_by_time_window(
            self.load_sessions()?,
            window_start,
            window_end,
        ))
    }

    fn load_sessions_from_jsonl(&self) -> Result<Vec<UsageSession>> {
        let path = self.sessions_path();
        if !path.exists() {
            return Ok(Vec::new());
        }

        load_jsonl_sessions_from_path(&path)
    }

    fn load_sessions_from_db(&self) -> Result<Vec<UsageSession>> {
        self.load_sessions_from_db_window(None, None)
    }

    fn load_last_session_from_db(&self) -> Result<Option<UsageSession>> {
        let sessions = self.query_sessions_from_db(
            "SELECT start_time, end_time, duration_seconds, app_name, bundle_id, title, category, url, activity_type
             FROM sessions
             ORDER BY end_unix_ms DESC, start_unix_ms DESC, id DESC
             LIMIT 1",
            [],
        )?;
        Ok(sessions.into_iter().next())
    }

    fn load_sessions_from_db_window(
        &self,
        window_start: Option<DateTime<Local>>,
        window_end: Option<DateTime<Local>>,
    ) -> Result<Vec<UsageSession>> {
        match (
            window_start.map(|time| time.timestamp_millis()),
            window_end.map(|time| time.timestamp_millis()),
        ) {
            (Some(start), Some(end)) => self.query_sessions_from_db(
                "SELECT start_time, end_time, duration_seconds, app_name, bundle_id, title, category, url, activity_type
                 FROM sessions
                 WHERE end_unix_ms > ?1 AND start_unix_ms < ?2
                 ORDER BY start_unix_ms, end_unix_ms, id",
                params![start, end],
            ),
            (Some(start), None) => self.query_sessions_from_db(
                "SELECT start_time, end_time, duration_seconds, app_name, bundle_id, title, category, url, activity_type
                 FROM sessions
                 WHERE end_unix_ms > ?1
                 ORDER BY start_unix_ms, end_unix_ms, id",
                params![start],
            ),
            (None, Some(end)) => self.query_sessions_from_db(
                "SELECT start_time, end_time, duration_seconds, app_name, bundle_id, title, category, url, activity_type
                 FROM sessions
                 WHERE start_unix_ms < ?1
                 ORDER BY start_unix_ms, end_unix_ms, id",
                params![end],
            ),
            (None, None) => self.query_sessions_from_db(
                "SELECT start_time, end_time, duration_seconds, app_name, bundle_id, title, category, url, activity_type
                 FROM sessions
                 ORDER BY start_unix_ms, end_unix_ms, id",
                [],
            ),
        }
    }

    fn query_sessions_from_db<P: rusqlite::Params>(
        &self,
        sql: &str,
        params: P,
    ) -> Result<Vec<UsageSession>> {
        let conn = Connection::open(self.db_path())?;
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map(params, db_session_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter().map(DbSessionRow::into_session).collect()
    }

    pub fn sessions_for_day(&self, date: NaiveDate) -> Result<Vec<UsageSession>> {
        let (start, end) = day_bounds(date)?;
        self.sessions_in_window(Some(start), Some(end))
    }

    pub fn load_sessions_with_open(
        &self,
        now: DateTime<Local>,
        recent_gap_seconds: u64,
    ) -> Result<Vec<UsageSession>> {
        let mut sessions = self.load_sessions()?;
        if let Some(session) = self.provisional_open_session(now, recent_gap_seconds)? {
            sessions.push(session);
        }
        Ok(sessions)
    }

    pub fn sessions_for_day_with_open(
        &self,
        date: NaiveDate,
        now: DateTime<Local>,
        recent_gap_seconds: u64,
    ) -> Result<Vec<UsageSession>> {
        let (start, end) = day_bounds(date)?;
        self.sessions_in_window_with_open(Some(start), Some(end), now, recent_gap_seconds)
    }

    pub fn sessions_in_window_with_open(
        &self,
        window_start: Option<DateTime<Local>>,
        window_end: Option<DateTime<Local>>,
        now: DateTime<Local>,
        recent_gap_seconds: u64,
    ) -> Result<Vec<UsageSession>> {
        let mut sessions = self.sessions_in_window(window_start, window_end)?;
        if let Some(session) = self.provisional_open_session(now, recent_gap_seconds)?
            && window_start.is_none_or(|start| session.end_time > start)
            && window_end.is_none_or(|end| session.start_time < end)
        {
            sessions.push(session);
        }
        Ok(sessions)
    }

    pub fn write_csv(&self, path: &Path, sessions: &[UsageSession]) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut writer = Writer::from_path(path)?;
        writer.write_record([
            "Start Time",
            "End Time",
            "Duration (seconds)",
            "App Name",
            "Bundle ID",
            "Title",
            "Category",
            "Activity Type",
            "URL",
        ])?;

        for session in sessions {
            writer.write_record([
                session.start_time.to_rfc3339(),
                session.end_time.to_rfc3339(),
                format!("{:.3}", session.duration_seconds),
                session.app_name.clone(),
                session.bundle_id.clone(),
                session.title.clone().unwrap_or_default(),
                session.category.clone(),
                session.activity_type.to_string(),
                session.url.clone().unwrap_or_default(),
            ])?;
        }

        writer.flush()?;
        Ok(())
    }

    pub fn refresh_default_csv(&self) -> Result<()> {
        let sessions = self.load_sessions()?;
        self.write_csv(&self.csv_path(), &sessions)
    }

    pub fn import_csv(&self, path: &Path, dry_run: bool) -> Result<ImportReport> {
        let existing_sessions = self.load_sessions()?;
        if !dry_run {
            self.ensure_dirs()?;
        }
        let mut existing_keys: HashSet<_> = existing_sessions
            .into_iter()
            .map(|session| SessionKey::from(&session))
            .collect();
        let mut reader = csv::Reader::from_path(path)?;
        let headers = reader.headers()?.clone();
        let columns = CsvColumns::from_headers(&headers)?;
        let mut report = ImportReport {
            dry_run,
            ..ImportReport::default()
        };

        for record in reader.records() {
            let record = record?;
            report.scanned += 1;
            let session = columns.session_from_record(&record)?;
            let key = SessionKey::from(&session);
            if !existing_keys.insert(key) {
                report.skipped_duplicates += 1;
                continue;
            }
            report.imported += 1;
            if !dry_run {
                self.append_session(&session)?;
            }
        }

        if !dry_run && report.imported > 0 {
            self.refresh_default_csv()?;
        }
        Ok(report)
    }

    pub fn reclassify_sessions(&self, dry_run: bool) -> Result<ReclassifyReport> {
        self.ensure_dirs()?;
        let sessions = self.load_sessions_from_db()?;
        let mut report = ReclassifyReport {
            dry_run,
            ..ReclassifyReport::default()
        };

        for session in sessions {
            report.scanned += 1;
            let category = category_for_session(&session);
            if category == session.category {
                continue;
            }

            report.changed += 1;
            if !dry_run {
                self.update_session_category(&session, &category)?;
            }
        }

        if !dry_run && report.changed > 0 {
            self.rewrite_jsonl_mirror_from_db()?;
            self.refresh_default_csv()?;
        }

        Ok(report)
    }

    pub fn repair_gaps(
        &self,
        gap_threshold_seconds: f64,
        dry_run: bool,
    ) -> Result<RepairGapsReport> {
        self.ensure_dirs()?;
        let sessions = self.load_sessions()?;
        let audit = audit_sessions(&sessions, gap_threshold_seconds);
        let mut report = RepairGapsReport {
            scanned: sessions.len(),
            gaps_found: audit.gap_count,
            dry_run,
            ..RepairGapsReport::default()
        };

        for gap in audit.gaps {
            let Some(session) = untracked_session(gap.start_time, gap.end_time) else {
                continue;
            };
            let repaired = dry_run || self.append_session_if_new(&session)?;
            if repaired {
                report.repaired += 1;
            }
        }

        Ok(report)
    }

    pub fn repair_titles(&self, dry_run: bool) -> Result<RepairTitlesReport> {
        self.ensure_dirs()?;
        let sessions = self.load_sessions_from_db()?;
        let browser_titles = unique_browser_titles_by_url(&sessions);
        let mut report = RepairTitlesReport {
            dry_run,
            ..RepairTitlesReport::default()
        };

        for session in sessions {
            report.scanned += 1;
            let Some(repair) = repaired_title_for_session(&session, &browser_titles) else {
                continue;
            };

            report.repaired += 1;
            match repair.source {
                TitleRepairSource::NativeApp => report.native_repaired += 1,
                TitleRepairSource::UniqueBrowserUrl => report.browser_repaired += 1,
            }
            if !dry_run {
                self.update_session_title(&session, &repair.title)?;
            }
        }

        if !dry_run && report.repaired > 0 {
            self.rewrite_jsonl_mirror_from_db()?;
            self.refresh_default_csv()?;
        }

        Ok(report)
    }

    pub fn repair_urls(&self, dry_run: bool) -> Result<RepairUrlsReport> {
        self.ensure_dirs()?;
        let sessions = self.load_sessions_from_db()?;
        let mut report = RepairUrlsReport {
            dry_run,
            ..RepairUrlsReport::default()
        };

        for (index, session) in sessions.iter().enumerate() {
            report.scanned += 1;
            let previous = index.checked_sub(1).and_then(|index| sessions.get(index));
            let next = sessions.get(index + 1);
            let Some(repair) = repaired_url_for_session(session, previous, next) else {
                continue;
            };

            report.repaired += 1;
            match repair.source {
                UrlRepairSource::BlankTabUrl => report.blank_tab_urls += 1,
                UrlRepairSource::BlankTabContext => report.blank_tab_context_urls += 1,
            }
            if !dry_run {
                self.update_session_url(session, &repair.url)?;
            }
        }

        if !dry_run && report.repaired > 0 {
            self.rewrite_jsonl_mirror_from_db()?;
            self.refresh_default_csv()?;
        }

        Ok(report)
    }

    pub fn checkpoint_session(
        &self,
        entity: &ActiveEntity,
        start_time: DateTime<Local>,
        last_seen_at: DateTime<Local>,
    ) -> Result<()> {
        self.ensure_database_ready()?;
        let conn = Connection::open(self.db_path())?;
        conn.execute(
            "INSERT INTO open_session
             (id, start_time, last_seen_at, app_name, bundle_id, title, category, url, activity_type)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                 start_time = excluded.start_time,
                 last_seen_at = excluded.last_seen_at,
                 app_name = excluded.app_name,
                 bundle_id = excluded.bundle_id,
                 title = excluded.title,
                 category = excluded.category,
                 url = excluded.url,
                 activity_type = excluded.activity_type",
            params![
                start_time.to_rfc3339(),
                last_seen_at.to_rfc3339(),
                &entity.name,
                &entity.bundle_id,
                &entity.title,
                &entity.category,
                &entity.url,
                entity.activity_type.to_string(),
            ],
        )?;
        Ok(())
    }

    pub fn clear_checkpoint(&self) -> Result<()> {
        self.ensure_database_ready()?;
        let conn = Connection::open(self.db_path())?;
        conn.execute("DELETE FROM open_session WHERE id = 1", [])?;
        Ok(())
    }

    pub fn open_session_checkpoint(&self) -> Result<Option<SessionCheckpoint>> {
        self.ensure_database_ready()?;
        self.load_open_session_checkpoint()
    }

    pub fn provisional_open_session(
        &self,
        now: DateTime<Local>,
        recent_gap_seconds: u64,
    ) -> Result<Option<UsageSession>> {
        self.ensure_database_ready()?;
        let Some(checkpoint) = self.load_open_session_checkpoint()? else {
            return Ok(None);
        };

        let end_time = recovered_checkpoint_end(&checkpoint, now, recent_gap_seconds);
        Ok(UsageSession::from_entity(
            &checkpoint.entity,
            checkpoint.start_time,
            end_time,
        ))
    }

    pub fn recover_open_session(
        &self,
        now: DateTime<Local>,
        recent_gap_seconds: u64,
    ) -> Result<Option<UsageSession>> {
        self.ensure_dirs()?;
        let Some(checkpoint) = self.load_open_session_checkpoint()? else {
            return Ok(None);
        };

        let end_time = recovered_checkpoint_end(&checkpoint, now, recent_gap_seconds);
        let recovered =
            UsageSession::from_entity(&checkpoint.entity, checkpoint.start_time, end_time);
        if let Some(session) = &recovered {
            self.append_session(session)?;
        }
        self.clear_checkpoint()?;
        Ok(recovered)
    }

    fn init_database(&self) -> Result<()> {
        let conn = Connection::open(self.db_path())?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS sessions (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 start_time TEXT NOT NULL,
                 end_time TEXT NOT NULL,
                 duration_seconds REAL NOT NULL,
                 app_name TEXT NOT NULL,
                 bundle_id TEXT NOT NULL,
                 title TEXT,
                 category TEXT NOT NULL,
                 url TEXT,
                 activity_type TEXT NOT NULL DEFAULT 'active',
                 start_unix_ms INTEGER,
                 end_unix_ms INTEGER,
                 created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             );
             CREATE UNIQUE INDEX IF NOT EXISTS sessions_unique_idx
             ON sessions (
                 start_time,
                 end_time,
                 app_name,
                 bundle_id,
                 IFNULL(title, ''),
                 IFNULL(url, ''),
                 activity_type
             );
             CREATE TABLE IF NOT EXISTS open_session (
                 id INTEGER PRIMARY KEY CHECK (id = 1),
                 start_time TEXT NOT NULL,
                 last_seen_at TEXT NOT NULL,
                 app_name TEXT NOT NULL,
                 bundle_id TEXT NOT NULL,
                 title TEXT,
                 category TEXT NOT NULL,
                 url TEXT,
                 activity_type TEXT NOT NULL DEFAULT 'active'
             );",
        )?;
        self.ensure_session_epoch_columns(&conn)?;
        Ok(())
    }

    fn ensure_session_epoch_columns(&self, conn: &Connection) -> Result<()> {
        if !table_column_exists(conn, "sessions", "start_unix_ms")? {
            conn.execute("ALTER TABLE sessions ADD COLUMN start_unix_ms INTEGER", [])?;
        }
        if !table_column_exists(conn, "sessions", "end_unix_ms")? {
            conn.execute("ALTER TABLE sessions ADD COLUMN end_unix_ms INTEGER", [])?;
        }
        conn.execute(
            "CREATE INDEX IF NOT EXISTS sessions_time_idx
             ON sessions (start_unix_ms, end_unix_ms, id)",
            [],
        )?;
        self.backfill_session_epoch_columns(conn)?;
        Ok(())
    }

    fn backfill_session_epoch_columns(&self, conn: &Connection) -> Result<()> {
        let mut stmt = conn.prepare(
            "SELECT id, start_time, end_time
             FROM sessions
             WHERE start_unix_ms IS NULL OR end_unix_ms IS NULL",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(DbEpochBackfillRow {
                    id: row.get(0)?,
                    start_time: row.get(1)?,
                    end_time: row.get(2)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        for row in rows {
            let start_time = parse_local_datetime(&row.start_time)?;
            let end_time = parse_local_datetime(&row.end_time)?;
            conn.execute(
                "UPDATE sessions
                 SET start_unix_ms = ?1, end_unix_ms = ?2
                 WHERE id = ?3",
                params![
                    start_time.timestamp_millis(),
                    end_time.timestamp_millis(),
                    row.id
                ],
            )?;
        }
        Ok(())
    }

    fn load_open_session_checkpoint(&self) -> Result<Option<SessionCheckpoint>> {
        let conn = Connection::open(self.db_path())?;
        let result = conn.query_row(
            "SELECT start_time, last_seen_at, app_name, bundle_id, title, category, url, activity_type
             FROM open_session
             WHERE id = 1",
            [],
            |row| {
                Ok(DbCheckpointRow {
                    start_time: row.get(0)?,
                    last_seen_at: row.get(1)?,
                    app_name: row.get(2)?,
                    bundle_id: row.get(3)?,
                    title: row.get(4)?,
                    category: row.get(5)?,
                    url: row.get(6)?,
                    activity_type: row.get(7)?,
                })
            },
        );

        match result {
            Ok(row) => row.into_checkpoint().map(Some),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn migrate_jsonl_to_db_if_needed(&self) -> Result<bool> {
        if self.db_session_count()? > 0 {
            return Ok(false);
        }

        let mut migrated = false;
        let mut jsonl_paths = vec![self.sessions_path()];
        if self.is_default_root()
            && let Some(path) = legacy_sessions_path()
            && !jsonl_paths.iter().any(|existing| existing == &path)
        {
            jsonl_paths.push(path);
        }

        for path in jsonl_paths {
            if !path.exists() {
                continue;
            }
            for session in load_jsonl_sessions_from_path(&path)? {
                migrated |= self.insert_session_db(&session)?;
            }
        }
        Ok(migrated)
    }

    fn db_session_count(&self) -> Result<u64> {
        let conn = Connection::open(self.db_path())?;
        let count = conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
        Ok(count)
    }

    fn insert_session_db(&self, session: &UsageSession) -> Result<bool> {
        let conn = Connection::open(self.db_path())?;
        let changed = conn.execute(
            "INSERT OR IGNORE INTO sessions
             (start_time, end_time, duration_seconds, app_name, bundle_id, title, category, url, activity_type, start_unix_ms, end_unix_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                session.start_time.to_rfc3339(),
                session.end_time.to_rfc3339(),
                session.duration_seconds,
                &session.app_name,
                &session.bundle_id,
                &session.title,
                &session.category,
                &session.url,
                session.activity_type.to_string(),
                session.start_time.timestamp_millis(),
                session.end_time.timestamp_millis(),
            ],
        )?;
        Ok(changed > 0)
    }

    fn update_session_category(&self, session: &UsageSession, category: &str) -> Result<()> {
        let conn = Connection::open(self.db_path())?;
        conn.execute(
            "UPDATE sessions
             SET category = ?1
             WHERE start_time = ?2
               AND end_time = ?3
               AND app_name = ?4
               AND bundle_id = ?5
               AND title IS ?6
               AND url IS ?7
               AND activity_type = ?8",
            params![
                category,
                session.start_time.to_rfc3339(),
                session.end_time.to_rfc3339(),
                &session.app_name,
                &session.bundle_id,
                &session.title,
                &session.url,
                session.activity_type.to_string(),
            ],
        )?;
        Ok(())
    }

    fn update_session_title(&self, session: &UsageSession, title: &str) -> Result<()> {
        let conn = Connection::open(self.db_path())?;
        conn.execute(
            "UPDATE sessions
             SET title = ?1
             WHERE start_time = ?2
               AND end_time = ?3
               AND app_name = ?4
               AND bundle_id = ?5
               AND title IS ?6
               AND url IS ?7
               AND activity_type = ?8",
            params![
                title,
                session.start_time.to_rfc3339(),
                session.end_time.to_rfc3339(),
                &session.app_name,
                &session.bundle_id,
                &session.title,
                &session.url,
                session.activity_type.to_string(),
            ],
        )?;
        Ok(())
    }

    fn update_session_url(&self, session: &UsageSession, url: &str) -> Result<()> {
        let conn = Connection::open(self.db_path())?;
        conn.execute(
            "UPDATE sessions
             SET url = ?1
             WHERE start_time = ?2
               AND end_time = ?3
               AND app_name = ?4
               AND bundle_id = ?5
               AND title IS ?6
               AND url IS ?7
               AND activity_type = ?8",
            params![
                url,
                session.start_time.to_rfc3339(),
                session.end_time.to_rfc3339(),
                &session.app_name,
                &session.bundle_id,
                &session.title,
                &session.url,
                session.activity_type.to_string(),
            ],
        )?;
        Ok(())
    }

    fn append_session_jsonl(&self, session: &UsageSession) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.sessions_path())?;
        serde_json::to_writer(&mut file, session)?;
        file.write_all(b"\n")?;
        file.flush()?;
        Ok(())
    }

    fn ensure_jsonl_mirror_exists(&self) -> Result<()> {
        if self.sessions_path().exists() {
            return Ok(());
        }
        self.rewrite_jsonl_mirror_from_db()
    }

    fn rewrite_jsonl_mirror_from_db(&self) -> Result<()> {
        let sessions = self.load_sessions_from_db()?;
        if sessions.is_empty() {
            return Ok(());
        }

        let mut file = File::create(self.sessions_path())?;
        for session in sessions {
            serde_json::to_writer(&mut file, &session)?;
            file.write_all(b"\n")?;
        }
        file.flush()?;
        Ok(())
    }

    fn is_default_root(&self) -> bool {
        default_data_dir().is_ok_and(|path| path == self.root)
    }
}

#[must_use]
pub fn summarize_all(sessions: &[UsageSession]) -> ActivitySummary {
    summarize_with_seconds(sessions, |session| session.duration_seconds)
}

pub fn summarize_day(sessions: &[UsageSession], date: NaiveDate) -> Result<ActivitySummary> {
    let (start, end) = day_bounds(date)?;
    Ok(summarize_with_seconds(sessions, |session| {
        session.seconds_within(start, end)
    }))
}

#[must_use]
pub fn audit_sessions(sessions: &[UsageSession], gap_threshold_seconds: f64) -> ActivityAudit {
    let mut sorted = sessions.to_vec();
    sorted.sort_by_key(|session| (session.start_time, session.end_time));

    let mut gaps = Vec::new();
    let mut overlaps = Vec::new();
    let invalid_sessions = sorted
        .iter()
        .filter_map(invalid_session)
        .collect::<Vec<_>>();
    let active_session_count = sorted
        .iter()
        .filter(|session| session.activity_type == ActivityType::Active)
        .count();
    let idle_session_count = sorted
        .iter()
        .filter(|session| session.activity_type == ActivityType::Idle)
        .count();
    let untracked_session_count = sorted
        .iter()
        .filter(|session| session.activity_type == ActivityType::Untracked)
        .count();
    let missing_title_count = sorted
        .iter()
        .filter(|session| session_missing_title(session))
        .count();
    let browser_session_count = sorted
        .iter()
        .filter(|session| is_browser(&session.bundle_id))
        .count();
    let browser_missing_url_count = sorted
        .iter()
        .filter(|session| browser_missing_url(session))
        .count();
    let browser_blank_tab_count = sorted
        .iter()
        .filter(|session| browser_blank_tab(session))
        .count();
    let uncategorized_session_count = sorted
        .iter()
        .filter(|session| session.category == "Uncategorized")
        .count();
    let missing_title_by_app = quality_rows(
        sorted
            .iter()
            .filter(|session| session_missing_title(session))
            .map(app_identity),
    );
    let browser_missing_url_by_app = quality_rows(
        sorted
            .iter()
            .filter(|session| browser_missing_url(session))
            .map(app_identity),
    );
    let browser_missing_url_by_title = quality_rows(
        sorted
            .iter()
            .filter(|session| browser_missing_url(session))
            .map(|session| {
                session
                    .title
                    .as_deref()
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or("<missing title>")
                    .to_string()
            }),
    );
    let browser_blank_tab_by_app = quality_rows(
        sorted
            .iter()
            .filter(|session| browser_blank_tab(session))
            .map(app_identity),
    );
    let uncategorized_by_app = quality_rows(
        sorted
            .iter()
            .filter(|session| session.category == "Uncategorized")
            .map(app_identity),
    );
    let quality_issues = quality_issue_rows(&sorted, MAX_AUDIT_QUALITY_ISSUES);
    let gap_threshold_seconds = gap_threshold_seconds.max(0.0);

    for pair in sorted.windows(2) {
        let Some(previous) = pair.first() else {
            continue;
        };
        let Some(next) = pair.get(1) else {
            continue;
        };

        if let Some(duration_seconds) = seconds_between(previous.end_time, next.start_time)
            && duration_seconds >= gap_threshold_seconds
        {
            gaps.push(AuditGap {
                start_time: previous.end_time,
                end_time: next.start_time,
                duration_seconds,
                previous_app: previous.app_name.clone(),
                next_app: next.app_name.clone(),
            });
            continue;
        }

        if next.start_time < previous.end_time {
            let overlap_end = min_datetime(previous.end_time, next.end_time);
            if let Some(duration_seconds) = seconds_between(next.start_time, overlap_end) {
                overlaps.push(AuditOverlap {
                    start_time: next.start_time,
                    end_time: overlap_end,
                    duration_seconds,
                    first_app: previous.app_name.clone(),
                    second_app: next.app_name.clone(),
                });
            }
        }
    }

    let total_gap_seconds = gaps.iter().map(|gap| gap.duration_seconds).sum();
    let longest_gap_seconds = gaps
        .iter()
        .map(|gap| gap.duration_seconds)
        .fold(0.0, f64::max);

    ActivityAudit {
        session_count: sessions.len(),
        gap_count: gaps.len(),
        overlap_count: overlaps.len(),
        invalid_session_count: invalid_sessions.len(),
        active_session_count,
        idle_session_count,
        untracked_session_count,
        missing_title_count,
        browser_session_count,
        browser_missing_url_count,
        browser_blank_tab_count,
        uncategorized_session_count,
        missing_title_by_app,
        browser_missing_url_by_app,
        browser_missing_url_by_title,
        browser_blank_tab_by_app,
        uncategorized_by_app,
        quality_issues,
        total_gap_seconds,
        longest_gap_seconds,
        gaps,
        overlaps,
        invalid_sessions,
    }
}

#[must_use]
pub fn timeline_blocks(sessions: &[UsageSession]) -> Vec<TimelineBlock> {
    let mut sorted = sessions.to_vec();
    sorted.sort_by_key(|session| (session.start_time, session.end_time));
    let mut blocks = Vec::new();

    for session in sorted {
        let domain = session.url.as_deref().and_then(domain_from_url);
        if let Some(block) = blocks.last_mut()
            && can_merge_timeline_block(block, &session, domain.as_deref())
        {
            merge_timeline_session(block, &session, domain);
            continue;
        }

        blocks.push(TimelineBlock {
            start_time: session.start_time,
            end_time: session.end_time,
            duration_seconds: session.duration_seconds,
            activity_type: session.activity_type,
            category: session.category,
            app_name: session.app_name,
            bundle_id: session.bundle_id,
            domain,
            title: session.title,
            url: session.url,
            session_count: 1,
        });
    }

    blocks
}

#[must_use]
pub fn filter_sessions(
    sessions: Vec<UsageSession>,
    app: Option<&str>,
    title: Option<&str>,
    category: Option<&str>,
    domain: Option<&str>,
    activity_type: Option<&str>,
    limit: Option<usize>,
) -> Vec<UsageSession> {
    let app = app.map(str::to_lowercase);
    let title = title.map(str::to_lowercase);
    let category = category.map(str::to_lowercase);
    let domain = domain.map(str::to_lowercase);
    let activity_type = activity_type.map(str::to_lowercase);

    let mut filtered: Vec<_> = sessions
        .into_iter()
        .filter(|session| {
            app.as_ref().is_none_or(|needle| {
                session.app_name.to_lowercase().contains(needle)
                    || session.bundle_id.to_lowercase().contains(needle)
            })
        })
        .filter(|session| {
            title.as_ref().is_none_or(|needle| {
                session
                    .title
                    .as_deref()
                    .is_some_and(|title| title.to_lowercase().contains(needle))
            })
        })
        .filter(|session| {
            category
                .as_ref()
                .is_none_or(|needle| session.category.to_lowercase().contains(needle))
        })
        .filter(|session| {
            domain.as_ref().is_none_or(|needle| {
                session
                    .url
                    .as_deref()
                    .and_then(domain_from_url)
                    .is_some_and(|host| host.contains(needle))
            })
        })
        .filter(|session| {
            activity_type
                .as_ref()
                .is_none_or(|needle| session.activity_type.as_str().contains(needle))
        })
        .collect();

    filtered.sort_by_key(|session| session.start_time);
    if let Some(limit) = limit {
        filtered.truncate(limit);
    }
    filtered
}

#[must_use]
pub fn filter_sessions_by_time_window(
    sessions: Vec<UsageSession>,
    window_start: Option<DateTime<Local>>,
    window_end: Option<DateTime<Local>>,
) -> Vec<UsageSession> {
    let mut filtered: Vec<_> = sessions
        .into_iter()
        .filter(|session| {
            window_start.is_none_or(|start| session.end_time > start)
                && window_end.is_none_or(|end| session.start_time < end)
        })
        .collect();
    filtered.sort_by_key(|session| (session.start_time, session.end_time));
    filtered
}

pub fn query_time_window(
    input: QueryTimeWindowInput<'_>,
    now: DateTime<Local>,
) -> Result<QueryTimeWindow> {
    if input.last_minutes.is_some()
        && (input.from.is_some()
            || input.to.is_some()
            || input.since.is_some()
            || input.until.is_some())
    {
        return Err(TrackerError::ConflictingQueryWindowArgs(
            "--last-minutes cannot be combined with --from, --to, --since, or --until",
        ));
    }
    if input.from.is_some() && input.since.is_some() {
        return Err(TrackerError::ConflictingQueryWindowArgs(
            "--from and --since both set a query start",
        ));
    }
    if input.to.is_some() && input.until.is_some() {
        return Err(TrackerError::ConflictingQueryWindowArgs(
            "--to and --until both set a query end",
        ));
    }

    let from_date = input.from.map(parse_date).transpose()?;
    let to_date = input.to.map(parse_date).transpose()?;
    if let (Some(from_date), Some(to_date)) = (from_date, to_date)
        && to_date < from_date
    {
        return Err(TrackerError::InvalidDateRange {
            from: input.from.unwrap_or_default().to_string(),
            to: input.to.unwrap_or_default().to_string(),
        });
    }

    let since = input.since.map(parse_local_datetime).transpose()?;
    let until = input.until.map(parse_local_datetime).transpose()?;
    if let (Some(since), Some(until)) = (since, until)
        && until < since
    {
        return Err(TrackerError::InvalidTimeRange {
            since: input.since.unwrap_or_default().to_string(),
            until: input.until.unwrap_or_default().to_string(),
        });
    }

    let last_window = input
        .last_minutes
        .map(|minutes| last_minutes_bounds(minutes, now))
        .transpose()?;
    let start = match (last_window, since, from_date) {
        (Some((start, _)), _, _) | (None, Some(start), _) => Some(start),
        (None, None, Some(date)) => Some(day_bounds(date)?.0),
        (None, None, None) => None,
    };
    let end = match (last_window, until, to_date) {
        (Some((_, end)), _, _) | (None, Some(end), _) => Some(end),
        (None, None, Some(date)) => Some(day_bounds(date)?.1),
        (None, None, None) => None,
    };

    Ok(QueryTimeWindow {
        from: from_date,
        to: to_date,
        since,
        until,
        last_minutes: input.last_minutes,
        start,
        end,
    })
}

pub fn parse_date(input: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(input, "%Y-%m-%d")
        .map_err(|_| TrackerError::InvalidDate(input.to_string()))
}

pub fn parse_local_datetime(input: &str) -> Result<DateTime<Local>> {
    DateTime::parse_from_rfc3339(input)
        .map(|value| value.with_timezone(&Local))
        .map_err(|_| TrackerError::InvalidTimestamp(input.to_string()))
}

pub fn day_bounds(date: NaiveDate) -> Result<(DateTime<Local>, DateTime<Local>)> {
    let start = local_midnight(date)?;
    let next_day = date.succ_opt().ok_or(TrackerError::InvalidLocalDay(date))?;
    let end = local_midnight(next_day)?;
    Ok((start, end))
}

#[must_use]
pub fn category_for_activity(bundle_id: &str, name: &str, url: Option<&str>) -> String {
    url.and_then(category_for_url)
        .map(str::to_string)
        .unwrap_or_else(|| category_for(bundle_id, name))
}

#[must_use]
pub fn category_for_session(session: &UsageSession) -> String {
    match session.activity_type {
        ActivityType::Idle => return "Idle".to_string(),
        ActivityType::Untracked => return "Untracked".to_string(),
        ActivityType::Active => {}
    }
    category_for_activity(
        &session.bundle_id,
        &session.app_name,
        session.url.as_deref(),
    )
}

#[must_use]
pub fn category_for(bundle_id: &str, name: &str) -> String {
    match bundle_id {
        "company.thebrowser.Browser"
        | "company.thebrowser.dia"
        | "com.apple.Safari"
        | "com.brave.Browser"
        | "com.google.Chrome"
        | "com.google.Chrome.canary"
        | "com.microsoft.edgemac"
        | "org.mozilla.firefox" => "Browser",
        "com.apple.Terminal" | "com.googlecode.iterm2" => "Terminal",
        "com.apple.dt.Xcode"
        | "com.microsoft.VSCode"
        | "com.mitchellh.ghostty"
        | "com.warp.dev"
        | "com.todesktop.230313mzl4w4u92"
        | "com.cmuxterm.app"
        | "dev.zed.Zed" => "Development",
        "com.apple.mail" | "com.microsoft.Outlook" => "Email",
        "com.figma.Desktop" | "com.bohemiancoding.sketch3" | "dev.pencil.desktop" => "Design",
        "com.microsoft.teams2"
        | "net.whatsapp.WhatsApp"
        | "com.tinyspeck.slackmacgap"
        | "com.apple.MobileSMS"
        | "us.zoom.xos" => "Communication",
        "com.electron.wispr-flow" => "Productivity",
        "com.apple.Notes" | "com.apple.Preview" | "com.apple.TextEdit" | "com.notion.id" => {
            "Writing"
        }
        "com.apple.finder" | "com.apple.systempreferences" | "com.apple.systemsettings" => "System",
        _ if name.eq_ignore_ascii_case("Finder") => "System",
        _ => "Uncategorized",
    }
    .to_string()
}

#[must_use]
pub fn category_for_url(url: &str) -> Option<&'static str> {
    let host = domain_from_url(url)?;
    if host_matches(&host, "slack.com") || host_matches(&host, "whatsapp.com") {
        return Some("Communication");
    }
    if host == "mail.google.com" {
        return Some("Email");
    }
    if host == "calendar.google.com" {
        return Some("Calendar");
    }
    if host_matches(&host, "github.com")
        || host == "localhost"
        || host_matches(&host, "leanscale.com")
    {
        return Some("Development");
    }
    if host_matches(&host, "claude.ai") || host_matches(&host, "chating.io") {
        return Some("AI");
    }
    if host_matches(&host, "figma.com")
        || host_matches(&host, "mermaid.ai")
        || host_matches(&host, "ilograph.com")
    {
        return Some("Design");
    }
    if host_matches(&host, "clickup.com")
        || host_matches(&host, "notion.so")
        || host_matches(&host, "ottokeep.com")
    {
        return Some("Productivity");
    }
    if host_matches(&host, "x.com") || host_matches(&host, "twitter.com") {
        return Some("Social");
    }
    if host_matches(&host, "google.com") {
        return Some("Research");
    }
    None
}

fn host_matches(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

#[must_use]
pub fn idle_entity() -> ActiveEntity {
    ActiveEntity {
        bundle_id: IDLE_BUNDLE_ID.to_string(),
        name: "Idle".to_string(),
        title: None,
        url: None,
        category: "Idle".to_string(),
        activity_type: ActivityType::Idle,
    }
}

#[must_use]
pub fn untracked_session(
    start_time: DateTime<Local>,
    end_time: DateTime<Local>,
) -> Option<UsageSession> {
    seconds_between(start_time, end_time).map(|duration_seconds| UsageSession {
        start_time,
        end_time,
        duration_seconds,
        app_name: "Untracked".to_string(),
        bundle_id: UNTRACKED_BUNDLE_ID.to_string(),
        title: None,
        category: "Untracked".to_string(),
        url: None,
        activity_type: ActivityType::Untracked,
    })
}

#[must_use]
pub fn domain_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }

    let after_scheme = trimmed
        .split_once("://")
        .map_or(trimmed, |(_, remainder)| remainder);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let without_user = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = without_user
        .split_once(':')
        .map_or(without_user, |(host, _)| host)
        .trim()
        .trim_start_matches("www.")
        .to_lowercase();

    if host.is_empty() { None } else { Some(host) }
}

pub trait ActivityProbe {
    fn active_entity(&self) -> Result<Option<ActiveEntity>>;

    fn idle_seconds(&self) -> Result<Option<f64>> {
        Ok(None)
    }
}

#[derive(Debug, Default)]
pub struct MacOsProbe;

pub const BROWSER_NEW_TAB_URL: &str = "about:newtab";
const BROWSER_TAB_FIELD_SEPARATOR: &str = "__ACTIVITY_TRACKER_BROWSER_FIELD__";

#[derive(Debug, Clone)]
struct ActiveAppSnapshot {
    bundle_id: String,
    name: String,
    title: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct BrowserTabSnapshot {
    title: Option<String>,
    url: Option<String>,
}

impl ActivityProbe for MacOsProbe {
    fn active_entity(&self) -> Result<Option<ActiveEntity>> {
        let Some(active_app) = active_app_snapshot()? else {
            return Ok(None);
        };
        let ActiveAppSnapshot {
            bundle_id,
            name,
            title: native_title,
        } = active_app;
        let (title, url) = if is_browser(&bundle_id) {
            let browser_tab = browser_tab_snapshot(&bundle_id).unwrap_or_default();
            let title = browser_tab.title;
            let url = normalize_browser_tab_url(title.as_deref(), browser_tab.url);
            (title, url)
        } else {
            (
                native_title.or_else(|| non_empty_string(name.clone())),
                None,
            )
        };
        let category = category_for_activity(&bundle_id, &name, url.as_deref());
        Ok(Some(ActiveEntity {
            bundle_id,
            name,
            title,
            url,
            category,
            activity_type: ActivityType::Active,
        }))
    }

    fn idle_seconds(&self) -> Result<Option<f64>> {
        hid_idle_seconds()
    }
}

fn normalize_browser_tab_url(title: Option<&str>, url: Option<String>) -> Option<String> {
    if title.is_some_and(is_browser_blank_tab_title) {
        return Some(BROWSER_NEW_TAB_URL.to_string());
    }
    url.map(|url| {
        if is_browser_blank_tab_url(&url) {
            BROWSER_NEW_TAB_URL.to_string()
        } else {
            url
        }
    })
}

#[must_use]
pub fn is_browser(bundle_id: &str) -> bool {
    matches!(
        bundle_id,
        "company.thebrowser.Browser"
            | "company.thebrowser.dia"
            | "com.apple.Safari"
            | "com.brave.Browser"
            | "com.google.Chrome"
            | "com.google.Chrome.canary"
            | "com.microsoft.edgemac"
    )
}

pub fn run_osascript(script: &str) -> Result<String> {
    let output = output_with_timeout(
        Command::new("osascript").arg("-e").arg(script),
        StdDuration::from_secs(2),
        "osascript",
    )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(TrackerError::AppleScript(stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() || stdout == "missing value" {
        return Err(TrackerError::AppleScript(
            "AppleScript returned no value".to_string(),
        ));
    }
    Ok(stdout)
}

pub fn active_app_info() -> Result<Option<(String, String)>> {
    Ok(active_app_snapshot()?.map(|snapshot| (snapshot.bundle_id, snapshot.name)))
}

fn active_app_snapshot() -> Result<Option<ActiveAppSnapshot>> {
    let script = r#"tell application "System Events"
set frontApp to first application process whose frontmost is true
set appName to name of frontApp
set bundleId to bundle identifier of frontApp
set contextTitle to ""
try
  set frontWindow to window 1 of frontApp
  try
    set windowName to name of frontWindow
    if windowName is not missing value and windowName is not "" then set contextTitle to windowName
  end try
  if contextTitle is "" then
    try
      set axTitle to value of attribute "AXTitle" of frontWindow
      if axTitle is not missing value and axTitle is not "" then set contextTitle to axTitle
    end try
  end if
  if contextTitle is "" then
    try
      set axDocument to value of attribute "AXDocument" of frontWindow
      if axDocument is not missing value and axDocument is not "" then set contextTitle to axDocument
    end try
  end if
end try
if contextTitle is "" then
  try
    set processTitle to title of frontApp
    if processTitle is not missing value and processTitle is not "" then set contextTitle to processTitle
  end try
end if
if contextTitle is "" then
  try
    set displayName to displayed name of frontApp
    if displayName is not missing value and displayName is not "" then set contextTitle to displayName
  end try
end if
if contextTitle is "" then set contextTitle to appName
return bundleId & linefeed & appName & linefeed & contextTitle
end tell"#;

    match run_osascript(script) {
        Ok(output) => Ok(parse_active_app_snapshot(&output)),
        Err(error) => {
            tracing::warn!(error = %error, "active app probe failed");
            Ok(None)
        }
    }
}

fn parse_active_app_snapshot(output: &str) -> Option<ActiveAppSnapshot> {
    let mut lines = output.lines();
    let bundle_id = lines
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let name = lines
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let title = lines.next().map(str::trim).and_then(|value| {
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    });
    Some(ActiveAppSnapshot {
        bundle_id: bundle_id.to_string(),
        name: name.to_string(),
        title,
    })
}

fn browser_tab_snapshot(bundle_id: &str) -> Option<BrowserTabSnapshot> {
    if !is_browser(bundle_id) {
        return None;
    }

    let separator = escape_applescript_string(BROWSER_TAB_FIELD_SEPARATOR);
    let script = if bundle_id == "com.apple.Safari" {
        format!(
            r#"tell application id "com.apple.Safari"
set tabTitle to ""
set tabUrl to ""
try
  set tabTitle to name of current tab of front window
end try
try
  set tabUrl to URL of current tab of front window
end try
return tabTitle & "{}" & tabUrl
end tell"#,
            separator
        )
    } else {
        format!(
            r#"tell application id "{}"
set tabTitle to ""
set tabUrl to ""
try
  set tabTitle to title of active tab of front window
end try
try
  set tabUrl to URL of active tab of front window
end try
return tabTitle & "{}" & tabUrl
end tell"#,
            escape_applescript_string(bundle_id),
            separator
        )
    };

    match run_osascript(&script) {
        Ok(output) => Some(parse_browser_tab_snapshot(&output)),
        Err(error) => {
            tracing::debug!(bundle_id, error = %error, "browser tab probe failed");
            None
        }
    }
}

fn parse_browser_tab_snapshot(output: &str) -> BrowserTabSnapshot {
    let (title, url) = output
        .split_once(BROWSER_TAB_FIELD_SEPARATOR)
        .unwrap_or((output, ""));
    BrowserTabSnapshot {
        title: non_empty_borrowed(title).map(str::to_string),
        url: non_empty_borrowed(url).map(str::to_string),
    }
}

pub fn browser_tab_url(bundle_id: &str) -> Option<String> {
    browser_tab_snapshot(bundle_id).and_then(|snapshot| snapshot.url)
}

pub fn browser_tab_title(bundle_id: &str) -> Option<String> {
    browser_tab_snapshot(bundle_id).and_then(|snapshot| snapshot.title)
}

pub fn active_window_title() -> Option<String> {
    let script = r#"tell application "System Events"
set frontApp to first application process whose frontmost is true
try
  set frontWindow to window 1 of frontApp
on error
  return ""
end try
try
  set windowName to name of frontWindow
  if windowName is not missing value and windowName is not "" then return windowName
end try
try
  set axTitle to value of attribute "AXTitle" of frontWindow
  if axTitle is not missing value and axTitle is not "" then return axTitle
end try
try
  set axDocument to value of attribute "AXDocument" of frontWindow
  if axDocument is not missing value and axDocument is not "" then return axDocument
end try
return ""
end tell"#;

    match run_osascript(script) {
        Ok(title) => non_empty_string(title),
        Err(error) => {
            tracing::debug!(error = %error, "window title probe failed");
            None
        }
    }
}

pub fn hid_idle_seconds() -> Result<Option<f64>> {
    let output = output_with_timeout(
        Command::new("ioreg").args(["-c", "IOHIDSystem", "-r", "-d", "1"]),
        StdDuration::from_secs(2),
        "ioreg",
    )?;
    if !output.status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_hid_idle_nanoseconds(&stdout).map(|nanos| nanos as f64 / 1_000_000_000.0))
}

#[must_use]
pub fn parse_hid_idle_nanoseconds(output: &str) -> Option<u64> {
    output.lines().find_map(|line| {
        let (_, raw_value) = line.split_once("\"HIDIdleTime\" = ")?;
        raw_value.trim().parse::<u64>().ok()
    })
}

pub fn install_launch_agent(binary: &Path, store: &LogStore, load: bool) -> Result<PathBuf> {
    store.ensure_dirs()?;
    let plist_path = launch_agent_path()?;
    if let Some(parent) = plist_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let stdout = store.logs_dir().join("launchd.out.log");
    let stderr = store.logs_dir().join("launchd.err.log");
    let plist = launch_agent_plist(binary, &stdout, &stderr);

    if load {
        let _bootout_result = launchctl_bootout();
        wait_for_service_unloaded(StdDuration::from_secs(5))?;
    }

    fs::write(&plist_path, plist)?;

    if load {
        thread::sleep(StdDuration::from_millis(250));
        launchctl_bootstrap(&plist_path)?;
        launchctl_kickstart()?;
    }

    Ok(plist_path)
}

pub fn uninstall_launch_agent(unload: bool) -> Result<PathBuf> {
    let plist_path = launch_agent_path()?;
    if unload {
        let _result = launchctl_bootout();
    }
    if plist_path.exists() {
        fs::remove_file(&plist_path)?;
    }
    Ok(plist_path)
}

pub fn service_status() -> Result<String> {
    let target = launchctl_target()?;
    let output = Command::new("launchctl")
        .args(["print", &format!("{target}/{SERVICE_LABEL}")])
        .output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(TrackerError::Command {
            command: "launchctl print".to_string(),
            stderr,
        })
    }
}

pub fn service_status_report() -> ServiceStatusReport {
    match service_status() {
        Ok(raw) => parse_service_status(&raw),
        Err(error) => ServiceStatusReport {
            label: SERVICE_LABEL.to_string(),
            loaded: false,
            running: false,
            pid: None,
            raw: None,
            error: Some(error.to_string()),
        },
    }
}

#[must_use]
pub fn parse_service_status(raw: &str) -> ServiceStatusReport {
    let running = raw.lines().any(|line| line.trim() == "state = running");
    let pid = raw.lines().find_map(parse_service_pid);
    ServiceStatusReport {
        label: SERVICE_LABEL.to_string(),
        loaded: true,
        running,
        pid,
        raw: Some(raw.to_string()),
        error: None,
    }
}

fn parse_service_pid(line: &str) -> Option<u32> {
    line.trim().strip_prefix("pid = ")?.parse().ok()
}

#[must_use]
pub fn launch_agent_plist(binary: &Path, stdout: &Path, stderr: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{binary}</string>
    <string>track</string>
    <string>--quiet</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{stdout}</string>
  <key>StandardErrorPath</key>
  <string>{stderr}</string>
</dict>
</plist>
"#,
        label = SERVICE_LABEL,
        binary = xml_escape(&binary.display().to_string()),
        stdout = xml_escape(&stdout.display().to_string()),
        stderr = xml_escape(&stderr.display().to_string())
    )
}

#[must_use]
pub fn format_seconds(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    let hours = total / 3_600;
    let minutes = (total % 3_600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn summarize_with_seconds<F>(sessions: &[UsageSession], seconds_for: F) -> ActivitySummary
where
    F: Fn(&UsageSession) -> f64,
{
    let mut total_seconds = 0.0;
    let mut by_activity_type = HashMap::<String, f64>::new();
    let mut by_category = HashMap::<String, f64>::new();
    let mut by_app = HashMap::<String, f64>::new();
    let mut by_domain = HashMap::<String, f64>::new();

    for session in sessions {
        let seconds = seconds_for(session);
        if seconds <= 0.0 {
            continue;
        }
        total_seconds += seconds;
        *by_activity_type
            .entry(session.activity_type.to_string())
            .or_default() += seconds;
        *by_category.entry(session.category.clone()).or_default() += seconds;
        *by_app
            .entry(format!("{} ({})", session.app_name, session.bundle_id))
            .or_default() += seconds;
        if let Some(domain) = session.url.as_deref().and_then(domain_from_url) {
            *by_domain.entry(domain).or_default() += seconds;
        }
    }

    ActivitySummary {
        session_count: sessions.len(),
        total_seconds,
        by_activity_type: sorted_rows(by_activity_type, total_seconds),
        by_category: sorted_rows(by_category, total_seconds),
        by_app: sorted_rows(by_app, total_seconds),
        by_domain: sorted_rows(by_domain, total_seconds),
    }
}

fn sorted_rows(map: HashMap<String, f64>, total_seconds: f64) -> Vec<SummaryRow> {
    let mut rows: Vec<_> = map
        .into_iter()
        .map(|(name, seconds)| SummaryRow {
            name,
            seconds,
            percentage: if total_seconds > 0.0 {
                (seconds / total_seconds) * 100.0
            } else {
                0.0
            },
        })
        .collect();
    rows.sort_by(|a, b| {
        b.seconds
            .partial_cmp(&a.seconds)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows
}

fn quality_rows<I>(names: I) -> Vec<AuditQualityRow>
where
    I: IntoIterator<Item = String>,
{
    let mut counts = HashMap::<String, usize>::new();
    for name in names {
        *counts.entry(name).or_default() += 1;
    }

    let mut rows: Vec<_> = counts
        .into_iter()
        .map(|(name, count)| AuditQualityRow { name, count })
        .collect();
    rows.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    rows
}

fn quality_issue_rows(sessions: &[UsageSession], limit: usize) -> Vec<AuditQualityIssue> {
    let mut rows = Vec::new();
    for session in sessions {
        if session_missing_title(session) {
            rows.push(quality_issue_row(
                AuditQualityIssueKind::MissingTitle,
                session,
            ));
        }
        if rows.len() >= limit {
            break;
        }
        if browser_missing_url(session) {
            rows.push(quality_issue_row(
                AuditQualityIssueKind::BrowserMissingUrl,
                session,
            ));
        }
        if rows.len() >= limit {
            break;
        }
        if session.category == "Uncategorized" {
            rows.push(quality_issue_row(
                AuditQualityIssueKind::Uncategorized,
                session,
            ));
        }
        if rows.len() >= limit {
            break;
        }
    }
    rows
}

fn quality_issue_row(kind: AuditQualityIssueKind, session: &UsageSession) -> AuditQualityIssue {
    AuditQualityIssue {
        kind,
        start_time: session.start_time,
        end_time: session.end_time,
        duration_seconds: session.duration_seconds,
        app_name: session.app_name.clone(),
        bundle_id: session.bundle_id.clone(),
        title: session.title.clone(),
        category: session.category.clone(),
        url: session.url.clone(),
        activity_type: session.activity_type,
    }
}

fn app_identity(session: &UsageSession) -> String {
    format!("{} ({})", session.app_name, session.bundle_id)
}

fn session_missing_title(session: &UsageSession) -> bool {
    session.activity_type == ActivityType::Active
        && !browser_blank_tab(session)
        && session
            .title
            .as_deref()
            .is_none_or(|title| title.trim().is_empty())
}

fn browser_blank_tab(session: &UsageSession) -> bool {
    is_browser(&session.bundle_id)
        && (session
            .title
            .as_deref()
            .is_some_and(is_browser_blank_tab_title)
            || session.url.as_deref().is_some_and(is_browser_blank_tab_url))
}

fn is_browser_blank_tab_title(title: &str) -> bool {
    matches!(
        title.trim().to_ascii_lowercase().as_str(),
        "new tab" | "start page"
    )
}

fn is_browser_blank_tab_url(url: &str) -> bool {
    matches!(
        url.trim().to_ascii_lowercase().as_str(),
        "about:blank"
            | "about:newtab"
            | "about://newtab"
            | "chrome://newtab/"
            | "brave://newtab/"
            | "edge://newtab/"
    )
}

fn browser_missing_url(session: &UsageSession) -> bool {
    is_browser(&session.bundle_id)
        && !browser_blank_tab(session)
        && session
            .url
            .as_deref()
            .is_none_or(|url| url.trim().is_empty())
}

#[derive(Debug, Clone)]
struct UrlRepair {
    url: String,
    source: UrlRepairSource,
}

#[derive(Debug, Clone, Copy)]
enum UrlRepairSource {
    BlankTabUrl,
    BlankTabContext,
}

fn repaired_url_for_session(
    session: &UsageSession,
    previous: Option<&UsageSession>,
    next: Option<&UsageSession>,
) -> Option<UrlRepair> {
    if session.activity_type != ActivityType::Active || !is_browser(&session.bundle_id) {
        return None;
    }

    if let Some(url) = session.url.as_deref() {
        return (is_browser_blank_tab_url(url) && url.trim() != BROWSER_NEW_TAB_URL).then(|| {
            UrlRepair {
                url: BROWSER_NEW_TAB_URL.to_string(),
                source: UrlRepairSource::BlankTabUrl,
            }
        });
    }

    let previous_blank = previous.is_some_and(browser_blank_tab);
    let next_blank = next.is_some_and(browser_blank_tab);
    (previous_blank && next_blank).then(|| UrlRepair {
        url: BROWSER_NEW_TAB_URL.to_string(),
        source: UrlRepairSource::BlankTabContext,
    })
}

#[derive(Debug, Clone)]
struct TitleRepair {
    title: String,
    source: TitleRepairSource,
}

#[derive(Debug, Clone, Copy)]
enum TitleRepairSource {
    NativeApp,
    UniqueBrowserUrl,
}

fn repaired_title_for_session(
    session: &UsageSession,
    browser_titles: &HashMap<String, String>,
) -> Option<TitleRepair> {
    if session.activity_type != ActivityType::Active {
        return None;
    }

    if is_browser(&session.bundle_id) {
        if !session_missing_title(session) {
            return None;
        }
        let url = session.url.as_deref().and_then(non_empty_borrowed)?;
        let title = browser_titles.get(url)?;
        return Some(TitleRepair {
            title: title.clone(),
            source: TitleRepairSource::UniqueBrowserUrl,
        });
    }

    let title = non_empty_string(session.app_name.clone())?;
    let should_repair = if session.bundle_id == "com.cmuxterm.app" {
        session
            .title
            .as_deref()
            .is_none_or(|current| current.trim() != title)
    } else {
        session_missing_title(session)
    };
    should_repair.then_some(TitleRepair {
        title,
        source: TitleRepairSource::NativeApp,
    })
}

fn unique_browser_titles_by_url(sessions: &[UsageSession]) -> HashMap<String, String> {
    let mut observed = HashMap::<String, Option<String>>::new();
    for session in sessions {
        if session.activity_type != ActivityType::Active || !is_browser(&session.bundle_id) {
            continue;
        }
        let Some(url) = session
            .url
            .as_deref()
            .and_then(non_empty_borrowed)
            .map(str::to_string)
        else {
            continue;
        };
        let Some(title) = session
            .title
            .as_deref()
            .and_then(non_empty_borrowed)
            .map(str::to_string)
        else {
            continue;
        };

        match observed.entry(url) {
            Entry::Vacant(entry) => {
                entry.insert(Some(title));
            }
            Entry::Occupied(mut entry) => {
                if entry.get().as_deref() != Some(title.as_str()) {
                    entry.insert(None);
                }
            }
        }
    }

    observed
        .into_iter()
        .filter_map(|(url, title)| title.map(|title| (url, title)))
        .collect()
}

fn seconds_between(start_time: DateTime<Local>, end_time: DateTime<Local>) -> Option<f64> {
    if end_time <= start_time {
        return None;
    }
    let millis = end_time
        .signed_duration_since(start_time)
        .num_milliseconds();
    Some(millis as f64 / 1_000.0)
}

fn age_seconds(observed_at: DateTime<Local>, now: DateTime<Local>) -> f64 {
    seconds_between(observed_at, now).unwrap_or(0.0)
}

fn latest_observed_at(
    last_completed_session: Option<&UsageSession>,
    open_session: Option<&SessionCheckpoint>,
) -> Option<DateTime<Local>> {
    match (last_completed_session, open_session) {
        (Some(session), Some(checkpoint)) => {
            Some(max_datetime(session.end_time, checkpoint.last_seen_at))
        }
        (Some(session), None) => Some(session.end_time),
        (None, Some(checkpoint)) => Some(checkpoint.last_seen_at),
        (None, None) => None,
    }
}

fn idle_start(
    observed_at: DateTime<Local>,
    idle_seconds: Option<f64>,
    threshold_seconds: u64,
) -> Option<DateTime<Local>> {
    let idle_seconds = idle_seconds?;
    if idle_seconds < threshold_seconds as f64 {
        return None;
    }
    let bounded = idle_seconds.floor().clamp(0.0, i64::MAX as f64) as i64;
    Some(observed_at - TimeDelta::seconds(bounded))
}

fn last_minutes_bounds(
    minutes: u64,
    now: DateTime<Local>,
) -> Result<(DateTime<Local>, DateTime<Local>)> {
    if minutes == 0 {
        return Err(TrackerError::InvalidDuration(minutes.to_string()));
    }
    let minutes =
        i64::try_from(minutes).map_err(|_| TrackerError::InvalidDuration(minutes.to_string()))?;
    Ok((now - TimeDelta::minutes(minutes), now))
}

fn max_datetime(a: DateTime<Local>, b: DateTime<Local>) -> DateTime<Local> {
    if a >= b { a } else { b }
}

fn min_datetime(a: DateTime<Local>, b: DateTime<Local>) -> DateTime<Local> {
    if a <= b { a } else { b }
}

fn invalid_session(session: &UsageSession) -> Option<AuditInvalidSession> {
    if session.end_time <= session.start_time {
        return Some(AuditInvalidSession {
            start_time: session.start_time,
            end_time: session.end_time,
            duration_seconds: session.duration_seconds,
            app_name: session.app_name.clone(),
            reason: "end_time_not_after_start_time".to_string(),
        });
    }

    if session.duration_seconds <= 0.0 {
        return Some(AuditInvalidSession {
            start_time: session.start_time,
            end_time: session.end_time,
            duration_seconds: session.duration_seconds,
            app_name: session.app_name.clone(),
            reason: "non_positive_duration".to_string(),
        });
    }

    None
}

fn can_merge_timeline_block(
    block: &TimelineBlock,
    session: &UsageSession,
    domain: Option<&str>,
) -> bool {
    block.activity_type == session.activity_type
        && block.category == session.category
        && block.app_name == session.app_name
        && block.bundle_id == session.bundle_id
        && block.domain.as_deref() == domain
        && seconds_between(block.end_time, session.start_time).is_none_or(|gap| gap <= 5.0)
}

fn merge_timeline_session(
    block: &mut TimelineBlock,
    session: &UsageSession,
    domain: Option<String>,
) {
    block.end_time = max_datetime(block.end_time, session.end_time);
    block.duration_seconds += session.duration_seconds;
    block.domain = domain.or_else(|| block.domain.clone());
    if session.title.is_some() {
        block.title = session.title.clone();
    }
    if session.url.is_some() {
        block.url = session.url.clone();
    }
    block.session_count += 1;
}

fn local_midnight(date: NaiveDate) -> Result<DateTime<Local>> {
    match Local.with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0) {
        LocalResult::Single(value) | LocalResult::Ambiguous(value, _) => Ok(value),
        LocalResult::None => Err(TrackerError::InvalidLocalDay(date)),
    }
}

fn default_data_dir() -> Result<PathBuf> {
    dirs::home_dir()
        .map(|path| path.join(".activity_tracker"))
        .ok_or(TrackerError::HomeNotFound)
}

#[must_use]
pub fn legacy_data_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|path| path.join("activity_tracker"))
}

#[must_use]
pub fn legacy_sessions_path() -> Option<PathBuf> {
    legacy_data_dir().map(|path| path.join("sessions.jsonl"))
}

fn launch_agent_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or(TrackerError::HomeNotFound)?;
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{SERVICE_LABEL}.plist")))
}

fn launchctl_target() -> Result<String> {
    let output = Command::new("id").arg("-u").output()?;
    if output.status.success() {
        let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(format!("gui/{uid}"))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(TrackerError::Command {
            command: "id -u".to_string(),
            stderr,
        })
    }
}

fn launchctl_bootstrap(plist_path: &Path) -> Result<()> {
    let target = launchctl_target()?;
    let output = Command::new("launchctl")
        .args(["bootstrap", &target, &plist_path.display().to_string()])
        .output()?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.contains("Bootstrap failed: 5") || stderr.contains("Input/output error") {
        launchctl_bootout()?;
        let retry = Command::new("launchctl")
            .args(["bootstrap", &target, &plist_path.display().to_string()])
            .output()?;
        if retry.status.success() {
            return Ok(());
        }
        return Err(TrackerError::Command {
            command: "launchctl bootstrap".to_string(),
            stderr: String::from_utf8_lossy(&retry.stderr).trim().to_string(),
        });
    }

    Err(TrackerError::Command {
        command: "launchctl bootstrap".to_string(),
        stderr,
    })
}

fn launchctl_kickstart() -> Result<()> {
    let target = launchctl_target()?;
    let service = format!("{target}/{SERVICE_LABEL}");
    let output = Command::new("launchctl")
        .args(["kickstart", "-k", &service])
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(TrackerError::Command {
            command: "launchctl kickstart".to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

fn launchctl_bootout() -> Result<()> {
    let target = launchctl_target()?;
    let service = format!("{target}/{SERVICE_LABEL}");
    let output = Command::new("launchctl")
        .args(["bootout", &service])
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(TrackerError::Command {
            command: "launchctl bootout".to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

fn wait_for_service_unloaded(timeout: StdDuration) -> Result<()> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if !launchctl_service_loaded()? {
            return Ok(());
        }
        thread::sleep(StdDuration::from_millis(100));
    }
    Ok(())
}

fn launchctl_service_loaded() -> Result<bool> {
    let target = launchctl_target()?;
    let service = format!("{target}/{SERVICE_LABEL}");
    let output = output_with_timeout(
        Command::new("launchctl").args(["print", &service]),
        StdDuration::from_secs(2),
        "launchctl print",
    )?;
    Ok(output.status.success())
}

fn escape_applescript_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn non_empty_string(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SessionKey {
    start_time: String,
    end_time: String,
    app_name: String,
    bundle_id: String,
    title: Option<String>,
    url: Option<String>,
    activity_type: ActivityType,
}

impl From<&UsageSession> for SessionKey {
    fn from(session: &UsageSession) -> Self {
        Self {
            start_time: session.start_time.to_rfc3339(),
            end_time: session.end_time.to_rfc3339(),
            app_name: session.app_name.clone(),
            bundle_id: session.bundle_id.clone(),
            title: session.title.clone(),
            url: session.url.clone(),
            activity_type: session.activity_type,
        }
    }
}

#[derive(Debug, Clone)]
struct CsvColumns {
    start_time: usize,
    end_time: usize,
    duration_seconds: usize,
    app_name: usize,
    bundle_id: usize,
    title: Option<usize>,
    category: usize,
    activity_type: Option<usize>,
    url: usize,
}

impl CsvColumns {
    fn from_headers(headers: &csv::StringRecord) -> Result<Self> {
        Ok(Self {
            start_time: required_column(headers, "Start Time")?,
            end_time: required_column(headers, "End Time")?,
            duration_seconds: required_column(headers, "Duration (seconds)")?,
            app_name: required_column(headers, "App Name")?,
            bundle_id: required_column(headers, "Bundle ID")?,
            title: optional_column(headers, "Title"),
            category: required_column(headers, "Category")?,
            activity_type: optional_column(headers, "Activity Type"),
            url: required_column(headers, "URL")?,
        })
    }

    fn session_from_record(&self, record: &csv::StringRecord) -> Result<UsageSession> {
        let start_time = parse_local_datetime(field(record, self.start_time))?;
        let end_time = parse_local_datetime(field(record, self.end_time))?;
        let duration_seconds = field(record, self.duration_seconds)
            .parse::<f64>()
            .ok()
            .or_else(|| seconds_between(start_time, end_time))
            .unwrap_or(0.0);
        let app_name = field(record, self.app_name).to_string();
        let bundle_id = field(record, self.bundle_id).to_string();
        let url = non_empty_borrowed(field(record, self.url)).map(str::to_string);
        let category = non_empty_borrowed(field(record, self.category))
            .map(str::to_string)
            .unwrap_or_else(|| category_for_activity(&bundle_id, &app_name, url.as_deref()));
        let activity_type = self
            .activity_type
            .map_or(Ok(ActivityType::Active), |idx| field(record, idx).parse())?;

        Ok(UsageSession {
            start_time,
            end_time,
            duration_seconds,
            app_name,
            bundle_id,
            title: self
                .title
                .and_then(|idx| non_empty_borrowed(field(record, idx)).map(str::to_string)),
            category,
            url,
            activity_type,
        })
    }
}

fn required_column(headers: &csv::StringRecord, name: &'static str) -> Result<usize> {
    optional_column(headers, name).ok_or(TrackerError::MissingCsvColumn(name))
}

fn optional_column(headers: &csv::StringRecord, name: &str) -> Option<usize> {
    headers.iter().position(|header| header == name)
}

fn field(record: &csv::StringRecord, idx: usize) -> &str {
    record.get(idx).unwrap_or("")
}

fn table_column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(columns.iter().any(|existing| existing == column))
}

fn non_empty_borrowed(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

#[derive(Debug, Clone)]
struct DbSessionRow {
    start_time: String,
    end_time: String,
    duration_seconds: f64,
    app_name: String,
    bundle_id: String,
    title: Option<String>,
    category: String,
    url: Option<String>,
    activity_type: String,
}

#[derive(Debug, Clone)]
struct DbEpochBackfillRow {
    id: i64,
    start_time: String,
    end_time: String,
}

fn db_session_row(row: &Row<'_>) -> rusqlite::Result<DbSessionRow> {
    Ok(DbSessionRow {
        start_time: row.get(0)?,
        end_time: row.get(1)?,
        duration_seconds: row.get(2)?,
        app_name: row.get(3)?,
        bundle_id: row.get(4)?,
        title: row.get(5)?,
        category: row.get(6)?,
        url: row.get(7)?,
        activity_type: row.get(8)?,
    })
}

impl DbSessionRow {
    fn into_session(self) -> Result<UsageSession> {
        Ok(UsageSession {
            start_time: parse_local_datetime(&self.start_time)?,
            end_time: parse_local_datetime(&self.end_time)?,
            duration_seconds: self.duration_seconds,
            app_name: self.app_name,
            bundle_id: self.bundle_id,
            title: self.title,
            category: self.category,
            url: self.url,
            activity_type: self.activity_type.parse()?,
        })
    }
}

#[derive(Debug, Clone)]
struct DbCheckpointRow {
    start_time: String,
    last_seen_at: String,
    app_name: String,
    bundle_id: String,
    title: Option<String>,
    category: String,
    url: Option<String>,
    activity_type: String,
}

impl DbCheckpointRow {
    fn into_checkpoint(self) -> Result<SessionCheckpoint> {
        Ok(SessionCheckpoint {
            start_time: parse_local_datetime(&self.start_time)?,
            last_seen_at: parse_local_datetime(&self.last_seen_at)?,
            entity: ActiveEntity {
                bundle_id: self.bundle_id,
                name: self.app_name,
                title: self.title,
                url: self.url,
                category: self.category,
                activity_type: self.activity_type.parse()?,
            },
        })
    }
}

fn recovered_checkpoint_end(
    checkpoint: &SessionCheckpoint,
    now: DateTime<Local>,
    recent_gap_seconds: u64,
) -> DateTime<Local> {
    if seconds_between(checkpoint.last_seen_at, now)
        .is_some_and(|seconds| seconds <= recent_gap_seconds as f64)
    {
        now
    } else {
        checkpoint.last_seen_at
    }
}

fn load_jsonl_sessions_from_path(path: &Path) -> Result<Vec<UsageSession>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut sessions = Vec::new();

    for (idx, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let session = serde_json::from_str::<UsageSession>(trimmed).map_err(|source| {
            TrackerError::JsonLine {
                path: path.to_path_buf(),
                line: idx + 1,
                source,
            }
        })?;
        sessions.push(session);
    }

    Ok(sessions)
}

fn output_with_timeout(command: &mut Command, timeout: StdDuration, label: &str) -> Result<Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let started = Instant::now();

    loop {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }
        if started.elapsed() >= timeout {
            let _kill_result = child.kill();
            let _wait_result = child.wait();
            return Err(TrackerError::CommandTimeout(label.to_string()));
        }
        thread::sleep(StdDuration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result as AnyhowResult;
    use chrono::TimeZone;

    fn entity() -> ActiveEntity {
        ActiveEntity {
            bundle_id: "com.google.Chrome".to_string(),
            name: "Google Chrome".to_string(),
            title: Some("Example Project".to_string()),
            url: Some("https://www.example.com/path".to_string()),
            category: "Browser".to_string(),
            activity_type: ActivityType::Active,
        }
    }

    #[test]
    fn active_app_snapshot_parser_keeps_native_title() -> AnyhowResult<()> {
        let snapshot = parse_active_app_snapshot("com.cmuxterm.app\ncmux\ncmux\n")
            .ok_or_else(|| anyhow::anyhow!("snapshot should parse"))?;

        assert_eq!(snapshot.bundle_id, "com.cmuxterm.app");
        assert_eq!(snapshot.name, "cmux");
        assert_eq!(snapshot.title.as_deref(), Some("cmux"));
        Ok(())
    }

    #[test]
    fn browser_tab_snapshot_parser_keeps_url_when_title_missing() {
        let snapshot = parse_browser_tab_snapshot(&format!(
            "{BROWSER_TAB_FIELD_SEPARATOR}https://github.com/ertyurk/activity_tracker"
        ));

        assert_eq!(snapshot.title, None);
        assert_eq!(
            snapshot.url.as_deref(),
            Some("https://github.com/ertyurk/activity_tracker")
        );
    }

    #[test]
    fn browser_tab_snapshot_parser_keeps_title_when_url_missing() {
        let snapshot = parse_browser_tab_snapshot(&format!("New Tab{BROWSER_TAB_FIELD_SEPARATOR}"));

        assert_eq!(snapshot.title.as_deref(), Some("New Tab"));
        assert_eq!(snapshot.url, None);
        assert_eq!(
            normalize_browser_tab_url(snapshot.title.as_deref(), snapshot.url),
            Some(BROWSER_NEW_TAB_URL.to_string())
        );
    }

    #[test]
    fn session_duration_uses_full_start_end_window() -> AnyhowResult<()> {
        let start = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 0, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing start"))?;
        let end = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 5, 30)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing end"))?;

        let session = UsageSession::from_entity(&entity(), start, end)
            .ok_or_else(|| anyhow::anyhow!("missing session"))?;

        assert_eq!(session.duration_seconds, 330.0);
        assert_eq!(session.activity_type, ActivityType::Active);
        assert_eq!(session.title.as_deref(), Some("Example Project"));
        Ok(())
    }

    #[test]
    fn store_round_trips_jsonl_and_filters_day() -> AnyhowResult<()> {
        let dir = tempfile::tempdir()?;
        let store = LogStore::new(dir.path().to_path_buf());
        let start = Local
            .with_ymd_and_hms(2026, 6, 3, 23, 59, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing start"))?;
        let end = Local
            .with_ymd_and_hms(2026, 6, 4, 0, 1, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing end"))?;
        let session = UsageSession::from_entity(&entity(), start, end)
            .ok_or_else(|| anyhow::anyhow!("missing session"))?;

        store.append_session(&session)?;

        let all = store.load_sessions()?;
        assert!(store.db_path().exists());
        assert_eq!(all.len(), 1);
        assert_eq!(store.sessions_for_day(parse_date("2026-06-03")?)?.len(), 1);
        assert_eq!(store.sessions_for_day(parse_date("2026-06-04")?)?.len(), 1);
        let (window_start, window_end) = day_bounds(parse_date("2026-06-03")?)?;
        assert_eq!(
            store
                .sessions_in_window(Some(window_start), Some(window_end))?
                .len(),
            1
        );
        Ok(())
    }

    #[test]
    fn ensure_dirs_backfills_missing_jsonl_mirror_from_db() -> AnyhowResult<()> {
        let dir = tempfile::tempdir()?;
        let store = LogStore::new(dir.path().to_path_buf());
        let start = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 0, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing start"))?;
        let end = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 1, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing end"))?;
        let session = UsageSession::from_entity(&entity(), start, end)
            .ok_or_else(|| anyhow::anyhow!("missing session"))?;

        store.append_session(&session)?;
        fs::remove_file(store.sessions_path())?;
        store.ensure_dirs()?;

        let mirrored = load_jsonl_sessions_from_path(&store.sessions_path())?;
        assert_eq!(mirrored.len(), 1);
        assert_eq!(
            mirrored.first().map(|session| &session.app_name),
            Some(&session.app_name)
        );
        Ok(())
    }

    #[test]
    fn ensure_dirs_migrates_epoch_columns_for_existing_db() -> AnyhowResult<()> {
        let dir = tempfile::tempdir()?;
        let store = LogStore::new(dir.path().to_path_buf());
        fs::create_dir_all(dir.path())?;
        let conn = Connection::open(store.db_path())?;
        conn.execute_batch(
            "CREATE TABLE sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                start_time TEXT NOT NULL,
                end_time TEXT NOT NULL,
                duration_seconds REAL NOT NULL,
                app_name TEXT NOT NULL,
                bundle_id TEXT NOT NULL,
                title TEXT,
                category TEXT NOT NULL,
                url TEXT,
                activity_type TEXT NOT NULL DEFAULT 'active',
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );
            INSERT INTO sessions (
                start_time, end_time, duration_seconds, app_name, bundle_id, title, category, url, activity_type
            ) VALUES (
                '2026-06-03T08:00:00+02:00',
                '2026-06-03T08:01:00+02:00',
                60.0,
                'Dia',
                'company.thebrowser.dia',
                'Project',
                'Browser',
                'https://github.com/org',
                'active'
            );",
        )?;
        drop(conn);

        store.ensure_dirs()?;
        let conn = Connection::open(store.db_path())?;
        assert!(table_column_exists(&conn, "sessions", "start_unix_ms")?);
        assert!(table_column_exists(&conn, "sessions", "end_unix_ms")?);
        let (window_start, window_end) = day_bounds(parse_date("2026-06-03")?)?;
        let sessions = store.sessions_in_window(Some(window_start), Some(window_end))?;

        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions
                .first()
                .map(|session| session.start_time.timestamp_millis()),
            Some(parse_local_datetime("2026-06-03T08:00:00+02:00")?.timestamp_millis())
        );
        Ok(())
    }

    #[test]
    fn storage_health_reports_freshness_from_checkpoint() -> AnyhowResult<()> {
        let dir = tempfile::tempdir()?;
        let store = LogStore::new(dir.path().to_path_buf());
        let session_start = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 0, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing session start"))?;
        let session_end = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 1, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing session end"))?;
        let checkpoint_start = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 2, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing checkpoint start"))?;
        let checkpoint_seen = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 2, 30)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing checkpoint seen"))?;
        let now = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 2, 45)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing now"))?;
        let session = UsageSession::from_entity(&entity(), session_start, session_end)
            .ok_or_else(|| anyhow::anyhow!("missing session"))?;

        store.append_session(&session)?;
        store.checkpoint_session(&entity(), checkpoint_start, checkpoint_seen)?;
        let health = store.storage_health(now, 60)?;

        assert!(health.fresh);
        assert_eq!(health.session_count, 1);
        assert_eq!(health.latest_observed_at, Some(checkpoint_seen));
        assert_eq!(health.latest_observed_age_seconds, Some(15.0));
        assert!(health.open_session.is_some());
        Ok(())
    }

    #[test]
    fn recover_recent_checkpoint_extends_to_restart_time() -> AnyhowResult<()> {
        let dir = tempfile::tempdir()?;
        let store = LogStore::new(dir.path().to_path_buf());
        let start = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 0, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing start"))?;
        let last_seen = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 1, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing last_seen"))?;
        let restarted_at = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 1, 10)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing restarted_at"))?;

        store.checkpoint_session(&entity(), start, last_seen)?;
        let recovered = store.recover_open_session(restarted_at, 30)?;
        let sessions = store.load_sessions()?;

        assert_eq!(
            recovered.as_ref().map(|session| session.end_time),
            Some(restarted_at)
        );
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions.first().map(|session| session.end_time),
            Some(restarted_at)
        );
        assert!(store.open_session_checkpoint()?.is_none());
        Ok(())
    }

    #[test]
    fn recover_stale_checkpoint_closes_at_last_seen() -> AnyhowResult<()> {
        let dir = tempfile::tempdir()?;
        let store = LogStore::new(dir.path().to_path_buf());
        let start = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 0, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing start"))?;
        let last_seen = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 1, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing last_seen"))?;
        let restarted_at = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 10, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing restarted_at"))?;

        store.checkpoint_session(&entity(), start, last_seen)?;
        let recovered = store.recover_open_session(restarted_at, 30)?;
        let sessions = store.load_sessions()?;

        assert_eq!(
            recovered.as_ref().map(|session| session.end_time),
            Some(last_seen)
        );
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions.first().map(|session| session.end_time),
            Some(last_seen)
        );
        assert!(store.open_session_checkpoint()?.is_none());
        Ok(())
    }

    #[test]
    fn sessions_for_day_with_open_includes_provisional_session() -> AnyhowResult<()> {
        let dir = tempfile::tempdir()?;
        let store = LogStore::new(dir.path().to_path_buf());
        let start = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 0, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing start"))?;
        let last_seen = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 1, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing last_seen"))?;
        let now = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 1, 10)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing now"))?;

        store.checkpoint_session(&entity(), start, last_seen)?;
        let sessions = store.sessions_for_day_with_open(parse_date("2026-06-03")?, now, 30)?;

        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions.first().map(|session| session.start_time),
            Some(start)
        );
        assert_eq!(sessions.first().map(|session| session.end_time), Some(now));
        assert!(store.load_sessions()?.is_empty());
        assert!(store.open_session_checkpoint()?.is_some());
        Ok(())
    }

    #[test]
    fn imports_legacy_csv_and_skips_duplicates() -> AnyhowResult<()> {
        let dir = tempfile::tempdir()?;
        let store = LogStore::new(dir.path().join("store"));
        let csv_path = dir.path().join("legacy.csv");
        fs::write(
            &csv_path,
            "Start Time,End Time,Duration (seconds),App Name,Bundle ID,Category,URL\n\
2026-06-03T08:00:00+02:00,2026-06-03T08:01:00+02:00,60.000,Dia,company.thebrowser.dia,Browser,https://example.com/\n",
        )?;

        let first = store.import_csv(&csv_path, false)?;
        let second = store.import_csv(&csv_path, false)?;
        let sessions = store.load_sessions()?;

        assert_eq!(first.scanned, 1);
        assert_eq!(first.imported, 1);
        assert_eq!(second.imported, 0);
        assert_eq!(second.skipped_duplicates, 1);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions.first().and_then(|s| s.title.as_deref()), None);
        assert_eq!(
            sessions.first().map(|s| s.activity_type),
            Some(ActivityType::Active)
        );
        Ok(())
    }

    #[test]
    fn reclassify_sessions_updates_domain_categories() -> AnyhowResult<()> {
        let dir = tempfile::tempdir()?;
        let store = LogStore::new(dir.path().to_path_buf());
        let start = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 0, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing start"))?;
        let end = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 1, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing end"))?;
        let mut session = UsageSession::from_entity(&entity(), start, end)
            .ok_or_else(|| anyhow::anyhow!("missing session"))?;
        session.category = "Browser".to_string();
        session.url = Some("https://app.slack.com/client/example".to_string());

        store.append_session(&session)?;
        let dry_run = store.reclassify_sessions(true)?;
        let report = store.reclassify_sessions(false)?;
        let sessions = store.load_sessions()?;

        assert_eq!(dry_run.scanned, 1);
        assert_eq!(dry_run.changed, 1);
        assert_eq!(report.changed, 1);
        assert_eq!(
            sessions.first().map(|session| session.category.as_str()),
            Some("Communication")
        );
        Ok(())
    }

    #[test]
    fn repair_gaps_inserts_untracked_sessions_idempotently() -> AnyhowResult<()> {
        let dir = tempfile::tempdir()?;
        let store = LogStore::new(dir.path().to_path_buf());
        let first = UsageSession::from_entity(
            &entity(),
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 0, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing first start"))?,
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 1, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing first end"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing first"))?;
        let second = UsageSession::from_entity(
            &entity(),
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 3, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing second start"))?,
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 4, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing second end"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing second"))?;

        store.append_session(&first)?;
        store.append_session(&second)?;
        let dry_run = store.repair_gaps(30.0, true)?;
        let first_repair = store.repair_gaps(30.0, false)?;
        let second_repair = store.repair_gaps(30.0, false)?;
        let sessions = store.load_sessions()?;

        assert_eq!(dry_run.gaps_found, 1);
        assert_eq!(dry_run.repaired, 1);
        assert_eq!(first_repair.repaired, 1);
        assert_eq!(second_repair.repaired, 0);
        assert_eq!(sessions.len(), 3);
        assert!(
            sessions
                .iter()
                .any(|session| session.activity_type == ActivityType::Untracked)
        );
        assert_eq!(audit_sessions(&sessions, 30.0).gap_count, 0);
        Ok(())
    }

    #[test]
    fn repair_titles_backfills_native_app_context_only() -> AnyhowResult<()> {
        let dir = tempfile::tempdir()?;
        let store = LogStore::new(dir.path().to_path_buf());
        let start = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 0, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing start"))?;
        let end = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 1, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing end"))?;
        let cmux_entity = ActiveEntity {
            bundle_id: "com.cmuxterm.app".to_string(),
            name: "cmux".to_string(),
            title: Some("Wispr Flow".to_string()),
            url: None,
            category: "Development".to_string(),
            activity_type: ActivityType::Active,
        };
        let cmux = UsageSession::from_entity(&cmux_entity, start, end)
            .ok_or_else(|| anyhow::anyhow!("missing cmux"))?;
        let mut browser = UsageSession::from_entity(
            &entity(),
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 1, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing browser start"))?,
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 2, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing browser end"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing browser"))?;
        browser.title = None;

        store.append_session(&cmux)?;
        store.append_session(&browser)?;
        let dry_run = store.repair_titles(true)?;
        let report = store.repair_titles(false)?;
        let sessions = store.load_sessions()?;

        assert_eq!(dry_run.repaired, 1);
        assert_eq!(dry_run.native_repaired, 1);
        assert_eq!(dry_run.browser_repaired, 0);
        assert_eq!(report.repaired, 1);
        assert_eq!(report.native_repaired, 1);
        assert_eq!(report.browser_repaired, 0);
        assert!(
            sessions
                .iter()
                .any(|session| session.bundle_id == "com.cmuxterm.app"
                    && session.title.as_deref() == Some("cmux"))
        );
        assert!(
            sessions
                .iter()
                .any(|session| session.bundle_id == "com.google.Chrome" && session.title.is_none())
        );
        Ok(())
    }

    #[test]
    fn repair_titles_uses_unique_browser_url_title_only() -> AnyhowResult<()> {
        let dir = tempfile::tempdir()?;
        let store = LogStore::new(dir.path().to_path_buf());
        let start = Local
            .with_ymd_and_hms(2026, 6, 3, 9, 0, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing start"))?;
        let mut observed = UsageSession::from_entity(
            &entity(),
            start,
            start + TimeDelta::try_seconds(60).ok_or_else(|| anyhow::anyhow!("missing delta"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing observed"))?;
        observed.url = Some("https://example.com/stable".to_string());
        observed.title = Some("Stable Title".to_string());
        let mut missing = observed.clone();
        missing.start_time =
            start + TimeDelta::try_seconds(60).ok_or_else(|| anyhow::anyhow!("missing delta"))?;
        missing.end_time =
            start + TimeDelta::try_seconds(120).ok_or_else(|| anyhow::anyhow!("missing delta"))?;
        missing.duration_seconds = 60.0;
        missing.title = None;

        let mut ambiguous_first = observed.clone();
        ambiguous_first.start_time =
            start + TimeDelta::try_seconds(120).ok_or_else(|| anyhow::anyhow!("missing delta"))?;
        ambiguous_first.end_time =
            start + TimeDelta::try_seconds(180).ok_or_else(|| anyhow::anyhow!("missing delta"))?;
        ambiguous_first.url = Some("https://example.com/ambiguous".to_string());
        ambiguous_first.title = Some("First Title".to_string());
        let mut ambiguous_second = ambiguous_first.clone();
        ambiguous_second.start_time =
            start + TimeDelta::try_seconds(180).ok_or_else(|| anyhow::anyhow!("missing delta"))?;
        ambiguous_second.end_time =
            start + TimeDelta::try_seconds(240).ok_or_else(|| anyhow::anyhow!("missing delta"))?;
        ambiguous_second.title = Some("Second Title".to_string());
        let mut ambiguous_missing = ambiguous_first.clone();
        ambiguous_missing.start_time =
            start + TimeDelta::try_seconds(240).ok_or_else(|| anyhow::anyhow!("missing delta"))?;
        ambiguous_missing.end_time =
            start + TimeDelta::try_seconds(300).ok_or_else(|| anyhow::anyhow!("missing delta"))?;
        ambiguous_missing.title = None;

        store.append_session(&observed)?;
        store.append_session(&missing)?;
        store.append_session(&ambiguous_first)?;
        store.append_session(&ambiguous_second)?;
        store.append_session(&ambiguous_missing)?;
        let dry_run = store.repair_titles(true)?;
        let report = store.repair_titles(false)?;
        let second_report = store.repair_titles(false)?;
        let sessions = store.load_sessions()?;

        assert_eq!(dry_run.repaired, 1);
        assert_eq!(dry_run.browser_repaired, 1);
        assert_eq!(dry_run.native_repaired, 0);
        assert_eq!(report.repaired, 1);
        assert_eq!(report.browser_repaired, 1);
        assert_eq!(report.native_repaired, 0);
        assert_eq!(second_report.repaired, 0);
        assert!(sessions.iter().any(|session| session.url.as_deref()
            == Some("https://example.com/stable")
            && session.title.as_deref() == Some("Stable Title")));
        assert!(sessions.iter().any(|session| session.url.as_deref()
            == Some("https://example.com/ambiguous")
            && session.title.is_none()));
        Ok(())
    }

    #[test]
    fn repair_urls_canonicalizes_browser_blank_tabs_only() -> AnyhowResult<()> {
        let dir = tempfile::tempdir()?;
        let store = LogStore::new(dir.path().to_path_buf());
        let start = Local
            .with_ymd_and_hms(2026, 6, 3, 10, 0, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing start"))?;
        let mut blank = UsageSession::from_entity(
            &entity(),
            start,
            start + TimeDelta::try_seconds(60).ok_or_else(|| anyhow::anyhow!("missing delta"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing blank"))?;
        blank.title = None;
        blank.url = Some("about:blank".to_string());
        let mut normal = blank.clone();
        normal.start_time =
            start + TimeDelta::try_seconds(60).ok_or_else(|| anyhow::anyhow!("missing delta"))?;
        normal.end_time =
            start + TimeDelta::try_seconds(120).ok_or_else(|| anyhow::anyhow!("missing delta"))?;
        normal.title = Some("Example Project".to_string());
        normal.url = Some("https://example.com/path".to_string());

        store.append_session(&blank)?;
        store.append_session(&normal)?;
        let dry_run = store.repair_urls(true)?;
        let report = store.repair_urls(false)?;
        let second_report = store.repair_urls(false)?;
        let sessions = store.load_sessions()?;

        assert_eq!(dry_run.repaired, 1);
        assert_eq!(dry_run.blank_tab_urls, 1);
        assert_eq!(dry_run.blank_tab_context_urls, 0);
        assert_eq!(report.repaired, 1);
        assert_eq!(report.blank_tab_urls, 1);
        assert_eq!(report.blank_tab_context_urls, 0);
        assert_eq!(second_report.repaired, 0);
        assert!(sessions.iter().any(
            |session| session.url.as_deref() == Some(BROWSER_NEW_TAB_URL)
                && session.title.is_none()
        ));
        assert!(sessions.iter().any(|session| session.url.as_deref()
            == Some("https://example.com/path")
            && session.title.as_deref() == Some("Example Project")));
        Ok(())
    }

    #[test]
    fn repair_urls_marks_missing_url_between_blank_tabs() -> AnyhowResult<()> {
        let dir = tempfile::tempdir()?;
        let store = LogStore::new(dir.path().to_path_buf());
        let start = Local
            .with_ymd_and_hms(2026, 6, 3, 10, 0, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing start"))?;
        let mut first_blank = UsageSession::from_entity(
            &entity(),
            start,
            start + TimeDelta::try_seconds(60).ok_or_else(|| anyhow::anyhow!("missing delta"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing first blank"))?;
        first_blank.title = Some("New Tab".to_string());
        first_blank.url = Some(BROWSER_NEW_TAB_URL.to_string());
        let mut missing_context = first_blank.clone();
        missing_context.start_time =
            start + TimeDelta::try_seconds(60).ok_or_else(|| anyhow::anyhow!("missing delta"))?;
        missing_context.end_time =
            start + TimeDelta::try_seconds(120).ok_or_else(|| anyhow::anyhow!("missing delta"))?;
        missing_context.title = None;
        missing_context.url = None;
        let mut second_blank = first_blank.clone();
        second_blank.start_time =
            start + TimeDelta::try_seconds(120).ok_or_else(|| anyhow::anyhow!("missing delta"))?;
        second_blank.end_time =
            start + TimeDelta::try_seconds(180).ok_or_else(|| anyhow::anyhow!("missing delta"))?;

        store.append_session(&first_blank)?;
        store.append_session(&missing_context)?;
        store.append_session(&second_blank)?;
        let dry_run = store.repair_urls(true)?;
        let report = store.repair_urls(false)?;
        let sessions = store.load_sessions()?;

        assert_eq!(dry_run.repaired, 1);
        assert_eq!(dry_run.blank_tab_urls, 0);
        assert_eq!(dry_run.blank_tab_context_urls, 1);
        assert_eq!(report.repaired, 1);
        assert_eq!(report.blank_tab_context_urls, 1);
        assert_eq!(audit_sessions(&sessions, 30.0).missing_title_count, 0);
        assert!(sessions.iter().any(
            |session| session.url.as_deref() == Some(BROWSER_NEW_TAB_URL)
                && session.title.is_none()
        ));
        Ok(())
    }

    #[test]
    fn import_csv_dry_run_does_not_write() -> AnyhowResult<()> {
        let dir = tempfile::tempdir()?;
        let store = LogStore::new(dir.path().join("store"));
        let csv_path = dir.path().join("new.csv");
        fs::write(
            &csv_path,
            "Start Time,End Time,Duration (seconds),App Name,Bundle ID,Title,Category,Activity Type,URL\n\
2026-06-03T08:00:00+02:00,2026-06-03T08:01:00+02:00,60.000,Idle,local.activity_tracker.idle,,Idle,idle,\n",
        )?;

        let report = store.import_csv(&csv_path, true)?;

        assert_eq!(report.scanned, 1);
        assert_eq!(report.imported, 1);
        assert!(!store.sessions_path().exists());
        Ok(())
    }

    #[test]
    fn old_jsonl_records_default_to_active_activity_type() -> AnyhowResult<()> {
        let dir = tempfile::tempdir()?;
        let store = LogStore::new(dir.path().to_path_buf());
        fs::write(
            store.sessions_path(),
            r#"{"start_time":"2026-06-03T08:00:00+02:00","end_time":"2026-06-03T08:01:00+02:00","duration_seconds":60.0,"app_name":"Dia","bundle_id":"company.thebrowser.dia","category":"Browser","url":null}"#,
        )?;

        let sessions = store.load_sessions()?;

        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions.first().map(|s| s.activity_type),
            Some(ActivityType::Active)
        );
        Ok(())
    }

    #[test]
    fn activity_type_parses_untracked() -> AnyhowResult<()> {
        assert_eq!(
            "untracked".parse::<ActivityType>()?,
            ActivityType::Untracked
        );
        Ok(())
    }

    #[test]
    fn day_summary_clips_cross_midnight_sessions() -> AnyhowResult<()> {
        let start = Local
            .with_ymd_and_hms(2026, 6, 3, 23, 59, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing start"))?;
        let end = Local
            .with_ymd_and_hms(2026, 6, 4, 0, 1, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing end"))?;
        let session = UsageSession::from_entity(&entity(), start, end)
            .ok_or_else(|| anyhow::anyhow!("missing session"))?;

        let summary = summarize_day(&[session], parse_date("2026-06-03")?)?;

        assert_eq!(summary.total_seconds, 60.0);
        assert_eq!(summary.by_activity_type.len(), 1);
        Ok(())
    }

    #[test]
    fn audit_sessions_reports_gaps_above_threshold() -> AnyhowResult<()> {
        let first = UsageSession::from_entity(
            &entity(),
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 0, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing first start"))?,
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 1, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing first end"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing first"))?;
        let second = UsageSession::from_entity(
            &entity(),
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 3, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing second start"))?,
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 4, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing second end"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing second"))?;

        let audit = audit_sessions(&[first, second], 30.0);

        assert_eq!(audit.gap_count, 1);
        assert_eq!(audit.total_gap_seconds, 120.0);
        assert_eq!(audit.longest_gap_seconds, 120.0);
        assert_eq!(audit.overlap_count, 0);
        Ok(())
    }

    #[test]
    fn audit_sessions_reports_overlaps_and_invalid_rows() -> AnyhowResult<()> {
        let first = UsageSession::from_entity(
            &entity(),
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 0, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing first start"))?,
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 5, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing first end"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing first"))?;
        let second = UsageSession::from_entity(
            &entity(),
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 4, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing second start"))?,
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 6, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing second end"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing second"))?;
        let invalid_start = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 7, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing invalid start"))?;
        let invalid = UsageSession {
            start_time: invalid_start,
            end_time: invalid_start,
            duration_seconds: 0.0,
            app_name: "Broken".to_string(),
            bundle_id: "broken".to_string(),
            title: None,
            category: "Uncategorized".to_string(),
            url: None,
            activity_type: ActivityType::Active,
        };

        let audit = audit_sessions(&[first, second, invalid], 30.0);

        assert_eq!(audit.overlap_count, 1);
        assert_eq!(
            audit
                .overlaps
                .first()
                .map(|overlap| overlap.duration_seconds),
            Some(60.0)
        );
        assert_eq!(audit.invalid_session_count, 1);
        assert_eq!(
            audit
                .invalid_sessions
                .first()
                .map(|row| row.reason.as_str()),
            Some("end_time_not_after_start_time")
        );
        Ok(())
    }

    #[test]
    fn audit_sessions_reports_context_quality_counts() -> AnyhowResult<()> {
        let mut browser_missing_context = UsageSession::from_entity(
            &entity(),
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 0, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing browser start"))?,
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 1, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing browser end"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing browser session"))?;
        browser_missing_context.title = None;
        browser_missing_context.url = None;
        browser_missing_context.category = "Uncategorized".to_string();
        let idle = UsageSession::from_entity(
            &idle_entity(),
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 1, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing idle start"))?,
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 2, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing idle end"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing idle session"))?;
        let untracked = untracked_session(
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 2, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing untracked start"))?,
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 3, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing untracked end"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing untracked session"))?;
        let mut browser_blank_tab = UsageSession::from_entity(
            &entity(),
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 3, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing blank tab start"))?,
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 4, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing blank tab end"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing blank tab session"))?;
        browser_blank_tab.title = Some("New Tab".to_string());
        browser_blank_tab.url = None;

        let audit = audit_sessions(
            &[browser_missing_context, idle, untracked, browser_blank_tab],
            30.0,
        );

        assert_eq!(audit.active_session_count, 2);
        assert_eq!(audit.idle_session_count, 1);
        assert_eq!(audit.untracked_session_count, 1);
        assert_eq!(audit.missing_title_count, 1);
        assert_eq!(audit.browser_session_count, 2);
        assert_eq!(audit.browser_missing_url_count, 1);
        assert_eq!(audit.browser_blank_tab_count, 1);
        assert_eq!(audit.uncategorized_session_count, 1);
        assert_eq!(
            audit
                .missing_title_by_app
                .first()
                .map(|row| (row.name.as_str(), row.count)),
            Some(("Google Chrome (com.google.Chrome)", 1))
        );
        assert_eq!(
            audit
                .browser_missing_url_by_title
                .first()
                .map(|row| (row.name.as_str(), row.count)),
            Some(("<missing title>", 1))
        );
        assert_eq!(
            audit
                .browser_blank_tab_by_app
                .first()
                .map(|row| (row.name.as_str(), row.count)),
            Some(("Google Chrome (com.google.Chrome)", 1))
        );
        assert_eq!(
            audit
                .uncategorized_by_app
                .first()
                .map(|row| (row.name.as_str(), row.count)),
            Some(("Google Chrome (com.google.Chrome)", 1))
        );
        let issue_kinds = audit
            .quality_issues
            .iter()
            .map(|issue| issue.kind)
            .collect::<Vec<_>>();
        assert_eq!(
            issue_kinds,
            vec![
                AuditQualityIssueKind::MissingTitle,
                AuditQualityIssueKind::BrowserMissingUrl,
                AuditQualityIssueKind::Uncategorized,
            ]
        );
        assert_eq!(
            audit
                .quality_issues
                .first()
                .map(|issue| issue.url.as_deref()),
            Some(None)
        );
        Ok(())
    }

    #[test]
    fn browser_new_tab_url_is_canonicalized() {
        assert_eq!(
            normalize_browser_tab_url(Some("New Tab"), Some("https://x.com/home".to_string())),
            Some(BROWSER_NEW_TAB_URL.to_string())
        );
        assert_eq!(
            normalize_browser_tab_url(None, Some("about:blank".to_string())),
            Some(BROWSER_NEW_TAB_URL.to_string())
        );
        assert_eq!(
            normalize_browser_tab_url(
                Some("Example Project"),
                Some("https://www.example.com/path".to_string()),
            ),
            Some("https://www.example.com/path".to_string())
        );
    }

    #[test]
    fn browser_blank_tab_url_is_not_missing_title_or_url() -> AnyhowResult<()> {
        let mut session = UsageSession::from_entity(
            &entity(),
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 0, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing start"))?,
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 1, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing end"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing session"))?;
        session.title = None;
        session.url = Some("about:blank".to_string());

        let audit = audit_sessions(&[session], 30.0);

        assert_eq!(audit.missing_title_count, 0);
        assert_eq!(audit.browser_missing_url_count, 0);
        assert_eq!(audit.browser_blank_tab_count, 1);
        Ok(())
    }

    #[test]
    fn timeline_blocks_merge_adjacent_same_identity_sessions() -> AnyhowResult<()> {
        let mut first = UsageSession::from_entity(
            &entity(),
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 0, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing first start"))?,
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 1, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing first end"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing first"))?;
        let mut second = UsageSession::from_entity(
            &entity(),
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 1, 1)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing second start"))?,
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 2, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing second end"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing second"))?;
        first.url = Some("https://github.com/org/a".to_string());
        second.url = Some("https://github.com/org/b".to_string());
        first.category = "Development".to_string();
        second.category = "Development".to_string();

        let blocks = timeline_blocks(&[first, second]);

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks.first().map(|block| block.session_count), Some(2));
        assert_eq!(
            blocks.first().and_then(|block| block.domain.as_deref()),
            Some("github.com")
        );
        assert_eq!(
            blocks.first().map(|block| block.duration_seconds),
            Some(119.0)
        );
        Ok(())
    }

    #[test]
    fn timeline_blocks_split_different_domains() -> AnyhowResult<()> {
        let mut first = UsageSession::from_entity(
            &entity(),
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 0, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing first start"))?,
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 1, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing first end"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing first"))?;
        let mut second = UsageSession::from_entity(
            &entity(),
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 1, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing second start"))?,
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 2, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing second end"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing second"))?;
        first.url = Some("https://github.com/org/a".to_string());
        second.url = Some("https://app.slack.com/client/example".to_string());

        let blocks = timeline_blocks(&[first, second]);

        assert_eq!(blocks.len(), 2);
        Ok(())
    }

    #[test]
    fn filters_sessions_by_title() -> AnyhowResult<()> {
        let start = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 0, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing start"))?;
        let end = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 1, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing end"))?;
        let session = UsageSession::from_entity(&entity(), start, end)
            .ok_or_else(|| anyhow::anyhow!("missing session"))?;

        let filtered =
            filter_sessions(vec![session], None, Some("project"), None, None, None, None);

        assert_eq!(filtered.len(), 1);
        Ok(())
    }

    #[test]
    fn filters_sessions_by_time_window_overlap() -> AnyhowResult<()> {
        let window_start = Local
            .with_ymd_and_hms(2026, 6, 3, 8, 0, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing window start"))?;
        let window_end = Local
            .with_ymd_and_hms(2026, 6, 3, 9, 0, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing window end"))?;
        let mut before = UsageSession::from_entity(
            &entity(),
            Local
                .with_ymd_and_hms(2026, 6, 3, 7, 0, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing before start"))?,
            Local
                .with_ymd_and_hms(2026, 6, 3, 7, 30, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing before end"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing before"))?;
        let mut overlapping = UsageSession::from_entity(
            &entity(),
            Local
                .with_ymd_and_hms(2026, 6, 3, 7, 30, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing overlapping start"))?,
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 10, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing overlapping end"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing overlapping"))?;
        let mut inside = UsageSession::from_entity(
            &entity(),
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 15, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing inside start"))?,
            Local
                .with_ymd_and_hms(2026, 6, 3, 8, 30, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing inside end"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing inside"))?;
        let mut after = UsageSession::from_entity(
            &entity(),
            Local
                .with_ymd_and_hms(2026, 6, 3, 9, 0, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing after start"))?,
            Local
                .with_ymd_and_hms(2026, 6, 3, 9, 30, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing after end"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("missing after"))?;
        before.title = Some("before".to_string());
        overlapping.title = Some("overlapping".to_string());
        inside.title = Some("inside".to_string());
        after.title = Some("after".to_string());

        let filtered = filter_sessions_by_time_window(
            vec![before, overlapping, inside, after],
            Some(window_start),
            Some(window_end),
        );
        let titles = filtered
            .iter()
            .filter_map(|session| session.title.as_deref())
            .collect::<Vec<_>>();

        assert_eq!(titles, vec!["overlapping", "inside"]);
        Ok(())
    }

    #[test]
    fn query_time_window_uses_date_bounds() -> AnyhowResult<()> {
        let now = Local
            .with_ymd_and_hms(2026, 6, 3, 12, 0, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing now"))?;
        let window = query_time_window(
            QueryTimeWindowInput {
                from: Some("2026-06-03"),
                to: Some("2026-06-04"),
                ..QueryTimeWindowInput::default()
            },
            now,
        )?;
        let (expected_start, _) = day_bounds(parse_date("2026-06-03")?)?;
        let (_, expected_end) = day_bounds(parse_date("2026-06-04")?)?;

        assert_eq!(window.start, Some(expected_start));
        assert_eq!(window.end, Some(expected_end));
        Ok(())
    }

    #[test]
    fn query_time_window_uses_precise_timestamps() -> AnyhowResult<()> {
        let now = Local
            .with_ymd_and_hms(2026, 6, 3, 12, 0, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing now"))?;
        let since = "2026-06-03T08:00:00+02:00";
        let until = "2026-06-03T09:00:00+02:00";
        let window = query_time_window(
            QueryTimeWindowInput {
                since: Some(since),
                until: Some(until),
                ..QueryTimeWindowInput::default()
            },
            now,
        )?;

        assert_eq!(window.start, Some(parse_local_datetime(since)?));
        assert_eq!(window.end, Some(parse_local_datetime(until)?));
        Ok(())
    }

    #[test]
    fn query_time_window_uses_last_minutes() -> AnyhowResult<()> {
        let now = Local
            .with_ymd_and_hms(2026, 6, 3, 12, 0, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing now"))?;
        let window = query_time_window(
            QueryTimeWindowInput {
                last_minutes: Some(90),
                ..QueryTimeWindowInput::default()
            },
            now,
        )?;

        assert_eq!(window.start, Some(now - TimeDelta::minutes(90)));
        assert_eq!(window.end, Some(now));
        Ok(())
    }

    #[test]
    fn query_time_window_rejects_conflicts_and_bad_ranges() -> AnyhowResult<()> {
        let now = Local
            .with_ymd_and_hms(2026, 6, 3, 12, 0, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing now"))?;
        let conflict = query_time_window(
            QueryTimeWindowInput {
                from: Some("2026-06-03"),
                last_minutes: Some(15),
                ..QueryTimeWindowInput::default()
            },
            now,
        );
        let bad_date = query_time_window(
            QueryTimeWindowInput {
                from: Some("2026-06-04"),
                to: Some("2026-06-03"),
                ..QueryTimeWindowInput::default()
            },
            now,
        );
        let bad_time = query_time_window(
            QueryTimeWindowInput {
                since: Some("2026-06-03T09:00:00+02:00"),
                until: Some("2026-06-03T08:00:00+02:00"),
                ..QueryTimeWindowInput::default()
            },
            now,
        );

        assert!(matches!(
            conflict,
            Err(TrackerError::ConflictingQueryWindowArgs(_))
        ));
        assert!(matches!(
            bad_date,
            Err(TrackerError::InvalidDateRange { .. })
        ));
        assert!(matches!(
            bad_time,
            Err(TrackerError::InvalidTimeRange { .. })
        ));
        Ok(())
    }

    #[test]
    fn category_covers_observed_apps() {
        assert_eq!(category_for("com.figma.Desktop", "Figma"), "Design");
        assert_eq!(category_for("dev.pencil.desktop", "Pencil"), "Design");
        assert_eq!(category_for("com.apple.Preview", "Preview"), "Writing");
        assert_eq!(
            category_for("com.apple.systempreferences", "System Settings"),
            "System"
        );
        assert_eq!(category_for("com.cmuxterm.app", "cmux"), "Development");
        assert_eq!(
            category_for("net.whatsapp.WhatsApp", "WhatsApp"),
            "Communication"
        );
        assert_eq!(
            category_for("com.electron.wispr-flow", "Wispr Flow"),
            "Productivity"
        );
    }

    #[test]
    fn category_uses_url_domain_when_available() {
        assert_eq!(
            category_for_activity(
                "company.thebrowser.dia",
                "Dia",
                Some("https://app.slack.com/client/example")
            ),
            "Communication"
        );
        assert_eq!(
            category_for_activity(
                "company.thebrowser.dia",
                "Dia",
                Some("https://github.com/org")
            ),
            "Development"
        );
        assert_eq!(
            category_for_activity(
                "company.thebrowser.dia",
                "Dia",
                Some("https://mail.google.com/mail/u/0")
            ),
            "Email"
        );
    }

    #[test]
    fn domain_parser_normalizes_browser_urls() {
        assert_eq!(
            domain_from_url("https://www.Example.com:443/a?b=c").as_deref(),
            Some("example.com")
        );
    }

    #[test]
    fn service_status_parser_extracts_running_pid() {
        let raw = format!("gui/501/{SERVICE_LABEL} = {{\n\tstate = running\n\tpid = 12345\n}}\n");

        let report = parse_service_status(&raw);

        assert!(report.loaded);
        assert!(report.running);
        assert_eq!(report.pid, Some(12_345));
        assert!(report.error.is_none());
    }

    #[test]
    fn service_status_parser_handles_loaded_not_running() {
        let raw = format!("gui/501/{SERVICE_LABEL} = {{\n\tstate = waiting\n}}\n");

        let report = parse_service_status(&raw);

        assert!(report.loaded);
        assert!(!report.running);
        assert_eq!(report.pid, None);
    }

    #[test]
    fn parser_extracts_hid_idle_nanoseconds() {
        assert_eq!(
            parse_hid_idle_nanoseconds(r#"      "HIDIdleTime" = 8099666"#),
            Some(8_099_666)
        );
    }

    #[test]
    fn probe_stabilizer_preserves_current_entity_for_short_misses() {
        let mut stabilizer = ProbeMissStabilizer::new(2);
        let current = entity();

        assert_eq!(
            stabilizer.stabilize(None, Some(&current)).as_ref(),
            Some(&current)
        );
        assert_eq!(
            stabilizer.stabilize(None, Some(&current)).as_ref(),
            Some(&current)
        );
        assert_eq!(stabilizer.stabilize(None, Some(&current)), None);
    }

    #[test]
    fn probe_stabilizer_resets_after_successful_sample() {
        let mut stabilizer = ProbeMissStabilizer::new(1);
        let current = entity();

        assert_eq!(
            stabilizer.stabilize(None, Some(&current)).as_ref(),
            Some(&current)
        );
        assert_eq!(
            stabilizer.stabilize(Some(current.clone()), Some(&current)),
            Some(current.clone())
        );
        assert_eq!(
            stabilizer.stabilize(None, Some(&current)).as_ref(),
            Some(&current)
        );
    }

    #[test]
    fn tracker_state_backdates_idle_transition() -> AnyhowResult<()> {
        let start = Local
            .with_ymd_and_hms(2026, 6, 3, 10, 0, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing start"))?;
        let sample_time = Local
            .with_ymd_and_hms(2026, 6, 3, 10, 10, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing sample"))?;
        let mut state = TrackerState::new(Some(entity()), start, 300);

        let active_session = state
            .apply_sample(Some(entity()), Some(300.0), sample_time)
            .ok_or_else(|| anyhow::anyhow!("missing active session"))?;

        assert_eq!(active_session.activity_type, ActivityType::Active);
        assert_eq!(active_session.duration_seconds, 300.0);
        assert_eq!(
            state.current_entity().map(|entity| entity.activity_type),
            Some(ActivityType::Idle)
        );
        assert_eq!(
            state.session_start(),
            Local
                .with_ymd_and_hms(2026, 6, 3, 10, 5, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("missing idle start"))?
        );
        Ok(())
    }

    #[test]
    fn tracker_state_closes_idle_when_activity_resumes() -> AnyhowResult<()> {
        let start = Local
            .with_ymd_and_hms(2026, 6, 3, 10, 5, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing start"))?;
        let sample_time = Local
            .with_ymd_and_hms(2026, 6, 3, 10, 6, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing sample"))?;
        let mut state = TrackerState::new(Some(idle_entity()), start, 300);

        let idle_session = state
            .apply_sample(Some(entity()), Some(0.0), sample_time)
            .ok_or_else(|| anyhow::anyhow!("missing idle session"))?;

        assert_eq!(idle_session.activity_type, ActivityType::Idle);
        assert_eq!(idle_session.duration_seconds, 60.0);
        assert_eq!(
            state.current_entity().map(|entity| entity.activity_type),
            Some(ActivityType::Active)
        );
        Ok(())
    }

    #[test]
    fn tracker_state_records_untracked_when_probe_recovers() -> AnyhowResult<()> {
        let active_start = Local
            .with_ymd_and_hms(2026, 6, 3, 10, 0, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing active start"))?;
        let lost_at = Local
            .with_ymd_and_hms(2026, 6, 3, 10, 1, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing lost time"))?;
        let recovered_at = Local
            .with_ymd_and_hms(2026, 6, 3, 10, 3, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing recovered time"))?;
        let mut state = TrackerState::new(Some(entity()), active_start, 300);

        let active_session = state
            .apply_sample(None, Some(0.0), lost_at)
            .ok_or_else(|| anyhow::anyhow!("missing active session"))?;
        let untracked = state
            .apply_sample(Some(entity()), Some(0.0), recovered_at)
            .ok_or_else(|| anyhow::anyhow!("missing untracked session"))?;

        assert_eq!(active_session.activity_type, ActivityType::Active);
        assert_eq!(active_session.duration_seconds, 60.0);
        assert_eq!(untracked.activity_type, ActivityType::Untracked);
        assert_eq!(untracked.duration_seconds, 120.0);
        assert_eq!(state.session_start(), recovered_at);
        Ok(())
    }

    #[test]
    fn tracker_state_records_untracked_before_idle_recovery() -> AnyhowResult<()> {
        let unknown_start = Local
            .with_ymd_and_hms(2026, 6, 3, 10, 0, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing unknown start"))?;
        let sample_time = Local
            .with_ymd_and_hms(2026, 6, 3, 10, 10, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing sample time"))?;
        let idle_start = Local
            .with_ymd_and_hms(2026, 6, 3, 10, 5, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("missing idle start"))?;
        let mut state = TrackerState::new(None, unknown_start, 300);

        let untracked = state
            .apply_sample(Some(entity()), Some(300.0), sample_time)
            .ok_or_else(|| anyhow::anyhow!("missing untracked session"))?;

        assert_eq!(untracked.activity_type, ActivityType::Untracked);
        assert_eq!(untracked.duration_seconds, 300.0);
        assert_eq!(
            state.current_entity().map(|entity| entity.activity_type),
            Some(ActivityType::Idle)
        );
        assert_eq!(state.session_start(), idle_start);
        Ok(())
    }

    #[test]
    fn launch_agent_plist_contains_track_command() {
        let plist = launch_agent_plist(
            Path::new("/tmp/activity_tracker"),
            Path::new("/tmp/out.log"),
            Path::new("/tmp/err.log"),
        );
        assert!(plist.contains("<string>track</string>"));
        assert!(plist.contains("<string>--quiet</string>"));
    }
}
