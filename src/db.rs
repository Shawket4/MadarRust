//! Tenant-scoped database access (Row-Level Security enforcement point).
//!
//! Postgres RLS policies (see `migrations/*_rls_*.sql`) only apply to the
//! `madar_app` role — the base pool connects as the table owner and therefore
//! BYPASSES them (migrations, seeder, background jobs, super-admin surface).
//! Merchant-facing handlers must instead run on a pool whose connections have:
//!
//!   1. `set_config('app.org_id', <org uuid>, false)`  — the tenant identity,
//!      taken from the *verified JWT claims*, never from user input; and
//!   2. `SET ROLE madar_app`                            — the RLS-enforced role.
//!
//! Both are applied in `after_connect`, so every connection a tenant pool ever
//! hands out is scoped before it can run a single query — a handler that
//! forgets a `WHERE org_id = …` filter can no longer read or write another
//! merchant's rows.
//!
//! [`Db`] is the actix extractor handlers use in place of
//! `web::Data<PgPool>`. It resolves the caller's org from the request's
//! [`Claims`] (inserted by `JwtMiddleware`) and hands back:
//!
//!   * org-bound caller  → the per-org RLS pool for the innermost registered
//!     `web::Data<PgPool>` (so the `/reports` scope transparently gets
//!     tenant-scoped *read-replica* pools);
//!   * super admin (no org) → the base pool unchanged — the deliberate
//!     cross-tenant bypass path;
//!   * no claims → 401 (the route was not behind `JwtMiddleware`).
//!
//! Pools are cached process-wide keyed by (database identity, org) with an
//! idle TTL, so `#[sqlx::test]` apps — which register their throwaway per-test
//! DB pool as plain app data — exercise the exact same RLS path as production
//! with zero extra wiring.

use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::OnceLock;
use std::time::Duration;

use actix_web::{FromRequest, HttpMessage, HttpRequest, dev::Payload, web};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use crate::{auth::jwt::Claims, errors::AppError, models::UserRole};

/// The RLS-enforced role tenant connections switch to. Created (NOLOGIN,
/// NOBYPASSRLS) by the RLS migration; it owns nothing, so `FORCE ROW LEVEL
/// SECURITY` is not required for policies to bind it.
pub const APP_ROLE: &str = "madar_app";

/// Default per-org connection cap. Tenant pools are many and mostly idle, so
/// they get a small per-org cap instead of the base pool's `DB_MAX_CONNECTIONS`.
/// Under `cfg(test)` this drops to a small value: the lib suite runs many
/// `#[sqlx::test]` DBs in parallel, so a low per-org cap keeps the aggregate
/// resident connection count under Postgres's ceiling. It must still be ≥ the
/// most connections a single request holds at once — handlers routinely run a
/// write transaction (`pool.begin()`) while a helper concurrently reads on a
/// second pooled connection (e.g. ticket fire → `resolve_ticket_lines`), so a
/// cap of 1 would self-deadlock. 4 covers that with headroom; fast idle reaping
/// (below) keeps only actively-running tests holding connections.
#[cfg(test)]
const DEFAULT_TENANT_MAX_CONNECTIONS: u32 = 4;
#[cfg(not(test))]
const DEFAULT_TENANT_MAX_CONNECTIONS: u32 = 5;

/// Idle connections are reaped this fast so a finished tenant's connections
/// don't linger. Very short under test (throwaway DBs churn in milliseconds),
/// relaxed in production (avoid needless reconnect churn).
#[cfg(test)]
const TENANT_IDLE_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(not(test))]
const TENANT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-org pool sizing.
fn tenant_max_connections() -> u32 {
    static V: OnceLock<u32> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("DB_TENANT_MAX_CONNECTIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_TENANT_MAX_CONNECTIONS)
    })
}

/// How long an org's pool may sit unused before its connections are released.
fn tenant_idle_secs() -> u64 {
    static V: OnceLock<u64> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("DB_TENANT_POOL_IDLE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(600)
    })
}

/// Process-wide registry of tenant pools, keyed by (database identity, org).
/// The database identity keeps the primary and a read replica (or two
/// different `#[sqlx::test]` databases) from ever sharing a pool.
struct TenantPools {
    pools: moka::future::Cache<(String, Uuid), PgPool>,
}

fn registry() -> &'static TenantPools {
    static REGISTRY: OnceLock<TenantPools> = OnceLock::new();
    REGISTRY.get_or_init(|| TenantPools {
        pools: moka::future::Cache::builder()
            .max_capacity(10_000)
            .time_to_idle(Duration::from_secs(tenant_idle_secs()))
            .eviction_listener(|_key, pool: PgPool, _cause| {
                // Close evicted pools gracefully when we're on a runtime;
                // otherwise dropping the last clone still severs connections.
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move { pool.close().await });
                }
            })
            .build(),
    })
}

