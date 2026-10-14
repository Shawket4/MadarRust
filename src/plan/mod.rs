//! The Free plan on the server (FREEMIUM_SPEC_2026-10-11.md §4).
//!
//! What a plan allows is defined once, in madar-shared's `madar-plan`: the
//! feature table, the limits, the capability ceiling, the refusals and their
//! wording. This module only asks it, with the organization's stored plan
//! (`organizations.plan`), and enforces the answer at the places the server
//! decides things:
//!
//! - the capability ceiling, at the one authz chokepoint (`authz::load::Loaded::
//!   resolve_at`), so every `require`/`can`/legacy check and every till's signed
//!   snapshot is cut to it;
//! - branch and device limits ([`check_may_add_branch`], [`check_activate`]);
//! - the public pages ([`public_gate_org`], [`public_gate_branch`]): a Free shop
//!   has no such page, so the answer is 404, never 403;
//! - jobs ([`org_has_feature`] and the SQL fragments in [`FREE_ORG_SQL`]);
//! - the downgrade: one till per branch stays active ([`change_plan`]).
//!
//! The server keeps no plan rule of its own: a new rule is a change to
//! `authz/spec/plans.toml` in madar-shared.

pub mod handlers;
pub mod signup;

use chrono::{DateTime, Utc};
use madar_plan::{DeviceCounts, DeviceKind, Feature, Plan, Refusal};
use sqlx::{Executor, PgConnection, Postgres};
use uuid::Uuid;

use crate::errors::AppError;

// Jobs that sweep every org (loyalty winback, birthdays and wallet refresh,
// bookings, delivery, Dawam) skip Free orgs in their own SQL: `o.plan IS
// DISTINCT FROM 'free'`, the crate's reading ("free" is Free, anything else
// Paid) and the same one the paid-by-default column gives every current org.
// `tests/free_plan.rs` runs each job over a Free and a Paid org.

pub fn refuse(r: Refusal) -> AppError {
    AppError::PlanRefused(r)
}

/// When `cap` is refused because the plan removes it, the plan's own refusal
/// (403 `plan_feature` naming the feature, in both languages) instead of a bare
/// "no permission": the person can do nothing about the latter, and the
/// former says the feature exists and how to get it.
pub fn refusal_for(plan: Plan, cap: crate::authz::Cap) -> Option<AppError> {
    let f = madar_plan::Feature::all().find(|f| f.caps().contains(cap))?;
    madar_plan::require(plan, f)
        .err()
        .map(AppError::PlanRefused)
}

/// The organization's plan. A missing row, or a value this server does not
/// know, is Paid: never lock a paying shop out.
pub async fn plan_of<'e, E>(ex: E, org: Uuid) -> Result<Plan, AppError>
where
    E: Executor<'e, Database = Postgres>,
{
    let plan: Option<String> = sqlx::query_scalar("SELECT plan FROM organizations WHERE id = $1")
        .bind(org)
        .fetch_optional(ex)
        .await?;
    Ok(plan.as_deref().map_or(Plan::Paid, Plan::parse))
}

/// The org's plan as the wire says it (`free` | `paid`); `paid` for no org.
pub async fn plan_name<'e, E>(ex: E, org: Option<Uuid>) -> Result<String, AppError>
where
    E: Executor<'e, Database = Postgres>,
{
    Ok(match org {
        Some(o) => plan_of(ex, o).await?.as_str().to_string(),
        None => Plan::Paid.as_str().to_string(),
    })
}

pub async fn org_has_feature<'e, E>(ex: E, org: Uuid, f: Feature) -> Result<bool, AppError>
where
    E: Executor<'e, Database = Postgres>,
{
    Ok(madar_plan::has_feature(plan_of(ex, org).await?, f))
}

