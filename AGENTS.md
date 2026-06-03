# AGENTS.md

## Mission

Build `activity_tracker` into a reliable local-first macOS activity history app. It should run quietly in the background, append durable logs, preserve enough context for useful retrospection, and expose AI-friendly CLI commands for querying history by day, app, category, URL/domain, and export format.

## Product Rules

- Local-only by default. Do not send activity data to network services.
- Source of truth is append-only `sessions.jsonl`; CSV is an export/view.
- Preserve timestamps, app name, bundle ID, category, URL when available, and exact duration.
- Preserve `activity_type` and treat idle as first-class log data, not as foreground app time.
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
cargo run -- day 2026-06-03 --json
cargo run -- logs 2026-06-03 --domain github --json
cargo run -- logs 2026-06-03 --activity-type idle --json
cargo run -- export --date 2026-06-03 --format csv
```

## Repo-Local Skill

Use `.codex/skills/activity-tracker-ai/SKILL.md` when asked to query local activity history, improve collector fidelity, add AI hooks, or reason about the app goal.

## Git

Keep commits atomic and push `main` regularly when tests pass. Never commit generated activity logs, exported CSV/JSONL history, target output, or local LaunchAgent plists.
