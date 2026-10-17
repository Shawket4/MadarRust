//! The one LISTEN connection: the changefeed's realtime nudge (R-realtime,
//! `sync.changed` on `Topic::Sync`, debounced per branch) and the fast paths'
//! invalidations (see the module doc of [`crate::fastpath`]).
//!
//! A lost connection is reported by `try_recv` as `None`: notifications may
//! have been dropped while it was down, so the fast-path session ends (nothing
//! is served from memory) and the listener is rebuilt; the new session starts
//! only once every channel is LISTENed again.
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::realtime::hub::BranchEventHub;
use crate::sync::pull::listener::{CHANNEL as SYNC_CHANNEL, Debouncer, publish};

/// The sweeper raised a branch's purge watermark (a cursor below it must now
/// resync): drops the branch's cached idle answer, nudges no device.
pub const PURGED: &str = "sync_feed_purged";
pub const AUTHZ_EPOCH: &str = "authz_epoch";
pub const AUTHZ_USER: &str = "authz_user";
pub const CATALOG: &str = "catalog_revision";
/// Test barrier (see [`crate::fastpath::barrier`]).
pub const BARRIER: &str = "madar_fastpath_barrier";

const CHANNELS: [&str; 6] = [
    SYNC_CHANNEL,
    PURGED,
    AUTHZ_EPOCH,
    AUTHZ_USER,
    CATALOG,
    BARRIER,
];

fn barriers() -> std::sync::MutexGuard<'static, HashMap<Uuid, oneshot::Sender<()>>> {
    static B: LazyLock<Mutex<HashMap<Uuid, oneshot::Sender<()>>>> = LazyLock::new(Default::default);
    B.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn await_barrier(nonce: Uuid) -> oneshot::Receiver<()> {
    let (tx, rx) = oneshot::channel();
    barriers().insert(nonce, tx);
    rx
}

pub fn spawn(pool: PgPool, hub: BranchEventHub) {
    crate::db::spawn(async move {
        loop {
            if let Err(e) = run(&pool, &hub).await {
                tracing::warn!(error = %e, "LISTEN task restarting");
            }
            crate::fastpath::session_lost();
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}

async fn run(pool: &PgPool, hub: &BranchEventHub) -> Result<(), sqlx::Error> {
    let mut listener = PgListener::connect_with(pool).await?;
    listener.listen_all(CHANNELS).await?;
    // From here every commit notifies us: what is read from now on can be
    // cached.
    crate::fastpath::session_started();
    let mut debounce = Debouncer::default();
    loop {
        let next = tokio::time::timeout(Duration::from_millis(250), listener.try_recv()).await;
        let now = Instant::now();
        if let Ok(n) = next {
            let Some(n) = n? else {
                // Lost and (maybe) reconnected: notifications may be gone.
                return Ok(());
            };
            handle(hub, &mut debounce, n.channel(), n.payload(), now);
        }
        for branch in debounce.due(now) {
            publish(hub, branch);
        }
    }
}

fn handle(
    hub: &BranchEventHub,
    debounce: &mut Debouncer,
    channel: &str,
    payload: &str,
    now: Instant,
) {
    match channel {
        SYNC_CHANNEL => {
            if let Ok(branch) = Uuid::parse_str(payload) {
                // Invalidate BEFORE the devices are told: a pull the nudge
                // causes never meets the old answer.
                crate::fastpath::on_branch_changed(branch);
                if debounce.on_notify(branch, now) {
                    publish(hub, branch);
                }
            }
        }
        PURGED => {
            if let Ok(branch) = Uuid::parse_str(payload) {
                crate::fastpath::on_branch_changed(branch);
            }
        }
        AUTHZ_EPOCH | AUTHZ_USER => crate::fastpath::on_authz(),
        CATALOG => {
            if let Some(org) = payload
                .split(':')
                .next()
                .and_then(|o| Uuid::parse_str(o).ok())
            {
                crate::fastpath::on_catalog(org);
            }
        }
        BARRIER => {
            if let Ok(nonce) = Uuid::parse_str(payload)
                && let Some(tx) = barriers().remove(&nonce)
            {
                let _ = tx.send(());
            }
        }
        _ => {}
    }
}
