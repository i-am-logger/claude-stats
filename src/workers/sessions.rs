use crate::data::sessions;
use crate::event::{AppEvent, EventTx};
use notify::{RecursiveMode, Watcher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Poll interval (in ticks) when the filesystem watcher is unavailable.
/// At 500ms per tick this gives a 5-second scan cadence, matching the
/// usage/status workers.
const FALLBACK_POLL_TICKS: u64 = 10;

pub fn spawn(tx: EventTx) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let dirty = Arc::new(AtomicBool::new(false));
        let watcher = setup_watcher(&dirty);
        let has_watcher = watcher.is_some();
        let _watcher = watcher; // prevent drop

        let mut interval = tokio::time::interval(Duration::from_millis(500));
        let mut poll_counter: u64 = 0;
        loop {
            interval.tick().await;

            let should_scan = if has_watcher {
                dirty.swap(false, Ordering::AcqRel)
            } else {
                poll_counter += 1;
                poll_counter >= FALLBACK_POLL_TICKS && {
                    poll_counter = 0;
                    true
                }
            };

            if should_scan {
                let data = tokio::task::spawn_blocking(sessions::scan_active_sessions)
                    .await
                    .unwrap_or_else(|e| {
                        if e.is_panic() {
                            std::panic::resume_unwind(e.into_panic());
                        }
                        Vec::new()
                    });
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
