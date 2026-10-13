//! Background loops: one per source, plus a daily purge.

use crate::state::App;
use std::sync::Arc;
use std::time::Duration;

const SYNC_TIMEOUT: Duration = Duration::from_secs(600);

pub async fn run_source(app: Arc<App>, id: String) {
    let src = app.sources[&id].clone();
    let every = app.cfg.source[&id].every;
    let kick = app.kick[&id].clone();
    loop {
        let t0 = std::time::Instant::now();
        let result = match tokio::time::timeout(SYNC_TIMEOUT, src.sync(&app.store)).await {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!(
                "sync timed out after {}s",
                SYNC_TIMEOUT.as_secs()
            )),
        };
        match &result {
            Ok(r) => tracing::info!(
                source = %id, fetched = r.fetched, inserted = r.inserted, updated = r.updated, deleted = r.deleted,
                ms = t0.elapsed().as_millis() as u64, "sync"
            ),
            Err(e) => tracing::warn!(source = %id, "sync failed: {e:#}"),
        }
        app.record_sync(&id, &result);
        // back off on failure, but never longer than 30 min
        let wait = if result.is_err() {
            (every * 2).min(Duration::from_secs(1800))
        } else {
            every
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = kick.notified() => tracing::info!(source = %id, "sync requested"),
        }
    }
}

pub async fn purge_loop(app: Arc<App>) {
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        let before = chrono::Utc::now().timestamp() - app.cfg.daemon.retention_days as i64 * 86400;
        match app.store.lock().unwrap().purge_older_than(before) {
            Ok(n) if n > 0 => tracing::info!(purged = n, "retention"),
            Ok(_) => {}
            Err(e) => tracing::warn!("purge failed: {e:#}"),
        }
    }
}
