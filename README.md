[![build](https://github.com/i-am-logger/claude-stats/actions/workflows/ci.yml/badge.svg)](https://github.com/i-am-logger/claude-stats/actions/workflows/ci.yml)
[![release](https://github.com/i-am-logger/claude-stats/actions/workflows/release-please.yml/badge.svg)](https://github.com/i-am-logger/claude-stats/actions/workflows/release-please.yml)
[![License: CC BY-NC-SA 4.0](https://img.shields.io/badge/License-CC%20BY--NC--SA%204.0-lightgrey.svg)](https://creativecommons.org/licenses/by-nc-sa/4.0/)

# claude-stats

A TUI dashboard for Claude Code usage limits.

<img width="479" height="443" alt="image" src="https://github.com/user-attachments/assets/5a8b7892-0906-4ff3-a1ee-c88d3a47dd7e" />


## Features

- Live usage gauges for session, weekly (all models), Opus, and Sonnet limits
- Auto-refreshes every 5 seconds
- Displays your plan type (Pro, Max, Team, Enterprise)
- Countdown timers showing when limits reset
- Color-coded warnings at 70% and 85% utilization
- Activity and health status indicators

## Install

```bash
cargo install --git https://github.com/i-am-logger/claude-stats
```

## Usage

```bash
claude-stats
```

Press `q` or `Esc` to quit.

Requires a valid Claude Code OAuth token in `~/.claude/.credentials.json`.

## Development

This project uses [devenv](https://devenv.sh/) for development environment management.

```bash
devenv shell
dev-run       # Run the application
dev-build     # Build the application
dev-test      # Run tests
```

## License

CC BY-NC-SA 4.0
