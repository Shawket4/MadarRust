//! Load a person's grants from the architecture E tables into the shared
//! crate's types. Cached per (user, org epoch): any grant change bumps the
//! epoch, so a stale entry is never served.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use crate::plan::Access;
use moka::future::Cache;
use sqlx::PgConnection;
use uuid::Uuid;

use super::{
    AssignmentDef, Cap, CapSet, Limits, OrgPolicy, OverrideDef, Principal, RoleDef, RoleKind,
};
use crate::errors::AppError;

/// Everything a decision about one person needs.
#[derive(Clone, Debug)]
pub struct Loaded {
    pub org_id: Option<Uuid>,
    pub epoch: i64,
    pub principal: Principal,
    pub policy: OrgPolicy,
    /// The org's plan and add-ons. Part of the epoch-keyed entry, so the entry
    /// never outlives a change: triggers on `organizations.plan` and
    /// `org_addons` bump the org's `authz_epoch` (20261013100000,
    /// 20261025100000).
    pub access: Access,
}

impl Loaded {
    /// The person's effective set at `scope`, cut to the plan's ceiling. THE
    /// way to resolve a person on the server: a bare `resolve` would let a Free
    /// org's role keep a paid capability.
    pub fn resolve_at(&self, scope: super::Scope<'_>, now: i64) -> super::EffectiveSet {
        self.access
            .apply(super::resolve(&self.principal, scope, now, &self.policy))
    }
}

static CACHE: LazyLock<Cache<(Uuid, i64), Arc<Loaded>>> = LazyLock::new(|| {
    Cache::builder()
        .max_capacity(20_000)
        .time_to_live(Duration::from_secs(600))
        .build()
});

fn limits_from(v: Option<serde_json::Value>) -> Option<Limits> {
    v.and_then(|j| serde_json::from_value::<Limits>(j).ok())
}

/// The org epoch for a user (0 when the org has none yet).
pub async fn epoch_of(conn: &mut PgConnection, user_id: Uuid) -> Result<i64, AppError> {
    Ok(org_epoch_of(conn, user_id).await?.map_or(0, |r| r.1))
}

