use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use activity_tracker::{
    ActivityProbe, DEFAULT_AUDIT_GAP_THRESHOLD_SECONDS, DEFAULT_IDLE_THRESHOLD_SECONDS,
    DEFAULT_INTERVAL_SECONDS, DEFAULT_PROBE_MISS_TOLERANCE, DEFAULT_RECENT_CHECKPOINT_SECONDS,
    LogStore, MacOsProbe, ProbeMissStabilizer, QueryTimeWindowInput, Result, TrackerError,
    TrackerState, UsageSession, audit_sessions, day_bounds, filter_sessions,
    filter_sessions_by_time_window, format_seconds, install_launch_agent, legacy_data_dir,
    legacy_sessions_path, parse_date, query_time_window, service_status, service_status_report,
    summarize_all, summarize_day, timeline_blocks, uninstall_launch_agent,
};
use chrono::{Local, NaiveDate};
use clap::{Args, Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

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
    /// Print storage and service paths.
    Paths(OutputArgs),
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
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RepairGapsArgs {
    #[arg(long, default_value_t = DEFAULT_AUDIT_GAP_THRESHOLD_SECONDS)]
    gap_threshold_seconds: f64,
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
        Command::Paths(args) => print_paths(&store, args),
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
    checkpoint_current_session(store, &state, Local::now())?;

    if !args.quiet {
        println!("tracking -> {}", store.db_path().display());
    }

    while running.load(Ordering::SeqCst) {
        thread::sleep(interval);
        let next_entity =
            stabilizer.stabilize(active_entity_or_none(&probe), state.current_entity());
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
    let sessions = store.load_sessions_with_open(now, DEFAULT_RECENT_CHECKPOINT_SECONDS)?;
    let sessions = filter_sessions_by_time_window(sessions, window.start, window.end);
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
    let report = store.reclassify_sessions(args.dry_run)?;
    if args.json {
        print_json(&report)
    } else {
        println!("scanned: {}", report.scanned);
        println!("changed: {}", report.changed);
        println!("dry_run: {}", yes_no(report.dry_run));
        Ok(())
    }
}

fn repair_gaps(store: &LogStore, args: RepairGapsArgs) -> Result<()> {
    let report = store.repair_gaps(args.gap_threshold_seconds, args.dry_run)?;
    if args.json {
        print_json(&report)
    } else {
        println!("scanned: {}", report.scanned);
        println!("gaps_found: {}", report.gaps_found);
        println!("repaired: {}", report.repaired);
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
