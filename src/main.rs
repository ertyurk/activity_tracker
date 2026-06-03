use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use activity_tracker::{
    ActivityProbe, DEFAULT_IDLE_THRESHOLD_SECONDS, DEFAULT_INTERVAL_SECONDS, LogStore, MacOsProbe,
    Result, TrackerError, TrackerState, UsageSession, filter_sessions, format_seconds,
    install_launch_agent, parse_date, service_status, summarize_all, summarize_day,
    uninstall_launch_agent,
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
    /// Print raw sessions. Defaults to today.
    Logs(LogsArgs),
    /// Print all-time summary.
    Summary(OutputArgs),
    /// Export sessions to CSV or JSONL.
    Export(ExportArgs),
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
struct LogsArgs {
    date: Option<String>,
    #[arg(long)]
    app: Option<String>,
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
    Status,
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
        Command::Logs(args) => print_logs(&store, args),
        Command::Summary(args) => print_summary(&store, args),
        Command::Export(args) => export_sessions(&store, args),
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
    let probe = MacOsProbe;
    let interval = Duration::from_secs(args.interval_seconds.max(1));
    let mut state = TrackerState::new(
        observed_entity(&probe, args.idle_threshold_seconds)?,
        Local::now(),
        args.idle_threshold_seconds,
    );

    if !args.quiet {
        println!("tracking -> {}", store.sessions_path().display());
    }

    while running.load(Ordering::SeqCst) {
        thread::sleep(interval);
        let next_entity = active_entity_or_none(&probe);
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
    }

    if let Some(session) = state.finish(Local::now()) {
        persist_session(store, session, args.quiet)?;
    }
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

fn print_day(store: &LogStore, args: DayArgs) -> Result<()> {
    let date = date_or_today(args.date.as_deref())?;
    let sessions = store.sessions_for_day(date)?;
    let summary = summarize_day(&sessions, date)?;
    if args.json {
        print_json(&summary)
    } else {
        print_summary_text(Some(date), &summary);
        Ok(())
    }
}

fn print_logs(store: &LogStore, args: LogsArgs) -> Result<()> {
    let date = date_or_today(args.date.as_deref())?;
    let sessions = store.sessions_for_day(date)?;
    let sessions = filter_sessions(
        sessions,
        args.app.as_deref(),
        args.category.as_deref(),
        args.domain.as_deref(),
        args.activity_type.as_deref(),
        args.limit,
    );

    if args.json {
        print_json(&sessions)
    } else {
        for session in sessions {
            let url = session.url.unwrap_or_default();
            println!(
                "{} -> {} | {} | {} | {} | {} | {}",
                session.start_time.format("%H:%M:%S"),
                session.end_time.format("%H:%M:%S"),
                format_seconds(session.duration_seconds),
                session.activity_type,
                session.category,
                session.app_name,
                url
            );
        }
        Ok(())
    }
}

fn print_summary(store: &LogStore, args: OutputArgs) -> Result<()> {
    let sessions = store.load_sessions()?;
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

fn print_paths(store: &LogStore, args: OutputArgs) -> Result<()> {
    if args.json {
        let value = serde_json::json!({
            "root": store.root(),
            "sessions_jsonl": store.sessions_path(),
            "csv": store.csv_path(),
            "exports": store.exports_dir(),
            "logs": store.logs_dir(),
        });
        print_json(&value)
    } else {
        println!("root: {}", store.root().display());
        println!("sessions_jsonl: {}", store.sessions_path().display());
        println!("csv: {}", store.csv_path().display());
        println!("exports: {}", store.exports_dir().display());
        println!("logs: {}", store.logs_dir().display());
        Ok(())
    }
}

fn doctor(store: &LogStore, args: OutputArgs) -> Result<()> {
    store.ensure_dirs()?;
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
            "sessions_path": store.sessions_path(),
        });
        print_json(&value)
    } else {
        println!("data_dir_writable: yes");
        println!("osascript: {}", yes_no(osascript));
        if let Some(entity) = active {
            println!(
                "active_entity: {} | {} | {}",
                entity.name, entity.bundle_id, entity.category
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
        ServiceAction::Status => {
            print!("{}", service_status()?);
            io::stdout().flush()?;
            Ok(())
        }
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
