# Activity Tracker

In-progress local-first macOS activity tracker for building near-perfect personal work logs. The goal is a quiet background service substrate that records active app/browser sessions durably, then gives humans, a future SwiftUI app, and internal AI agents clean CLI hooks to query history by day, app, title, category, URL/domain, and export format.

## Current Shape

- Tracks active macOS app sessions with start/end timestamps and exact duration.
- Detects idle time from macOS HID idle state and records it as `activity_type: "idle"` instead of blaming the foreground app.
- Tolerates brief active-app probe misses so transient AppleScript/macOS hiccups do not create false gaps.
- Tolerates initial active-app/idle probe failures so the background service can start and recover instead of exiting.
- Records longer unknown spans as `activity_type: "untracked"` when probing recovers, so missing time stays visible.
- Captures active browser tab title and URL from the same active-tab AppleScript sample when the browser supports it.
- Stabilizes brief same-browser title/URL probe misses from current tab context without mixing conflicting tab data.
- Converts longer browser tab-context outages into `activity_type: "untracked"` instead of low-quality browser rows with empty title/URL.
- Captures native app window title when macOS reports it, and atomically falls back to the foreground app title/name when window-level title is unavailable.
- Stores source-of-truth logs in SQLite under `~/.activity_tracker/activity.db`.
- Configures SQLite connections with WAL, normal synchronous mode, foreign keys, and a busy timeout so CLI reads can coexist with the background writer.
- Maintains indexed epoch timestamps in SQLite for scalable day/range queries.
- Mirrors completed sessions to JSONL for audit/export fallback and keeps the default CSV view current.
- Maintains an open-session checkpoint so a restart can recover the active span instead of losing it.
- Reports collector health, freshness, service state, and today's data-quality audit via `health --json`.
- Generates CSV exports for spreadsheet workflows.
- Provides `--json` output for AI/tool callers.
- Provides `now --json` for cheap current activity polling by SwiftUI/menu-bar clients.
- Provides `inventory --json` for SwiftUI/AI filter menus across apps, domains, categories, and activity types.
- Provides `schema --json` so SwiftUI/AI tool harnesses can discover the CLI/data contract without parsing help text.
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
activity_tracker schema --json
activity_tracker now --json
activity_tracker agent --json
activity_tracker agent --last-minutes 240 --json
activity_tracker agent 2026-06-03 --json
activity_tracker health --json
activity_tracker doctor --json
activity_tracker verify --json
activity_tracker service status --json
activity_tracker service logs --lines 80 --json
activity_tracker day 2026-06-03 --json
activity_tracker report 2026-06-03 --json
activity_tracker timeline 2026-06-03 --json
activity_tracker query --from 2026-06-03 --to 2026-06-03 --domain github --json
activity_tracker query --since 2026-06-03T08:00:00+02:00 --until 2026-06-03T09:00:00+02:00 --json
activity_tracker query --last-minutes 120 --json
activity_tracker query --last-minutes 120 --order desc --limit 20 --json
activity_tracker query --category Development --limit 50 --json
activity_tracker inventory --last-minutes 240 --limit 20 --json
activity_tracker logs 2026-06-03 --json
activity_tracker audit 2026-06-03 --json
activity_tracker audit --last-minutes 120 --json
activity_tracker logs 2026-06-03 --domain github --json
activity_tracker logs 2026-06-03 --app "Code" --json
activity_tracker logs 2026-06-03 --title "project" --json
activity_tracker logs 2026-06-03 --url "pull/123" --json
activity_tracker query --text "driverry devops" --json
activity_tracker logs 2026-06-03 --activity-type idle --json
activity_tracker summary --json
activity_tracker export --date 2026-06-03 --format csv
activity_tracker export --date 2026-06-03 --format jsonl
activity_tracker import-csv ~/Desktop/usage_stats.csv --dry-run --json
activity_tracker reclassify --dry-run --json
activity_tracker reclassify --from 2026-06-03 --to 2026-06-03 --dry-run --json
activity_tracker repair-gaps --dry-run --json
activity_tracker repair-titles --dry-run --json
activity_tracker repair-urls --dry-run --json
activity_tracker repair-context --dry-run --json
activity_tracker repair-context --last-minutes 120 --dry-run --json
activity_tracker repair-mirror --json
```

`agent --json` is the preferred first call for internal AI/reporting tools: it returns service readiness, storage verification, `report_ready`, `action_required`, a repair plan with actionable commands, a window-scoped `quality` gate with score/status/scoped candidate repair commands, actionable/residual item counts, freshness, warnings, window audit with bounded quality issue samples, today's audit/quality for background context, bounded summary/timeline context, open checkpoint, and paths. It defaults to the last 120 minutes, top 12 summary rows, and the 20 most recent timeline blocks; pass a date for one day, tune `--summary-limit`/`--timeline-limit`, or add `--include-sessions` when a tool needs raw sessions.
`agent.report_ready` requires the background service, service arguments, freshness, coverage, and full storage verification to be clean. If JSONL or CSV derived storage is broken while SQLite is healthy, `agent.repair_plan.actionable_commands` includes `activity_tracker repair-mirror --json`.
For rolling `agent --last-minutes` windows, the window audit also treats uncovered leading/trailing spans inside the requested window as gaps, so auto-report tools can detect partial coverage even when no two stored rows are separated.
`report --json` is the preferred full daily payload for AI agents: it includes the day summary, raw sessions, current open-session checkpoint, provisional active session, and storage paths. `query --json` is the preferred cross-day/all-history search payload: it accepts optional `--from`/`--to` local dates, precise RFC3339 `--since`/`--until` timestamps, or `--last-minutes` for auto-report windows, plus the same app/title/url/text/category/domain/activity-type filters as `logs`, and returns summary, compact timeline, raw sessions, filters, and open checkpoint.
`day`, `logs`, `query`, `summary`, and `report` include the active open session when it overlaps the query; exports stay based on completed sessions. `--text` searches across app, bundle, title, URL, domain, category, and activity type when agents need a broad recall query. `query` and `logs` accept `--order asc|desc`; use `--order desc --limit N` for newest matching rows.
Windowed summaries and timelines are clipped to the requested day/range/last-minutes bounds so auto reports do not overcount sessions that started before the report window. Raw session arrays keep original persisted bounds for audit/debug context.
`timeline --json` returns compact ordered blocks grouped by app/domain/category so agents can write reports without reading every raw session.
`inventory --json` returns windowed app, domain, category, and activity-type facets with clipped seconds, percentages, session counts, first/last seen timestamps, and latest title/URL context for agents or SwiftUI filter pickers.
`audit --json` reports log quality for a day or explicit window: gaps above a configurable threshold, overlaps, invalid rows, missing titles, browser sessions missing URLs, browser blank tabs, suspicious browser title/URL mismatches, untracked/idle counts, uncategorized counts, by-app/by-title quality breakdowns, bounded quality issue samples, and current open-session state. With `--last-minutes`, `--since`/`--until`, or `--from`/`--to`, audit uses the same indexed window query as `query`; explicit windows include leading/trailing uncovered spans as gaps. Known browser blank tabs are canonicalized as `about:newtab` for new sessions.
`health --json` is the service substrate check for agents: launchd state, persisted service binary/argument validation, storage freshness, storage verification, latest observed activity age, open checkpoint, paths, and today's audit/quality counts and breakdowns.
`doctor --json` is the setup diagnostic payload: writable storage, osascript availability, active-app probe status/error, idle probe status/error, launchd service binary/config state, storage verification, paths, and permission/service hints. Probe failures are reported as fields instead of making the whole command unusable.
`now --json` is the cheapest current-state poll for SwiftUI/menu-bar clients: service running/config state, freshness, checkpoint recency, current provisional session, open checkpoint, latest completed session, and paths.
`verify --json` runs storage integrity checks: SQLite integrity, SQLite session count, JSONL mirror readability/content sync, and default CSV readability/content sync.
`service status --json` reports launchd load/running state, PID, program, arguments, and log paths without requiring agents to parse `launchctl` text.
`service logs --json` reports bounded launchd stdout/stderr tails with paths so agents can inspect service errors without shelling into log files.
`schema --json` reports the stable CLI/data contract: storage paths, default thresholds, activity types, known categories, session fields, agent fields, storage verification fields, supported window args, filters, read commands, repair commands, service commands, quality issue kinds, and local-privacy flags.
When a runtime command invoked with `--json` fails, it emits a JSON error envelope with `ok: false`, `generated_at`, `error.code`, and `error.message` so tool harnesses can branch without scraping stderr.
`reclassify` recomputes categories from current app and browser-domain rules, useful after improving category mappings.
`reclassify`, `repair-gaps`, `repair-titles`, `repair-urls`, and `repair-context` accept optional `--from`/`--to`, `--since`/`--until`, or `--last-minutes` windows so an agent can dry-run and apply repairs to the same audited window instead of touching all history.
`repair-gaps` converts audited gaps in completed logs into explicit `activity_type: "untracked"` sessions so missing time stays visible instead of disappearing from totals.
`repair-titles` backfills native-app title gaps with the app name when macOS exposes only app-level context, and repairs browser titles only when the exact URL has one unique observed title elsewhere in the log.
`repair-urls` canonicalizes safe URL-only fixes such as known browser blank-tab URLs and missing URLs surrounded by blank-tab samples to `about:newtab`.
`repair-context` repairs high-confidence browser title/URL mismatches and missing browser context from immediate neighbor evidence or a unique clean exact-URL title observation, converts unrecoverable all-missing browser context rows and short unrecoverable mixed-context or title-missing rows to untracked time, then recomputes category.
`repair-mirror` rewrites the JSONL mirror and CSV view from SQLite, using SQLite as the source of truth when mirror verification fails.

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
SQLite uses WAL plus a short busy timeout so agent/SwiftUI reads do not fail during normal heartbeat writes.

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
Categories are app-aware and domain-aware. Browser sessions can classify as Communication, Email, Calendar, Development, AI, Design, Productivity, Social, Writing, or Research based on URL domain.

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
activity_tracker service install --interval-seconds 2 --idle-threshold-seconds 300
activity_tracker service status --json
activity_tracker service logs --lines 80 --json
activity_tracker service uninstall
```

`service install` writes `~/Library/LaunchAgents/com.local.activity-tracker.plist`, loads it, and starts `activity_tracker --data-dir <root> track --quiet` with persisted data root, interval, and idle-threshold arguments.

Default idle threshold is 300 seconds and default sampling interval is 2 seconds. Foreground or service runs can override them:

```bash
activity_tracker track --idle-threshold-seconds 120
activity_tracker service install --idle-threshold-seconds 120
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
