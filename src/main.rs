mod app;
mod credentials;
mod data;
mod error;
mod event;
mod fmt;
mod ui;
mod workers;

use claude_stats as _;
#[cfg(test)]
use criterion as _;
#[cfg(test)]
use proptest as _;
#[cfg(test)]
use tempfile as _;

use anyhow::{Context, Result};
use crossterm::{
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{
    fs,
    io::{self, stdout, Write},
    panic,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() -> Result<()> {
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--version" | "-V" => {
                writeln!(stdout(), "claude-stats {VERSION}")?;
                return Ok(());
            }
            _ => {}
        }
    }

    let _log_guard = init_logging();

    install_panic_hook();

    tracing::info!("claude-stats v{VERSION} starting");

    let terminal = init_terminal().context("failed to initialize terminal")?;
    let mut app = app::App::new().await.context("failed to initialize app")?;
    let result = app.run(terminal).await;

    restore_terminal()?;
    tracing::info!("claude-stats shutting down");
    result
}

/// Initialize file-based logging. Returns a guard that must be held for the
/// lifetime of the program to ensure buffered logs are flushed on exit.
fn init_logging() -> tracing_appender::non_blocking::WorkerGuard {
    let log_dir = dirs::data_dir()
        .unwrap_or_else(|| "/tmp".into())
        .join("claude-stats");
    drop(fs::create_dir_all(&log_dir));

    let file_appender = tracing_appender::rolling::never(&log_dir, "claude-stats.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("claude_stats=info".parse().unwrap()),
        )
        .init();

    guard
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen, Clear(ClearType::All))?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    execute!(stdout(), LeaveAlternateScreen)?;
    Ok(())
}

fn install_panic_hook() {
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        drop(restore_terminal());
        original_hook(panic_info);
    }));
}