/// Stable identity for the server a pool points at.
fn db_key(opts: &sqlx::postgres::PgConnectOptions) -> String {
    format!(
        "{}:{}/{}/{}",
        opts.get_host(),
        opts.get_port(),
        opts.get_database().unwrap_or(""),
        opts.get_username(),
    )
}

/// Get (or lazily create) the RLS-scoped pool for `org_id` on the same
/// database as `base`. Connections inherit the base pool's connect options
/// (credentials, statement-cache config) and are scoped in `after_connect`
/// before first use.
pub async fn tenant_pool(base: &PgPool, org_id: Uuid) -> PgPool {
    let opts = base.connect_options().as_ref().clone();
    let key = (db_key(&opts), org_id);
    registry()
        .pools
        .get_with(key, async move {
            let org = org_id.to_string();
            PgPoolOptions::new()
                .max_connections(tenant_max_connections())
                // Hold no idle connections and reap quickly. Tenant pools are
                // many and bursty (one per active org), and under `#[sqlx::test]`
                // every test spins its own throwaway DB — without prompt reaping
                // their idle connections would accumulate and exhaust Postgres
                // mid-suite. min=0 + a short idle timeout keeps the resident
                // connection count proportional to *active* tenants, not total.
                .min_connections(0)
                .idle_timeout(TENANT_IDLE_TIMEOUT)
                .max_lifetime(Duration::from_secs(1800))
                .after_connect(move |conn, _meta| {
                    let org = org.clone();
                    Box::pin(async move {
                        // Session-level (not LOCAL): the pool never hands this
                        // connection to another org, and `SET ROLE` cannot be
                        // smuggled in via a bind parameter — the role name is
                        // a compile-time constant.
                        sqlx::query("SELECT set_config('app.org_id', $1, false)")
                            .bind(org)
                            .execute(&mut *conn)
                            .await?;
                        sqlx::query("SET ROLE madar_app")
                            .execute(&mut *conn)
                            .await?;
                        Ok(())
                    })
                })
                .connect_lazy_with(opts)
        })
        .await
}

/// Tenant-scoped database handle. Extract this instead of
/// `web::Data<PgPool>` in every JWT-protected handler.
///
/// Derefs to [`PgPool`], and mirrors `web::Data`'s `get_ref` so existing
/// handler bodies (`pool.get_ref()`, `&**pool`, `pool.begin()`) compile
/// unchanged after the signature swap.
#[derive(Clone)]
pub struct Db(PgPool);

impl Db {
    pub fn get_ref(&self) -> &PgPool {
        &self.0
    }

    pub fn into_inner(self) -> PgPool {
        self.0
    }

    /// Build a tenant-scoped handle directly (tests, non-HTTP call sites).
    pub async fn for_org(base: &PgPool, org_id: Uuid) -> Db {
        Db(tenant_pool(base, org_id).await)
    }

    /// The deliberate RLS bypass path (super admin / cross-tenant jobs).
    pub fn bypass(base: &PgPool) -> Db {
        Db(base.clone())
    }
}

impl Deref for Db {
    type Target = PgPool;
    fn deref(&self) -> &PgPool {
        &self.0
    }
}

impl AsRef<PgPool> for Db {
    fn as_ref(&self) -> &PgPool {
        &self.0
    }
}

impl FromRequest for Db {
    type Error = AppError;
    type Future = Pin<Box<dyn Future<Output = Result<Db, AppError>>>>;

    fn from_request(req: &HttpRequest, _payload: &mut Payload) -> Self::Future {
        let req = req.clone();
        Box::pin(async move {
            let claims = req
                .extensions()
                .get::<Claims>()
                .cloned()
                .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))?;

            // Innermost registered pool wins: inside the /reports scope this
            // is the read-replica pool, everywhere else the primary.
            let base = req
                .app_data::<web::Data<PgPool>>()
                .cloned()
                .ok_or(AppError::Internal)?;

            match claims.org_id() {
                // Tenant identity comes from the *verified token*, never from
                // any user-supplied value (path, query, header, body).
                Some(org) => Ok(Db(tenant_pool(base.get_ref(), org).await)),
                // Super admins carry no org: cross-tenant by design.
                None if claims.role == UserRole::SuperAdmin => Ok(Db::bypass(base.get_ref())),
                None => Err(AppError::Forbidden(
                    "Token carries no organization scope".into(),
                )),
            }
        })
    }
}
