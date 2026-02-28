use crate::credentials::Plan;
use crate::data::{
    claude_version::ClaudeVersion, incidents::StatusData, self_version::SelfVersion,
    sessions::SessionData, usage::UsageData, HealthStatus,
};
use crate::error::FetchError;
use crate::event::{AppEvent, EventRx, EventTx, ResourceKind};
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
pub(crate) struct State {
    pub(crate) usage: Option<UsageData>,
    pub(crate) sessions: Vec<SessionData>,
    pub(crate) status: Option<StatusData>,
    pub(crate) error: Option<FetchError>,
    pub(crate) status_error: Option<FetchError>,
    pub(crate) usage_health: HealthStatus,
    pub(crate) usage_fetching: bool,
    pub(crate) plan: Option<Plan>,
    pub(crate) account_email: Option<String>,
    pub(crate) claude_version: Option<ClaudeVersion>,
    pub(crate) self_version: Option<SelfVersion>,
    pub(crate) net_active: u8,
    pub(crate) disk_active: u8,
    pub(crate) tick: u64,
}

impl State {
    /// Composite health: worst-case across all error sources.
    pub(crate) fn health(&self) -> HealthStatus {
        let status_health = if self.status_error.is_some() {
            HealthStatus::Error
        } else {
            HealthStatus::Ok
        };
        self.usage_health.max(status_health)
    }

