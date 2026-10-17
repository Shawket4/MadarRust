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
///
/// `cfg(test)` alone is NOT enough any more. The suites are integration tests
/// now, each its own binary, and they link this library the way production does
/// — without `cfg(test)`. They were silently getting the 30 s production value,
/// and it cost 5 s of wall time PER TEST: a tenant connection opened by a
/// request outlives the test, so `#[sqlx::test]`'s teardown cannot drop the
/// throwaway database ("is being accessed by other users") until the reaper
/// lets go. Measured 2026-09-20: an orders test whose body takes 75 ms reported
/// 5.25 s.
///
/// `MADAR_FAST_TEST_POOLS` restores the short reaping for those binaries, and
/// It is gated on `debug_assertions` as well, so a release
/// build cannot be talked into it whatever the environment says.
fn tenant_idle_timeout() -> Duration {
    if cfg!(test) {
        return Duration::from_secs(2);
    }
    #[cfg(debug_assertions)]
    {
        if std::env::var_os("MADAR_FAST_TEST_POOLS").is_some() {
            return Duration::from_millis(200);
        }
    }
    // Ten minutes, not thirty seconds. A cafe's traffic is BURSTY: a till syncs,
    // then nothing happens for a minute while the barista makes the drink. At
    // 30 s the pool was empty again before the next order, so a quiet shop paid
    // a full reconnect — TCP, Postgres auth, `set_config`, `SET ROLE`, and then
    // sqlx re-preparing every statement, because the statement cache lives on
    // the CONNECTION. Measured on the production box: ~0.5 s, which is most of
    // the 760 ms a single order fetch was taking.
    //
    // This must outlast the gap between two customers, not the gap between two
    // requests in one burst. Ten minutes does; it also matches the registry's
    // own `time_to_idle`, so a pool's connections and the pool itself now go
    // away together instead of the connections dying first and leaving an empty
    // pool that still has to reconnect.
    Duration::from_secs(600)
}

/// One warm connection per active tenant in production, none under test.
///
/// Gated exactly like `tenant_idle_timeout`, and for the same reason: `cfg!(test)`
/// is FALSE in the integration suites (they are separate binaries that link this
/// library the way production does), so gating on it alone would hand every test
/// a held connection — and a held connection makes `#[sqlx::test]`'s teardown
/// fail to drop the throwaway database ("is being accessed by other users").
fn tenant_min_connections() -> u32 {
    if cfg!(test) {
        return 0;
    }
    #[cfg(debug_assertions)]
    {
        if std::env::var_os("MADAR_FAST_TEST_POOLS").is_some() {
            return 0;
        }
    }
    1
}

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

    // Preferred: authenticate tenant connections AS `madar_app`. Previously they
    // connected as the owner (a superuser) and stepped DOWN via `SET ROLE`, which
    // is reversible — anything able to issue `RESET ROLE` on that session would
    // regain owner rights and with them an RLS bypass. Logging in as the role
    // directly removes the privileged identity from the session entirely, so
    // there is nothing to return to.
    //
    // Falls back to the `SET ROLE` path when no password is configured, which is
    // what `#[sqlx::test]` and local development use (the throwaway per-test
    // databases have the role but no credentials for it).
    let app_password = std::env::var("MADAR_APP_DB_PASSWORD")
        .ok()
        .filter(|p| !p.trim().is_empty());
    let login_as_app = app_password.is_some();
    let opts = match &app_password {
        Some(pw) => opts.username(APP_ROLE).password(pw),
        None => opts,
    };
    registry()
        .pools
        .get_with(key, async move {
            let org = org_id.to_string();
            PgPoolOptions::new()
                .max_connections(tenant_max_connections())
                // Keep ONE connection warm per active tenant, and reap the
                // rest. The resident count stays proportional to *active*
                // tenants (the registry evicts a pool that goes ten minutes
                // unused, closing its connections with it), but an org that is
                // merely between customers no longer pays a cold connect —
                // which, with sqlx's per-connection statement cache, meant a
                // reconnect AND a re-prepare of every query in the handler.
                //
                // Under test this must stay 0: `#[sqlx::test]` gives every test
                // its own throwaway database, and a held connection blocks the
                // DROP at teardown ("is being accessed by other users").
                .min_connections(tenant_min_connections())
                .idle_timeout(tenant_idle_timeout())
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
                        // Only needed on the fallback path: when the session
                        // already authenticated as `madar_app` there is no
                        // higher-privileged role to step down from.
                        session_setup(conn, !login_as_app).await
                    })
                })
                .connect_lazy_with(opts)
        })
        .await
}

