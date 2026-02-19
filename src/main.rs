mod data;
mod ui;

use anyhow::Result;
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use data::{usage, HealthStatus};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{
    fs,
    io::{self, stdout},
    panic,
    sync::mpsc,
    time::{Duration, Instant},
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> Result<()> {
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("claude-stats {}", VERSION);
                return Ok(());
            }
            _ => {}
        }
    }

    install_panic_hook();
    color_eyre::install().map_err(|e| anyhow::anyhow!("{}", e))?;

    let terminal = init_terminal()?;
    let result = run(terminal);

    restore_terminal()?;
    result
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
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
        let _ = restore_terminal();
        original_hook(panic_info);
    }));
}

struct FetchResult {
    data: std::result::Result<usage::UsageData, String>,
    elapsed: Duration,
    plan: Option<String>,
}

struct State {
    usage: Option<usage::UsageData>,
    error: Option<String>,
    last_fetch: Option<Instant>,
    health: Option<HealthStatus>,
    working: bool,
    plan: Option<String>,
}

struct Credentials {
    token: String,
    plan: Option<String>,
}

fn get_credentials() -> Option<Credentials> {
    let path = dirs::home_dir()?.join(".claude").join(".credentials.json");
    let contents = fs::read_to_string(path).ok()?;
    let creds: serde_json::Value = serde_json::from_str(&contents).ok()?;
    let oauth = &creds["claudeAiOauth"];
    let token = oauth["accessToken"].as_str()?.to_string();
    let plan = oauth["subscriptionType"].as_str().map(|s| {
        match s {
            "max" => "Claude Max",
            "pro" => "Claude Pro",
            "team" => "Claude Team",
            "enterprise" => "Claude Enterprise",
            other => other,
        }
        .to_string()
    });
    Some(Credentials { token, plan })
}

fn spawn_fetch(tx: &mpsc::Sender<FetchResult>) {
    let tx = tx.clone();
    std::thread::spawn(move || {
        let creds = match get_credentials() {
            Some(c) => c,
            None => {
                let _ = tx.send(FetchResult {
                    data: Err("No OAuth token found in ~/.claude/.credentials.json".into()),
                    elapsed: Duration::ZERO,
                    plan: None,
                });
                return;
            }
        };
        let token = creds.token;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let start = Instant::now();
        let result = rt.block_on(usage::fetch_usage(&token));
        let elapsed = start.elapsed();

        let _ = tx.send(FetchResult {
            data: result.map_err(|e| e.to_string()),
            elapsed,
            plan: creds.plan,
        });
    });
}

fn run(mut terminal: Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let refresh_interval = Duration::from_secs(5);
    let tick_rate = Duration::from_millis(100);

    let (tx, rx) = mpsc::channel::<FetchResult>();

    let mut state = State {
        usage: None,
        error: None,
        last_fetch: None,
        health: None,
        working: true,
        plan: None,
    };

    // Initial fetch
    spawn_fetch(&tx);

    loop {
        // Check for fetch results (non-blocking)
        if let Ok(result) = rx.try_recv() {
            if result.plan.is_some() {
                state.plan = result.plan;
            }
            match result.data {
                Ok(data) => {
                    state.usage = Some(data);
                    state.error = None;
                    state.health = if result.elapsed > Duration::from_secs(3) {
                        Some(HealthStatus::Slow)
                    } else {
                        Some(HealthStatus::Ok)
                    };
                }
                Err(e) => {
                    state.error = Some(e);
                    state.health = Some(HealthStatus::Error);
                }
            }
            state.working = false;
            state.last_fetch = Some(Instant::now());
        }

        // Auto-refresh
        if !state.working {
            if let Some(last) = state.last_fetch {
                if last.elapsed() >= refresh_interval {
                    state.working = true;
                    spawn_fetch(&tx);
                }
            }
        }

        terminal.draw(|frame| {
            ui::render(
                &state.usage,
                &state.error,
                state.health,
                state.working,
                &state.plan,
                frame,
            );
        })?;

        // Handle events
        if crossterm::event::poll(tick_rate)? {
            if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
                if key.kind == crossterm::event::KeyEventKind::Press
                    && matches!(
                        key.code,
                        crossterm::event::KeyCode::Char('q') | crossterm::event::KeyCode::Esc
                    )
                {
                    break;
                }
            }
        }
    }

    Ok(())
}
