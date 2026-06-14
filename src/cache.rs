//! Process-wide in-memory caches (moka) for hot read paths.
//!
//! Today this caches the two cacheable parts of the per-request permission check —
//! role defaults (`role_permissions`) and per-user overrides (`permissions`) — cutting
//! the common protected request from 3 DB round-trips to 1. The security-critical
//! "account disabled" check (V28) is intentionally NOT cached so a deactivated user is
//! revoked immediately.
//!
//! Caching is bypassed under `cfg!(test)`: the suite runs many isolated test databases
//! in a single process, so a shared global cache would leak results across tests.

use std::future::Future;
use std::sync::LazyLock;
use std::time::Duration;

use moka::future::Cache;
use uuid::Uuid;

use crate::errors::AppError;

/// `role_permissions`: (role, resource, action) -> Some(granted) | None (no rule).
static ROLE_PERM: LazyLock<Cache<(String, String, String), Option<bool>>> = LazyLock::new(|| {
    Cache::builder()
        .max_capacity(10_000)
        .time_to_live(Duration::from_secs(300))
        .build()
});

/// `permissions` (per-user override): (user_id, resource, action) -> Some(granted) | None.
static USER_OVERRIDE: LazyLock<Cache<(Uuid, String, String), Option<bool>>> = LazyLock::new(|| {
    Cache::builder()
        .max_capacity(50_000)
        .time_to_live(Duration::from_secs(60))
        .build()
});

/// Role-default lookup with caching. `load` runs only on a cache miss.
pub async fn role_default<F, Fut>(
    role: &str,
    resource: &str,
    action: &str,
    load: F,
) -> Result<Option<bool>, AppError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Option<bool>, AppError>>,
{
    if cfg!(test) {
        return load().await;
    }
    let key = (role.to_owned(), resource.to_owned(), action.to_owned());
    if let Some(v) = ROLE_PERM.get(&key).await {
        return Ok(v);
    }
    let v = load().await?;
    ROLE_PERM.insert(key, v).await;
    Ok(v)
}

/// Per-user override lookup with caching. `load` runs only on a cache miss.
pub async fn user_override<F, Fut>(
    user_id: Uuid,
    resource: &str,
    action: &str,
    load: F,
) -> Result<Option<bool>, AppError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Option<bool>, AppError>>,
{
    if cfg!(test) {
        return load().await;
    }
    let key = (user_id, resource.to_owned(), action.to_owned());
    if let Some(v) = USER_OVERRIDE.get(&key).await {
        return Ok(v);
    }
    let v = load().await?;
    USER_OVERRIDE.insert(key, v).await;
    Ok(v)
}

/// Invalidate one cached role default. Call after writing `role_permissions`.
pub async fn invalidate_role_default(role: &str, resource: &str, action: &str) {
    ROLE_PERM
        .invalidate(&(role.to_owned(), resource.to_owned(), action.to_owned()))
        .await;
}

/// Invalidate one cached user override. Call after writing `permissions`.
pub async fn invalidate_user_override(user_id: Uuid, resource: &str, action: &str) {
    USER_OVERRIDE
        .invalidate(&(user_id, resource.to_owned(), action.to_owned()))
        .await;
}
