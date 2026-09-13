//! The effective timezone — ONE answer to "what wall-clock does this branch
//! read": the branch's zone, else its org's, else [`DEFAULT_TZ`].
//!
//! Every date shown, printed or bucketed for a branch is that instant read in
//! this zone. It travels on order, shift and ticket payloads as `timezone`, so a
//! client formats a payload in the zone the server meant rather than a cached
//! guess. SQL reads it through `effective_timezone(branch_id)` (same rule).

use chrono_tz::Tz;
use sqlx::PgExecutor;
use uuid::Uuid;

use crate::errors::AppError;

/// Product-home default and the DB column default.
pub const DEFAULT_TZ: &str = "Africa/Cairo";

/// Parse an IANA name, falling back to [`DEFAULT_TZ`] — loudly, because a
/// branch shown in the wrong zone is otherwise invisible.
pub fn parse(name: &str) -> Tz {
    name.parse().unwrap_or_else(|_| {
        tracing::warn!(
            timezone = name,
            "unknown timezone; falling back to {DEFAULT_TZ}"
        );
        chrono_tz::Africa::Cairo
    })
}

/// The branch's effective IANA name. `NotFound` for an unknown branch.
pub async fn effective_tz_name<'e, E: PgExecutor<'e>>(
    exec: E,
    branch_id: Uuid,
) -> Result<String, AppError> {
    sqlx::query_scalar::<_, Option<String>>("SELECT effective_timezone($1)")
        .bind(branch_id)
        .fetch_one(exec)
        .await?
        .ok_or_else(|| AppError::NotFound("Branch not found".into()))
}

/// [`effective_tz_name`], parsed.
pub async fn effective_tz<'e, E: PgExecutor<'e>>(exec: E, branch_id: Uuid) -> Result<Tz, AppError> {
    Ok(parse(&effective_tz_name(exec, branch_id).await?))
}

/// The zone a report scope reads in: the branch's effective zone, or — for an
/// "all branches" scope (no such branch) — the org's, else the default.
pub async fn scope_tz_name<'e, E: PgExecutor<'e>>(
    exec: E,
    branch_id: Uuid,
    org_id: Uuid,
) -> Result<String, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE(effective_timezone($1),
                         (SELECT timezone::text FROM organizations WHERE id = $2),
                         $3)",
    )
    .bind(branch_id)
    .bind(org_id)
    .bind(DEFAULT_TZ)
    .fetch_one(exec)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    async fn a_branch_zone_overrides_its_orgs(pool: sqlx::PgPool) {
        let org: Uuid = sqlx::query_scalar(
            "INSERT INTO organizations (name, slug, timezone) VALUES ('Tz', $1, 'Europe/London') RETURNING id",
        )
        .bind(format!("tz-{}", Uuid::new_v4()))
        .fetch_one(&pool)
        .await
        .unwrap();
        let branch = |tz: Option<&'static str>| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, Uuid>(
                    "INSERT INTO branches (org_id, name, timezone) VALUES ($1, $3, $2::timezone_name) RETURNING id",
                )
                .bind(org)
                .bind(tz)
                .bind(tz.unwrap_or("inherits"))
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        let own = branch(Some("Africa/Cairo")).await;
        let inherits = branch(None).await;
        assert_eq!(effective_tz_name(&pool, own).await.unwrap(), "Africa/Cairo");
        assert_eq!(
            effective_tz_name(&pool, inherits).await.unwrap(),
            "Europe/London"
        );
        assert!(effective_tz_name(&pool, Uuid::new_v4()).await.is_err());
        assert_eq!(
            scope_tz_name(&pool, Uuid::nil(), org).await.unwrap(),
            "Europe/London"
        );
    }

    #[test]
    fn parses_or_falls_back_to_the_default() {
        assert_eq!(parse("Europe/Paris"), chrono_tz::Europe::Paris);
        assert_eq!(parse("Not/AZone"), chrono_tz::Africa::Cairo);
        assert_eq!(DEFAULT_TZ.parse::<Tz>().unwrap(), chrono_tz::Africa::Cairo);
    }
}