// ── The shared tenant pool ───────────────────────────────────────────────
//
// One pool per org kept a warm connection for every org with a till checking
// in, and grew it to 2–3 under a sale and a pull at once. Postgres pays ~7.5 MB
// per connection, so memory grew with the number of ACTIVE SHOPS, not with
// traffic: a 2 vCPU / 8 GB box was OOM-killed at ~320 one-branch orgs while its
// CPU sat at half its quota (load test 2026-10-11). An OOM kill restarts every
// backend, so it took down every shop, not the one that tipped it over.
//
// The shared pool holds `madar_app` connections for ALL orgs and stamps each
// one with the caller's org every time it is handed out (`before_acquire` for
// an idle connection, `after_connect` for a new one — sqlx runs both inline in
// the future that asked, so they see the request's task-local). A connection
// handed out with no org in scope is stamped '' and every RLS policy then fails
// its `''::uuid` cast: no org is an error, never another org's rows.
//
// The org lives in a task-local CELL that `tenant_scope` (outermost middleware)
// installs empty for every request and the `Db` extractor fills from the
// verified claims. Work spawned from a request must go through [`spawn`], which
// carries the cell over; a bare `tokio::spawn` would run unscoped and fail
// closed. Outside a scope (tests that build their own `App`, jobs, binaries)
// everything falls back to the per-org pools above, unchanged.
//
// `MADAR_SHARED_TENANT_POOL=0` switches the shared pool off (rollback switch).

tokio::task_local! {
    static TENANT: std::cell::Cell<Option<Uuid>>;
}

fn shared_enabled() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| {
        !matches!(
            std::env::var("MADAR_SHARED_TENANT_POOL").ok().as_deref(),
            Some("0" | "false" | "off")
        )
    })
}

/// Connections in the shared pool (per database: primary and replica each).
fn shared_max_connections() -> u32 {
    static V: OnceLock<u32> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("DB_TENANT_SHARED_MAX_CONNECTIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&n| n >= 2)
            .unwrap_or(40)
    })
}

/// The org stamped on connections handed out in this task, if any.
pub fn current_org() -> Option<Uuid> {
    TENANT.try_with(|c| c.get()).ok().flatten()
}

/// Middleware: give every request an empty tenant cell. Must enclose the
/// handlers (it is the outermost `wrap` in `main`).
pub async fn tenant_scope(
    req: actix_web::dev::ServiceRequest,
    next: actix_web::middleware::Next<impl actix_web::body::MessageBody>,
) -> Result<actix_web::dev::ServiceResponse<impl actix_web::body::MessageBody>, actix_web::Error> {
    TENANT
        .scope(std::cell::Cell::new(None), next.call(req))
        .await
}

/// Run `fut` in this task's tenant scope, if there is one.
pub async fn scoped<F: Future>(org: Option<Uuid>, fut: F) -> F::Output {
    TENANT.scope(std::cell::Cell::new(org), fut).await
}

/// `tokio::spawn`, carrying the caller's tenant scope into the new task. Use it
/// for anything spawned from a request that may touch a [`Db`].
pub fn spawn<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    match TENANT.try_with(|c| c.get()) {
        Ok(org) => tokio::spawn(TENANT.scope(std::cell::Cell::new(org), fut)),
        Err(_) => tokio::spawn(fut),
    }
}

/// `SET ROLE madar_app` (the fallback path) plus the tenant session limits,
/// in ONE round trip. The limits are `madar_app`'s role settings too
/// (migration `20261016130000_app_role_timeouts`), but a role setting applies
/// only to a session that LOGS IN as the role, not to one that `SET ROLE`s
/// into it — so they are set here for both paths alike. Owner connections
/// (migrations, jobs, the seeder) never pass through here.
async fn session_setup(conn: &mut sqlx::PgConnection, set_role: bool) -> Result<(), sqlx::Error> {
    // A `&str` with no arguments goes out as ONE simple Query: both SETs in
    // one round trip.
    sqlx::Executor::execute(&mut *conn, session_sql(set_role))
        .await
        .map(|_| ())
}

/// The statements [`session_setup`] sends (built once; `'static`).
fn session_sql(set_role: bool) -> &'static str {
    static WITH_ROLE: OnceLock<String> = OnceLock::new();
    static WITHOUT: OnceLock<String> = OnceLock::new();
    if set_role {
        WITH_ROLE.get_or_init(|| format!("SET ROLE madar_app; {}", session_limits_sql()))
    } else {
        WITHOUT.get_or_init(|| session_limits_sql().to_string())
    }
}

