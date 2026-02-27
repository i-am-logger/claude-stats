# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

claude-stats is a Rust TUI dashboard for monitoring Claude Code usage limits and active sessions. It displays real-time usage gauges, active session context windows, subagent tracking, and Claude API status/incidents.

## Commands

```bash
cargo build --release        # Build
cargo test                   # Run all tests
cargo test <test_name>       # Run a single test
cargo fmt --check            # Check formatting
cargo clippy -- -D warnings  # Lint
cargo bench --bench parsing  # Run criterion benchmarks
cargo deny check             # License/advisory audit
cargo bloat --release --crates  # Binary size breakdown
cargo mutants                # Mutation testing
```

With devenv: `dev-run`, `dev-build`, `dev-test`, `dev-watch`, `dev-coverage`, `dev-bench`, `dev-bloat`, `dev-mutants`

## Architecture

Event-driven TUI application using ratatui + tokio:

- **`app.rs`** — Central event loop and state machine. `State` holds all mutable state, `App::run()` drives the loop with `tokio::select!`. `AppEvent` enum defines all state transitions.
- **`workers/`** — Background async tasks that fetch data and emit `AppEvent`s via tokio MPSC channel:
  - `usage.rs` — Polls usage data from Anthropic OAuth API (5s interval, 30s backoff on errors)
  - `status.rs` — Polls Claude system status from statuspage API
  - `sessions.rs` — Watches `~/.claude/projects/` for active JSONL session files via `notify` crate with polling fallback
- **`data/`** — Data models and parsing:
  - `usage.rs` — `UsageData`, `UsageLimit` structs and API deserialization
  - `sessions/` — JSONL session file parsing, context window calculation, subagent scanning, session state detection (Idle/Thinking/Working)
  - `incidents.rs` — Statuspage API models
- **`ui/`** — Modular rendering via `Section` trait. Each section computes its `height()` and implements `render()`. Main `render()` divides terminal space with ratatui `Layout`.
- **`credentials.rs`** — Loads OAuth token from `~/.claude/.credentials.json`

## Linting Rules

Strict lint configuration in `Cargo.toml` — all clippy warnings are errors:
- `clippy::all`, `clippy::pedantic`, and `clippy::nursery` are set to `deny`
- `unsafe_code` is forbidden
- `print_stdout`, `print_stderr`, `dbg_macro`, `todo`, `unimplemented` are denied (use ratatui for output)
- Some pedantic overrides allowed: `module_name_repetitions`, `cast_possible_truncation`, `cast_sign_loss`, `cast_precision_loss`, `wildcard_imports`

## Conventions

- Error handling: `anyhow::Result` for top-level, `thiserror` derive for domain errors
- Workers recover from errors with exponential backoff (3+ consecutive errors → 30s)
- Visibility: prefer `pub(crate)` for internal APIs
- Tests go in `#[cfg(test)] mod tests` at the bottom of each file
- Property tests use proptest in a nested `mod prop` inside `mod tests`, with functions prefixed `prop_`
- `src/lib.rs` re-exports modules for benchmark access; benchmark-target functions use `pub` visibility with `#![allow(unreachable_pub)]`
