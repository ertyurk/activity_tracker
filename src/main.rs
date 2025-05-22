use chrono::{DateTime, Local};
use csv::Writer;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct ActiveEntity {
    bundle_id: String,
    name: String,
    url: Option<String>,
    category: Option<String>, // New field for categorizing apps
}

#[derive(Debug, Serialize, Deserialize)]
struct UsageSession {
    #[serde(rename = "Start Time")]
    start_time: DateTime<Local>,
    #[serde(rename = "End Time")]
    end_time: DateTime<Local>,
    #[serde(rename = "Duration (seconds)")]
    duration_seconds: f64,
    #[serde(rename = "App Name")]
    app_name: String,
    #[serde(rename = "Bundle ID")]
    bundle_id: String,
    #[serde(rename = "Category")]
    category: String,
    #[serde(rename = "URL")]
    url: String,
}

impl UsageSession {
    fn from_entity(
        entity: &ActiveEntity,
        start: DateTime<Local>,
        end: DateTime<Local>,
        duration: Duration,
    ) -> Self {
        Self {
            start_time: start,
            end_time: end,
            duration_seconds: duration.as_secs_f64(),
            app_name: entity.name.clone(),
            bundle_id: entity.bundle_id.clone(),
            category: entity
                .category
                .clone()
                .unwrap_or_else(|| "Uncategorized".to_string()),
            url: entity.url.clone().unwrap_or_else(|| "".to_string()),
        }
    }
}

#[derive(Debug)]
struct UsageStats {
    sessions: Vec<UsageSession>,
    total_duration: Duration,
    last_updated: DateTime<Local>,
}

impl UsageStats {
    fn new() -> Self {
        Self {
            sessions: Vec::new(),
            total_duration: Duration::ZERO,
            last_updated: Local::now(),
        }
    }

    fn add_session(
        &mut self,
        entity: &ActiveEntity,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
        duration: Duration,
    ) {
        self.sessions.push(UsageSession::from_entity(
            entity, start_time, end_time, duration,
        ));
        self.total_duration += duration;
        self.last_updated = Local::now();
    }

    fn save_to_file(&self, path: &PathBuf) -> Result<(), String> {
        let mut wtr =
            Writer::from_path(path).map_err(|e| format!("Failed to create CSV file: {}", e))?;

        // Write header
        wtr.write_record(&[
            "Start Time",
            "End Time",
            "Duration (seconds)",
            "App Name",
            "Bundle ID",
            "Category",
            "URL",
        ])
        .map_err(|e| format!("Failed to write CSV header: {}", e))?;

        // Write each session
        for session in &self.sessions {
            wtr.serialize(session)
                .map_err(|e| format!("Failed to write session to CSV: {}", e))?;
        }

        wtr.flush()
            .map_err(|e| format!("Failed to flush CSV file: {}", e))?;

        Ok(())
    }

    fn load_from_file(path: &PathBuf) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self::new());
        }

        let mut rdr =
            csv::Reader::from_path(path).map_err(|e| format!("Failed to read CSV file: {}", e))?;

        let mut stats = Self::new();
        let mut total_duration = Duration::ZERO;

        for result in rdr.deserialize() {
            let session: UsageSession =
                result.map_err(|e| format!("Failed to parse CSV record: {}", e))?;

            total_duration += Duration::from_secs_f64(session.duration_seconds);
            stats.sessions.push(session);
        }

        stats.total_duration = total_duration;
        stats.last_updated = Local::now();
        Ok(stats)
    }
}

fn run_osascript(script: &str) -> Result<String, String> {
    let output = Command::new("osascript").arg("-e").arg(script).output();

    match output {
        Ok(out) => {
            if out.status.success() {
                let stdout = String::from_utf8(out.stdout)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if stdout == "missing value" || stdout.is_empty() {
                    Err("AppleScript returned missing value or empty string".to_string())
                } else {
                    Ok(stdout)
                }
            } else {
                let stderr = String::from_utf8(out.stderr).unwrap_or_default();
                Err(format!("osascript error: {}", stderr))
            }
        }
        Err(e) => Err(format!("Failed to execute osascript: {}", e)),
    }
}