/// A Postgres interval setting such as `30s`, `500ms`, `2min` or `0`; anything
/// else falls back to the default (env values never reach SQL unchecked).
fn timeout_setting(var: &str, default: &'static str) -> String {
    let v = std::env::var(var).ok().map(|v| v.trim().to_string());
    let ok = |v: &str| {
        let digits = v.trim_end_matches(|c: char| c.is_ascii_alphabetic());
        !digits.is_empty()
            && digits.chars().all(|c| c.is_ascii_digit())
            && matches!(&v[digits.len()..], "" | "ms" | "s" | "min" | "h")
    };
    match v {
        Some(v) if ok(&v) => v,
        _ => default.to_string(),
    }
}

/// `statement_timeout` / `idle_in_transaction_session_timeout` for tenant
/// sessions: `MADAR_APP_STATEMENT_TIMEOUT` (default 30s) and
/// `MADAR_APP_IDLE_TX_TIMEOUT` (default 60s); `0` turns one off.
fn session_limits_sql() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        format!(
            "SET statement_timeout = '{}'; SET idle_in_transaction_session_timeout = '{}';",
            timeout_setting("MADAR_APP_STATEMENT_TIMEOUT", "30s"),
            timeout_setting("MADAR_APP_IDLE_TX_TIMEOUT", "60s"),
        )
    })
}

fn shared_registry() -> &'static std::sync::Mutex<std::collections::HashMap<String, PgPool>> {
    static R: OnceLock<std::sync::Mutex<std::collections::HashMap<String, PgPool>>> =
        OnceLock::new();
    R.get_or_init(Default::default)
}

async fn stamp(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    let org = current_org().map(|o| o.to_string()).unwrap_or_default();
    sqlx::query("SELECT set_config('app.org_id', $1, false)")
        .bind(org)
        .execute(conn)
        .await?;
    Ok(())
}

/// The shared `madar_app` pool on `base`'s database.
fn shared_pool(base: &PgPool) -> PgPool {
    let opts = base.connect_options().as_ref().clone();
    let key = db_key(&opts);
    let mut reg = shared_registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = reg.get(&key) {
        return p.clone();
    }
    let app_password = std::env::var("MADAR_APP_DB_PASSWORD")
        .ok()
        .filter(|p| !p.trim().is_empty());
    let login_as_app = app_password.is_some();
    let opts = match &app_password {
        Some(pw) => opts.username(APP_ROLE).password(pw),
        None => opts,
    };
    let pool = PgPoolOptions::new()
        .max_connections(shared_max_connections())
        .min_connections(tenant_min_connections())
        .idle_timeout(tenant_idle_timeout())
        .max_lifetime(Duration::from_secs(1800))
        .after_connect(move |conn, _meta| {
            Box::pin(async move {
                session_setup(conn, !login_as_app).await?;
                stamp(conn).await
            })
        })
        .before_acquire(|conn, _meta| {
            Box::pin(async move {
                stamp(conn).await?;
                Ok(true)
            })
        })
        .connect_lazy_with(opts);
    reg.insert(key, pool.clone());
    pool
}

/// The pool a request for `org` should use: the shared pool when this task is
/// in a tenant scope that is empty or already `org` (claiming it), else the
/// org's own pool. A second org inside one request never re-stamps the first.
async fn pool_for(base: &PgPool, org: Uuid) -> PgPool {
    if shared_enabled() {
        let claimed = TENANT.try_with(|c| match c.get() {
            None => {
                c.set(Some(org));
                true
            }
            Some(o) => o == org,
        });
        if claimed == Ok(true) {
            return shared_pool(base);
        }
    }
    tenant_pool(base, org).await
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
        Db(pool_for(base, org_id).await)
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
            // Innermost registered pool wins: inside the /reports scope this
            // is the read-replica pool, everywhere else the primary.
            let base = req
                .app_data::<web::Data<PgPool>>()
                .cloned()
                .ok_or(AppError::Internal)?;

            let claims = req.extensions().get::<Claims>().cloned();
            let Some(claims) = claims else {
                // The staff app's employee (a verified staff token, see
                // `staff::principal`): scoped to the token's org like anyone.
                let staff = req
                    .extensions()
                    .get::<crate::staff::principal::StaffPrincipal>()
                    .map(|p| p.org_id);
                return match staff {
                    Some(org) => Ok(Db(pool_for(base.get_ref(), org).await)),
                    None => Err(AppError::Unauthorized("Missing claims".into())),
                };
            };

            match claims.org_id() {
                // Tenant identity comes from the *verified token*, never from
                // any user-supplied value (path, query, header, body).
                Some(org) => Ok(Db(pool_for(base.get_ref(), org).await)),
                // Super admins carry no org: cross-tenant by design.
                None if claims.role == UserRole::SuperAdmin => Ok(Db::bypass(base.get_ref())),
                None => Err(AppError::Forbidden(
                    "Token carries no organization scope".into(),
                )),
            }
        })
    }
}

