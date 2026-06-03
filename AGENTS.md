# AGENTS.md

## Mission

Build `activity_tracker` into a reliable local-first macOS activity history service substrate. It should run quietly in the background, append durable logs, preserve enough context for useful retrospection, and expose AI-friendly CLI commands for querying history by day/range, app, title, category, URL/domain, and export format. A SwiftUI app and internal AI reporting agent will sit on top later; do not build that UI here.

## Product Rules

- Local-only by default. Do not send activity data to network services.
- Source of truth is SQLite at `~/.activity_tracker/activity.db`; JSONL is mirror/fallback; CSV is an export/view.
- Configure every SQLite connection with the service pragmas: WAL, normal synchronous mode, foreign keys, and busy timeout.
- Keep day/range reads backed by indexed SQLite time columns; do not scan all history for routine queries.
- Preserve timestamps, app name, bundle ID, category, title, URL when available, and exact duration.
- Categories should use app identity and browser URL domain when available; run `reclassify` after category rules improve.
- Keep observed browser-domain mappings current for recurring work tools, communication, AI, writing, research, and development domains.
- Browser title and URL should come from one active-tab probe sample so log rows do not mix context from different tabs.
- Brief browser title/URL probe misses may reuse current tab context only when same browser app and no conflicting title/URL evidence exists.
- Browser samples with no tab title and no URL beyond the short miss tolerance should become unknown/untracked time, not active browser rows with empty context.
- Preserve `activity_type` and treat idle as first-class log data, not as foreground app time.
- Preserve audited gaps as explicit `activity_type: "untracked"` sessions when repairing coverage.
- Preserve longer unknown/probe-unavailable spans as `activity_type: "untracked"` when the collector recovers.
- Keep an `open_session` heartbeat checkpoint so service restarts recover the current span instead of dropping it.
- Live query commands should include the current open session provisionally; exports should stay completed-session based.
- Windowed summaries and timelines must clip overlapping sessions to the requested day/range/last-minutes bounds; raw session arrays may keep original persisted bounds for audit/debug context.
- Rolling `agent --last-minutes` quality should count uncovered leading/trailing spans inside the requested window as coverage gaps.
- `audit` should support day and explicit window args, using boundary-gap checks for explicit windows.
- Agent/report readiness and quality gates should be scoped to the requested window; include today-wide audit as separate background context.
- `query` and `logs` should support narrow filters plus broad `--text` recall across app, bundle, title, URL, domain, category, and activity type.
- `query` and `logs` should support `--order asc|desc`; agents should use `--order desc --limit N` for latest matching rows.
- `inventory --json` should provide windowed app/domain/category/activity-type facets for SwiftUI filter menus and AI planning without raw-log scans.
- `schema --json` should expose CLI/data-contract capabilities, including agent output and storage verification fields, for SwiftUI and tool harness setup.
- `now --json` should remain a cheap current-activity poll for SwiftUI/menu-bar clients.
- `verify --json` should prove SQLite integrity and JSONL mirror readability/count/content sync.
- `repair-mirror --json` should rebuild JSONL mirror and CSV view from SQLite source of truth when verification fails.
- Keep window-scoped quality issue samples in `agent --json`; compact today-wide audit samples to keep payload bounded.
- Keep `agent` repair commands scoped to the same audited window, and keep `reclassify`/repair commands window-aware so agents do not mutate all history for a narrow report.
- Use `agent.repair_plan.actionable_commands` for automated fixes; `agent.quality.repair_commands` are candidates and may explain non-repairable quality warnings.
- `agent --json` should expose `report_ready`/`action_required` and distinguish actionable repairs from residual non-repairable warnings so reporting agents do not guess.
- Tolerate brief active-app probe misses; do not turn transient macOS/AppleScript failures into fake gaps.
- Tracker startup should tolerate initial active-app or idle probe failures and begin with no active entity instead of exiting.
- Audit should expose suspicious browser title/URL mismatches so old mixed-context rows are visible to agents.
- Use `repair-context` only for high-confidence browser title/URL mismatch or missing-context repairs with neighbor/exact-URL evidence, or to convert all-missing and short unrecoverable mixed-context/title-missing browser rows to untracked time.
- Day queries must handle cross-midnight sessions by overlap, not only start date.
- Background mode should use `launchd` via `activity_tracker service install`.
- `service install` should persist configured `--interval-seconds` and `--idle-threshold-seconds` into LaunchAgent arguments.
- `service status --json` should expose normalized program, arguments, stdout path, and stderr path.
- `service logs --json` should expose bounded launchd stdout/stderr tails with paths for service diagnostics.
- CLI output should support plain text for humans and `--json` for agents; quality commands should expose both time coverage and context richness.

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
cargo run -- health --json
cargo run -- agent --json
cargo run -- agent --last-minutes 240 --json
cargo run -- doctor
cargo run -- verify --json
cargo run -- paths --json
cargo run -- schema --json
cargo run -- now --json
cargo run -- service status --json
cargo run -- service logs --lines 80 --json
cargo run -- day 2026-06-03 --json
cargo run -- report 2026-06-03 --json
cargo run -- timeline 2026-06-03 --json
cargo run -- query --from 2026-06-03 --to 2026-06-03 --domain github --json
cargo run -- query --since 2026-06-03T08:00:00+02:00 --until 2026-06-03T09:00:00+02:00 --json
cargo run -- query --last-minutes 120 --json
cargo run -- query --last-minutes 120 --order desc --limit 20 --json
cargo run -- query --category Development --limit 50 --json
cargo run -- inventory --last-minutes 240 --limit 20 --json
cargo run -- audit 2026-06-03 --json
cargo run -- audit --last-minutes 120 --json
cargo run -- logs 2026-06-03 --domain github --json
cargo run -- logs 2026-06-03 --title project --json
cargo run -- logs 2026-06-03 --url pull/123 --json
cargo run -- query --text "driverry devops" --json
cargo run -- logs 2026-06-03 --activity-type idle --json
cargo run -- export --date 2026-06-03 --format csv
cargo run -- import-csv ~/Desktop/usage_stats.csv --dry-run --json
cargo run -- reclassify --dry-run --json
cargo run -- reclassify --from 2026-06-03 --to 2026-06-03 --dry-run --json
cargo run -- repair-gaps --dry-run --json
cargo run -- repair-titles --dry-run --json
cargo run -- repair-urls --dry-run --json
cargo run -- repair-context --dry-run --json
cargo run -- repair-context --last-minutes 120 --dry-run --json
cargo run -- repair-mirror --json
```

## Repo-Local Skill

Use `.codex/skills/activity-tracker-ai/SKILL.md` when asked to query local activity history, improve collector fidelity, add AI hooks, or reason about the app goal.

## Git

Keep commits atomic and push `main` regularly when tests pass. Never commit generated activity logs, exported CSV/JSONL history, target output, or local LaunchAgent plists.
