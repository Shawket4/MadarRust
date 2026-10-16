//! Running the old and new permission models side by side.
//!
//! `MADAR_AUTHZ_MODE`:
//! - `shadow` (Phase 2): serve the LEGACY decision; compute the new one
//!   and log any divergence (`madar.authz.shadow`).
//! - `enforce` (Phase 3, the default): serve the NEW decision; log divergence the same way.
//! - `legacy`: legacy only, no second computation (emergency switch).
//!
//! A failure to compute the new decision never fails a request in `shadow`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use sqlx::PgConnection;
use uuid::Uuid;

use super::{Scope, legacy};
use crate::errors::AppError;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Legacy,
    Shadow,
    Enforce,
}

pub static MODE: LazyLock<Mode> =
    LazyLock::new(|| match std::env::var("MADAR_AUTHZ_MODE").as_deref() {
        Ok("legacy") => Mode::Legacy,
        Ok("shadow") => Mode::Shadow,
        _ => Mode::Enforce,
    });

/// Divergences seen since start (exported for observability).
pub static DIVERGENCES: AtomicU64 = AtomicU64::new(0);

/// One disagreement, as `authz_shadow_disagreements` keys it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    day: chrono::NaiveDate,
    org_id: Option<Uuid>,
    user_id: Uuid,
    resource: String,
    action: String,
    legacy: bool,
    new: bool,
}

#[derive(Clone, Copy, Debug)]
struct Seen {
    hits: i64,
    first: chrono::DateTime<chrono::Utc>,
    last: chrono::DateTime<chrono::Utc>,
}

/// Disagreements not yet written. Bounded: past [`PENDING_CAP`] distinct keys a
/// new key is only counted in [`DIVERGENCES`] (a storm must not grow memory).
static PENDING: LazyLock<Mutex<HashMap<Key, Seen>>> = LazyLock::new(Default::default);
const PENDING_CAP: usize = 50_000;
const FLUSH_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

fn record(org_id: Option<Uuid>, user_id: Uuid, resource: &str, action: &str, legacy: bool, new: bool) {
    let now = chrono::Utc::now();
    let key = Key {
        day: now.date_naive(),
        org_id,
        user_id,
        resource: resource.to_string(),
        action: action.to_string(),
        legacy,
        new,
    };
    let Ok(mut m) = PENDING.lock() else { return };
    if let Some(s) = m.get_mut(&key) {
        s.hits += 1;
        s.last = now;
    } else if m.len() < PENDING_CAP {
        m.insert(key, Seen { hits: 1, first: now, last: now });
    }
}

/// Write the pending disagreements (adding to today's rows). Returns how many
/// distinct rows were written. On a failure the batch is put back, so a hit is
/// never lost to a database blip.
pub async fn flush(pool: &sqlx::PgPool) -> Result<usize, sqlx::Error> {
    let batch: Vec<(Key, Seen)> = match PENDING.lock() {
        Ok(mut m) => m.drain().collect(),
        Err(_) => return Ok(0),
    };
    if batch.is_empty() {
        return Ok(0);
    }
    let mut tx = pool.begin().await?;
    let mut failed = None;
    for (k, s) in &batch {
        if let Err(e) = sqlx::query!(
            "INSERT INTO authz_shadow_disagreements
                 (day, org_id, user_id, resource, action, legacy, new, hits, first_seen, last_seen)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (day, user_id, resource, action, legacy, new) DO UPDATE
                SET hits = authz_shadow_disagreements.hits + EXCLUDED.hits,
                    last_seen = GREATEST(authz_shadow_disagreements.last_seen, EXCLUDED.last_seen),
                    org_id = COALESCE(authz_shadow_disagreements.org_id, EXCLUDED.org_id)",
            k.day,
            k.org_id,
            k.user_id,
            k.resource,
            k.action,
            k.legacy,
            k.new,
            s.hits,
            s.first,
            s.last,
        )
        .execute(&mut *tx)
        .await
        {
            failed = Some(e);
            break;
        }
    }
    let result = match failed {
        None => tx.commit().await,
        Some(e) => Err(e),
    };
    if let Err(e) = result {
        if let Ok(mut m) = PENDING.lock() {
            for (k, s) in batch {
                let row = m.entry(k).or_insert(Seen { hits: 0, ..s });
                row.hits += s.hits;
                row.first = row.first.min(s.first);
                row.last = row.last.max(s.last);
            }
        }
        return Err(e);
    }
    Ok(batch.len())
}

/// Flush the counter once a minute, for the life of the process.
pub fn spawn_flusher(pool: sqlx::PgPool) {
    crate::db::spawn(async move {
        let mut tick = tokio::time::interval(FLUSH_EVERY);
        loop {
            tick.tick().await;
            if let Err(e) = flush(&pool).await {
                tracing::warn!(target: "madar.authz.shadow", error = %e, "disagreement counter flush failed");
            }
        }
    });
}

/// The new model's answer for a legacy cell.
pub async fn new_decision(
    conn: &mut PgConnection,
    user_id: Uuid,
    resource: &str,
    action: &str,
) -> Result<Option<bool>, AppError> {
    let Some(loaded) = super::load::load(conn, user_id).await? else {
        return Ok(None);
    };
    let now = chrono::Utc::now().timestamp();
    let eff = loaded.resolve_at(Scope::Anywhere, now);
    Ok(Some(legacy::granted(&eff, resource, action)))
}