fn get_active_app_info() -> Option<(String, String)> {
    // (bundle_id, name)
    let script_bundle_id = r#"tell application "System Events" to get bundle identifier of first process whose frontmost is true"#;
    let script_name =
        r#"tell application "System Events" to get name of first process whose frontmost is true"#;

    match (run_osascript(script_bundle_id), run_osascript(script_name)) {
        (Ok(bundle_id), Ok(name)) => Some((bundle_id, name)),
        (Err(e_bundle), _) => {
            // Only log errors if they're not empty and not during shutdown
            if !e_bundle.is_empty() && !e_bundle.contains("execution of AppleScript failed") {
                eprintln!("Error getting bundle_id: {}", e_bundle);
            }
            None
        }
        (_, Err(e_name)) => {
            if !e_name.is_empty() && !e_name.contains("execution of AppleScript failed") {
                eprintln!("Error getting name: {}", e_name);
            }
            None
        }
    }
}

fn get_browser_tab_url(bundle_id: &str) -> Option<String> {
    let script = match bundle_id {
        "company.thebrowser.dia"
        | "com.google.Chrome"
        | "com.google.Chrome.canary"
        | "com.brave.Browser" => {
            r#"tell application id "com.google.Chrome" to get URL of active tab of front window"#
        }
        "com.apple.Safari" => {
            r#"tell application "Safari" to get URL of current tab of front window"#
        }
        "com.microsoft.edgemac" => {
            // Added Edge explicitly
            r#"tell application id "com.microsoft.edgemac" to get URL of active tab of front window"#
        }
        _ => return None,
    };

    let mut result = run_osascript(script);
    if result.is_err() && bundle_id == "com.brave.Browser" {
        // Specific fallback for Brave if generic Chrome ID fails
        let brave_script =
            r#"tell application id "com.brave.Browser" to get URL of active tab of front window"#;
        result = run_osascript(brave_script);
    }

    match result {
        Ok(url) => Some(url),
        Err(e) => {
            if !e.contains("missing value")
                && !e.contains("Can't get window 1")
                && !e.contains("Can't get current tab of window 1")
            {
                eprintln!("Error getting URL for {}: {}", bundle_id, e);
            }
            None
        }
    }
}

fn get_app_category(bundle_id: &str, name: &str) -> Option<String> {
    // Simple categorization logic - can be expanded
    match bundle_id {
        "com.google.Chrome"
        | "com.google.Chrome.canary"
        | "com.apple.Safari"
        | "com.brave.Browser"
        | "com.microsoft.edgemac" => Some("Browser".to_string()),
        "com.apple.Terminal" | "com.apple.iTerm2" => Some("Terminal".to_string()),
        "com.apple.mail" | "com.microsoft.Outlook" => Some("Email".to_string()),
        "com.apple.Slack" | "com.microsoft.Teams" => Some("Communication".to_string()),
        "com.apple.Notes" | "com.apple.TextEdit" => Some("Productivity".to_string()),
        _ => None,
    }
}

fn get_desktop_path() -> Result<PathBuf, String> {
    let home = env::var("HOME").map_err(|_| "Could not find HOME directory".to_string())?;

    let desktop = PathBuf::from(home).join("Desktop");
    if !desktop.exists() {
        return Err("Desktop directory not found".to_string());
    }

    Ok(desktop)
}

