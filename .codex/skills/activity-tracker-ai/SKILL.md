---
name: activity-tracker-ai
description: "Use when Codex needs to query, operate, or improve this local macOS activity tracker: reading day history, exporting logs, checking background service state, adding AI-facing CLI hooks, improving collector fidelity, or applying Rust best practices to the tracker."
---

# Activity Tracker AI

## Overview

Use this skill to work with `activity_tracker`, a local-first macOS service substrate that records active app/browser sessions and exposes queryable logs for AI agents and a future SwiftUI app. Treat `~/.activity_tracker/activity.db` as source of truth, JSONL as mirror/fallback, and CLI JSON output as preferred agent interface.

## Query Workflow

1. Run `cargo run -- paths --json` to discover storage paths.
2. Use `cargo run -- report YYYY-MM-DD --json` for the one-call AI payload: summary, sessions, open checkpoint, and paths.
3. Use `cargo run -- day YYYY-MM-DD --json` for daily summaries.
4. Use `cargo run -- logs YYYY-MM-DD --json` for raw sessions.
5. Narrow logs with `--app`, `--title`, `--category`, `--domain`, `--activity-type active|idle`, and `--limit`.
6. Export with `cargo run -- export --date YYYY-MM-DD --format csv|jsonl`.
7. Import old CSV with `cargo run -- import-csv PATH --dry-run --json`, then rerun without `--dry-run`.

## Operations

- Health check: `cargo run -- doctor --json`
- Foreground tracking: `cargo run -- track`
- Background install: `cargo run --release -- service install`
- Background status: `cargo run -- service status`
- Background remove: `cargo run -- service uninstall`
- CSV import: `cargo run -- import-csv ~/Desktop/usage_stats.csv --json`

## Implementation Rules

- Keep storage/query code testable without macOS permissions.
- Persist each completed session immediately to SQLite and mirror it to JSONL; CSV is derived.
- Maintain the SQLite `open_session` heartbeat so crash/restart recovery does not lose the active span.
- Include the provisional open session in live query commands (`day`, `logs`, `summary`, `report`) when it overlaps the query.
- Preserve current session through short active-app probe misses; only create gaps after repeated misses.
- Record idle as `activity_type: "idle"` with `bundle_id: "local.activity_tracker.idle"` once HID idle time crosses threshold.
- Day math must include overlapping sessions and clip summary duration to local day bounds.
- Add `--json` for new read commands so AI tools can consume them.
- Keep local privacy: no network sync unless explicitly requested.
- Validate with `cargo fmt`, `cargo test`, and `cargo clippy --all-targets --all-features -- -D warnings`.

## Data Contract

Each JSONL record is one completed session with `start_time`, `end_time`, `duration_seconds`, `app_name`, `bundle_id`, optional `title`, `category`, `activity_type`, and optional `url`. Missing `activity_type` in old records defaults to `active`; missing `title` defaults to null. Prefer CLI reads over manually parsing files unless debugging storage.
