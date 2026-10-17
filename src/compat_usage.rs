//! `compat_usage`: how often each compatibility shim is still used, per org
//! per day (DB_ARCHITECTURE_AUDIT_2026-10-16 §7.3). It answers "can I remove
//! X yet": a shim is a candidate once it has 0 hits for 30 consecutive days.
//!
//! Two ways in:
//! * every [`crate::client_seen::legacy_hit`] (a CLIENT took an old path) is
//!   counted here as well as recorded on the device's `client_seen` row;
//! * [`count_internal`] for the SERVER's own reads of a compat view (the
//!   order path pricing through `addon_items`, say). Those are counted only:
//!   they say nothing about the device, so they never reach `client_seen`.
//!
//! Counting is one map update in memory; [`spawn`] writes the totals once a
//! minute (`INSERT … ON CONFLICT DO UPDATE SET hits = hits + …`). A crash loses
//! at most a minute of counts, never a request. Nothing here can fail or slow
//! a request.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// A server read of a compat VIEW; the site names the module. Since DB Phase
/// 3.1 the server's own code reads the unified tables and nothing counts this;
/// it stays for any new internal compat read (`tests/compat_contract_tests.rs`
/// forbids unregistered ones).
pub const KIND_COMPAT_VIEW_SERVER: &str = "compat_view_server";
/// A client read an old menu shape (`/addon-items`, `/branch-addon-overrides`,
/// `/branch-menu-overrides`, `/recipes/addons/{id}`, `/recipes/drinks`,
/// `/menu-items?full=true`'s legacy fields); the site names the route.
pub const KIND_MENU_LEGACY_READ: &str = "menu_legacy_read";
/// A retired write into a compat view answered 410 `SHIM_WRITE_REMOVED`.
pub const KIND_SHIM_WRITE_GONE: &str = "shim_write_gone";

#[derive(Clone, Copy)]
struct Tally {
    hits: i64,
    first: DateTime<Utc>,
    last: DateTime<Utc>,
}

type Key = (&'static str, &'static str, NaiveDate, Uuid);

fn tallies() -> &'static Mutex<HashMap<Key, Tally>> {
    static T: OnceLock<Mutex<HashMap<Key, Tally>>> = OnceLock::new();
    T.get_or_init(Default::default)
}

/// Count one use of `kind` at `site` for `org` (nil uuid when unknown).
pub fn count(kind: &'static str, site: &'static str, org: Option<Uuid>) {
    let now = Utc::now();
    let key = (kind, site, now.date_naive(), org.unwrap_or(Uuid::nil()));
    if let Ok(mut map) = tallies().lock() {
        // Bounded: a flush empties it; a stuck flusher must not grow it forever.
        if map.len() > 100_000 && !map.contains_key(&key) {
            return;
        }
        map.entry(key)
            .and_modify(|t| {
                t.hits += 1;
                t.last = now;
            })
            .or_insert(Tally {
                hits: 1,
                first: now,
                last: now,
            });
    }
}

/// A server-side read of a compat view, counted for the org this task serves.
pub fn count_internal(site: &'static str) {
    count(KIND_COMPAT_VIEW_SERVER, site, crate::db::current_org());
}

/// Write and clear the in-memory totals. Returns the number of rows upserted.
/// On a failure the totals are put back for the next flush.
pub async fn flush(pool: &PgPool) -> Result<usize, sqlx::Error> {
    let drained: Vec<(Key, Tally)> = match tallies().lock() {
        Ok(mut map) => map.drain().collect(),
        Err(_) => return Ok(0),
    };
    if drained.is_empty() {
        return Ok(0);
    }
    let n = drained.len();
    let kinds: Vec<&str> = drained.iter().map(|((k, ..), _)| *k).collect();
    let sites: Vec<&str> = drained.iter().map(|((_, s, ..), _)| *s).collect();
    let days: Vec<NaiveDate> = drained.iter().map(|((_, _, d, _), _)| *d).collect();
    let orgs: Vec<Uuid> = drained.iter().map(|((.., o), _)| *o).collect();
    let hits: Vec<i64> = drained.iter().map(|(_, t)| t.hits).collect();
    let firsts: Vec<DateTime<Utc>> = drained.iter().map(|(_, t)| t.first).collect();
    let lasts: Vec<DateTime<Utc>> = drained.iter().map(|(_, t)| t.last).collect();
    let res = sqlx::query!(
        "INSERT INTO compat_usage (kind, site, day, org_id, hits, first_at, last_at)
         SELECT * FROM UNNEST($1::text[], $2::text[], $3::date[], $4::uuid[],
                              $5::int8[], $6::timestamptz[], $7::timestamptz[])
         ON CONFLICT (kind, site, day, org_id) DO UPDATE
            SET hits = compat_usage.hits + EXCLUDED.hits,
                last_at = GREATEST(compat_usage.last_at, EXCLUDED.last_at)",
        &kinds as &[&str],
        &sites as &[&str],
        &days,
        &orgs,
        &hits,
        &firsts,
        &lasts,
    )
    .execute(pool)
    .await;
    if let Err(e) = res {
        if let Ok(mut map) = tallies().lock() {
            for (k, t) in drained {
                map.entry(k)
                    .and_modify(|x| {
                        x.hits += t.hits;
                        x.first = x.first.min(t.first);
                        x.last = x.last.max(t.last);
                    })
                    .or_insert(t);
            }
        }
        return Err(e);
    }
    Ok(n)
}

/// The once-a-minute writer. Spawned once in `main`.
pub fn spawn(pool: PgPool) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(e) = flush(&pool).await {
                tracing::debug!(target: "madar.compat_usage", error = %e, "compat_usage flush failed");
            }
        }
    });
}

/// The answer of a retired write into a compat view: 410 `SHIM_WRITE_REMOVED`.
///
/// In production these relations have been read-only views since the menu
/// unification (now migration `20261018200000_p2_converge_legacy_catalog`), so every one of these
/// writes already failed there; no dashboard or POS build calls them.
/// Counted (`shim_write_gone`, site = the route) like any legacy path.
pub fn shim_write_removed(route: &'static str) -> crate::errors::AppError {
    crate::client_seen::legacy_hit_at(KIND_SHIM_WRITE_GONE, route);
    crate::errors::AppError::Coded {
        status: 410,
        code: "SHIM_WRITE_REMOVED",
        reason: format!(
            "{route} was retired: the legacy add-on catalog is read-only. \
             Add-ons are modifier options now; edit them through the modifier-group \
             endpoints (/modifier-groups) and their recipes through the option recipe endpoints."
        ),
    }
}
