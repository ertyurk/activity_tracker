# Activity Tracker

In-progress local-first macOS activity tracker for building near-perfect personal work logs. The goal is a quiet background app that records active app/browser sessions durably, then gives humans and AI agents clean CLI hooks to query history by day, app, category, URL/domain, and export format.

## Current Shape

- Tracks active macOS app sessions with start/end timestamps and exact duration.
- Captures browser URL when AppleScript supports the active browser.
- Stores source-of-truth logs as append-only JSONL in the user data directory.
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
activity_tracker day 2026-06-03 --json
activity_tracker logs 2026-06-03 --json
activity_tracker logs 2026-06-03 --domain github --json
activity_tracker logs 2026-06-03 --app "Code" --json
activity_tracker summary --json
activity_tracker export --date 2026-06-03 --format csv
activity_tracker export --date 2026-06-03 --format jsonl
```

No subcommand defaults to `track`, preserving the original simple run behavior.

## Storage

Default root is the platform data dir:

```text
~/Library/Application Support/activity_tracker
```

Files:

- `sessions.jsonl`: append-only source of truth
- `usage_stats.csv`: refreshed CSV view
- `exports/`: explicit CLI exports
- `logs/`: launchd stdout/stderr logs

Override per command:

```bash
ACTIVITY_TRACKER_HOME=/tmp/activity-tracker activity_tracker paths
activity_tracker --data-dir /tmp/activity-tracker day --json
```

## Data Contract

Each JSONL record is one completed session:

```json
{
  "start_time": "2026-06-03T08:00:00+02:00",
  "end_time": "2026-06-03T08:05:30+02:00",
  "duration_seconds": 330.0,
  "app_name": "Google Chrome",
  "bundle_id": "com.google.Chrome",
  "category": "Browser",
  "url": "https://example.com/path"
}
```

Day summaries include sessions overlapping that local day and clip cross-midnight durations to the requested day.

## Service Commands

```bash
activity_tracker service install
activity_tracker service status
activity_tracker service uninstall
```

`service install` writes `~/Library/LaunchAgents/com.local.activity-tracker.plist`, loads it, and starts `activity_tracker track --quiet`.

## Development

```bash
cargo fmt
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Project rules for future agents live in `AGENTS.md`. Repo-local skill metadata lives in `.codex/skills/activity-tracker-ai/`.

## Privacy

All data is local. The app does not send logs to external services. Browser URLs are sensitive; keep exports private and do not commit logs.