/// A user's org and that org's epoch; `None` when there is no such user.
/// Remembered by the fast path (`fastpath::user_epoch`) until an authz
/// notification or a local write of the org says it may have moved.
async fn org_epoch_of(
    conn: &mut PgConnection,
    user_id: Uuid,
) -> Result<Option<(Option<Uuid>, i64)>, AppError> {
    let cap = crate::fastpath::capture(None, None);
    let row = sqlx::query!(
        r#"SELECT u.org_id,
                  COALESCE((SELECT e.epoch FROM authz_epoch e WHERE e.org_id = u.org_id), 0) AS "epoch!"
             FROM users u WHERE u.id = $1"#,
        user_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .map(|r| (r.org_id, r.epoch));
    if let (Some(cap), Some((org, epoch))) = (cap, row) {
        crate::fastpath::put_user_epoch(cap, user_id, org, epoch);
    }
    Ok(row)
}

/// A person's grants from memory alone: their org's epoch is known to the
/// fast path and the entry for it is cached. `None` = ask the database
/// ([`load`]); never a stale entry (the epoch is the cache key, and the fast
/// path forgets an epoch the moment it may have moved).
pub async fn cached(user_id: Uuid) -> Option<Arc<Loaded>> {
    if cfg!(test) {
        return None;
    }
    let (_, epoch) = crate::fastpath::user_epoch(user_id)?;
    CACHE.get(&(user_id, epoch)).await
}

/// Load (or reuse) a person's grants. `None` when there is no such user.
pub async fn load(conn: &mut PgConnection, user_id: Uuid) -> Result<Option<Arc<Loaded>>, AppError> {
    if let Some(hit) = cached(user_id).await {
        return Ok(Some(hit));
    }
    let epoch = epoch_of(conn, user_id).await?;
    if !cfg!(test)
        && let Some(hit) = CACHE.get(&(user_id, epoch)).await
    {
        return Ok(Some(hit));
    }
    let Some(loaded) = load_uncached(conn, user_id, epoch).await? else {
        return Ok(None);
    };
    let loaded = Arc::new(loaded);
    if !cfg!(test) {
        CACHE.insert((user_id, epoch), loaded.clone()).await;
    }
    Ok(Some(loaded))
}

async fn load_uncached(
    conn: &mut PgConnection,
    user_id: Uuid,
    epoch: i64,
) -> Result<Option<Loaded>, AppError> {
    #[allow(clippy::type_complexity)]
    let row: Option<(
        Option<Uuid>,
        bool,
        bool,
        Option<String>,
        Option<Vec<String>>,
    )> = sqlx::query_as(
        "SELECT u.org_id, (u.is_active AND u.deleted_at IS NULL), u.is_owner,
                (SELECT o.plan FROM organizations o WHERE o.id = u.org_id),
                CASE WHEN u.org_id IS NULL THEN NULL ELSE org_active_addons(u.org_id) END
           FROM users u WHERE u.id = $1",
    )
    .bind(user_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((org_id, active, is_owner, plan, addons)) = row else {
        return Ok(None);
    };

    #[allow(clippy::type_complexity)]
    let assignments: Vec<(Uuid, Uuid, String, bool, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT ra.id, r.id, r.kind::text, ra.all_branches,
                floor(extract(epoch FROM ra.valid_from))::bigint,
                floor(extract(epoch FROM ra.valid_to))::bigint
           FROM role_assignments ra
           JOIN org_roles r ON r.id = ra.org_role_id AND r.deleted_at IS NULL
          WHERE ra.user_id = $1 AND ra.revoked_at IS NULL",
    )
    .bind(user_id)
    .fetch_all(&mut *conn)
    .await?;

    let role_ids: Vec<Uuid> = assignments.iter().map(|a| a.1).collect();
    let assignment_ids: Vec<Uuid> = assignments.iter().map(|a| a.0).collect();

    let grants: Vec<(Uuid, i16, serde_json::Value)> = sqlx::query_as(
        "SELECT org_role_id, capability_id, limits FROM org_role_grants WHERE org_role_id = ANY($1)",
    )
    .bind(&role_ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut roles: HashMap<Uuid, (CapSet, BTreeMap<u16, Limits>)> = HashMap::new();
    for (role, cap, limits) in grants {
        let Some(c) = Cap::from_id(cap as u16) else {
            continue;
        };
        let e = roles.entry(role).or_default();
        e.0.insert(c);
        if let Some(l) = limits_from(Some(limits)).filter(|l| !l.is_unlimited()) {
            e.1.insert(c.id(), l);
        }
    }

    let branches: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT assignment_id, branch_id FROM role_assignment_branches WHERE assignment_id = ANY($1)",
    )
    .bind(&assignment_ids)
    .fetch_all(&mut *conn)
    .await?;

    type Overrides = Vec<(
        i16,
        String,
        Option<Uuid>,
        Option<serde_json::Value>,
        Option<i64>,
    )>;
    let overrides: Overrides = sqlx::query_as(
        "SELECT capability_id, effect, branch_id, limits,
                    floor(extract(epoch FROM valid_to))::bigint
               FROM user_overrides WHERE user_id = $1 AND revoked_at IS NULL",
    )
    .bind(user_id)
    .fetch_all(&mut *conn)
    .await?;

    let ask: Vec<i16> =
        match org_id {
            Some(org) => sqlx::query_scalar(
                "SELECT capability_id FROM org_capability_policy WHERE org_id = $1 AND ask_manager",
            )
            .bind(org)
            .fetch_all(&mut *conn)
            .await?,
            None => vec![],
        };

    let principal = Principal {
        user_id: user_id.to_string(),
        active,
        is_owner,
        assignments: assignments
            .into_iter()
            .filter_map(|(aid, rid, kind, all, from, to)| {
                let kind = RoleKind::parse(&kind)?;
                let (grants, limits) = roles.get(&rid).cloned().unwrap_or_default();
                Some(AssignmentDef {
                    role: RoleDef {
                        id: rid.to_string(),
                        kind,
                        grants,
                        limits,
                    },
                    all_branches: all,
                    branches: branches
                        .iter()
                        .filter(|(a, _)| *a == aid)
                        .map(|(_, b)| b.to_string())
                        .collect(),
                    valid_from: from,
                    valid_to: to,
                })
            })
            .collect(),
        overrides: overrides
            .into_iter()
            .map(|(cap, effect, branch, limits, valid_to)| OverrideDef {
                cap: cap as u16,
                allow: effect == "allow",
                branch: branch.map(|b| b.to_string()),
                limits: limits_from(limits),
                valid_to,
            })
            .collect(),
    };
    let policy = OrgPolicy {
        ask_manager: ask
            .into_iter()
            .filter_map(|c| Cap::from_id(c as u16))
            .collect(),
    };
    Ok(Some(Loaded {
        org_id,
        epoch,
        principal,
        policy,
        // No org (a super admin) or an unreadable plan is Paid: never lock out.
        access: Access::new(plan.as_deref(), &addons.unwrap_or_default()),
    }))
}
