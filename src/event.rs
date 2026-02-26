use crate::data::{incidents::StatusData, sessions::SessionData, usage::UsageData};
use crate::error::FetchError;
use std::time::Duration;

pub(crate) type EventTx = tokio::sync::mpsc::Sender<AppEvent>;
pub(crate) type EventRx = tokio::sync::mpsc::Receiver<AppEvent>;

pub(crate) enum AppEvent {
    UsageFetching,
    UsageUpdated {
        data: Result<UsageData, FetchError>,
        elapsed: Duration,
        plan: Option<crate::credentials::Plan>,
    },
    StatusUpdated(Result<StatusData, FetchError>),
    SessionsUpdated(Vec<SessionData>),
    Key(crossterm::event::KeyEvent),
    Shutdown,
}
