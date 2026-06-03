use std::collections::HashMap;
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
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_INTERVAL_SECONDS: u64 = 2;
pub const DEFAULT_IDLE_THRESHOLD_SECONDS: u64 = 300;
pub const SERVICE_LABEL: &str = "com.local.activity-tracker";
pub const IDLE_BUNDLE_ID: &str = "local.activity_tracker.idle";

#[derive(Debug, Error)]
pub enum TrackerError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("CSV error: {0}")]
    Csv(#[from] csv::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid JSONL record in {path} at line {line}: {source}")]
    JsonLine {
        path: PathBuf,
        line: usize,
        source: serde_json::Error,
    },
    #[error("invalid date `{0}`; expected YYYY-MM-DD")]
    InvalidDate(String),
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
}

impl ActivityType {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Idle => "idle",
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
        let switching_to_idle = desired_entity
            .as_ref()
            .is_some_and(|entity| entity.activity_type == ActivityType::Idle)
            && previous.is_some_and(|entity| entity.activity_type != ActivityType::Idle);
        let end_time = if switching_to_idle {
            max_datetime(self.session_start, idle_started_at.unwrap_or(observed_at))
        } else {
            observed_at
        };

        let completed = previous
            .and_then(|entity| UsageSession::from_entity(entity, self.session_start, end_time));
        self.current_entity = desired_entity;
        self.session_start = if completed.is_some() {
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
        fs::create_dir_all(&self.root)?;
        fs::create_dir_all(self.exports_dir())?;
        fs::create_dir_all(self.logs_dir())?;
        Ok(())
    }

    pub fn append_session(&self, session: &UsageSession) -> Result<()> {
        self.ensure_dirs()?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.sessions_path())?;
        serde_json::to_writer(&mut file, session)?;
        file.write_all(b"\n")?;
        file.flush()?;
        Ok(())
    }

    pub fn load_sessions(&self) -> Result<Vec<UsageSession>> {
        let path = self.sessions_path();
        if !path.exists() {
            return Ok(Vec::new());
        }

        let file = File::open(&path)?;
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
                    path: path.clone(),
                    line: idx + 1,
                    source,
                }
            })?;
            sessions.push(session);
        }

        Ok(sessions)
    }

    pub fn sessions_for_day(&self, date: NaiveDate) -> Result<Vec<UsageSession>> {
        let (start, end) = day_bounds(date)?;
        Ok(self
            .load_sessions()?
            .into_iter()
            .filter(|session| session.overlaps(start, end))
            .collect())
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
pub fn filter_sessions(
    sessions: Vec<UsageSession>,
    app: Option<&str>,
    category: Option<&str>,
    domain: Option<&str>,
    activity_type: Option<&str>,
    limit: Option<usize>,
) -> Vec<UsageSession> {
    let app = app.map(str::to_lowercase);
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

pub fn parse_date(input: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(input, "%Y-%m-%d")
        .map_err(|_| TrackerError::InvalidDate(input.to_string()))
}

pub fn day_bounds(date: NaiveDate) -> Result<(DateTime<Local>, DateTime<Local>)> {
    let start = local_midnight(date)?;
    let next_day = date.succ_opt().ok_or(TrackerError::InvalidLocalDay(date))?;
    let end = local_midnight(next_day)?;
    Ok((start, end))
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
        "com.microsoft.teams2"
        | "com.tinyspeck.slackmacgap"
        | "com.apple.MobileSMS"
        | "us.zoom.xos" => "Communication",
        "com.apple.Notes" | "com.apple.TextEdit" | "com.notion.id" => "Writing",
        "com.apple.finder" => "System",
        _ if name.eq_ignore_ascii_case("Finder") => "System",
        _ => "Uncategorized",
    }
    .to_string()
}

#[must_use]
pub fn idle_entity() -> ActiveEntity {
    ActiveEntity {
        bundle_id: IDLE_BUNDLE_ID.to_string(),
        name: "Idle".to_string(),
        url: None,
        category: "Idle".to_string(),
        activity_type: ActivityType::Idle,
    }
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

impl ActivityProbe for MacOsProbe {
    fn active_entity(&self) -> Result<Option<ActiveEntity>> {
        let Some((bundle_id, name)) = active_app_info()? else {
            return Ok(None);
        };
        let url = browser_tab_url(&bundle_id);
        let category = category_for(&bundle_id, &name);
        Ok(Some(ActiveEntity {
            bundle_id,
            name,
            url,
            category,
            activity_type: ActivityType::Active,
        }))
    }

    fn idle_seconds(&self) -> Result<Option<f64>> {
        hid_idle_seconds()
    }
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
    let script = r#"tell application "System Events"
set frontApp to first application process whose frontmost is true
set appName to name of frontApp
set bundleId to bundle identifier of frontApp
return bundleId & linefeed & appName
end tell"#;

    match run_osascript(script) {
        Ok(output) => {
            let mut lines = output.lines();
            let Some(bundle_id) = lines
                .next()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                return Ok(None);
            };
            let Some(name) = lines
                .next()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                return Ok(None);
            };
            Ok(Some((bundle_id.to_string(), name.to_string())))
        }
        Err(error) => {
            tracing::warn!(error = %error, "active app probe failed");
            Ok(None)
        }
    }
}

