use crate::data::{incidents::StatusData, sessions::SessionData, usage::UsageData, HealthStatus};
use crate::error::FetchError;
use crate::event::{AppEvent, EventRx, EventTx};
use crate::ui;
use anyhow::{Context, Result};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::time::Duration;
use tokio::task::JoinHandle;

const TICK_INTERVAL: Duration = Duration::from_millis(100);
const SLOW_API_THRESHOLD: Duration = Duration::from_secs(3);
const EVENT_CHANNEL_CAPACITY: usize = 64;

pub struct State {
    pub usage: Option<UsageData>,
    pub sessions: Vec<SessionData>,
    pub status: Option<StatusData>,
    pub error: Option<FetchError>,
    pub status_error: Option<FetchError>,
    pub health: Option<HealthStatus>,
    pub fetching: bool,
    pub plan: Option<String>,
    pub tick: u64,
}

pub struct App {
    state: State,
    rx: EventRx,
    _workers: Vec<JoinHandle<()>>,
}

impl App {
    pub async fn new() -> Result<Self> {
        let (tx, rx) = tokio::sync::mpsc::channel(EVENT_CHANNEL_CAPACITY);

        let initial_plan = tokio::task::spawn_blocking(crate::credentials::get_credentials)
            .await
            .ok()
            .and_then(Result::ok)
            .and_then(|c| c.plan);

        let initial_sessions =
            tokio::task::spawn_blocking(crate::data::sessions::scan_active_sessions)
                .await
                .unwrap_or_default();

        let state = State {
            usage: None,
            sessions: initial_sessions,
            status: None,
            error: None,
            status_error: None,
            health: None,
            fetching: true,
            plan: initial_plan,
            tick: 0,
        };

        let workers = spawn_workers(&tx)?;
        install_signal_handler(&tx);
        spawn_input_reader(&tx);

        Ok(Self {
            state,
            rx,
            _workers: workers,
        })
    }

    pub async fn run(
        &mut self,
        mut terminal: Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<()> {
        let mut tick = tokio::time::interval(TICK_INTERVAL);
        loop {
            terminal.draw(|f| ui::render(&self.state, f))?;

            tokio::select! {
                _ = tick.tick() => {
                    self.state.tick += 1;
                }
                event = self.rx.recv() => {
                    match event {
                        Some(event) => self.handle(event),
                        None => break,
                    }
                }
            }

            // Drain pending events before next render
            while let Ok(event) = self.rx.try_recv() {
                self.handle(event);
            }
        }
        Ok(())
    }

    fn handle(&mut self, event: AppEvent) {
        match event {
            AppEvent::UsageFetching => {
                self.state.fetching = true;
            }
            AppEvent::UsageUpdated {
                data,
                elapsed,
                plan,
            } => {
                if plan.is_some() {
                    self.state.plan = plan;
                }
                match data {
                    Ok(data) => {
                        self.state.usage = Some(data);
                        self.state.error = None;
                        self.state.health = if elapsed > SLOW_API_THRESHOLD {
                            Some(HealthStatus::Slow)
                        } else {
                            Some(HealthStatus::Ok)
                        };
                    }
                    Err(e) => {
                        self.state.error = Some(e);
                        self.state.health = Some(HealthStatus::Error);
                    }
                }
                self.state.fetching = false;
            }
            AppEvent::StatusUpdated(result) => match result {
                Ok(data) => self.state.status = Some(data),
                Err(e) => self.state.status_error = Some(e),
            },
            AppEvent::SessionsUpdated(sessions) => {
                self.state.sessions = sessions;
            }
            AppEvent::Key(key) => {
                if key.kind == crossterm::event::KeyEventKind::Press
                    && matches!(
                        key.code,
                        crossterm::event::KeyCode::Char('q') | crossterm::event::KeyCode::Esc
                    )
                {
                    self.rx.close();
                }
            }
            AppEvent::Shutdown => {
                self.rx.close();
            }
        }
    }
}

fn spawn_workers(tx: &EventTx) -> Result<Vec<JoinHandle<()>>> {
    let client = crate::workers::build_http_client().context("failed to build HTTP client")?;
    let handles = vec![
        crate::workers::usage::spawn(tx.clone(), client.clone()),
        crate::workers::status::spawn(tx.clone(), client),
        crate::workers::sessions::spawn(tx.clone()),
    ];
    Ok(handles)
}

fn install_signal_handler(tx: &EventTx) {
    let tx = tx.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let Ok(mut sigterm) = signal(SignalKind::terminate()) else {
                return;
            };
            sigterm.recv().await;
            let _ = tx.send(AppEvent::Shutdown).await;
        }
    });
}

fn spawn_input_reader(tx: &EventTx) {
    let tx = tx.clone();
    std::thread::spawn(move || loop {
        match crossterm::event::read() {
            Ok(crossterm::event::Event::Key(key)) => {
                if tx.blocking_send(AppEvent::Key(key)).is_err() {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    });
}
