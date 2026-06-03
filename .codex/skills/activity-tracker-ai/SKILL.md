---
name: activity-tracker-ai
description: "Use when Codex needs to query, operate, or improve this local macOS activity tracker: reading day history, exporting logs, checking background service state, adding AI-facing CLI hooks, improving collector fidelity, or applying Rust best practices to the tracker."
---

# Activity Tracker AI

## Overview

Use this skill to work with `activity_tracker`, a local-first macOS service substrate that records active app/browser sessions and exposes queryable logs for AI agents and a future SwiftUI app. Treat `~/.activity_tracker/activity.db` as source of truth, JSONL as mirror/fallback, and CLI JSON output as preferred agent interface.

## Query Workflow

1. Run `cargo run -- paths --json` to discover storage paths.
2. Run `cargo run -- schema --json` when an app/tool harness needs the CLI/data contract.
3. Use `cargo run -- now --json` for cheap current activity polling.
4. Use `cargo run -- agent --json` as the first AI/reporting hook: readiness, storage verification, `report_ready`, `action_required`, repair plan with actionable/residual item counts, `next_action.commands`, window-scoped quality score/status/scoped candidate repair commands, warnings, window audit with bounded quality issue samples, today's audit/quality for background context, bounded summary, most recent timeline context, open checkpoint, and paths.
5. Use `cargo run -- agent --last-minutes N --json` for rolling auto-report windows, or `cargo run -- agent YYYY-MM-DD --json` for compact day context.
6. Add `--include-sessions` to `agent` only when raw sessions are necessary.
7. Use `cargo run -- health --json` before reports when you need the full launchd/storage health payload.
8. Use `cargo run -- report YYYY-MM-DD --json` for the full daily AI payload: summary, sessions, open checkpoint, and paths.
9. Use `cargo run -- timeline YYYY-MM-DD --json` for compact ordered blocks grouped by app/domain/category.
10. Use `cargo run -- audit YYYY-MM-DD --json` or `cargo run -- audit --last-minutes N --json` to inspect log quality gaps, overlaps, invalid rows, missing titles, browser sessions missing URLs, browser blank tabs, suspicious browser title/URL mismatches, untracked/idle counts, uncategorized counts, by-app/by-title quality breakdowns, bounded quality issue samples, and open checkpoint state.
11. Use `cargo run -- query --from YYYY-MM-DD --to YYYY-MM-DD --json` for cross-day search payloads with summary, compact timeline, sessions, filters, and open checkpoint.
12. Use `cargo run -- query --since RFC3339 --until RFC3339 --json` for precise report windows, or `cargo run -- query --last-minutes N --json` for rolling auto-report windows.
13. Omit window args on `query` for all-history search.
14. Use `cargo run -- day YYYY-MM-DD --json` for daily summaries.
15. Use `cargo run -- logs YYYY-MM-DD --json` for one-day raw sessions.
16. Narrow `query` or `logs` with `--app`, `--title`, `--url`, `--text`, `--category`, `--domain`, `--activity-type active|idle|untracked`, `--order asc|desc`, and `--limit`; use `--text` for broad recall across app, bundle, title, URL, domain, category, and activity type, and use `--order desc --limit N` for newest matching rows.
17. Use `cargo run -- inventory --last-minutes N --limit 20 --json` for app/domain/category/activity-type facets before choosing filters or populating UI picker options.
18. Export with `cargo run -- export --date YYYY-MM-DD --format csv|jsonl --json`.
19. Import old CSV with `cargo run -- import-csv PATH --dry-run --json`, then rerun without `--dry-run`.
20. After category rule changes, run `cargo run -- reclassify --dry-run --json`, then rerun without `--dry-run`.
21. After auditing gaps, run `cargo run -- repair-gaps --dry-run --json`, then rerun without `--dry-run` to insert explicit untracked sessions.
22. After improving title capture, run `cargo run -- repair-titles --dry-run --json`, then rerun without `--dry-run` to backfill native app titles and browser titles whose exact URL has one unique observed title.
23. After improving URL normalization, run `cargo run -- repair-urls --dry-run --json`, then rerun without `--dry-run` to canonicalize safe URL-only fixes such as known or surrounded browser blank tabs.
24. After exposing browser context mismatches or missing browser context, run `cargo run -- repair-context --dry-run --json`, then rerun without `--dry-run` only for high-confidence neighbor/exact-URL repairs, all-missing browser rows, or short unrecoverable mixed-context/title-missing rows that should become untracked.
25. Prefer scoped repair commands returned by `agent.repair_plan.actionable_commands`; `agent.quality.repair_commands` are candidates and may explain quality warnings that are not safely repairable.

