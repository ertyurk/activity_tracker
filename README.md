# Activity Tracker

In-progress local-first macOS activity tracker for building near-perfect personal work logs. The goal is a quiet background service substrate that records active app/browser sessions durably, then gives humans, a future SwiftUI app, and internal AI agents clean CLI hooks to query history by day, app, title, category, URL/domain, and export format.

## Current Shape

- Tracks active macOS app sessions with start/end timestamps and exact duration.
- Detects idle time from macOS HID idle state and records it as `activity_type: "idle"` instead of blaming the foreground app.
- Tolerates brief active-app probe misses so transient AppleScript/macOS hiccups do not create false gaps.
- Records longer unknown spans as `activity_type: "untracked"` when probing recovers, so missing time stays visible.
- Captures browser URL when AppleScript supports the active browser.
- Captures active browser tab/window title when macOS reports it.
- Stores source-of-truth logs in SQLite under `~/.activity_tracker/activity.db`.
- Mirrors completed sessions to JSONL for audit/export fallback.
- Maintains an open-session checkpoint so a restart can recover the active span instead of losing it.
- Generates CSV exports for spreadsheet workflows.
- Provides `--json` output for AI/tool callers.
- Installs as a `launchd` user service for behind-the-scenes collection.

## Install

```bash
cargo build --release
```

Binary:

```bash
./target/release/activity_tracker
```

Run foreground tracker:

```bash
./target/release/activity_tracker track
```

Install background tracker:

```bash
./target/release/activity_tracker service install
```

macOS will likely ask for Accessibility permission for the terminal or binary host. Without it, active app detection can be unavailable.

## AI-Friendly CLI

```bash
activity_tracker paths --json
activity_tracker doctor --json
activity_tracker service status --json
activity_tracker day 2026-06-03 --json
activity_tracker report 2026-06-03 --json
activity_tracker timeline 2026-06-03 --json
activity_tracker query --from 2026-06-03 --to 2026-06-03 --domain github --json
activity_tracker query --since 2026-06-03T08:00:00+02:00 --until 2026-06-03T09:00:00+02:00 --json
activity_tracker query --last-minutes 120 --json
activity_tracker query --category Development --limit 50 --json
activity_tracker logs 2026-06-03 --json
activity_tracker audit 2026-06-03 --json
activity_tracker logs 2026-06-03 --domain github --json
activity_tracker logs 2026-06-03 --app "Code" --json
activity_tracker logs 2026-06-03 --title "project" --json
activity_tracker logs 2026-06-03 --activity-type idle --json
activity_tracker summary --json
activity_tracker export --date 2026-06-03 --format csv
activity_tracker export --date 2026-06-03 --format jsonl
activity_tracker import-csv ~/Desktop/usage_stats.csv --dry-run --json
activity_tracker reclassify --dry-run --json
activity_tracker repair-gaps --dry-run --json
```

`report --json` is the preferred one-call daily payload for AI agents: it includes the day summary, raw sessions, current open-session checkpoint, provisional active session, and storage paths. `query --json` is the preferred cross-day/all-history search payload: it accepts optional `--from`/`--to` local dates, precise RFC3339 `--since`/`--until` timestamps, or `--last-minutes` for auto-report windows, plus the same app/title/category/domain/activity-type filters as `logs`, and returns summary, compact timeline, raw sessions, filters, and open checkpoint.
`day`, `logs`, `query`, `summary`, and `report` include the active open session when it overlaps the query; exports stay based on completed sessions.
`timeline --json` returns compact ordered blocks grouped by app/domain/category so agents can write reports without reading every raw session.
`audit --json` reports log quality for a day: gaps above a configurable threshold, overlaps, invalid rows, and current open-session state.
`service status --json` reports launchd load/running state and PID without requiring agents to parse `launchctl` text.
`reclassify` recomputes categories from current app and browser-domain rules, useful after improving category mappings.
`repair-gaps` converts audited gaps in completed logs into explicit `activity_type: "untracked"` sessions so missing time stays visible instead of disappearing from totals.

No subcommand defaults to `track`, preserving the original simple run behavior.

## Storage

Default root is a home dotdir, similar to Codex-style local state but separate from `~/.codex`:

```text
~/.activity_tracker
```

Files:

- `activity.db`: SQLite source of truth
- `sessions.jsonl`: append-only mirror/fallback
- `usage_stats.csv`: refreshed CSV view
- `exports/`: explicit CLI exports
- `logs/`: launchd stdout/stderr logs

Legacy data from `~/Library/Application Support/activity_tracker/sessions.jsonl` is auto-migrated into SQLite on first service/doctor/write run.
The SQLite DB also keeps a single `open_session` heartbeat row while the tracker is running. Clean shutdown clears it; restart recovery converts it into a completed session and then starts a fresh checkpoint.

Override per command:

```bash
ACTIVITY_TRACKER_HOME=/tmp/activity-tracker activity_tracker paths
activity_tracker --data-dir /tmp/activity-tracker day --json
```

Import old/exported CSV into SQLite plus the JSONL mirror:

```bash
activity_tracker import-csv ~/Desktop/usage_stats.csv --json
```

Imports skip duplicates using session start/end/app/bundle/title/url/activity type.
Categories are app-aware and domain-aware. Browser sessions can classify as Communication, Email, Calendar, Development, AI, Design, Productivity, Social, or Research based on URL domain.

## Data Contract

Each JSONL record is one completed session:

```json
{
  "start_time": "2026-06-03T08:00:00+02:00",
  "end_time": "2026-06-03T08:05:30+02:00",
  "duration_seconds": 330.0,
  "app_name": "Google Chrome",
  "bundle_id": "com.google.Chrome",
  "title": "Example Project",
  "category": "Browser",
  "activity_type": "active",
  "url": "https://example.com/path"
}
```

Idle sessions use `app_name: "Idle"`, `bundle_id: "local.activity_tracker.idle"`, `category: "Idle"`, and `activity_type: "idle"`.
Repaired gap sessions use `app_name: "Untracked"`, `bundle_id: "local.activity_tracker.untracked"`, `category: "Untracked"`, and `activity_type: "untracked"`.

Day summaries include sessions overlapping that local day and clip cross-midnight durations to the requested day. Range queries include sessions overlapping the optional `[from midnight, day after to midnight)` local-date window, precise RFC3339 timestamp windows, or last-N-minute windows. Live query commands include the current open session provisionally; persisted JSONL records only contain completed sessions.

## Service Commands

```bash
activity_tracker service install
activity_tracker service status --json
activity_tracker service uninstall
```

`service install` writes `~/Library/LaunchAgents/com.local.activity-tracker.plist`, loads it, and starts `activity_tracker track --quiet`.

Default idle threshold is 300 seconds. Foreground runs can override it:

```bash
activity_tracker track --idle-threshold-seconds 120
```

## Development

```bash
cargo fmt
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Project rules for future agents live in `AGENTS.md`. Repo-local skill metadata lives in `.codex/skills/activity-tracker-ai/`.

## Privacy

All data is local. The app does not send logs to external services. Browser URLs are sensitive; keep exports private and do not commit logs.
