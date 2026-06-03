---
name: activity-tracker-ai
description: "Use when Codex needs to query, operate, or improve this local macOS activity tracker: reading day history, exporting logs, checking background service state, adding AI-facing CLI hooks, improving collector fidelity, or applying Rust best practices to the tracker."
---

# Activity Tracker AI

## Overview

Use this skill to work with `activity_tracker`, a local-first macOS app that records active app/browser sessions and exposes queryable logs for AI agents. Treat `sessions.jsonl` as source of truth and CLI JSON output as preferred agent interface.

## Query Workflow

1. Run `cargo run -- paths --json` to discover storage paths.
2. Use `cargo run -- day YYYY-MM-DD --json` for daily summaries.
3. Use `cargo run -- logs YYYY-MM-DD --json` for raw sessions.
4. Narrow logs with `--app`, `--title`, `--category`, `--domain`, `--activity-type active|idle`, and `--limit`.
5. Export with `cargo run -- export --date YYYY-MM-DD --format csv|jsonl`.

## Operations

- Health check: `cargo run -- doctor --json`
- Foreground tracking: `cargo run -- track`
- Background install: `cargo run --release -- service install`
- Background status: `cargo run -- service status`
- Background remove: `cargo run -- service uninstall`

## Implementation Rules

- Keep storage/query code testable without macOS permissions.
- Append each completed session immediately to JSONL; CSV is derived.
- Record idle as `activity_type: "idle"` with `bundle_id: "local.activity_tracker.idle"` once HID idle time crosses threshold.
- Day math must include overlapping sessions and clip summary duration to local day bounds.
- Add `--json` for new read commands so AI tools can consume them.
- Keep local privacy: no network sync unless explicitly requested.
- Validate with `cargo fmt`, `cargo test`, and `cargo clippy --all-targets --all-features -- -D warnings`.

## Data Contract

Each JSONL record is one completed session with `start_time`, `end_time`, `duration_seconds`, `app_name`, `bundle_id`, optional `title`, `category`, `activity_type`, and optional `url`. Missing `activity_type` in old records defaults to `active`; missing `title` defaults to null. Prefer CLI reads over manually parsing files unless debugging storage.
