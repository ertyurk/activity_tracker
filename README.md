# Activity Tracker

In-progress local-first macOS activity tracker for building near-perfect personal work logs.

The goal is a quiet background service substrate that records active app/browser sessions durably, then gives humans, a future SwiftUI app, and internal AI agents clean CLI hooks to query history by day, app, title, category, URL/domain, and export format.

## Current Shape

- Tracks active macOS app sessions with start/end timestamps and exact duration.
- Detects idle time from macOS HID idle state and records it as `activity_type: "idle"` instead of blaming the foreground app.
- Tolerates brief active-app probe misses so transient AppleScript/macOS hiccups do not create false gaps.
- Tolerates initial active-app/idle probe failures so the background service can start and recover instead of exiting.
- Records longer unknown spans as `activity_type: "untracked"` when probing recovers, so missing time stays visible.
- Captures active browser tab title and URL from the same active-tab AppleScript sample when the browser supports it.
- Stabilizes brief same-browser title/URL probe misses from current tab context without mixing conflicting tab data.
- Converts longer browser tab-context outages into `activity_type: "untracked"` instead of low-quality browser rows with empty title/URL.
- Captures native app window title when macOS reports it.
- Falls back to the foreground app title/name when window-level title is unavailable.
- Stores source-of-truth logs in SQLite under `~/.activity_tracker/activity.db`.
- Configures SQLite with WAL, normal synchronous mode, foreign keys, and a busy timeout.
- Lets CLI reads coexist with the background writer.
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
activity_tracker service install --no-load --json
activity_tracker service status --json
activity_tracker service logs --lines 80 --json
activity_tracker service uninstall --json
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
activity_tracker export --date 2026-06-03 --format jsonl --json
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

Preferred first call for internal AI/reporting tools:

`agent --json` returns:

- service readiness, `report_ready`, and `action_required`
- repair plan with actionable commands
- `next_action` with the next repair/report/inspection commands
- window-scoped quality gate, warnings, and audit
- today-wide audit context
- bounded summary and recent timeline context
- open checkpoint and storage paths

Defaults:

- window: last 120 minutes
- summary rows: top 12
- timeline blocks: 20 most recent

Useful variants:

- `agent YYYY-MM-DD --json` for one-day report context
- `agent --summary-limit N --timeline-limit N --json` for larger/smaller context
- `agent --include-sessions --json` when a tool needs raw sessions

Readiness and quality:

- `agent.report_ready` requires service binary/config, freshness, coverage, and full storage verification to be clean.
- If derived JSONL/CSV storage is broken while SQLite is healthy, repair plan includes `activity_tracker repair-mirror --json`.
- Rolling `agent --last-minutes` windows treat uncovered leading/trailing spans as gaps.
- `audit --json` reports coverage, context richness, quality breakdowns, bounded samples, and open-session state.
- Audit quality includes gaps, overlaps, invalid rows, missing titles/URLs, blank tabs, context mismatches, idle/untracked counts, and uncategorized counts.

History reads:

- `report --json` is the preferred full daily payload.
- Report includes day summary, raw sessions, current checkpoint, provisional active session, and storage paths.
- `query --json` is the preferred cross-day/all-history search payload.
- Query supports local date windows, RFC3339 windows, rolling `--last-minutes`, filters, summary, compact timeline, raw sessions, and open checkpoint.
- `day`, `logs`, `query`, `summary`, and `report` include the active open session when it overlaps the query.
- Exports stay based on completed sessions.
- `--text` searches app, bundle, title, URL, domain, category, and activity type.
- `query` and `logs` accept `--order asc|desc`; use `--order desc --limit N` for newest matching rows.
- Windowed summaries and timelines are clipped to requested day/range/last-minutes bounds.
- Raw session arrays keep original persisted bounds for audit/debug context.
- `timeline --json` returns compact ordered blocks grouped by app/domain/category.
- `inventory --json` returns windowed app, domain, category, and activity-type facets for agents or SwiftUI filter pickers.

