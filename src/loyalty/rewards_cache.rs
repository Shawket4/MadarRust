//! The Rewards module's read caches (REWARDS_DASHBOARD_DESIGN.md §12).
//!
//! Same shape as `crate::cache` and `menu::cache`: process-wide moka caches
//! keyed by org, filled on a miss, INVALIDATED by the write that changes them,
//! with a TTL as the backstop. The box runs one API process, so an in-process
//! invalidation is the whole story; the TTL bounds anything written outside
//! these handlers (a menu item deleted under a track, a hand edit).
//!
//! | Cache | Read by | Invalidated by | TTL |
//! |---|---|---|---|
//! | tracks (definitions + items) | lookup, award, redemption, counter, passes, CRM | `PUT /loyalty/tracks` | 5 min |
//! | published design | every designed pass build, the Google hero | publish / unpublish | 5 min |
//! | figures (`/loyalty/stats`) | the overview | — (an aggregate: TTL only) | 60 s |
//! | counts (CRM total, audience size) | the members table, the composer | — (TTL only) | 60 s |
//!
//! The figures and counts are aggregates over a whole org; serving them for
//! up to a minute is the trade (the overview says "updated a minute ago" at
//! worst), and the stamp a teller just gave shows on the member at once
//! because member reads are never cached.

use std::future::Future;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use moka::future::Cache;
use uuid::Uuid;

use crate::errors::AppError;

fn cache<K, V>(cap: u64, ttl: u64) -> Cache<K, V>
where
    K: std::hash::Hash + Eq + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    Cache::builder()
        .max_capacity(cap)
        .time_to_live(Duration::from_secs(ttl))
        .build()
}

/// Raw track rows of an org: `(position, name_en, name_ar, size, shape, colour, per_item, cap, items)`.
pub type TrackRows = Arc<Vec<super::tracks::TrackDef>>;

static TRACKS: LazyLock<Cache<Uuid, TrackRows>> = LazyLock::new(|| cache(20_000, 300));
/// An org's published design and its revision, if any.
pub type Published = Arc<Option<(i32, madar_card::CardDesign)>>;

static DESIGNS: LazyLock<Cache<Uuid, Published>> = LazyLock::new(|| cache(20_000, 300));
static STATS: LazyLock<Cache<String, Arc<serde_json::Value>>> = LazyLock::new(|| cache(5_000, 60));
static COUNTS: LazyLock<Cache<String, i64>> = LazyLock::new(|| cache(20_000, 60));

async fn through<K, V, F, Fut>(c: &Cache<K, V>, key: K, load: F) -> Result<V, AppError>
where
    K: std::hash::Hash + Eq + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<V, AppError>>,
{
    if let Some(v) = c.get(&key).await {
        return Ok(v);
    }
    let v = load().await?;
    c.insert(key, v.clone()).await;
    Ok(v)
}

pub async fn tracks<F, Fut>(org: Uuid, load: F) -> Result<TrackRows, AppError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<TrackRows, AppError>>,
{
    through(&TRACKS, org, load).await
}

pub async fn invalidate_tracks(org: Uuid) {
    TRACKS.invalidate(&org).await;
}

pub async fn design<F, Fut>(
    org: Uuid,
    load: F,
) -> Result<Arc<Option<(i32, madar_card::CardDesign)>>, AppError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Arc<Option<(i32, madar_card::CardDesign)>>, AppError>>,
{
    through(&DESIGNS, org, load).await
}

pub async fn invalidate_design(org: Uuid) {
    DESIGNS.invalidate(&org).await;
}

pub async fn stats<F, Fut>(key: String, load: F) -> Result<Arc<serde_json::Value>, AppError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Arc<serde_json::Value>, AppError>>,
{
    through(&STATS, key, load).await
}

pub async fn count<F, Fut>(key: String, load: F) -> Result<i64, AppError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<i64, AppError>>,
{
    through(&COUNTS, key, load).await
}
