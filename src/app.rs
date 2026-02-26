use crate::credentials::Plan;
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

#[derive(Default)]
pub struct State {
    pub usage: Option<UsageData>,
    pub sessions: Vec<SessionData>,
    pub status: Option<StatusData>,
    pub error: Option<FetchError>,
    pub status_error: Option<FetchError>,
    pub health: Option<HealthStatus>,
    pub fetching: bool,
    pub plan: Option<Plan>,
    pub tick: u64,
}

impl State {
    /// Process a single event, updating state accordingly.
    /// Returns `true` when the application should shut down.
    pub fn handle(&mut self, event: AppEvent) -> bool {
        match event {
            AppEvent::UsageFetching => {
                self.fetching = true;
            }
            AppEvent::UsageUpdated {
                data,
                elapsed,
                plan,
            } => {
                if plan.is_some() {
                    self.plan = plan;
                }
                match data {
                    Ok(data) => {
                        self.usage = Some(data);
                        self.error = None;
                        self.health = if elapsed > SLOW_API_THRESHOLD {
                            Some(HealthStatus::Slow)
                        } else {
                            Some(HealthStatus::Ok)
                        };
                    }
                    Err(e) => {
                        self.error = Some(e);
                        self.health = Some(HealthStatus::Error);
                    }
                }
                self.fetching = false;
            }
            AppEvent::StatusUpdated(result) => match result {
                Ok(data) => self.status = Some(data),
                Err(e) => self.status_error = Some(e),
            },
            AppEvent::SessionsUpdated(sessions) => {
                self.sessions = sessions;
            }
            AppEvent::Key(key) => {
                if key.kind == crossterm::event::KeyEventKind::Press
                    && matches!(
                        key.code,
                        crossterm::event::KeyCode::Char('q') | crossterm::event::KeyCode::Esc
                    )
                {
                    return true;
                }
            }
            AppEvent::Shutdown => {
                return true;
            }
        }
        false
    }
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
            .unwrap_or_else(|e| {
                if e.is_panic() {
                    std::panic::resume_unwind(e.into_panic());
                }
                Err(crate::error::CredentialError::FileNotFound)
            })
            .ok()
            .and_then(|c| c.plan);

        let initial_sessions =
            tokio::task::spawn_blocking(crate::data::sessions::scan_active_sessions)
                .await
                .unwrap_or_else(|e| {
                    if e.is_panic() {
                        std::panic::resume_unwind(e.into_panic());
                    }
                    Vec::new()
                });

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
        if self.state.handle(event) {
            self.rx.close();
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
            let Ok(mut sigint) = signal(SignalKind::interrupt()) else {
                return;
            };
            tokio::select! {
                _ = sigterm.recv() => {}
                _ = sigint.recv() => {}
            }
            let _ = tx.send(AppEvent::Shutdown).await;
        }
        #[cfg(not(unix))]
        {
            if tokio::signal::ctrl_c().await.is_ok() {
                let _ = tx.send(AppEvent::Shutdown).await;
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::incidents::{StatusData, StatusIndicator, StatusSummary};
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    fn make_key(code: KeyCode) -> AppEvent {
        AppEvent::Key(KeyEvent::new_with_kind(
            code,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ))
    }

    fn dummy_usage() -> UsageData {
        UsageData {
            five_hour: None,
            seven_day: None,
            seven_day_opus: None,
            seven_day_sonnet: None,
        }
    }

    fn dummy_status() -> StatusData {
        StatusData {
            summary: StatusSummary {
                indicator: StatusIndicator::None,
                description: "All Systems Operational".into(),
            },
            incidents: Vec::new(),
        }
    }

    #[test]
    fn handle_usage_fetching_sets_flag() {
        let mut state = State::default();
        assert!(!state.fetching);
        let shutdown = state.handle(AppEvent::UsageFetching);
        assert!(!shutdown);
        assert!(state.fetching);
    }

    #[test]
    fn handle_usage_updated_ok() {
        let mut state = State::default();
        state.fetching = true;
        let shutdown = state.handle(AppEvent::UsageUpdated {
            data: Ok(dummy_usage()),
            elapsed: Duration::from_millis(100),
            plan: None,
        });
        assert!(!shutdown);
        assert!(!state.fetching);
        assert!(state.usage.is_some());
        assert!(state.error.is_none());
        assert_eq!(state.health, Some(HealthStatus::Ok));
    }

    #[test]
    fn handle_usage_updated_error() {
        let mut state = State::default();
        state.fetching = true;
        let shutdown = state.handle(AppEvent::UsageUpdated {
            data: Err(FetchError::Timeout),
            elapsed: Duration::from_millis(100),
            plan: None,
        });
        assert!(!shutdown);
        assert!(!state.fetching);
        assert!(state.error.is_some());
        assert_eq!(state.health, Some(HealthStatus::Error));
    }

    #[test]
    fn handle_usage_updated_slow() {
        let mut state = State::default();
        let shutdown = state.handle(AppEvent::UsageUpdated {
            data: Ok(dummy_usage()),
            elapsed: Duration::from_secs(5),
            plan: None,
        });
        assert!(!shutdown);
        assert_eq!(state.health, Some(HealthStatus::Slow));
    }

    #[test]
    fn handle_status_updated_ok() {
        let mut state = State::default();
        let shutdown = state.handle(AppEvent::StatusUpdated(Ok(dummy_status())));
        assert!(!shutdown);
        assert!(state.status.is_some());
        assert!(state.status_error.is_none());
    }

    #[test]
    fn handle_sessions_updated() {
        let mut state = State::default();
        assert!(state.sessions.is_empty());
        let shutdown = state.handle(AppEvent::SessionsUpdated(vec![]));
        assert!(!shutdown);
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn handle_shutdown_returns_true() {
        let mut state = State::default();
        assert!(state.handle(AppEvent::Shutdown));
    }

    #[test]
    fn handle_key_q_returns_true() {
        let mut state = State::default();
        assert!(state.handle(make_key(KeyCode::Char('q'))));
    }

    #[test]
    fn handle_key_esc_returns_true() {
        let mut state = State::default();
        assert!(state.handle(make_key(KeyCode::Esc)));
    }

    #[test]
    fn handle_key_other_returns_false() {
        let mut state = State::default();
        assert!(!state.handle(make_key(KeyCode::Char('a'))));
    }
}