fn main() {
    let desktop_path = match get_desktop_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("Error: {}. Using current directory instead.", e);
            PathBuf::from(".")
        }
    };

    let stats_file = desktop_path.join("usage_stats.csv");
    let mut usage_stats = UsageStats::load_from_file(&stats_file).unwrap_or_else(|e| {
        eprintln!("Warning: Could not load existing stats: {}", e);
        UsageStats::new()
    });

    let mut current_entity: Option<ActiveEntity> = None;
    let mut last_check_time = Instant::now();
    let mut session_start_time = Local::now();
    let mut is_shutting_down = false;

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
        println!("\nShutting down gracefully...");
    })
    .expect("Error setting Ctrl-C handler");

    println!("Starting app tracker... Press Ctrl+C to stop and show summary.");

    while running.load(Ordering::SeqCst) {
        let new_entity_info = get_active_app_info();
        let mut new_active_entity: Option<ActiveEntity> = None;

        if let Some((bundle_id, name)) = new_entity_info {
            let mut url: Option<String> = None;
            match bundle_id.as_str() {
                "com.google.Chrome"
                | "com.google.Chrome.canary"
                | "com.apple.Safari"
                | "com.brave.Browser"
                | "com.microsoft.edgemac" => {
                    url = get_browser_tab_url(&bundle_id);
                }
                _ => {}
            }

            let category = get_app_category(&bundle_id, &name);
            new_active_entity = Some(ActiveEntity {
                bundle_id,
                name,
                url,
                category,
            });
        }

        let loop_instant = Instant::now();
        let elapsed_since_last_check = loop_instant.duration_since(last_check_time);

        if current_entity != new_active_entity {
            // Switched app/URL
            if let Some(ref entity) = current_entity {
                let session_end_time = Local::now();
                usage_stats.add_session(
                    entity,
                    session_start_time,
                    session_end_time,
                    elapsed_since_last_check,
                );
                if !is_shutting_down {
                    println!(
                        "Switched from: {:?} (spent: {:.2?})",
                        entity, elapsed_since_last_check
                    );
                }
            }

            current_entity = new_active_entity;
            session_start_time = Local::now();

            if let Some(ref entity) = current_entity {
                if !is_shutting_down {
                    println!("Started tracking: {:?}", entity);
                }
            }
        }

        last_check_time = loop_instant;
        thread::sleep(Duration::from_secs(2));
    }

    // Mark as shutting down to suppress unnecessary output
    is_shutting_down = true;

    // Save final session if there is one
    if let Some(ref entity) = current_entity {
        let final_exit_time = Local::now();
        let duration_spent_on_last_entity =
            final_exit_time.signed_duration_since(session_start_time);
        usage_stats.add_session(
            entity,
            session_start_time,
            final_exit_time,
            Duration::from_secs(duration_spent_on_last_entity.num_seconds() as u64),
        );
    }

    // Save stats to file
    if let Err(e) = usage_stats.save_to_file(&stats_file) {
        eprintln!("Warning: Failed to save usage stats: {}", e);
    }

    // Print summary
    println!("\n=== Usage Summary ===");

    // Group by category
    let mut category_stats: HashMap<String, Duration> = HashMap::new();
    let mut app_stats: HashMap<ActiveEntity, Duration> = HashMap::new();

    for session in &usage_stats.sessions {
        let category = session.category.clone();
        *category_stats
            .entry(category.clone())
            .or_insert(Duration::ZERO) += Duration::from_secs_f64(session.duration_seconds);
        *app_stats
            .entry(ActiveEntity {
                bundle_id: session.bundle_id.clone(),
                name: session.app_name.clone(),
                url: None,
                category: Some(category),
            })
            .or_insert(Duration::ZERO) += Duration::from_secs_f64(session.duration_seconds);
    }

    println!("\nBy Category:");
    let mut sorted_categories: Vec<_> = category_stats.into_iter().collect();
    sorted_categories.sort_by(|a, b| b.1.cmp(&a.1));

    for (category, duration) in sorted_categories {
        let percentage =
            (duration.as_secs_f64() / usage_stats.total_duration.as_secs_f64()) * 100.0;
        println!("{}: {:.2?} ({:.1}%)", category, duration, percentage);
    }

    println!("\nBy Application:");
    let mut sorted_apps: Vec<_> = app_stats.into_iter().collect();
    sorted_apps.sort_by(|a, b| b.1.cmp(&a.1));

    for (entity, duration) in sorted_apps {
        let percentage =
            (duration.as_secs_f64() / usage_stats.total_duration.as_secs_f64()) * 100.0;
        println!("\nApp: {} ({})", entity.name, entity.bundle_id);
        if let Some(url) = entity.url {
            println!("  URL: {}", url);
        }
        if let Some(category) = entity.category {
            println!("  Category: {}", category);
        }
        println!("  Total Time: {:.2?} ({:.1}%)", duration, percentage);
    }

    println!("\nTotal tracked time: {:.2?}", usage_stats.total_duration);
    println!("Stats saved to: {}", stats_file.display());
}
