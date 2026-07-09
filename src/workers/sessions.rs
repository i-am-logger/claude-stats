use super::ResourceGuard;
use crate::data::sessions;
use crate::event::{AppEvent, EventTx, ResourceKind};
use notify::{RecursiveMode, Watcher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Heartbeat rescan interval in ticks. At 500ms per tick this gives a
/// 5-second cadence, matching the usage/status workers. With a working
/// watcher this is a safety net: sessions change without emitting `.jsonl`
/// events (a `claude` process exits, activity labels age) and inotify
/// events can be missed or coalesced. Without a watcher it is the sole
/// scan trigger.
const HEARTBEAT_TICKS: u64 = 10;

pub(crate) fn spawn(tx: EventTx) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let dirty = Arc::new(AtomicBool::new(false));
        let watcher = setup_watcher(&dirty);
        let has_watcher = watcher.is_some();
        if !has_watcher {
            tracing::info!("filesystem watcher unavailable, falling back to polling");
        }
        let _watcher = watcher; // prevent drop

        let mut cache = sessions::SessionCache::new();
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        let mut poll_counter: u64 = 0;
        loop {
            interval.tick().await;

            poll_counter += 1;
            let heartbeat = poll_counter >= HEARTBEAT_TICKS;
            if heartbeat {
                poll_counter = 0;
            }
            // Watcher events give sub-second latency; the heartbeat keeps
            // process liveness and activity labels honest between events.
            let should_scan = heartbeat || (has_watcher && dirty.swap(false, Ordering::AcqRel));

            if should_scan {
                let (data, returned_cache) = {
                    let _guard = ResourceGuard::acquire(&tx, ResourceKind::Disk);
                    super::blocking(
                        move || {
                            let data = cache.scan();
                            (data, cache)
                        },
                        (Vec::new(), sessions::SessionCache::new()),
                    )
                    .await
                };
                cache = returned_cache;
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
