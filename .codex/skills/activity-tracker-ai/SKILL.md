---
name: activity-tracker-ai
description: "Use when Codex needs to query, operate, or improve this local macOS activity tracker: reading day history, exporting logs, checking background service state, adding AI-facing CLI hooks, improving collector fidelity, or applying Rust best practices to the tracker."
---

# Activity Tracker AI

## Overview

Use this skill to work with `activity_tracker`, a local-first macOS service substrate that records active app/browser sessions and exposes queryable logs for AI agents and a future SwiftUI app. Treat `~/.activity_tracker/activity.db` as source of truth, JSONL as mirror/fallback, and CLI JSON output as preferred agent interface.

## Query Workflow

1. Run `cargo run -- paths --json` to discover storage paths.
2. Use `cargo run -- health --json` before reports to verify launchd state, storage freshness, open checkpoint, paths, and today's audit counts.
3. Use `cargo run -- report YYYY-MM-DD --json` for the one-call AI payload: summary, sessions, open checkpoint, and paths.
4. Use `cargo run -- timeline YYYY-MM-DD --json` for compact ordered blocks grouped by app/domain/category.
5. Use `cargo run -- audit YYYY-MM-DD --json` to inspect log quality gaps, overlaps, invalid rows, and open checkpoint state.
6. Use `cargo run -- query --from YYYY-MM-DD --to YYYY-MM-DD --json` for cross-day search payloads with summary, compact timeline, sessions, filters, and open checkpoint.
7. Use `cargo run -- query --since RFC3339 --until RFC3339 --json` for precise report windows, or `cargo run -- query --last-minutes N --json` for rolling auto-report windows.
8. Omit window args on `query` for all-history search.
9. Use `cargo run -- day YYYY-MM-DD --json` for daily summaries.
10. Use `cargo run -- logs YYYY-MM-DD --json` for one-day raw sessions.
11. Narrow `query` or `logs` with `--app`, `--title`, `--category`, `--domain`, `--activity-type active|idle|untracked`, and `--limit`.
12. Export with `cargo run -- export --date YYYY-MM-DD --format csv|jsonl`.
13. Import old CSV with `cargo run -- import-csv PATH --dry-run --json`, then rerun without `--dry-run`.
14. After category rule changes, run `cargo run -- reclassify --dry-run --json`, then rerun without `--dry-run`.
15. After auditing gaps, run `cargo run -- repair-gaps --dry-run --json`, then rerun without `--dry-run` to insert explicit untracked sessions.

## Operations

- Health check: `cargo run -- doctor --json`
- Service substrate health: `cargo run -- health --json`
- Foreground tracking: `cargo run -- track`
- Background install: `cargo run --release -- service install`
- Background status: `cargo run -- service status --json`
- Background remove: `cargo run -- service uninstall`
- CSV import: `cargo run -- import-csv ~/Desktop/usage_stats.csv --json`

## Implementation Rules

- Keep storage/query code testable without macOS permissions.
- Keep routine day/range reads backed by SQLite indexed time columns rather than all-history scans.
- Persist each completed session immediately to SQLite and mirror it to JSONL; CSV is derived.
- Maintain the SQLite `open_session` heartbeat so crash/restart recovery does not lose the active span.
- Include the provisional open session in live query commands (`day`, `logs`, `query`, `summary`, `report`) when it overlaps the query.
- Use app identity plus browser URL domains for categories; reclassify stored sessions when mappings change.
- Preserve current session through short active-app probe misses; only create gaps after repeated misses.
- Record idle as `activity_type: "idle"` with `bundle_id: "local.activity_tracker.idle"` once HID idle time crosses threshold.
- Record longer unknown/probe-unavailable spans as `activity_type: "untracked"` when probing recovers.
- Repair real gaps as `activity_type: "untracked"` with `bundle_id: "local.activity_tracker.untracked"` rather than hiding missing time.
- Day math must include overlapping sessions and clip summary duration to local day bounds.
- Add `--json` for new read commands so AI tools can consume them.
- Keep local privacy: no network sync unless explicitly requested.
- Validate with `cargo fmt`, `cargo test`, and `cargo clippy --all-targets --all-features -- -D warnings`.

## Data Contract

Each JSONL record is one completed session with `start_time`, `end_time`, `duration_seconds`, `app_name`, `bundle_id`, optional `title`, `category`, `activity_type`, and optional `url`. Missing `activity_type` in old records defaults to `active`; missing `title` defaults to null. Prefer CLI reads over manually parsing files unless debugging storage.
