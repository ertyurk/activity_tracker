# AGENTS.md

## Mission

Build `activity_tracker` into a reliable local-first macOS activity history service substrate. It should run quietly in the background, append durable logs, preserve enough context for useful retrospection, and expose AI-friendly CLI commands for querying history by day/range, app, title, category, URL/domain, and export format. A SwiftUI app and internal AI reporting agent will sit on top later; do not build that UI here.

## Product Rules

- Local-only by default. Do not send activity data to network services.
- Source of truth is SQLite at `~/.activity_tracker/activity.db`; JSONL is mirror/fallback; CSV is an export/view.
- Preserve timestamps, app name, bundle ID, category, title, URL when available, and exact duration.
- Categories should use app identity and browser URL domain when available; run `reclassify` after category rules improve.
- Preserve `activity_type` and treat idle as first-class log data, not as foreground app time.
- Preserve audited gaps as explicit `activity_type: "untracked"` sessions when repairing coverage.
- Preserve longer unknown/probe-unavailable spans as `activity_type: "untracked"` when the collector recovers.
- Keep an `open_session` heartbeat checkpoint so service restarts recover the current span instead of dropping it.
- Live query commands should include the current open session provisionally; exports should stay completed-session based.
- Tolerate brief active-app probe misses; do not turn transient macOS/AppleScript failures into fake gaps.
- Day queries must handle cross-midnight sessions by overlap, not only start date.
- Background mode should use `launchd` via `activity_tracker service install`.
- CLI output should support plain text for humans and `--json` for agents.

## Rust Rules

- Use existing crate style: Rust 2024, typed errors, small testable functions, no hidden panics.
- Do not use `unwrap`, `expect`, `panic!`, indexing/slicing, `todo!`, or `unimplemented!`.
- Prefer `thiserror` for durable app errors and `anyhow` only in tests or throwaway tooling.
- Keep macOS probing isolated from storage/query logic so tests avoid AppleScript.
- Avoid ad hoc parsing when a standard API is available.

## Commands

```bash
cargo fmt
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo run -- doctor
cargo run -- paths --json
cargo run -- service status --json
cargo run -- day 2026-06-03 --json
cargo run -- report 2026-06-03 --json
cargo run -- timeline 2026-06-03 --json
cargo run -- query --from 2026-06-03 --to 2026-06-03 --domain github --json
cargo run -- query --since 2026-06-03T08:00:00+02:00 --until 2026-06-03T09:00:00+02:00 --json
cargo run -- query --last-minutes 120 --json
cargo run -- query --category Development --limit 50 --json
cargo run -- audit 2026-06-03 --json
cargo run -- logs 2026-06-03 --domain github --json
cargo run -- logs 2026-06-03 --title project --json
cargo run -- logs 2026-06-03 --activity-type idle --json
cargo run -- export --date 2026-06-03 --format csv
cargo run -- import-csv ~/Desktop/usage_stats.csv --dry-run --json
cargo run -- reclassify --dry-run --json
cargo run -- repair-gaps --dry-run --json
```

## Repo-Local Skill

Use `.codex/skills/activity-tracker-ai/SKILL.md` when asked to query local activity history, improve collector fidelity, add AI hooks, or reason about the app goal.

## Git

Keep commits atomic and push `main` regularly when tests pass. Never commit generated activity logs, exported CSV/JSONL history, target output, or local LaunchAgent plists.
