use crate::data::{incidents::StatusData, sessions::SessionData, usage::UsageData};
use crate::error::FetchError;
use std::time::Duration;

pub type EventTx = tokio::sync::mpsc::Sender<AppEvent>;
pub type EventRx = tokio::sync::mpsc::Receiver<AppEvent>;

pub enum AppEvent {
    UsageFetching,
    UsageUpdated {
        data: Result<UsageData, FetchError>,
        elapsed: Duration,
        plan: Option<String>,
    },
    StatusUpdated(Result<StatusData, FetchError>),
    SessionsUpdated(Vec<SessionData>),
    Key(crossterm::event::KeyEvent),
    Shutdown,
}
