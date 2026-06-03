---
name: activity-tracker-ai
description: "Use when Codex needs to query, operate, or improve this local macOS activity tracker: reading day history, exporting logs, checking background service state, adding AI-facing CLI hooks, improving collector fidelity, or applying Rust best practices to the tracker."
---

# Activity Tracker AI

## Overview

Use this skill to work with `activity_tracker`, a local-first macOS service substrate that records active app/browser sessions and exposes queryable logs for AI agents and a future SwiftUI app. Treat `~/.activity_tracker/activity.db` as source of truth, JSONL as mirror/fallback, and CLI JSON output as preferred agent interface.

## Query Workflow

1. Run `cargo run -- paths --json` to discover storage paths.
2. Use `cargo run -- agent --json` as the first AI/reporting hook: readiness, window-scoped quality score/status/scoped candidate repair commands, dry-run repair plan, warnings, window audit with bounded quality issue samples, today's audit/quality for background context, bounded summary, most recent timeline context, open checkpoint, and paths.
3. Use `cargo run -- agent --last-minutes N --json` for rolling auto-report windows, or `cargo run -- agent YYYY-MM-DD --json` for compact day context.
4. Add `--include-sessions` to `agent` only when raw sessions are necessary.
5. Use `cargo run -- health --json` before reports when you need the full launchd/storage health payload.
6. Use `cargo run -- report YYYY-MM-DD --json` for the full daily AI payload: summary, sessions, open checkpoint, and paths.
7. Use `cargo run -- timeline YYYY-MM-DD --json` for compact ordered blocks grouped by app/domain/category.
8. Use `cargo run -- audit YYYY-MM-DD --json` to inspect log quality gaps, overlaps, invalid rows, missing titles, browser sessions missing URLs, browser blank tabs, suspicious browser title/URL mismatches, untracked/idle counts, uncategorized counts, by-app/by-title quality breakdowns, bounded quality issue samples, and open checkpoint state.
9. Use `cargo run -- query --from YYYY-MM-DD --to YYYY-MM-DD --json` for cross-day search payloads with summary, compact timeline, sessions, filters, and open checkpoint.
10. Use `cargo run -- query --since RFC3339 --until RFC3339 --json` for precise report windows, or `cargo run -- query --last-minutes N --json` for rolling auto-report windows.
11. Omit window args on `query` for all-history search.
12. Use `cargo run -- day YYYY-MM-DD --json` for daily summaries.
13. Use `cargo run -- logs YYYY-MM-DD --json` for one-day raw sessions.
14. Narrow `query` or `logs` with `--app`, `--title`, `--url`, `--text`, `--category`, `--domain`, `--activity-type active|idle|untracked`, and `--limit`; use `--text` for broad recall across app, bundle, title, URL, domain, category, and activity type.
15. Use `cargo run -- inventory --last-minutes N --limit 20 --json` for app/domain/category/activity-type facets before choosing filters or populating UI picker options.
16. Export with `cargo run -- export --date YYYY-MM-DD --format csv|jsonl`.
17. Import old CSV with `cargo run -- import-csv PATH --dry-run --json`, then rerun without `--dry-run`.
18. After category rule changes, run `cargo run -- reclassify --dry-run --json`, then rerun without `--dry-run`.
19. After auditing gaps, run `cargo run -- repair-gaps --dry-run --json`, then rerun without `--dry-run` to insert explicit untracked sessions.
20. After improving title capture, run `cargo run -- repair-titles --dry-run --json`, then rerun without `--dry-run` to backfill native app titles and browser titles whose exact URL has one unique observed title.
21. After improving URL normalization, run `cargo run -- repair-urls --dry-run --json`, then rerun without `--dry-run` to canonicalize safe URL-only fixes such as known or surrounded browser blank tabs.
22. After exposing browser context mismatches or missing browser context, run `cargo run -- repair-context --dry-run --json`, then rerun without `--dry-run` only for high-confidence neighbor or exact-URL repairs.
23. Prefer scoped repair commands returned by `agent.repair_plan.actionable_commands`; `agent.quality.repair_commands` are candidates and may explain quality warnings that are not safely repairable.

## Operations

- Health check: `cargo run -- doctor --json`
- Service substrate health: `cargo run -- health --json`
- AI/reporting hook: `cargo run -- agent --json`
- Foreground tracking: `cargo run -- track`
- Background install: `cargo run --release -- service install`
- Background status: `cargo run -- service status --json`
- Background remove: `cargo run -- service uninstall`
- CSV import: `cargo run -- import-csv ~/Desktop/usage_stats.csv --json`
- Broad search: `cargo run -- query --text "driverry devops" --json`
- URL search: `cargo run -- logs 2026-06-03 --url pull/123 --json`
- Filter inventory: `cargo run -- inventory --last-minutes 240 --limit 20 --json`
- Scoped reclassify: `cargo run -- reclassify --from 2026-06-03 --to 2026-06-03 --dry-run --json`
- Title repair: `cargo run -- repair-titles --dry-run --json`
- URL repair: `cargo run -- repair-urls --dry-run --json`
- Context repair: `cargo run -- repair-context --dry-run --json`
- Scoped context repair: `cargo run -- repair-context --last-minutes 120 --dry-run --json`

## Implementation Rules

- Keep storage/query code testable without macOS permissions.
- Keep routine day/range reads backed by SQLite indexed time columns rather than all-history scans.
- Persist each completed session immediately to SQLite and mirror it to JSONL; CSV is derived.
- Configure each SQLite connection with WAL, normal synchronous mode, foreign keys, and busy timeout so background writes and agent reads coexist.
- Maintain the SQLite `open_session` heartbeat so crash/restart recovery does not lose the active span.
- Include the provisional open session in live query commands (`day`, `logs`, `query`, `summary`, `report`) when it overlaps the query.
- Keep windowed summaries and timelines clipped to requested day/range/last-minutes bounds; raw session arrays can retain original persisted start/end for audit/debug context.
- Treat uncovered leading/trailing spans inside rolling `agent --last-minutes` windows as coverage gaps so auto reports know when their requested interval is only partially observed.
- Keep `inventory --json` window-aware and backed by the same indexed query window; use it for filter menus instead of scanning raw history in callers.
- Use app identity plus browser URL domains for categories; reclassify stored sessions when mappings change.
- Keep observed domain mappings current for recurring work tools, communication, AI, writing, research, and development sites.
- For browsers, collect active tab title and URL from the same AppleScript sample so rows do not mix different tab states.
- Stabilize brief same-browser title/URL misses from current tab context only when observed context is missing or matches; never fill across conflicting titles or URLs.
- Keep suspicious browser title/URL mismatches visible in audit output so agents do not over-trust old mixed-context rows.
- Use `repair-context` to fix only high-confidence browser context mismatches and missing browser title/URL fields; do not infer from weak title/domain guesses alone.
- Keep `reclassify`, `repair-gaps`, `repair-titles`, `repair-urls`, and `repair-context` scoped to the audited window when an agent is fixing a specific report window.
- For native apps, collect app identity and title fallback from the same foreground probe sample: window title first, then app title/name when macOS exposes no window.
- Use `repair-titles` to backfill native app title gaps and exact-URL browser title gaps after title capture changes; do not mask browser misses with synthetic titles.
- Use `repair-urls` to canonicalize known or blank-tab-surrounded URLs; do not infer ordinary missing browser URLs from surrounding sessions.
- Preserve current session through short active-app probe misses; only create gaps after repeated misses.
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