## Operations

- Health check: `cargo run -- doctor --json`
- Service substrate health plus storage verification: `cargo run -- health --json`
- Storage verification: `cargo run -- verify --json`
- Storage mirror repair: `cargo run -- repair-mirror --json`
- CLI/data contract: `cargo run -- schema --json`
- Current activity: `cargo run -- now --json`
- AI/reporting hook: `cargo run -- agent --json`
- Foreground tracking: `cargo run -- track`
- Background install: `cargo run --release -- service install --interval-seconds 2 --idle-threshold-seconds 300`
- Background status: `cargo run -- service status --json`
- Background logs: `cargo run -- service logs --lines 80 --json`
- Background remove: `cargo run -- service uninstall`
- CSV import: `cargo run -- import-csv ~/Desktop/usage_stats.csv --json`
- Broad search: `cargo run -- query --text "driverry devops" --json`
- Latest search: `cargo run -- query --text "driverry devops" --order desc --limit 20 --json`
- URL search: `cargo run -- logs 2026-06-03 --url pull/123 --json`
- Filter inventory: `cargo run -- inventory --last-minutes 240 --limit 20 --json`
- Window audit: `cargo run -- audit --last-minutes 120 --json`
- Scoped reclassify: `cargo run -- reclassify --from 2026-06-03 --to 2026-06-03 --dry-run --json`
- Title repair: `cargo run -- repair-titles --dry-run --json`
- URL repair: `cargo run -- repair-urls --dry-run --json`
- Context repair: `cargo run -- repair-context --dry-run --json`
- Scoped context repair: `cargo run -- repair-context --last-minutes 120 --dry-run --json`

## Implementation Rules