/// 403 `plan_feature` unless the org's plan has `f`.
pub async fn require_feature<'e, E>(ex: E, org: Uuid, f: Feature) -> Result<(), AppError>
where
    E: Executor<'e, Database = Postgres>,
{
    madar_plan::require(plan_of(ex, org).await?, f).map_err(refuse)
}

fn not_found() -> AppError {
    AppError::NotFound("Not found".into())
}

/// A public page of a shop whose plan lacks `f` does not exist: 404 (a Free
/// shop has no such page), not 403, so a guest learns nothing about the shop.
pub async fn public_gate_org<'e, E>(ex: E, org: Uuid, f: Feature) -> Result<(), AppError>
where
    E: Executor<'e, Database = Postgres>,
{
    if org_has_feature(ex, org, f).await? {
        Ok(())
    } else {
        Err(not_found())
    }
}

/// [`public_gate_org`] for a page named by a branch id. A branch that does not
/// exist falls through (the handler's own 404 says so).
pub async fn public_gate_branch<'e, E>(ex: E, branch: Uuid, f: Feature) -> Result<(), AppError>
where
    E: Executor<'e, Database = Postgres>,
{
    let plan: Option<String> = sqlx::query_scalar(
        "SELECT o.plan FROM branches b JOIN organizations o ON o.id = b.org_id WHERE b.id = $1",
    )
    .bind(branch)
    .fetch_optional(ex)
    .await?;
    match plan.as_deref().map(Plan::parse) {
        Some(p) if !madar_plan::has_feature(p, f) => Err(not_found()),
        _ => Ok(()),
    }
}

// ── Limits ──────────────────────────────────────────────────────────────────

/// A `devices.kind` / slot kind as the plan crate names it.
pub fn device_kind(kind: &str) -> Option<DeviceKind> {
    match kind {
        "pos" => Some(DeviceKind::Pos),
        // A branch-plan slot calls the kitchen screen `kitchen`.
        "kds" | "kitchen" => Some(DeviceKind::Kds),
        "waiter" => Some(DeviceKind::Waiter),
        _ => None,
    }
}

/// The devices ACTIVE in a branch: live (not retired) and not switched off by
/// the plan. `except` leaves out the device being (re)activated, so activating
/// the same install twice never counts it against itself.
pub async fn device_counts(
    conn: &mut PgConnection,
    branch: Uuid,
    except: Option<Uuid>,
) -> Result<DeviceCounts, AppError> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT kind, count(*) FROM devices
          WHERE branch_id = $1 AND retired_at IS NULL AND NOT plan_inactive
            AND ($2::uuid IS NULL OR id <> $2)
          GROUP BY kind",
    )
    .bind(branch)
    .bind(except)
    .fetch_all(&mut *conn)
    .await?;
    let mut c = DeviceCounts::default();
    for (kind, n) in rows {
        let n = n as u32;
        match kind.as_str() {
            "pos" => c.pos = n,
            "kds" => c.kds = n,
            "waiter" => c.waiter = n,
            _ => {}
        }
    }
    Ok(c)
}

