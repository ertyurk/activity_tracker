use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use activity_tracker::{
    ActivityAudit, ActivityProbe, BrowserContextStabilizer, DEFAULT_AUDIT_GAP_THRESHOLD_SECONDS,
    DEFAULT_HEALTH_STALE_THRESHOLD_SECONDS, DEFAULT_IDLE_THRESHOLD_SECONDS,
    DEFAULT_INTERVAL_SECONDS, DEFAULT_PROBE_MISS_TOLERANCE, DEFAULT_RECENT_CHECKPOINT_SECONDS,
    LogStore, MacOsProbe, ProbeMissStabilizer, QueryTimeWindow, QueryTimeWindowInput, Result,
    TrackerError, TrackerState, UsageSession, audit_sessions, day_bounds, filter_sessions,
    format_seconds, install_launch_agent, legacy_data_dir, legacy_sessions_path, parse_date,
    query_time_window, service_status, service_status_report, summarize_all, summarize_day,
    timeline_blocks, uninstall_launch_agent,
};
use chrono::{Local, NaiveDate};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use tracing_subscriber::EnvFilter;

const DEFAULT_AGENT_LAST_MINUTES: u64 = 120;
const DEFAULT_AGENT_TIMELINE_LIMIT: usize = 20;
const DEFAULT_AGENT_SUMMARY_LIMIT: usize = 12;

#[derive(Debug, Parser)]
#[command(
    name = "activity_tracker",
    version,
    about = "Local macOS activity tracker"
)]
struct Cli {
    #[arg(long, global = true, value_name = "DIR")]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run foreground tracker loop. Omit subcommand to do this.
    Track(TrackArgs),
    /// Print one-day summary. Defaults to today.
    Day(DayArgs),
    /// Print one-day AI report with summary, sessions, checkpoint, and paths.
    Report(ReportArgs),
    /// Print compact one-day timeline blocks. Defaults to today.
    Timeline(TimelineArgs),
    /// Query sessions across an optional date range.
    Query(QueryArgs),
    /// Print raw sessions. Defaults to today.
    Logs(LogsArgs),
    /// Audit one-day log quality for gaps, overlaps, and invalid rows.
    Audit(AuditArgs),
    /// Print all-time summary.
    Summary(OutputArgs),
    /// Export sessions to CSV or JSONL.
    Export(ExportArgs),
    /// Import legacy/exported CSV into local storage with duplicate skipping.
    ImportCsv(ImportCsvArgs),
    /// Recompute categories from current app/domain rules.
    Reclassify(ReclassifyArgs),
    /// Insert explicit untracked sessions for completed-log gaps.
    RepairGaps(RepairGapsArgs),
    /// Backfill native app titles when only app-level context is available.
    RepairTitles(RepairTitlesArgs),
    /// Canonicalize safe URL repairs, such as known browser blank tabs.
    RepairUrls(RepairUrlsArgs),
    /// Repair high-confidence browser title/URL mismatches.
    RepairContext(RepairContextArgs),
    /// Print storage and service paths.
    Paths(OutputArgs),
    /// Print collector health, freshness, service state, and today audit.
    Health(HealthArgs),
    /// Print compact AI-agent readiness and recent report context.
    Agent(AgentArgs),
    /// Check macOS permissions, probes, and writable storage.
    Doctor(OutputArgs),
    /// Install, uninstall, or inspect launchd background service.
    Service(ServiceCommand),
}

#[derive(Debug, Args, Default)]
struct TrackArgs {
    #[arg(long, default_value_t = DEFAULT_INTERVAL_SECONDS)]
    interval_seconds: u64,
    #[arg(long, default_value_t = DEFAULT_IDLE_THRESHOLD_SECONDS)]
    idle_threshold_seconds: u64,
    #[arg(long)]
    quiet: bool,
}

