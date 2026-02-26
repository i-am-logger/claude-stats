use crate::data::{
    claude_version::ClaudeVersion, incidents::StatusData, sessions::SessionData, usage::UsageData,
};
use crate::error::FetchError;
use std::time::Duration;

pub(crate) type EventTx = tokio::sync::mpsc::Sender<AppEvent>;
pub(crate) type EventRx = tokio::sync::mpsc::Receiver<AppEvent>;

pub(crate) enum AppEvent {
    UsageFetching,
    UsageUpdated {
        data: Result<UsageData, FetchError>,
        elapsed: Duration,
    },
    AccountUpdated {
        email: Option<String>,
        plan: Option<crate::credentials::Plan>,
    },
    StatusUpdated(Result<StatusData, FetchError>),
    SessionsUpdated(Vec<SessionData>),
    ClaudeVersionUpdated(ClaudeVersion),
    Key(crossterm::event::KeyEvent),
    Shutdown,
}