/// May one more device of `kind` be activated in `branch`? Locks the branch row
/// so two tablets activating together cannot both take the last place; call it
/// inside the transaction that then writes the device.
pub async fn check_activate(
    conn: &mut PgConnection,
    org: Uuid,
    branch: Uuid,
    kind: &str,
    except: Option<Uuid>,
) -> Result<(), AppError> {
    let plan = plan_of(&mut *conn, org).await?;
    let Some(kind) = device_kind(kind) else {
        return Ok(());
    };
    // A plan with no device limits (Paid) skips the lock and the count.
    let limits = madar_plan::entitlements(plan).limits;
    if limits.pos_devices_per_branch.is_none()
        && limits.kds_devices.is_none()
        && limits.waiter_devices.is_none()
    {
        return Ok(());
    }
    sqlx::query("SELECT 1 FROM branches WHERE id = $1 FOR UPDATE")
        .bind(branch)
        .fetch_optional(&mut *conn)
        .await?;
    // A kind with no places at all (Free: kitchen screens `plan_feature`,
    // waiter devices `plan_device_limit`) is refused even for an install that
    // already exists: at zero active devices only a limit of 0 refuses.
    madar_plan::may_activate(plan, kind, DeviceCounts::default()).map_err(refuse)?;
    // An install that is already a live device of this kind in this branch is
    // not ADDING one: activating it again (a new code after a lost credential,
    // registering on every start) is never refused, whichever of the branch's
    // tills is the active one.
    if let Some(device) = except {
        let present: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM devices
                            WHERE id = $1 AND branch_id = $2 AND kind = $3 AND retired_at IS NULL)",
        )
        .bind(device)
        .bind(branch)
        .bind(match kind {
            DeviceKind::Pos => "pos",
            DeviceKind::Kds => "kds",
            DeviceKind::Waiter => "waiter",
        })
        .fetch_one(&mut *conn)
        .await?;
        if present {
            return Ok(());
        }
    }
    let counts = device_counts(conn, branch, except).await?;
    madar_plan::may_activate(plan, kind, counts).map_err(refuse)
}

/// May the org add a branch? Counts the live selling branches; a warehouse is
/// stock, which is the `inventory` feature.
pub async fn check_may_add_branch(
    conn: &mut PgConnection,
    org: Uuid,
    warehouse: bool,
) -> Result<(), AppError> {
    let plan = plan_of(&mut *conn, org).await?;
    if warehouse {
        return madar_plan::require(plan, Feature::Inventory).map_err(refuse);
    }
    // Serialize two creates racing for the last place.
    sqlx::query("SELECT 1 FROM organizations WHERE id = $1 FOR UPDATE")
        .bind(org)
        .fetch_optional(&mut *conn)
        .await?;
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM branches WHERE org_id = $1 AND deleted_at IS NULL AND kind = 'branch'",
    )
    .bind(org)
    .fetch_one(&mut *conn)
    .await?;
    madar_plan::may_add_branch(plan, live.max(0) as u32).map_err(refuse)
}

// ── Inactive tills (a downgraded org) ───────────────────────────────────────

/// A live (non-replay) sale or till session from a till the plan switched off.
/// `device` is the install the request names (`X-Madar-Device` or the body).
pub async fn guard_till_live<'e, E>(ex: E, device: Option<Uuid>) -> Result<(), AppError>
where
    E: Executor<'e, Database = Postgres>,
{
    let Some(device) = device else {
        return Ok(());
    };
    let inactive: Option<bool> =
        sqlx::query_scalar("SELECT plan_inactive FROM devices WHERE id = $1")
            .bind(device)
            .fetch_optional(ex)
            .await?;
    if inactive == Some(true) {
        Err(refuse(Refusal::new(madar_plan::Code::TillInactive, None)))
    } else {
        Ok(())
    }
}

/// What a replayed op from a till does, given the till's plan state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayVerdict {
    /// Accept. Nothing to say: the till is active, or the op is older than
    /// the flag.
    Accept,
    /// Accept, and tell the owner (`authz_replay_flags`): the till was switched
    /// off when this was rung, but had not been told yet, and the money has
    /// changed hands.
    AcceptFlagged,
    /// Refuse (`plan_till_inactive`): the till had been told, and started NEW
    /// selling anyway (an old build that never shows the blocker, or a client
    /// that was tampered with).
    Refuse,
}

