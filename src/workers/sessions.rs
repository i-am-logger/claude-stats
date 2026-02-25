use crate::data::sessions;
use crate::event::{AppEvent, EventTx};
use notify::{RecursiveMode, Watcher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub fn spawn(tx: EventTx) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let dirty = Arc::new(AtomicBool::new(false));
        let _watcher = setup_watcher(&dirty);

        let mut interval = tokio::time::interval(Duration::from_millis(500));
        loop {
            interval.tick().await;
            if dirty.swap(false, Ordering::AcqRel) {
                let data = tokio::task::spawn_blocking(sessions::scan_active_sessions)
                    .await
                    .unwrap_or_default();
                if tx.send(AppEvent::SessionsUpdated(data)).await.is_err() {
                    break;
                }
            }
        }
    })
}

fn setup_watcher(dirty: &Arc<AtomicBool>) -> Option<notify::RecommendedWatcher> {
    let claude_dir = dirs::home_dir()?.join(".claude").join("projects");
    if !claude_dir.is_dir() {
        return None;
    }

    let dirty = Arc::clone(dirty);
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            let has_jsonl = event
                .paths
                .iter()
                .any(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"));
            if has_jsonl {
                dirty.store(true, Ordering::Release);
            }
        }
    })
    .ok()?;

    watcher.watch(&claude_dir, RecursiveMode::Recursive).ok()?;
    Some(watcher)
}
