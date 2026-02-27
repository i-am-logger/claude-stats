use crate::data::{
    claude_version::ClaudeVersion, incidents::StatusData, self_version::SelfVersion,
    sessions::SessionData, usage::UsageData,
};
use crate::error::FetchError;
use std::time::Duration;

pub(crate) type EventTx = tokio::sync::mpsc::Sender<AppEvent>;
pub(crate) type EventRx = tokio::sync::mpsc::Receiver<AppEvent>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ResourceKind {
    Network,
    Disk,
}

#[derive(Debug)]
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
    SelfVersionUpdated(SelfVersion),
    ResourceBusy(ResourceKind),
    ResourceIdle(ResourceKind),
    Key(crossterm::event::KeyEvent),
    Shutdown,
}