/// [`new_decision`] with the person's org, for the counter.
async fn new_decision_in_org(
    conn: &mut PgConnection,
    user_id: Uuid,
    resource: &str,
    action: &str,
) -> Result<Option<(bool, Option<Uuid>)>, AppError> {
    let Some(loaded) = super::load::load(conn, user_id).await? else {
        return Ok(None);
    };
    let now = chrono::Utc::now().timestamp();
    let eff = loaded.resolve_at(Scope::Anywhere, now);
    Ok(Some((legacy::granted(&eff, resource, action), loaded.org_id)))
}

/// The plan's refusal when the cell's capability is one the person's plan
/// removes (`crate::plan::refusal_for`).
async fn plan_denial(
    conn: &mut PgConnection,
    user_id: Uuid,
    resource: &str,
    action: &str,
) -> Option<AppError> {
    let cap = super::Cap::from_legacy(resource, action)?;
    let plan = super::load::load(conn, user_id).await.ok()??.plan;
    crate::plan::refusal_for(plan, cap)
}

fn deny(resource: &str, action: &str) -> AppError {
    AppError::Forbidden(format!("Permission denied: {action} {resource}"))
}

/// Serve a decision per [`MODE`], logging divergence.
pub async fn observe(
    conn: &mut PgConnection,
    user_id: Uuid,
    resource: &str,
    action: &str,
    legacy_result: Result<(), AppError>,
) -> Result<(), AppError> {
    observe_in(*MODE, conn, user_id, resource, action, legacy_result).await
}

pub async fn observe_in(
    mode: Mode,
    conn: &mut PgConnection,
    user_id: Uuid,
    resource: &str,
    action: &str,
    legacy_result: Result<(), AppError>,
) -> Result<(), AppError> {
    let legacy_ok = match &legacy_result {
        Ok(()) => true,
        Err(AppError::Forbidden(_)) => false,
        // A database error is not a decision.
        Err(_) => return legacy_result,
    };
    if mode == Mode::Legacy {
        return legacy_result;
    }
    let new = new_decision_in_org(conn, user_id, resource, action).await;
    let (new, org_id) = match new {
        Ok(Some((n, org))) => (Ok(Some(n)), org),
        Ok(None) => (Ok(None), None),
        Err(e) => (Err(e), None),
    };
    match (&new, mode) {
        (Ok(Some(n)), _) if *n != legacy_ok => {
            DIVERGENCES.fetch_add(1, Ordering::Relaxed);
            record(org_id, user_id, resource, action, legacy_ok, *n);
            tracing::warn!(
                target: "madar.authz.shadow",
                %user_id, resource, action, legacy = legacy_ok, new = *n,
                "permission models disagree"
            );
        }
        (Err(e), _) => {
            tracing::warn!(target: "madar.authz.shadow", %user_id, resource, action, error = %e, "new model failed");
        }
        _ => {}
    }
    match mode {
        Mode::Enforce => match new {
            Ok(Some(true)) => Ok(()),
            Ok(Some(false)) => Err(plan_denial(conn, user_id, resource, action)
                .await
                .unwrap_or_else(|| deny(resource, action))),
            // No user row (service tokens): keep today's answer.
            Ok(None) => legacy_result,
            Err(e) => Err(e),
        },
        _ => legacy_result,
    }
}

/// One person × cell where the models disagree.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Mismatch {
    pub user_id: Uuid,
    pub user_name: String,
    pub role: String,
    pub resource: &'static str,
    pub action: &'static str,
    pub legacy: bool,
    pub new: bool,
    /// Why the new model differs on purpose, when it does.
    pub explained: Option<&'static str>,
}

/// Compare the two models for every active person of every org, over every
/// legacy cell. The Phase 2 migration's verification and the rehearsal tool.
pub async fn compare_all(pool: &sqlx::PgPool) -> Result<Vec<Mismatch>, AppError> {
    let users: Vec<(Uuid, String, crate::models::UserRole)> = sqlx::query_as(
        "SELECT id, name, role FROM users
          WHERE org_id IS NOT NULL AND deleted_at IS NULL AND role <> 'super_admin'
            AND NOT is_guest_principal
          ORDER BY org_id, name",
    )
    .fetch_all(pool)
    .await?;
    let mut out = vec![];
    let mut conn = pool.acquire().await?;
    let now = chrono::Utc::now().timestamp();
    for (id, name, role) in users {
        let Some(loaded) = super::load::load(&mut conn, id).await? else {
            continue;
        };
        let eff = loaded.resolve_at(Scope::Anywhere, now);
        for (r, a) in crate::permissions::permission_cells() {
            let legacy = crate::permissions::checker::check_permission_for_legacy(
                &mut conn, id, &role, r, a,
            )
            .await?;
            let new = legacy::granted(&eff, r, a);
            if legacy != new {
                let cap = super::Cap::from_legacy(r, a);
                let explained = match cap {
                    Some(c) if new && super::is_core_for(c, eff.kinds) => {
                        Some("core capability: a legacy deny no longer removes it")
                    }
                    _ => None,
                };
                out.push(Mismatch {
                    user_id: id,
                    user_name: name.clone(),
                    role: format!("{role:?}"),
                    resource: r,
                    action: a,
                    legacy,
                    new,
                    explained,
                });
            }
        }
    }
    Ok(out)
}