pub fn browser_tab_url(bundle_id: &str) -> Option<String> {
    if !is_browser(bundle_id) {
        return None;
    }

    let script = if bundle_id == "com.apple.Safari" {
        r#"tell application id "com.apple.Safari" to get URL of current tab of front window"#
            .to_string()
    } else {
        format!(
            r#"tell application id "{}" to get URL of active tab of front window"#,
            escape_applescript_string(bundle_id)
        )
    };

    match run_osascript(&script) {
        Ok(url) => Some(url),
        Err(error) => {
            tracing::debug!(bundle_id, error = %error, "browser URL probe failed");
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
    fs::write(&plist_path, plist)?;

    if load {
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

fn seconds_between(start_time: DateTime<Local>, end_time: DateTime<Local>) -> Option<f64> {
    if end_time <= start_time {
        return None;
    }
    let millis = end_time
        .signed_duration_since(start_time)
        .num_milliseconds();
    Some(millis as f64 / 1_000.0)
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

fn max_datetime(a: DateTime<Local>, b: DateTime<Local>) -> DateTime<Local> {
    if a >= b { a } else { b }
}

fn local_midnight(date: NaiveDate) -> Result<DateTime<Local>> {
    match Local.with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0) {
        LocalResult::Single(value) | LocalResult::Ambiguous(value, _) => Ok(value),
        LocalResult::None => Err(TrackerError::InvalidLocalDay(date)),
    }
}

fn default_data_dir() -> Result<PathBuf> {
    dirs::data_local_dir()
        .map(|path| path.join("activity_tracker"))
        .ok_or(TrackerError::DataDirNotFound)
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

fn escape_applescript_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
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
            url: Some("https://www.example.com/path".to_string()),
            category: "Browser".to_string(),
            activity_type: ActivityType::Active,
        }
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
        assert_eq!(all.len(), 1);
        assert_eq!(store.sessions_for_day(parse_date("2026-06-03")?)?.len(), 1);
        assert_eq!(store.sessions_for_day(parse_date("2026-06-04")?)?.len(), 1);
        Ok(())
    }

    #[test]
    fn old_jsonl_records_default_to_active_activity_type() -> AnyhowResult<()> {
        let dir = tempfile::tempdir()?;
        let store = LogStore::new(dir.path().to_path_buf());
        store.ensure_dirs()?;
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
    fn domain_parser_normalizes_browser_urls() {
        assert_eq!(
            domain_from_url("https://www.Example.com:443/a?b=c").as_deref(),
            Some("example.com")
        );
    }

    #[test]
    fn parser_extracts_hid_idle_nanoseconds() {
        assert_eq!(
            parse_hid_idle_nanoseconds(r#"      "HIDIdleTime" = 8099666"#),
            Some(8_099_666)
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