/// The REPLAY rule for a till switched off by the plan. A queued sale is
/// money already taken, and refusing it loses the record while the cash stays
/// in the drawer, so the rule is built to lose nothing a till could not have
/// known:
///
/// - the till is active, or the op is stamped BEFORE the flag was set: accept;
/// - the till has not been told yet (`plan_inactive_seen_at` is unset: it has
///   not pulled since the flag): it could not have known, accept and flag;
/// - stamped after the till was told: refuse, but ONLY ops that start new
///   selling (`starts_selling`: a till session, a sale, a table bill). Closing
///   the drawer, a cash movement, a refund, a void, a stock note are never
///   refused: they settle what already happened.
pub async fn replay_verdict(
    conn: &mut PgConnection,
    device: Option<Uuid>,
    occurred_at: DateTime<Utc>,
    starts_selling: bool,
) -> Result<ReplayVerdict, AppError> {
    let Some(device) = device else {
        return Ok(ReplayVerdict::Accept);
    };
    let row: Option<(bool, Option<DateTime<Utc>>, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT plan_inactive, plan_inactive_at, plan_inactive_seen_at FROM devices WHERE id = $1",
    )
    .bind(device)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((true, Some(flagged_at), seen_at)) = row else {
        return Ok(ReplayVerdict::Accept);
    };
    if occurred_at < flagged_at {
        return Ok(ReplayVerdict::Accept);
    }
    match seen_at {
        Some(seen) if occurred_at >= seen && starts_selling => Ok(ReplayVerdict::Refuse),
        _ => Ok(ReplayVerdict::AcceptFlagged),
    }
}

/// The till has now been told it is inactive (its pull answered, its snapshot
/// was served): from here on a NEW sale stamped later is refused on replay.
pub async fn mark_till_told<'e, E>(ex: E, device: Uuid) -> Result<(), AppError>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query(
        "UPDATE devices SET plan_inactive_seen_at = now()
          WHERE id = $1 AND plan_inactive AND plan_inactive_seen_at IS NULL",
    )
    .bind(device)
    .execute(ex)
    .await?;
    Ok(())
}

// ── Changing a plan ─────────────────────────────────────────────────────────

/// Last time each POS till of the org sold or held a session (epoch ms), for
/// `madar_plan::downgrade_keeps`: the greater of its last sale, its last
/// session and its last request.
async fn till_uses(
    conn: &mut PgConnection,
    org: Uuid,
) -> Result<Vec<madar_plan::TillUse>, AppError> {
    let rows: Vec<(Uuid, Uuid, Option<i64>)> = sqlx::query_as(
        "SELECT d.branch_id, d.id,
                (extract(epoch FROM GREATEST(
                    d.last_seen_at,
                    (SELECT max(o.created_at) FROM orders o WHERE o.device_id = d.id),
                    (SELECT max(t.opened_at) FROM tills t WHERE t.device_id = d.id)
                )) * 1000)::bigint
           FROM devices d
          WHERE d.org_id = $1 AND d.kind = 'pos' AND d.retired_at IS NULL AND d.branch_id IS NOT NULL",
    )
    .bind(org)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(branch, device, last)| madar_plan::TillUse {
            branch_id: branch.to_string(),
            device_id: device.to_string(),
            last_used: last,
        })
        .collect())
}

/// What a plan change did to the org's tills.
#[derive(Debug, Default)]
pub struct PlanChange {
    pub from: Option<Plan>,
    pub to: Option<Plan>,
    /// Tills switched off by a downgrade.
    pub deactivated: Vec<Uuid>,
    /// Tills switched back on by an upgrade.
    pub reactivated: Vec<Uuid>,
}