// ── Public routes (DB_ARCHITECTURE_AUDIT §8 Phase 2.5) ──────────────────────
//
// A public route (an online order, a QR table, a loyalty card, a booking page)
// carries no token, so there is no tenant to take from claims and `Db` would
// answer 401. Such handlers used to run everything on the OWNER pool, which
// bypasses RLS: one missing `org_id` filter and a shop's page could read
// another shop's rows.
//
// [`PublicDb`] splits that in two:
//   * [`PublicDb::resolver`] — the owner pool, for the ROUTING lookups only:
//     the thing the caller supplied that the database can vouch for (a branch,
//     a table, a shop's slug or host, a card or pass token) → its org;
//   * [`PublicDb::scoped`] — a [`Db`] stamped with that org; everything after
//     the resolution runs on it, under RLS, like a signed-in request.
// Keep the resolver's queries few and obviously org-free; anything that reads
// or writes a tenant's data belongs on the scoped handle.

/// The extractor public handlers take instead of `web::Data<PgPool>`.
#[derive(Clone)]
pub struct PublicDb(PgPool);

impl PublicDb {
    /// The owner pool, for resolving which org a public request belongs to.
    pub fn resolver(&self) -> &PgPool {
        &self.0
    }

    /// The tenant handle for `org`, once the request has been resolved to it.
    pub async fn scoped(&self, org: Uuid) -> Db {
        Db::for_org(&self.0, org).await
    }

    /// Build one directly (tests, non-HTTP call sites).
    pub fn new(base: &PgPool) -> PublicDb {
        PublicDb(base.clone())
    }

    /// The org of a live (active, not deleted) branch, or None.
    pub async fn branch_org(&self, branch_id: Uuid) -> Result<Option<Uuid>, AppError> {
        Ok(sqlx::query_scalar!(
            "SELECT org_id FROM branches WHERE id = $1 AND is_active = true AND deleted_at IS NULL",
            branch_id
        )
        .fetch_optional(&self.0)
        .await?)
    }

    /// (branch, org) of an active table at a live branch, or None.
    pub async fn table_branch_org(&self, table_id: Uuid) -> Result<Option<(Uuid, Uuid)>, AppError> {
        Ok(sqlx::query!(
            "SELECT t.branch_id, b.org_id FROM branch_tables t JOIN branches b ON b.id = t.branch_id \
              WHERE t.id = $1 AND t.is_active AND b.is_active AND b.deleted_at IS NULL",
            table_id
        )
        .fetch_optional(&self.0)
        .await?
        .map(|r| (r.branch_id, r.org_id)))
    }
}

impl FromRequest for PublicDb {
    type Error = AppError;
    type Future = std::future::Ready<Result<PublicDb, AppError>>;

    fn from_request(req: &HttpRequest, _payload: &mut Payload) -> Self::Future {
        std::future::ready(
            req.app_data::<web::Data<PgPool>>()
                .map(|p| PublicDb(p.get_ref().clone()))
                .ok_or(AppError::Internal),
        )
    }
}

// ── Transient-failure retry (DB_ARCHITECTURE_AUDIT §6.3, Phase 1.8) ─────────
//
// A deadlock (40P01) or a serialization failure (40001) aborts the whole
// transaction and leaves nothing behind, so an idempotent write can simply be
// run again. Only the OUTERMOST retrying caller in a task retries: replay
// retries the whole op, and the order path it reaches then runs once per
// attempt instead of multiplying the attempts.

tokio::task_local! {
    static RETRYING: ();
}

/// How many times a retrying caller runs its write at most.
pub const RETRY_ATTEMPTS: u32 = 3;

/// A deadlock or serialization failure: the transaction was rolled back whole.
pub fn is_transient(e: &AppError) -> bool {
    match e {
        AppError::Db(sqlx::Error::Database(d)) => {
            matches!(d.code().as_deref(), Some("40P01") | Some("40001"))
        }
        _ => false,
    }
}

/// True inside [`retrying`]: an enclosing caller already retries.
pub fn in_retry_scope() -> bool {
    RETRYING.try_with(|_| ()).is_ok()
}

/// Run `fut` marked as the retrying caller (see [`in_retry_scope`]).
pub async fn retrying<F: Future>(fut: F) -> F::Output {
    RETRYING.scope((), fut).await
}

/// The pause before attempt `next` (2, 3, …): short, growing, and jittered
/// so two tills that deadlocked do not collide again in step.
pub async fn retry_pause(next: u32) {
    let jitter = u64::from(Uuid::new_v4().as_bytes()[0] % 40);
    tokio::time::sleep(Duration::from_millis(20 * u64::from(next) + jitter)).await;
}