    /// Process a single event, updating state accordingly.
    /// Returns `true` when the application should shut down.
    pub(crate) fn handle(&mut self, event: AppEvent) -> bool {
        match event {
            AppEvent::UsageFetching => {
                self.usage_fetching = true;
            }
            AppEvent::UsageUpdated { data, elapsed } => {
                match data {
                    Ok(data) => {
                        self.usage = Some(data);
                        self.error = None;
                        self.usage_health = if elapsed > SLOW_API_THRESHOLD {
                            HealthStatus::Slow
                        } else {
                            HealthStatus::Ok
                        };
                    }
                    Err(e) => {
                        self.error = Some(e);
                        self.usage_health = HealthStatus::Error;
                    }
                }
                self.usage_fetching = false;
            }
            AppEvent::StatusUpdated(result) => match result {
                Ok(data) => {
                    self.status = Some(data);
                    self.status_error = None;
                }
                Err(e) => self.status_error = Some(e),
            },
            AppEvent::AccountUpdated { email, plan } => {
                self.account_email = email;
                self.plan = plan;
            }
            AppEvent::SessionsUpdated(sessions) => {
                self.sessions = sessions;
            }
            AppEvent::ClaudeVersionUpdated(version) => {
                self.claude_version = Some(version);
            }
            AppEvent::SelfVersionUpdated(version) => {
                self.self_version = Some(version);
            }
            AppEvent::ResourceBusy(kind) => match kind {
                ResourceKind::Network => {
                    self.net_active = self.net_active.saturating_add(1);
                }
                ResourceKind::Disk => {
                    self.disk_active = self.disk_active.saturating_add(1);
                }
            },
            AppEvent::ResourceIdle(kind) => match kind {
                ResourceKind::Network => {
                    self.net_active = self.net_active.saturating_sub(1);
                }
                ResourceKind::Disk => {
                    self.disk_active = self.disk_active.saturating_sub(1);
                }
            },
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

pub(crate) struct App {
    state: State,
    rx: EventRx,
    _workers: Vec<JoinHandle<()>>,
}

impl App {
    pub(crate) async fn new() -> Result<Self> {
        let (tx, rx) = tokio::sync::mpsc::channel(EVENT_CHANNEL_CAPACITY);

        let initial_sessions = crate::workers::blocking(
            || crate::data::sessions::SessionCache::new().scan(),
            Vec::new(),
        )
        .await;

        let state = State {
            sessions: initial_sessions,
            usage_fetching: true,
            ..State::default()
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

    pub(crate) async fn run(
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

            // Drain pending events before next render (bounded to prevent UI stutter)
            for _ in 0..10 {
                match self.rx.try_recv() {
                    Ok(event) => self.handle(event),
                    Err(_) => break,
                }
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
        crate::workers::status::spawn(tx.clone(), client.clone()),
        crate::workers::claude_version::spawn(tx.clone(), client.clone()),
        crate::workers::self_version::spawn(tx.clone(), client),
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
            drop(tx.send(AppEvent::Shutdown).await);
        }
        #[cfg(not(unix))]
        {
            if tokio::signal::ctrl_c().await.is_ok() {
                drop(tx.send(AppEvent::Shutdown).await);
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
    use crate::data::incidents::{StatusIndicator, StatusSummary};
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
        assert!(!state.usage_fetching);
        let shutdown = state.handle(AppEvent::UsageFetching);
        assert!(!shutdown);
        assert!(state.usage_fetching);
    }

    #[test]
    fn handle_usage_updated_ok() {
        let mut state = State {
            usage_fetching: true,
            ..State::default()
        };
        let shutdown = state.handle(AppEvent::UsageUpdated {
            data: Ok(dummy_usage()),
            elapsed: Duration::from_millis(100),
        });
        assert!(!shutdown);
        assert!(!state.usage_fetching);
        assert!(state.usage.is_some());
        assert!(state.error.is_none());
        assert_eq!(state.health(), HealthStatus::Ok);
    }

    #[test]
    fn handle_usage_updated_error() {
        let mut state = State {
            usage_fetching: true,
            ..State::default()
        };
        let shutdown = state.handle(AppEvent::UsageUpdated {
            data: Err(FetchError::Timeout),
            elapsed: Duration::from_millis(100),
        });
        assert!(!shutdown);
        assert!(!state.usage_fetching);
        assert!(state.error.is_some());
        assert_eq!(state.health(), HealthStatus::Error);
    }

    #[test]
    fn handle_usage_updated_slow() {
        let mut state = State::default();
        let shutdown = state.handle(AppEvent::UsageUpdated {
            data: Ok(dummy_usage()),
            elapsed: Duration::from_secs(5),
        });
        assert!(!shutdown);
        assert_eq!(state.health(), HealthStatus::Slow);
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

    #[test]
    fn handle_claude_version_updated() {
        let mut state = State::default();
        assert!(state.claude_version.is_none());
        let version = ClaudeVersion {
            installed: Some("1.0.16".into()),
            latest: Some("1.0.17".into()),
        };
        let shutdown = state.handle(AppEvent::ClaudeVersionUpdated(version.clone()));
        assert!(!shutdown);
        assert_eq!(state.claude_version, Some(version));
    }

    #[test]
    fn handle_self_version_updated() {
        let mut state = State::default();
        assert!(state.self_version.is_none());
        let version = SelfVersion {
            latest: Some("99.99.99".into()),
        };
        let shutdown = state.handle(AppEvent::SelfVersionUpdated(version.clone()));
        assert!(!shutdown);
        assert_eq!(state.self_version, Some(version));
    }

    #[test]
    fn handle_self_version_updated_replaces_previous() {
        let mut state = State::default();
        state.handle(AppEvent::SelfVersionUpdated(SelfVersion {
            latest: Some("1.0.0".into()),
        }));
        let new_version = SelfVersion {
            latest: Some("2.0.0".into()),
        };
        state.handle(AppEvent::SelfVersionUpdated(new_version.clone()));
        assert_eq!(state.self_version, Some(new_version));
    }

    #[test]
    fn handle_self_version_updated_none_latest() {
        let mut state = State::default();
        let version = SelfVersion { latest: None };
        state.handle(AppEvent::SelfVersionUpdated(version.clone()));
        assert_eq!(state.self_version, Some(version));
    }

    #[test]
    fn handle_account_updated_sets_email_and_plan() {
        let mut state = State::default();
        state.handle(AppEvent::AccountUpdated {
            email: Some("user@example.com".into()),
            plan: Some(Plan::Max),
        });
        assert_eq!(state.account_email.as_deref(), Some("user@example.com"));
        assert_eq!(state.plan, Some(Plan::Max));
    }

    #[test]
    fn handle_account_updated_clears_on_logout() {
        let mut state = State {
            account_email: Some("user@example.com".into()),
            plan: Some(Plan::Max),
            ..State::default()
        };
        state.handle(AppEvent::AccountUpdated {
            email: None,
            plan: None,
        });
        assert_eq!(state.account_email, None);
        assert_eq!(state.plan, None);
    }

    #[test]
    fn handle_account_updated_replaces_account() {
        let mut state = State {
            account_email: Some("old@example.com".into()),
            plan: Some(Plan::Pro),
            ..State::default()
        };
        state.handle(AppEvent::AccountUpdated {
            email: Some("new@example.com".into()),
            plan: Some(Plan::Max),
        });
        assert_eq!(state.account_email.as_deref(), Some("new@example.com"));
        assert_eq!(state.plan, Some(Plan::Max));
    }

    #[test]
    fn handle_resource_busy_increments_counters() {
        let mut state = State::default();
        assert_eq!(state.net_active, 0);
        assert_eq!(state.disk_active, 0);

        state.handle(AppEvent::ResourceBusy(ResourceKind::Network));
        assert_eq!(state.net_active, 1);

        state.handle(AppEvent::ResourceBusy(ResourceKind::Disk));
        assert_eq!(state.disk_active, 1);
    }

    #[test]
    fn handle_resource_idle_decrements_counters() {
        let mut state = State {
            net_active: 2,
            disk_active: 1,
            ..State::default()
        };

        state.handle(AppEvent::ResourceIdle(ResourceKind::Network));
        assert_eq!(state.net_active, 1);

        state.handle(AppEvent::ResourceIdle(ResourceKind::Disk));
        assert_eq!(state.disk_active, 0);
    }

    #[test]
    fn handle_resource_idle_saturates_at_zero() {
        let mut state = State::default();
        assert_eq!(state.net_active, 0);

        state.handle(AppEvent::ResourceIdle(ResourceKind::Network));
        assert_eq!(state.net_active, 0);

        state.handle(AppEvent::ResourceIdle(ResourceKind::Disk));
        assert_eq!(state.disk_active, 0);
    }

    #[test]
    fn handle_resource_multiple_busy() {
        let mut state = State::default();

        state.handle(AppEvent::ResourceBusy(ResourceKind::Network));
        state.handle(AppEvent::ResourceBusy(ResourceKind::Network));
        assert_eq!(state.net_active, 2);

        state.handle(AppEvent::ResourceIdle(ResourceKind::Network));
        assert_eq!(state.net_active, 1);
    }

    #[test]
    fn handle_status_updated_error() {
        let mut state = State::default();
        let shutdown = state.handle(AppEvent::StatusUpdated(Err(FetchError::Timeout)));
        assert!(!shutdown);
        assert!(state.status_error.is_some());
        assert_eq!(state.health(), HealthStatus::Error);
    }

    #[test]
    fn handle_status_updated_ok_clears_error() {
        let mut state = State {
            status_error: Some(FetchError::Timeout),
            ..State::default()
        };
        assert_eq!(state.health(), HealthStatus::Error);
        state.handle(AppEvent::StatusUpdated(Ok(dummy_status())));
        assert!(state.status_error.is_none());
        assert_eq!(state.health(), HealthStatus::Ok);
    }

    #[test]
    fn handle_usage_updated_at_threshold_is_ok() {
        let mut state = State::default();
        let shutdown = state.handle(AppEvent::UsageUpdated {
            data: Ok(dummy_usage()),
            elapsed: SLOW_API_THRESHOLD,
        });
        assert!(!shutdown);
        assert_eq!(state.usage_health, HealthStatus::Ok);
    }

    #[test]
    fn health_composite_worst_case() {
        let mut state = State::default();
        assert_eq!(state.health(), HealthStatus::Ok);

        // Usage slow, no status error → Slow
        state.handle(AppEvent::UsageUpdated {
            data: Ok(dummy_usage()),
            elapsed: Duration::from_secs(5),
        });
        assert_eq!(state.health(), HealthStatus::Slow);

        // Usage slow + status error → Error (worst case wins)
        state.handle(AppEvent::StatusUpdated(Err(FetchError::Timeout)));
        assert_eq!(state.health(), HealthStatus::Error);

        // Status recovers → back to Slow
        state.handle(AppEvent::StatusUpdated(Ok(dummy_status())));
        assert_eq!(state.health(), HealthStatus::Slow);

        // Usage recovers → Ok
        state.handle(AppEvent::UsageUpdated {
            data: Ok(dummy_usage()),
            elapsed: Duration::from_millis(100),
        });
        assert_eq!(state.health(), HealthStatus::Ok);
    }
}
