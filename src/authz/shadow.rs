//! Running the old and new permission models side by side.
//!
//! `MADAR_AUTHZ_MODE`:
//! - `shadow` (Phase 2 default): serve the LEGACY decision; compute the new one
//!   and log any divergence (`madar.authz.shadow`).
//! - `enforce` (Phase 3): serve the NEW decision; log divergence the same way.
//! - `legacy`: legacy only, no second computation (emergency switch).
//!
//! A failure to compute the new decision never fails a request in `shadow`.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::PgConnection;
use uuid::Uuid;

use super::{Scope, legacy, resolve};
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
        Ok("enforce") => Mode::Enforce,
        _ => Mode::Shadow,
    });

/// Divergences seen since start (exported for observability).
pub static DIVERGENCES: AtomicU64 = AtomicU64::new(0);

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
    let eff = resolve(&loaded.principal, Scope::Anywhere, now, &loaded.policy);
    Ok(Some(legacy::granted(&eff, resource, action)))
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
    let new = new_decision(conn, user_id, resource, action).await;
    match (&new, mode) {
        (Ok(Some(n)), _) if *n != legacy_ok => {
            DIVERGENCES.fetch_add(1, Ordering::Relaxed);
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
            Ok(Some(false)) => Err(deny(resource, action)),
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
        let eff = resolve(&loaded.principal, Scope::Anywhere, now, &loaded.policy);
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