- Keep storage/query code testable without macOS permissions.
- Keep routine day/range reads backed by SQLite indexed time columns rather than all-history scans.
- Persist each completed session immediately to SQLite, mirror it to JSONL, and append it to the default CSV view; repair commands may rewrite derived files from SQLite.
- Configure each SQLite connection with WAL, normal synchronous mode, foreign keys, and busy timeout so background writes and agent reads coexist.
- Use `verify --json` to check SQLite integrity plus JSONL and default CSV readability/count/content sync.
- Use `repair-mirror --json` to rebuild JSONL mirror and CSV view from SQLite if verification reports a broken or out-of-sync mirror.
- Maintain the SQLite `open_session` heartbeat so crash/restart recovery does not lose the active span.
- Include the provisional open session in live query commands (`day`, `logs`, `query`, `summary`, `report`) when it overlaps the query.
- Keep windowed summaries and timelines clipped to requested day/range/last-minutes bounds; raw session arrays can retain original persisted start/end for audit/debug context.
- Treat uncovered leading/trailing spans inside rolling `agent --last-minutes` windows as coverage gaps so auto reports know when their requested interval is only partially observed.
- Use windowed `audit` for exact report-window quality checks before calling repairs.
- Keep `health --json` gated on full storage verification so agents do not call a broken mirror/CSV setup healthy.
- Keep `export --json` returning artifact path, date scope, format, and session count so agents can hand off generated files without scraping text.
- Keep `inventory --json` window-aware and backed by the same indexed query window; use it for filter menus instead of scanning raw history in callers.
- Keep `schema --json` stable enough for SwiftUI/tool harness discovery; update it whenever commands, filters, agent fields, storage verification fields, session fields, defaults, or quality issue kinds change.
- Include read-command payload fields for day, summary, timeline, inventory, query, report, audit, and paths in `schema --json`.
- Include import and repair report fields in `schema --json` so agents do not infer JSON payload shapes.
- Keep `now --json` cheap and suitable for frequent SwiftUI/menu-bar polling; `ready` should require service config validation.
- Keep `agent --json` explicit about report readiness: `report_ready` should be true only when service binary/config, freshness, storage verification, and window coverage are ready and no actionable repair remains; `action_required` should reflect actionable repairs, while residual non-repairable warnings stay visible in the repair plan and quality fields.
- Keep `agent.next_action.commands` as the immediate workflow hint for AI/reporting callers: actionable repairs first, report/query commands when ready, inspection commands when readiness is blocked without safe repairs.
- If JSONL/CSV derived storage verification fails while SQLite is healthy, expose `activity_tracker repair-mirror --json` in `agent.repair_plan.actionable_commands`.
- Use app identity plus browser URL domains for categories; reclassify stored sessions when mappings change.
- Keep observed domain mappings current for recurring work tools, communication, AI, writing, research, and development sites.
- For browsers, collect active tab title and URL from the same AppleScript sample so rows do not mix different tab states.
- Stabilize brief same-browser title/URL misses from current tab context only when observed context is missing or matches; never fill across conflicting titles or URLs.
- Treat longer browser samples with no tab title and no URL as untracked time after the short tolerance instead of storing active browser rows with empty context.
- Keep suspicious browser title/URL mismatches visible in audit output so agents do not over-trust old mixed-context rows.
- Use `repair-context` to fix only high-confidence browser context mismatches and missing browser title/URL fields, or to convert all-missing and short unrecoverable mixed-context/title-missing browser rows to untracked time; do not infer from weak title/domain guesses alone.
- Keep `reclassify`, `repair-gaps`, `repair-titles`, `repair-urls`, and `repair-context` scoped to the audited window when an agent is fixing a specific report window.
- For native apps, collect app identity and title fallback from the same foreground probe sample: window title first, then app title/name when macOS exposes no window.
- Use `repair-titles` to backfill native app title gaps and exact-URL browser title gaps after title capture changes; do not mask browser misses with synthetic titles.
- Use `repair-urls` to canonicalize known or blank-tab-surrounded URLs; do not infer ordinary missing browser URLs from surrounding sessions.
- Preserve current session through short active-app probe misses; only create gaps after repeated misses.
- Keep tracker startup tolerant of initial active-app or idle probe failures so launchd does not exit on transient macOS/AppleScript errors.
- Keep `service install` rejecting non-absolute, missing, non-file, or non-executable binaries before writing LaunchAgent state and supporting `--json` for setup agents; LaunchAgent arguments must stay aligned with selected data root, configured sample interval, and idle threshold.
- Keep `service status --json` normalized enough for agents to inspect program, arguments, and log paths without parsing raw `launchctl` text.
- Keep `service logs --json` bounded and path-aware so agents can inspect launchd stdout/stderr tails without shell-specific file reads.
- Keep `service uninstall --json` exposing plist removal status for setup agents without text scraping.
- Keep `doctor --json` non-fatal for active-app or idle probe failures; expose probe status/error fields, launchd service binary/config validation, storage verification, and hints so setup agents can diagnose permissions/service setup.
- Keep runtime failures under `--json` machine-readable with `ok: false`, `error.code`, and `error.message`; do not make agents parse stderr for expected command errors.
- Keep global `--json` before subcommands working the same as command-local `--json` for JSON-capable commands.
- Keep global `--json` without a subcommand returning `missing_command` instead of starting the foreground tracker loop.
- Canonicalize known browser blank tabs as `about:newtab` and keep them separate from actionable missing-URL audit rows.
- Record idle as `activity_type: "idle"` with `bundle_id: "local.activity_tracker.idle"` once HID idle time crosses threshold.
- Record longer unknown/probe-unavailable spans as `activity_type: "untracked"` when probing recovers.
- Repair real gaps as `activity_type: "untracked"` with `bundle_id: "local.activity_tracker.untracked"` rather than hiding missing time.
- Day math must include overlapping sessions and clip summary duration to local day bounds.
- Add `--json` for new read commands so AI tools can consume them.
- Keep `agent --json` compact by default; it bounds summary/timeline rows and raw sessions should require `--include-sessions`.
- Keep audit issue samples bounded; use `query`/`logs` for full raw session context.
- Keep local privacy: no network sync unless explicitly requested.
- Validate with `cargo fmt`, `cargo test`, and `cargo clippy --all-targets --all-features -- -D warnings`.

## Data Contract

Each JSONL record is one completed session with `start_time`, `end_time`, `duration_seconds`, `app_name`, `bundle_id`, optional `title`, `category`, `activity_type`, and optional `url`. Missing `activity_type` in old records defaults to `active`; missing `title` defaults to null. Prefer CLI reads over manually parsing files unless debugging storage.