/// Move the org to `to`, inside the caller's transaction.
///
/// - paid -> free: in each branch the most recently used till stays active
///   (`madar_plan::downgrade_keeps`); every other POS till is `plan_inactive`.
///   Nothing is deleted: branches, devices, data stay.
/// - free -> paid: every till is active again.
/// - same plan: nothing is touched (so the owner's choice of active till is
///   never reset by saving the same plan twice).
///
/// The plan column's trigger bumps the org's authz epoch (every cached
/// permission and every till's snapshot refreshes) and re-emits the branch
/// settings on the sync feed.
pub async fn change_plan(
    conn: &mut PgConnection,
    org: Uuid,
    to: Plan,
) -> Result<PlanChange, AppError> {
    let from = plan_of(&mut *conn, org).await?;
    let mut out = PlanChange {
        from: Some(from),
        to: Some(to),
        ..Default::default()
    };
    if from == to {
        return Ok(out);
    }
    let updated = sqlx::query("UPDATE organizations SET plan = $2 WHERE id = $1")
        .bind(org)
        .bind(to.as_str())
        .execute(&mut *conn)
        .await?
        .rows_affected();
    if updated != 1 {
        return Err(AppError::NotFound("Org not found".into()));
    }
    match to {
        Plan::Free => {
            let tills = till_uses(conn, org).await?;
            let keeps = madar_plan::downgrade_keeps(&tills, &[]);
            let keep_ids: Vec<Uuid> = keeps
                .iter()
                .filter_map(|k| Uuid::parse_str(&k.device_id).ok())
                .collect();
            out.deactivated = sqlx::query_scalar(
                "UPDATE devices SET plan_inactive = true, plan_inactive_at = now(),
                        plan_inactive_seen_at = NULL
                  WHERE org_id = $1 AND kind = 'pos' AND retired_at IS NULL
                    AND branch_id IS NOT NULL AND NOT plan_inactive AND NOT (id = ANY($2))
              RETURNING id",
            )
            .bind(org)
            .bind(&keep_ids)
            .fetch_all(&mut *conn)
            .await?;
        }
        Plan::Paid => {
            out.reactivated = sqlx::query_scalar(
                "UPDATE devices SET plan_inactive = false, plan_inactive_at = NULL,
                        plan_inactive_seen_at = NULL
                  WHERE org_id = $1 AND plan_inactive
              RETURNING id",
            )
            .bind(org)
            .fetch_all(&mut *conn)
            .await?;
        }
    }
    Ok(out)
}

/// The owner's switch of a branch's active till (Free plan, downgraded org):
/// `device` becomes active, every other POS till of the branch is switched off.
pub async fn set_active_till(
    conn: &mut PgConnection,
    org: Uuid,
    branch: Uuid,
    device: Uuid,
) -> Result<(), AppError> {
    let plan = plan_of(&mut *conn, org).await?;
    if plan != Plan::Free {
        return Err(AppError::Refused {
            code: "PLAN_NOT_FREE",
            reason: "Only an organization on the Free plan has one active till per branch".into(),
        });
    }
    let ok: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM devices
                        WHERE id = $1 AND org_id = $2 AND branch_id = $3
                          AND kind = 'pos' AND retired_at IS NULL)",
    )
    .bind(device)
    .bind(org)
    .bind(branch)
    .fetch_one(&mut *conn)
    .await?;
    if !ok {
        return Err(AppError::NotFound("That till is not in this branch".into()));
    }
    sqlx::query(
        "UPDATE devices SET plan_inactive = false, plan_inactive_at = NULL,
                plan_inactive_seen_at = NULL
          WHERE id = $1 AND plan_inactive",
    )
    .bind(device)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE devices SET plan_inactive = true, plan_inactive_at = now(),
                plan_inactive_seen_at = NULL
          WHERE org_id = $1 AND branch_id = $2 AND kind = 'pos' AND retired_at IS NULL
            AND id <> $3 AND NOT plan_inactive",
    )
    .bind(org)
    .bind(branch)
    .bind(device)
    .execute(&mut *conn)
    .await?;
    // The set of who may sell changed: a till's snapshot and feed refresh.
    sqlx::query("SELECT authz_bump_epoch($1)")
        .bind(org)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_kinds_map_slots_and_devices() {
        assert_eq!(device_kind("pos"), Some(DeviceKind::Pos));
        assert_eq!(device_kind("kds"), Some(DeviceKind::Kds));
        assert_eq!(device_kind("kitchen"), Some(DeviceKind::Kds));
        assert_eq!(device_kind("waiter"), Some(DeviceKind::Waiter));
        assert_eq!(device_kind("printer"), None);
    }
}