Service/setup payloads:

- `health --json` is the service substrate check.
- Health includes launchd state, service binary/argument validation, freshness, storage verification, latest activity age, open checkpoint, paths, and today's audit.
- `doctor --json` is the setup diagnostic payload.
- Doctor includes writable storage, osascript availability, probe status/errors, launchd config validation, storage verification, paths, and hints.
- `now --json` is the cheap current-state poll for SwiftUI/menu-bar clients.
- `verify --json` checks SQLite integrity plus JSONL and default CSV readability/count/content sync.
- `service status --json` reports launchd load/running state, PID, program, arguments, and log paths.
- `service logs --json` reports bounded launchd stdout/stderr tails.
- `schema --json` reports the stable CLI/data contract, including read payload, import, and repair report fields.
- `schema --json` distinguishes full `paths --json` fields from embedded `paths` fields used by app/report payloads.
- `--json` can be passed command-locally (`doctor --json`) or globally before the subcommand (`--json doctor`).
- Global `--json` requires an explicit subcommand; plain no-subcommand mode still defaults to foreground `track` for humans.
- Runtime failures under `--json` emit `{ "ok": false, "error": { "code", "message" } }`.

Repair/export hooks:

- `reclassify` recomputes categories from current app and browser-domain rules.
- `reclassify`, `repair-gaps`, `repair-titles`, `repair-urls`, and `repair-context` accept scoped windows.
- Repair windows support `--from`/`--to`, `--since`/`--until`, or `--last-minutes`.
- JSON repair reports include `generated_at` plus `window`.
- `repair-gaps` converts audited gaps into explicit `activity_type: "untracked"` sessions.
- `repair-titles` backfills native-app title gaps and exact-URL browser title gaps.
- `repair-urls` canonicalizes safe URL-only fixes such as known browser blank tabs.
- `repair-context` repairs high-confidence browser context mismatches/missing fields.
- `repair-context` converts short unrecoverable context rows to untracked time.
- `repair-mirror` rewrites JSONL and CSV mirrors from SQLite.
- `export --json` writes CSV/JSONL and returns path, date scope, format, and session count.

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

The SQLite DB also keeps a single `open_session` heartbeat row while the tracker is running.
Clean shutdown clears it.
Restart recovery converts it into a completed session and then starts a fresh checkpoint.

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
Categories are app-aware and domain-aware.
Browser sessions can classify as Communication, Email, Calendar, Development, AI, Design, Productivity, Social, Writing, or Research based on URL domain.

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

Idle sessions:

- `app_name: "Idle"`
- `bundle_id: "local.activity_tracker.idle"`
- `category: "Idle"`
- `activity_type: "idle"`

Repaired gap sessions:

- `app_name: "Untracked"`
- `bundle_id: "local.activity_tracker.untracked"`
- `category: "Untracked"`
- `activity_type: "untracked"`

Day summaries include sessions overlapping that local day and clip cross-midnight durations to the requested day.
Range queries include sessions overlapping the optional `[from midnight, day after to midnight)` local-date window, precise RFC3339 timestamp windows, or last-N-minute windows.
Live query commands include the current open session provisionally.
Persisted JSONL records only contain completed sessions.

## Service Commands

```bash
activity_tracker service install
activity_tracker service install --interval-seconds 2 --idle-threshold-seconds 300
activity_tracker service status --json
activity_tracker service logs --lines 80 --json
activity_tracker service uninstall
```

`service install` validates that the selected binary is an absolute executable file, writes `~/Library/LaunchAgents/com.local.activity-tracker.plist`, loads it, and starts:

```bash
activity_tracker --data-dir <root> track --quiet
```

The LaunchAgent stores the data root, interval, and idle-threshold arguments.

`service install --json` returns:

- plist path
- binary path
- data root
- load state
- persisted interval and idle config

Invalid binaries fail with `error.code: "invalid_service_binary"`.

`service uninstall --json` returns the plist path and whether the plist is removed or already absent.

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