#[derive(Debug, Args)]
struct DayArgs {
    date: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ReportArgs {
    date: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct TimelineArgs {
    date: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct QueryArgs {
    #[arg(long)]
    from: Option<String>,
    #[arg(long)]
    to: Option<String>,
    #[arg(long)]
    since: Option<String>,
    #[arg(long)]
    until: Option<String>,
    #[arg(long)]
    last_minutes: Option<u64>,
    #[arg(long)]
    app: Option<String>,
    #[arg(long)]
    title: Option<String>,
    #[arg(long)]
    category: Option<String>,
    #[arg(long)]
    domain: Option<String>,
    #[arg(long)]
    activity_type: Option<String>,
    #[arg(long)]
    limit: Option<usize>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args, Clone, Default)]
struct RepairWindowArgs {
    #[arg(long)]
    from: Option<String>,
    #[arg(long)]
    to: Option<String>,
    #[arg(long)]
    since: Option<String>,
    #[arg(long)]
    until: Option<String>,
    #[arg(long)]
    last_minutes: Option<u64>,
}

#[derive(Debug, Args)]
struct LogsArgs {
    date: Option<String>,
    #[arg(long)]
    app: Option<String>,
    #[arg(long)]
    title: Option<String>,
    #[arg(long)]
    category: Option<String>,
    #[arg(long)]
    domain: Option<String>,
    #[arg(long)]
    activity_type: Option<String>,
    #[arg(long)]
    limit: Option<usize>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct AuditArgs {
    date: Option<String>,
    #[arg(long, default_value_t = DEFAULT_AUDIT_GAP_THRESHOLD_SECONDS)]
    gap_threshold_seconds: f64,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct HealthArgs {
    #[arg(long, default_value_t = DEFAULT_HEALTH_STALE_THRESHOLD_SECONDS)]
    stale_threshold_seconds: u64,
    #[arg(long, default_value_t = DEFAULT_AUDIT_GAP_THRESHOLD_SECONDS)]
    gap_threshold_seconds: f64,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct AgentArgs {
    date: Option<String>,
    #[arg(long)]
    last_minutes: Option<u64>,
    #[arg(long, default_value_t = DEFAULT_AGENT_TIMELINE_LIMIT)]
    timeline_limit: usize,
    #[arg(long, default_value_t = DEFAULT_AGENT_SUMMARY_LIMIT)]
    summary_limit: usize,
    #[arg(long)]
    include_sessions: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct OutputArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ExportArgs {
    #[arg(long)]
    date: Option<String>,
    #[arg(long, value_enum, default_value_t = ExportFormat::Csv)]
    format: ExportFormat,
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ImportCsvArgs {
    path: PathBuf,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ReclassifyArgs {
    #[command(flatten)]
    window: RepairWindowArgs,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RepairGapsArgs {
    #[command(flatten)]
    window: RepairWindowArgs,
    #[arg(long, default_value_t = DEFAULT_AUDIT_GAP_THRESHOLD_SECONDS)]
    gap_threshold_seconds: f64,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RepairTitlesArgs {
    #[command(flatten)]
    window: RepairWindowArgs,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RepairUrlsArgs {
    #[command(flatten)]
    window: RepairWindowArgs,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RepairContextArgs {
    #[command(flatten)]
    window: RepairWindowArgs,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ExportFormat {
    Csv,
    Jsonl,
}

#[derive(Debug, Args)]
struct ServiceCommand {
    #[command(subcommand)]
    action: ServiceAction,
}

#[derive(Debug, Subcommand)]
enum ServiceAction {
    /// Write LaunchAgent plist and load tracker now.
    Install(ServiceInstallArgs),
    /// Stop launchd service and remove plist.
    Uninstall,
    /// Print launchd service state.
    Status(OutputArgs),
}

#[derive(Debug, Args)]
struct ServiceInstallArgs {
    #[arg(long)]
    bin: Option<PathBuf>,
    #[arg(long)]
    no_load: bool,
}

fn main() -> ExitCode {
    init_tracing();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let store = match cli.data_dir {
        Some(path) => LogStore::new(path),
        None => LogStore::from_env()?,
    };

    match cli.command.unwrap_or(Command::Track(TrackArgs::default())) {
        Command::Track(args) => run_tracker(&store, args),
        Command::Day(args) => print_day(&store, args),
        Command::Report(args) => print_report(&store, args),
        Command::Timeline(args) => print_timeline(&store, args),
        Command::Query(args) => print_query(&store, args),
        Command::Logs(args) => print_logs(&store, args),
        Command::Audit(args) => print_audit(&store, args),
        Command::Summary(args) => print_summary(&store, args),
        Command::Export(args) => export_sessions(&store, args),
        Command::ImportCsv(args) => import_csv(&store, args),
        Command::Reclassify(args) => reclassify(&store, args),
        Command::RepairGaps(args) => repair_gaps(&store, args),
        Command::RepairTitles(args) => repair_titles(&store, args),
        Command::RepairUrls(args) => repair_urls(&store, args),
        Command::RepairContext(args) => repair_context(&store, args),
        Command::Paths(args) => print_paths(&store, args),
        Command::Health(args) => print_health(&store, args),
        Command::Agent(args) => print_agent(&store, args),
        Command::Doctor(args) => doctor(&store, args),
        Command::Service(command) => run_service(&store, command),
    }
}

fn run_tracker(store: &LogStore, args: TrackArgs) -> Result<()> {
    let running = Arc::new(AtomicBool::new(true));
    let signal = Arc::clone(&running);
    ctrlc::set_handler(move || {
        signal.store(false, Ordering::SeqCst);
    })
    .map_err(|error| TrackerError::CtrlC(error.to_string()))?;

    store.ensure_dirs()?;
    if let Some(session) =
        store.recover_open_session(Local::now(), DEFAULT_RECENT_CHECKPOINT_SECONDS)?
        && !args.quiet
    {
        println!(
            "recovered -> {} {} [{}]",
            session.app_name,
            format_seconds(session.duration_seconds),
            session.activity_type
        );
    }

    let probe = MacOsProbe;
    let interval = Duration::from_secs(args.interval_seconds.max(1));
    let mut state = TrackerState::new(
        observed_entity(&probe, args.idle_threshold_seconds)?,
        Local::now(),
        args.idle_threshold_seconds,
    );
    let mut stabilizer = ProbeMissStabilizer::new(DEFAULT_PROBE_MISS_TOLERANCE);
    let mut browser_context_stabilizer =
        BrowserContextStabilizer::new(DEFAULT_PROBE_MISS_TOLERANCE);
    checkpoint_current_session(store, &state, Local::now())?;

    if !args.quiet {
        println!("tracking -> {}", store.db_path().display());
    }

    while running.load(Ordering::SeqCst) {
        thread::sleep(interval);
        let next_entity =
            stabilizer.stabilize(active_entity_or_none(&probe), state.current_entity());
        let next_entity = browser_context_stabilizer.stabilize(next_entity, state.current_entity());
        let idle_seconds = idle_seconds_or_none(&probe);
        let now = Local::now();

        if let Some(session) = state.apply_sample(next_entity, idle_seconds, now) {
            persist_session(store, session, args.quiet)?;
            if !args.quiet
                && let Some(entity) = state.current_entity()
            {
                println!("active -> {} [{}]", entity.name, entity.category);
            }
        }
        checkpoint_current_session(store, &state, now)?;
    }

    if let Some(session) = state.finish(Local::now()) {
        persist_session(store, session, args.quiet)?;
    }
    store.clear_checkpoint()?;
    store.refresh_default_csv()?;
    if !args.quiet {
        println!("stopped");
    }
    Ok(())
}

fn persist_session(store: &LogStore, session: UsageSession, quiet: bool) -> Result<()> {
    store.append_session(&session)?;
    if !quiet {
        println!(
            "saved -> {} {} [{}]",
            session.app_name,
            format_seconds(session.duration_seconds),
            session.activity_type
        );
    }
    Ok(())
}

fn checkpoint_current_session(
    store: &LogStore,
    state: &TrackerState,
    observed_at: chrono::DateTime<Local>,
) -> Result<()> {
    if let Some(entity) = state.current_entity() {
        store.checkpoint_session(entity, state.session_start(), observed_at)
    } else {
        store.clear_checkpoint()
    }
}

fn print_day(store: &LogStore, args: DayArgs) -> Result<()> {
    let date = date_or_today(args.date.as_deref())?;
    let sessions =
        store.sessions_for_day_with_open(date, Local::now(), DEFAULT_RECENT_CHECKPOINT_SECONDS)?;
    let summary = summarize_day(&sessions, date)?;
    if args.json {
        print_json(&summary)
    } else {
        print_summary_text(Some(date), &summary);
        Ok(())
    }
}

fn print_report(store: &LogStore, args: ReportArgs) -> Result<()> {
    let date = date_or_today(args.date.as_deref())?;
    let now = Local::now();
    let active_session = store.provisional_open_session(now, DEFAULT_RECENT_CHECKPOINT_SECONDS)?;
    let sessions =
        store.sessions_for_day_with_open(date, now, DEFAULT_RECENT_CHECKPOINT_SECONDS)?;
    let summary = summarize_day(&sessions, date)?;
    let timeline = timeline_blocks(&sessions);
    let (day_start, day_end) = day_bounds(date)?;
    let includes_active_session = active_session
        .as_ref()
        .is_some_and(|session| session.overlaps(day_start, day_end));
    if args.json {
        let value = serde_json::json!({
            "date": date,
            "generated_at": now,
            "summary": summary,
            "timeline": timeline,
            "sessions": sessions,
            "active_session": active_session,
            "open_session": store.open_session_checkpoint()?,
            "includes_active_session": includes_active_session,
            "paths": {
                "root": store.root(),
                "sqlite": store.db_path(),
                "sessions_jsonl": store.sessions_path(),
                "csv": store.csv_path(),
                "exports": store.exports_dir(),
                "logs": store.logs_dir(),
            },
        });
        print_json(&value)
    } else {
        print_summary_text(Some(date), &summary);
        print_session_rows(&sessions);
        Ok(())
    }
}

fn print_timeline(store: &LogStore, args: TimelineArgs) -> Result<()> {
    let date = date_or_today(args.date.as_deref())?;
    let sessions =
        store.sessions_for_day_with_open(date, Local::now(), DEFAULT_RECENT_CHECKPOINT_SECONDS)?;
    let timeline = timeline_blocks(&sessions);
    if args.json {
        print_json(&timeline)
    } else {
        for block in timeline {
            println!(
                "{} -> {} | {} | {} | {} | {} | {} | {}",
                block.start_time.format("%H:%M:%S"),
                block.end_time.format("%H:%M:%S"),
                format_seconds(block.duration_seconds),
                block.activity_type,
                block.category,
                block.app_name,
                block.domain.unwrap_or_default(),
                block.title.unwrap_or_default()
            );
        }
        Ok(())
    }
}

fn print_query(store: &LogStore, args: QueryArgs) -> Result<()> {
    let now = Local::now();
    let window = query_time_window(
        QueryTimeWindowInput {
            from: args.from.as_deref(),
            to: args.to.as_deref(),
            since: args.since.as_deref(),
            until: args.until.as_deref(),
            last_minutes: args.last_minutes,
        },
        now,
    )?;
    let sessions = store.sessions_in_window_with_open(
        window.start,
        window.end,
        now,
        DEFAULT_RECENT_CHECKPOINT_SECONDS,
    )?;
    let sessions = filter_sessions(
        sessions,
        args.app.as_deref(),
        args.title.as_deref(),
        args.category.as_deref(),
        args.domain.as_deref(),
        args.activity_type.as_deref(),
        args.limit,
    );
    let summary = summarize_all(&sessions);
    let timeline = timeline_blocks(&sessions);

    if args.json {
        let value = serde_json::json!({
            "generated_at": now,
            "from": window.from,
            "to": window.to,
            "since": window.since,
            "until": window.until,
            "last_minutes": window.last_minutes,
            "window_start": window.start,
            "window_end": window.end,
            "filters": {
                "app": args.app.as_deref(),
                "title": args.title.as_deref(),
                "category": args.category.as_deref(),
                "domain": args.domain.as_deref(),
                "activity_type": args.activity_type.as_deref(),
                "limit": args.limit,
            },
            "summary": summary,
            "timeline": timeline,
            "sessions": sessions,
            "open_session": store.open_session_checkpoint()?,
        });
        print_json(&value)
    } else {
        print_summary_text(None, &summary);
        print_session_rows(&sessions);
        Ok(())
    }
}

fn print_logs(store: &LogStore, args: LogsArgs) -> Result<()> {
    let date = date_or_today(args.date.as_deref())?;
    let sessions =
        store.sessions_for_day_with_open(date, Local::now(), DEFAULT_RECENT_CHECKPOINT_SECONDS)?;
    let sessions = filter_sessions(
        sessions,
        args.app.as_deref(),
        args.title.as_deref(),
        args.category.as_deref(),
        args.domain.as_deref(),
        args.activity_type.as_deref(),
        args.limit,
    );

    if args.json {
        print_json(&sessions)
    } else {
        print_session_rows(&sessions);
        Ok(())
    }
}

fn print_audit(store: &LogStore, args: AuditArgs) -> Result<()> {
    let date = date_or_today(args.date.as_deref())?;
    let now = Local::now();
    let sessions =
        store.sessions_for_day_with_open(date, now, DEFAULT_RECENT_CHECKPOINT_SECONDS)?;
    let audit = audit_sessions(&sessions, args.gap_threshold_seconds);
    let summary = summarize_day(&sessions, date)?;
    if args.json {
        let value = serde_json::json!({
            "date": date,
            "generated_at": now,
            "gap_threshold_seconds": args.gap_threshold_seconds.max(0.0),
            "summary": summary,
            "audit": audit,
            "open_session": store.open_session_checkpoint()?,
        });
        print_json(&value)
    } else {
        println!("date: {date}");
        println!("sessions: {}", audit.session_count);
        println!("gaps: {}", audit.gap_count);
        println!("overlaps: {}", audit.overlap_count);
        println!("invalid_sessions: {}", audit.invalid_session_count);
        println!("missing_titles: {}", audit.missing_title_count);
        println!("browser_missing_urls: {}", audit.browser_missing_url_count);
        println!("browser_blank_tabs: {}", audit.browser_blank_tab_count);
        println!(
            "browser_context_mismatches: {}",
            audit.browser_context_mismatch_count
        );
        println!(
            "uncategorized_sessions: {}",
            audit.uncategorized_session_count
        );
        println!("untracked_sessions: {}", audit.untracked_session_count);
        print_quality_rows("missing_title_by_app", &audit.missing_title_by_app);
        print_quality_rows(
            "browser_missing_url_by_title",
            &audit.browser_missing_url_by_title,
        );
        print_quality_rows("browser_blank_tab_by_app", &audit.browser_blank_tab_by_app);
        print_quality_rows(
            "browser_context_mismatch_by_domain",
            &audit.browser_context_mismatch_by_domain,
        );
        print_quality_rows("uncategorized_by_app", &audit.uncategorized_by_app);
        println!("total_gap: {}", format_seconds(audit.total_gap_seconds));
        println!("longest_gap: {}", format_seconds(audit.longest_gap_seconds));
        Ok(())
    }
}

fn print_session_rows(sessions: &[UsageSession]) {
    for session in sessions {
        println!(
            "{} -> {} | {} | {} | {} | {} | {} | {}",
            session.start_time.format("%H:%M:%S"),
            session.end_time.format("%H:%M:%S"),
            format_seconds(session.duration_seconds),
            session.activity_type,
            session.category,
            session.app_name,
            session.title.as_deref().unwrap_or_default(),
            session.url.as_deref().unwrap_or_default()
        );
    }
}

fn print_summary(store: &LogStore, args: OutputArgs) -> Result<()> {
    let sessions =
        store.load_sessions_with_open(Local::now(), DEFAULT_RECENT_CHECKPOINT_SECONDS)?;
    let summary = summarize_all(&sessions);
    if args.json {
        print_json(&summary)
    } else {
        print_summary_text(None, &summary);
        Ok(())
    }
}

fn export_sessions(store: &LogStore, args: ExportArgs) -> Result<()> {
    let sessions = match args.date.as_deref() {
        Some(date) => store.sessions_for_day(parse_date(date)?)?,
        None => store.load_sessions()?,
    };

    let output = match args.output {
        Some(path) => path,
        None => default_export_path(store, args.date.as_deref(), args.format)?,
    };

    match args.format {
        ExportFormat::Csv => store.write_csv(&output, &sessions)?,
        ExportFormat::Jsonl => write_jsonl(&output, &sessions)?,
    }

    println!("{}", output.display());
    Ok(())
}

fn import_csv(store: &LogStore, args: ImportCsvArgs) -> Result<()> {
    let report = store.import_csv(&args.path, args.dry_run)?;
    if args.json {
        print_json(&report)
    } else {
        println!("scanned: {}", report.scanned);
        println!("imported: {}", report.imported);
        println!("skipped_duplicates: {}", report.skipped_duplicates);
        println!("dry_run: {}", yes_no(report.dry_run));
        Ok(())
    }
}

fn reclassify(store: &LogStore, args: ReclassifyArgs) -> Result<()> {
    let now = Local::now();
    let window = repair_window(&args.window, now)?;
    let report = store.reclassify_sessions_in_window(window.start, window.end, args.dry_run)?;
    if args.json {
        print_windowed_json(&report, &window, now)
    } else {
        print_window_text(&window);
        println!("scanned: {}", report.scanned);
        println!("changed: {}", report.changed);
        println!("dry_run: {}", yes_no(report.dry_run));
        Ok(())
    }
}

fn repair_gaps(store: &LogStore, args: RepairGapsArgs) -> Result<()> {
    let now = Local::now();
    let window = repair_window(&args.window, now)?;
    let report = store.repair_gaps_in_window(
        window.start,
        window.end,
        args.gap_threshold_seconds,
        args.dry_run,
    )?;
    if args.json {
        print_windowed_json(&report, &window, now)
    } else {
        print_window_text(&window);
        println!("scanned: {}", report.scanned);
        println!("gaps_found: {}", report.gaps_found);
        println!("repaired: {}", report.repaired);
        println!("dry_run: {}", yes_no(report.dry_run));
        Ok(())
    }
}

fn repair_titles(store: &LogStore, args: RepairTitlesArgs) -> Result<()> {
    let now = Local::now();
    let window = repair_window(&args.window, now)?;
    let report = store.repair_titles_in_window(window.start, window.end, args.dry_run)?;
    if args.json {
        print_windowed_json(&report, &window, now)
    } else {
        print_window_text(&window);
        println!("scanned: {}", report.scanned);
        println!("repaired: {}", report.repaired);
        println!("native_repaired: {}", report.native_repaired);
        println!("browser_repaired: {}", report.browser_repaired);
        println!("dry_run: {}", yes_no(report.dry_run));
        Ok(())
    }
}

fn repair_urls(store: &LogStore, args: RepairUrlsArgs) -> Result<()> {
    let now = Local::now();
    let window = repair_window(&args.window, now)?;
    let report = store.repair_urls_in_window(window.start, window.end, args.dry_run)?;
    if args.json {
        print_windowed_json(&report, &window, now)
    } else {
        print_window_text(&window);
        println!("scanned: {}", report.scanned);
        println!("repaired: {}", report.repaired);
        println!("blank_tab_urls: {}", report.blank_tab_urls);
        println!("blank_tab_context_urls: {}", report.blank_tab_context_urls);
        println!("dry_run: {}", yes_no(report.dry_run));
        Ok(())
    }
}

fn repair_context(store: &LogStore, args: RepairContextArgs) -> Result<()> {
    let now = Local::now();
    let window = repair_window(&args.window, now)?;
    let report = store.repair_context_in_window(window.start, window.end, args.dry_run)?;
    if args.json {
        print_windowed_json(&report, &window, now)
    } else {
        print_window_text(&window);
        println!("scanned: {}", report.scanned);
        println!("mismatches_found: {}", report.mismatches_found);
        println!("missing_titles_found: {}", report.missing_titles_found);
        println!("missing_urls_found: {}", report.missing_urls_found);
        println!("repaired: {}", report.repaired);
        println!("title_repaired: {}", report.title_repaired);
        println!("url_repaired: {}", report.url_repaired);
        println!("missing_title_repaired: {}", report.missing_title_repaired);
        println!("missing_url_repaired: {}", report.missing_url_repaired);
        println!("neighbor_repaired: {}", report.neighbor_repaired);
        println!(
            "unique_observation_repaired: {}",
            report.unique_observation_repaired
        );
        println!("dry_run: {}", yes_no(report.dry_run));
        Ok(())
    }
}

fn print_paths(store: &LogStore, args: OutputArgs) -> Result<()> {
    if args.json {
        let value = serde_json::json!({
            "root": store.root(),
            "sqlite": store.db_path(),
            "sessions_jsonl": store.sessions_path(),
            "csv": store.csv_path(),
            "exports": store.exports_dir(),
            "logs": store.logs_dir(),
            "legacy_root": legacy_data_dir(),
            "legacy_sessions_jsonl": legacy_sessions_path(),
        });
        print_json(&value)
    } else {
        println!("root: {}", store.root().display());
        println!("sqlite: {}", store.db_path().display());
        println!("sessions_jsonl: {}", store.sessions_path().display());
        println!("csv: {}", store.csv_path().display());
        println!("exports: {}", store.exports_dir().display());
        println!("logs: {}", store.logs_dir().display());
        if let Some(path) = legacy_data_dir() {
            println!("legacy_root: {}", path.display());
        }
        if let Some(path) = legacy_sessions_path() {
            println!("legacy_sessions_jsonl: {}", path.display());
        }
        Ok(())
    }
}

fn print_health(store: &LogStore, args: HealthArgs) -> Result<()> {
    let now = Local::now();
    let date = now.date_naive();
    let storage = store.storage_health(now, args.stale_threshold_seconds)?;
    let sessions =
        store.sessions_for_day_with_open(date, now, DEFAULT_RECENT_CHECKPOINT_SECONDS)?;
    let audit = audit_sessions(&sessions, args.gap_threshold_seconds);
    let service = service_status_report();
    let healthy = storage.fresh && service.running;

    if args.json {
        let today_audit = compact_audit(audit.clone());
        let value = serde_json::json!({
            "generated_at": now,
            "date": date,
            "healthy": healthy,
            "fresh": storage.fresh,
            "service": service,
            "storage": storage,
            "today_audit": today_audit,
            "gap_threshold_seconds": args.gap_threshold_seconds.max(0.0),
            "paths": {
                "root": store.root(),
                "sqlite": store.db_path(),
                "sessions_jsonl": store.sessions_path(),
                "csv": store.csv_path(),
                "exports": store.exports_dir(),
                "logs": store.logs_dir(),
            },
        });
        print_json(&value)
    } else {
        println!("healthy: {}", yes_no(healthy));
        println!("fresh: {}", yes_no(storage.fresh));
        println!("service_running: {}", yes_no(service.running));
        if let Some(pid) = service.pid {
            println!("service_pid: {pid}");
        }
        println!("session_count: {}", storage.session_count);
        if let Some(seconds) = storage.latest_observed_age_seconds {
            println!("latest_observed_age: {}", format_seconds(seconds));
        }
        println!("today_sessions: {}", audit.session_count);
        println!("today_gaps: {}", audit.gap_count);
        println!("today_overlaps: {}", audit.overlap_count);
        println!("today_invalid_sessions: {}", audit.invalid_session_count);
        println!("today_missing_titles: {}", audit.missing_title_count);
        println!(
            "today_browser_missing_urls: {}",
            audit.browser_missing_url_count
        );
        println!(
            "today_browser_blank_tabs: {}",
            audit.browser_blank_tab_count
        );
        println!(
            "today_browser_context_mismatches: {}",
            audit.browser_context_mismatch_count
        );
        println!(
            "today_uncategorized_sessions: {}",
            audit.uncategorized_session_count
        );
        println!(
            "today_untracked_sessions: {}",
            audit.untracked_session_count
        );
        print_quality_rows("today_missing_title_by_app", &audit.missing_title_by_app);
        print_quality_rows(
            "today_browser_missing_url_by_title",
            &audit.browser_missing_url_by_title,
        );
        print_quality_rows(
            "today_browser_blank_tab_by_app",
            &audit.browser_blank_tab_by_app,
        );
        print_quality_rows("today_uncategorized_by_app", &audit.uncategorized_by_app);
        Ok(())
    }
}

fn print_agent(store: &LogStore, args: AgentArgs) -> Result<()> {
    if args.date.is_some() && args.last_minutes.is_some() {
        return Err(TrackerError::ConflictingQueryWindowArgs(
            "agent date cannot be combined with --last-minutes",
        ));
    }

    let now = Local::now();
    let today = now.date_naive();
    let service = service_status_report();
    let storage = store.storage_health(now, DEFAULT_HEALTH_STALE_THRESHOLD_SECONDS)?;
    let today_sessions =
        store.sessions_for_day_with_open(today, now, DEFAULT_RECENT_CHECKPOINT_SECONDS)?;
    let today_audit = audit_sessions(&today_sessions, DEFAULT_AUDIT_GAP_THRESHOLD_SECONDS);
    let today_quality = agent_quality(&today_audit, AgentRepairWindow::Date(today));
    let today_warnings = agent_warnings(&service, &storage, &today_audit);

    let (
        window_mode,
        window_date,
        window_last_minutes,
        window_start,
        window_end,
        repair_window,
        sessions,
    ) = if let Some(date_input) = args.date.as_deref() {
        let date = parse_date(date_input)?;
        let (start, end) = day_bounds(date)?;
        (
            "day",
            Some(date),
            None,
            Some(start),
            Some(end),
            AgentRepairWindow::Date(date),
            store.sessions_for_day_with_open(date, now, DEFAULT_RECENT_CHECKPOINT_SECONDS)?,
        )
    } else {
        let last_minutes = args.last_minutes.unwrap_or(DEFAULT_AGENT_LAST_MINUTES);
        let window = query_time_window(
            QueryTimeWindowInput {
                from: None,
                to: None,
                since: None,
                until: None,
                last_minutes: Some(last_minutes),
            },
            now,
        )?;
        (
            "last_minutes",
            None,
            Some(last_minutes),
            window.start,
            window.end,
            AgentRepairWindow::LastMinutes(last_minutes),
            store.sessions_in_window_with_open(
                window.start,
                window.end,
                now,
                DEFAULT_RECENT_CHECKPOINT_SECONDS,
            )?,
        )
    };

    let window_audit = audit_sessions(&sessions, DEFAULT_AUDIT_GAP_THRESHOLD_SECONDS);
    let ready = agent_ready(&service, &storage, &window_audit);
    let warnings = agent_warnings(&service, &storage, &window_audit);
    let quality = agent_quality(&window_audit, repair_window);
    let summary = summarize_all(&sessions);
    let summary_truncated = summary_rows_truncated(&summary, args.summary_limit);
    let summary = limit_summary(summary, args.summary_limit);
    let timeline = timeline_blocks(&sessions);
    let timeline_count = timeline.len();
    let timeline = limit_timeline_blocks(timeline, args.timeline_limit);
    let timeline_truncated = timeline_count > timeline.len();
    let sessions_value = if args.include_sessions {
        serde_json::to_value(&sessions)?
    } else {
        serde_json::Value::Null
    };

    if args.json {
        let today_audit = compact_audit(today_audit.clone());
        let value = serde_json::json!({
            "generated_at": now,
            "ready": ready,
            "quality": quality,
            "warnings": warnings,
            "window": {
                "mode": window_mode,
                "date": window_date,
                "last_minutes": window_last_minutes,
                "start": window_start,
                "end": window_end,
            },
            "health": {
                "service_running": service.running,
                "service_pid": service.pid,
                "fresh": storage.fresh,
                "latest_observed_at": storage.latest_observed_at,
                "latest_observed_age_seconds": storage.latest_observed_age_seconds,
                "session_count": storage.session_count,
            },
            "window_audit": window_audit,
            "today_audit": today_audit,
            "today_quality": today_quality,
            "today_warnings": today_warnings,
            "summary": summary,
            "timeline": timeline,
            "timeline_count": timeline_count,
            "timeline_returned": timeline.len(),
            "timeline_truncated": timeline_truncated,
            "summary_limit": args.summary_limit,
            "summary_truncated": summary_truncated,
            "sessions": sessions_value,
            "include_sessions": args.include_sessions,
            "open_session": store.open_session_checkpoint()?,
            "paths": {
                "root": store.root(),
                "sqlite": store.db_path(),
                "sessions_jsonl": store.sessions_path(),
                "csv": store.csv_path(),
                "exports": store.exports_dir(),
                "logs": store.logs_dir(),
            },
        });
        print_json(&value)
    } else {
        println!("ready: {}", yes_no(ready));
        println!("quality_ready: {}", yes_no(quality.ready));
        println!("quality_score: {}", quality.score);
        println!("quality_status: {}", quality.status);
        println!("service_running: {}", yes_no(service.running));
        println!("fresh: {}", yes_no(storage.fresh));
        println!("window: {window_mode}");
        if let Some(date) = window_date {
            println!("date: {date}");
        }
        if let Some(minutes) = window_last_minutes {
            println!("last_minutes: {minutes}");
        }
        println!("sessions: {}", summary.session_count);
        println!("timeline_blocks: {}/{}", timeline.len(), timeline_count);
        if warnings.is_empty() {
            println!("warnings: none");
        } else {
            println!("warnings:");
            for warning in warnings {
                println!("  {warning}");
            }
        }
        if !quality.repair_commands.is_empty() {
            println!("repair_commands:");
            for command in quality.repair_commands {
                println!("  {command}");
            }
        }
        Ok(())
    }
}

fn compact_audit(mut audit: ActivityAudit) -> ActivityAudit {
    audit.quality_issues.clear();
    audit
}

fn doctor(store: &LogStore, args: OutputArgs) -> Result<()> {
    store.ensure_dirs()?;
    let checkpoint = store.open_session_checkpoint()?;
    let probe = MacOsProbe;
    let active = probe.active_entity()?;
    let idle_seconds = probe.idle_seconds()?;
    let osascript = std::process::Command::new("osascript")
        .arg("-e")
        .arg("return \"ok\"")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);

    if args.json {
        let value = serde_json::json!({
            "data_dir_writable": true,
            "osascript": osascript,
            "active_entity": active,
            "idle_seconds": idle_seconds,
            "sqlite": store.db_path(),
            "sessions_path": store.sessions_path(),
            "open_session": checkpoint,
            "legacy_sessions_path": legacy_sessions_path(),
        });
        print_json(&value)
    } else {
        println!("data_dir_writable: yes");
        println!("osascript: {}", yes_no(osascript));
        if let Some(entity) = active {
            println!(
                "active_entity: {} | {} | {} | {}",
                entity.name,
                entity.bundle_id,
                entity.category,
                entity.title.unwrap_or_default()
            );
        } else {
            println!("active_entity: unavailable");
            println!("hint: grant Accessibility permission to terminal/app running tracker");
        }
        if let Some(seconds) = idle_seconds {
            println!("idle_seconds: {seconds:.1}");
        }
        Ok(())
    }
}

fn run_service(store: &LogStore, command: ServiceCommand) -> Result<()> {
    match command.action {
        ServiceAction::Install(args) => {
            let binary = match args.bin {
                Some(path) => path,
                None => std::env::current_exe()?,
            };
            let plist = install_launch_agent(&binary, store, !args.no_load)?;
            println!("{}", plist.display());
            Ok(())
        }
        ServiceAction::Uninstall => {
            let plist = uninstall_launch_agent(true)?;
            println!("{}", plist.display());
            Ok(())
        }
        ServiceAction::Status(args) => print_service_status(args),
    }
}

fn print_service_status(args: OutputArgs) -> Result<()> {
    if args.json {
        print_json(&service_status_report())
    } else {
        print!("{}", service_status()?);
        io::stdout().flush()?;
        Ok(())
    }
}

fn print_summary_text(date: Option<NaiveDate>, summary: &activity_tracker::ActivitySummary) {
    if let Some(date) = date {
        println!("date: {date}");
    } else {
        println!("date: all");
    }
    println!("sessions: {}", summary.session_count);
    println!(
        "total: {} ({:.1}s)",
        format_seconds(summary.total_seconds),
        summary.total_seconds
    );
    print_rows("by_category", &summary.by_category);
    print_rows("by_activity_type", &summary.by_activity_type);
    print_rows("by_app", &summary.by_app);
    print_rows("by_domain", &summary.by_domain);
}

fn print_rows(label: &str, rows: &[activity_tracker::SummaryRow]) {
    if rows.is_empty() {
        return;
    }
    println!("{label}:");
    for row in rows {
        println!(
            "  {} | {} | {:.1}%",
            row.name,
            format_seconds(row.seconds),
            row.percentage
        );
    }
}

fn print_quality_rows(label: &str, rows: &[activity_tracker::AuditQualityRow]) {
    if rows.is_empty() {
        return;
    }
    println!("{label}:");
    for row in rows {
        println!("  {} | {}", row.name, row.count);
    }
}

#[derive(Debug, Clone, Serialize)]
struct AgentQuality {
    ready: bool,
    coverage_ready: bool,
    context_ready: bool,
    status: &'static str,
    score: u8,
    issue_count: usize,
    blocking_issue_count: usize,
    context_issue_count: usize,
    repair_commands: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
enum AgentRepairWindow {
    Date(NaiveDate),
    LastMinutes(u64),
}

fn agent_ready(
    service: &activity_tracker::ServiceStatusReport,
    storage: &activity_tracker::StorageHealth,
    audit: &activity_tracker::ActivityAudit,
) -> bool {
    service.running
        && storage.fresh
        && audit.gap_count == 0
        && audit.overlap_count == 0
        && audit.invalid_session_count == 0
}

fn agent_quality(
    audit: &activity_tracker::ActivityAudit,
    repair_window: AgentRepairWindow,
) -> AgentQuality {
    let blocking_issue_count = audit.gap_count + audit.overlap_count + audit.invalid_session_count;
    let context_issue_count = audit.uncategorized_session_count
        + audit.browser_missing_url_count
        + audit.browser_context_mismatch_count
        + audit.missing_title_count;
    let issue_count = blocking_issue_count + context_issue_count;
    let coverage_ready = blocking_issue_count == 0;
    let context_ready = context_issue_count == 0;
    let status = if !coverage_ready {
        "needs_coverage_repair"
    } else if !context_ready {
        "usable_with_context_warnings"
    } else {
        "clean"
    };
    let penalty = audit.gap_count * 20
        + audit.overlap_count * 20
        + audit.invalid_session_count * 20
        + audit.uncategorized_session_count * 3
        + audit.browser_missing_url_count * 5
        + audit.browser_context_mismatch_count * 5
        + audit.missing_title_count;
    let score = 100usize.saturating_sub(penalty.min(100)) as u8;
    let mut repair_commands = Vec::new();
    if audit.gap_count > 0 {
        repair_commands.push(agent_repair_command("repair-gaps", repair_window));
    }
    if audit.uncategorized_session_count > 0 {
        repair_commands.push(agent_repair_command("reclassify", repair_window));
    }
    if audit.missing_title_count > 0 {
        repair_commands.push(agent_repair_command("repair-titles", repair_window));
    }
    if audit.browser_missing_url_count > 0 {
        repair_commands.push(agent_repair_command("repair-urls", repair_window));
    }
    if audit.browser_context_mismatch_count > 0
        || audit.missing_title_count > 0
        || audit.browser_missing_url_count > 0
    {
        repair_commands.push(agent_repair_command("repair-context", repair_window));
    }

    AgentQuality {
        ready: coverage_ready && context_ready,
        coverage_ready,
        context_ready,
        status,
        score,
        issue_count,
        blocking_issue_count,
        context_issue_count,
        repair_commands,
    }
}

fn agent_repair_command(command: &str, window: AgentRepairWindow) -> String {
    match window {
        AgentRepairWindow::Date(date) => {
            format!("activity_tracker {command} --from {date} --to {date} --dry-run --json")
        }
        AgentRepairWindow::LastMinutes(minutes) => {
            format!("activity_tracker {command} --last-minutes {minutes} --dry-run --json")
        }
    }
}

fn agent_warnings(
    service: &activity_tracker::ServiceStatusReport,
    storage: &activity_tracker::StorageHealth,
    audit: &activity_tracker::ActivityAudit,
) -> Vec<String> {
    let mut warnings = Vec::new();
    if !service.running {
        warnings.push("service_not_running".to_string());
    }
    if !storage.fresh {
        warnings.push("storage_stale".to_string());
    }
    if audit.gap_count > 0 {
        warnings.push(format!("gaps_detected:{}", audit.gap_count));
    }
    if audit.overlap_count > 0 {
        warnings.push(format!("overlaps_detected:{}", audit.overlap_count));
    }
    if audit.invalid_session_count > 0 {
        warnings.push(format!("invalid_sessions:{}", audit.invalid_session_count));
    }
    if audit.uncategorized_session_count > 0 {
        warnings.push(format!(
            "uncategorized:{}",
            audit.uncategorized_session_count
        ));
    }
    if audit.browser_missing_url_count > 0 {
        warnings.push(format!(
            "browser_missing_urls:{}",
            audit.browser_missing_url_count
        ));
    }
    if audit.browser_context_mismatch_count > 0 {
        warnings.push(format!(
            "browser_context_mismatches:{}",
            audit.browser_context_mismatch_count
        ));
    }
    if audit.missing_title_count > 0 {
        warnings.push(format!("missing_titles:{}", audit.missing_title_count));
    }
    warnings
}

fn limit_timeline_blocks(
    timeline: Vec<activity_tracker::TimelineBlock>,
    limit: usize,
) -> Vec<activity_tracker::TimelineBlock> {
    let skip = timeline.len().saturating_sub(limit);
    timeline.into_iter().skip(skip).collect()
}

fn summary_rows_truncated(summary: &activity_tracker::ActivitySummary, limit: usize) -> bool {
    summary.by_category.len() > limit
        || summary.by_activity_type.len() > limit
        || summary.by_app.len() > limit
        || summary.by_domain.len() > limit
}

fn limit_summary(
    summary: activity_tracker::ActivitySummary,
    limit: usize,
) -> activity_tracker::ActivitySummary {
    activity_tracker::ActivitySummary {
        session_count: summary.session_count,
        total_seconds: summary.total_seconds,
        by_activity_type: limit_summary_rows(summary.by_activity_type, limit),
        by_category: limit_summary_rows(summary.by_category, limit),
        by_app: limit_summary_rows(summary.by_app, limit),
        by_domain: limit_summary_rows(summary.by_domain, limit),
    }
}

fn limit_summary_rows(
    rows: Vec<activity_tracker::SummaryRow>,
    limit: usize,
) -> Vec<activity_tracker::SummaryRow> {
    rows.into_iter().take(limit).collect()
}

fn repair_window(args: &RepairWindowArgs, now: chrono::DateTime<Local>) -> Result<QueryTimeWindow> {
    query_time_window(
        QueryTimeWindowInput {
            from: args.from.as_deref(),
            to: args.to.as_deref(),
            since: args.since.as_deref(),
            until: args.until.as_deref(),
            last_minutes: args.last_minutes,
        },
        now,
    )
}

fn print_windowed_json<T: Serialize>(
    report: &T,
    window: &QueryTimeWindow,
    generated_at: chrono::DateTime<Local>,
) -> Result<()> {
    let mut value = serde_json::to_value(report)?;
    if let Some(object) = value.as_object_mut() {
        object.insert("generated_at".to_string(), serde_json::json!(generated_at));
        object.insert(
            "window".to_string(),
            serde_json::json!({
                "from": window.from,
                "to": window.to,
                "since": window.since,
                "until": window.until,
                "last_minutes": window.last_minutes,
                "start": window.start,
                "end": window.end,
            }),
        );
    }
    print_json(&value)
}

fn print_window_text(window: &QueryTimeWindow) {
    match (window.start, window.end) {
        (None, None) => println!("window: all"),
        (Some(start), Some(end)) => println!("window: {start} -> {end}"),
        (Some(start), None) => println!("window: {start} -> all"),
        (None, Some(end)) => println!("window: all -> {end}"),
    }
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    serde_json::to_writer_pretty(&mut handle, value)?;
    writeln!(handle)?;
    Ok(())
}

fn write_jsonl(path: &PathBuf, sessions: &[UsageSession]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::File::create(path)?;
    for session in sessions {
        serde_json::to_writer(&mut file, session)?;
        file.write_all(b"\n")?;
    }
    file.flush()?;
    Ok(())
}

fn date_or_today(input: Option<&str>) -> Result<NaiveDate> {
    input.map_or_else(|| Ok(Local::now().date_naive()), parse_date)
}

fn default_export_path(
    store: &LogStore,
    date: Option<&str>,
    format: ExportFormat,
) -> Result<PathBuf> {
    let stem = match date {
        Some(value) => value.to_string(),
        None => "all".to_string(),
    };
    let extension = match format {
        ExportFormat::Csv => "csv",
        ExportFormat::Jsonl => "jsonl",
    };
    store.ensure_dirs()?;
    Ok(store.exports_dir().join(format!("{stem}.{extension}")))
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("activity_tracker=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

fn observed_entity(
    probe: &MacOsProbe,
    idle_threshold_seconds: u64,
) -> Result<Option<activity_tracker::ActiveEntity>> {
    let active = probe.active_entity()?;
    let idle_seconds = probe.idle_seconds()?;
    if idle_seconds.is_some_and(|seconds| seconds >= idle_threshold_seconds as f64) {
        Ok(Some(activity_tracker::idle_entity()))
    } else {
        Ok(active)
    }
}

fn active_entity_or_none(probe: &MacOsProbe) -> Option<activity_tracker::ActiveEntity> {
    match probe.active_entity() {
        Ok(entity) => entity,
        Err(error) => {
            tracing::warn!(error = %error, "active probe failed");
            None
        }
    }
}

fn idle_seconds_or_none(probe: &MacOsProbe) -> Option<f64> {
    match probe.idle_seconds() {
        Ok(seconds) => seconds,
        Err(error) => {
            tracing::warn!(error = %error, "idle probe failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audit_with_counts(
        gaps: usize,
        overlaps: usize,
        invalid: usize,
        missing_titles: usize,
        missing_urls: usize,
        mismatches: usize,
        uncategorized: usize,
    ) -> ActivityAudit {
        ActivityAudit {
            session_count: 10,
            gap_count: gaps,
            overlap_count: overlaps,
            invalid_session_count: invalid,
            active_session_count: 10,
            idle_session_count: 0,
            untracked_session_count: 0,
            missing_title_count: missing_titles,
            browser_session_count: 10,
            browser_missing_url_count: missing_urls,
            browser_blank_tab_count: 0,
            browser_context_mismatch_count: mismatches,
            uncategorized_session_count: uncategorized,
            missing_title_by_app: Vec::new(),
            browser_missing_url_by_app: Vec::new(),
            browser_missing_url_by_title: Vec::new(),
            browser_blank_tab_by_app: Vec::new(),
            browser_context_mismatch_by_domain: Vec::new(),
            uncategorized_by_app: Vec::new(),
            quality_issues: Vec::new(),
            total_gap_seconds: 0.0,
            longest_gap_seconds: 0.0,
            gaps: Vec::new(),
            overlaps: Vec::new(),
            invalid_sessions: Vec::new(),
        }
    }

    #[test]
    fn agent_quality_marks_clean_audit_ready() {
        let quality = agent_quality(
            &audit_with_counts(0, 0, 0, 0, 0, 0, 0),
            AgentRepairWindow::LastMinutes(120),
        );

        assert!(quality.ready);
        assert!(quality.coverage_ready);
        assert!(quality.context_ready);
        assert_eq!(quality.status, "clean");
        assert_eq!(quality.score, 100);
        assert!(quality.repair_commands.is_empty());
    }

    #[test]
    fn agent_quality_suggests_context_repairs_without_blocking_coverage() {
        let quality = agent_quality(
            &audit_with_counts(0, 0, 0, 3, 2, 1, 0),
            AgentRepairWindow::LastMinutes(120),
        );

        assert!(!quality.ready);
        assert!(quality.coverage_ready);
        assert!(!quality.context_ready);
        assert_eq!(quality.status, "usable_with_context_warnings");
        assert_eq!(quality.blocking_issue_count, 0);
        assert_eq!(quality.context_issue_count, 6);
        assert_eq!(
            quality.repair_commands,
            vec![
                "activity_tracker repair-titles --last-minutes 120 --dry-run --json",
                "activity_tracker repair-urls --last-minutes 120 --dry-run --json",
                "activity_tracker repair-context --last-minutes 120 --dry-run --json"
            ]
        );
    }

    #[test]
    fn agent_quality_suggests_coverage_repairs_for_structural_issues() -> Result<()> {
        let date = parse_date("2026-06-03")?;
        let quality = agent_quality(
            &audit_with_counts(1, 1, 1, 0, 0, 0, 0),
            AgentRepairWindow::Date(date),
        );

        assert!(!quality.ready);
        assert!(!quality.coverage_ready);
        assert!(quality.context_ready);
        assert_eq!(quality.status, "needs_coverage_repair");
        assert_eq!(quality.blocking_issue_count, 3);
        assert_eq!(
            quality.repair_commands,
            vec!["activity_tracker repair-gaps --from 2026-06-03 --to 2026-06-03 --dry-run --json"]
        );
        Ok(())
    }
}
