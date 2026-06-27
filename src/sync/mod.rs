//! Offline → online **replay** for the POS.
//!
//! A till is a SHARED, often-OFFLINE device. A teller can open a shift online,
//! lose the network, then close/open/sell across several shifts and tellers — all
//! queued locally. When connectivity returns, that backlog must flush regardless
//! of WHICH teller happens to be signed in (or none, with a device token).
//!
//! The live endpoints attribute every write to the JWT principal and guard it
//! against the caller's own shift/drawer (a teller can't post to another teller's
//! shift). That is correct for a fresh action but wrong for a REPLAY: the device
//! syncing the backlog may be a different teller than the one who rang the sale.
//!
//! `POST /sync/replay` solves this. Each queued op carries its ORIGINAL
//! `teller_id`; the endpoint authorizes the bearer as a member of the op's org,
//! then dispatches to the SAME handler logic the live route uses — but through an
//! [`ActingContext`] in `replay` mode, which (a) attributes the write to the
//! embedded teller, not the bearer, and (b) bypasses the teller-ownership /
//! one-open-per-branch / cash-continuity guards (it's recorded history, not a new
//! action). Structural integrity — FKs, unique indexes, idempotency early-returns,
//! org scoping, and the shift-must-be-open guard for orders — still holds.

pub mod handlers;
pub mod routes;

use uuid::Uuid;

use crate::auth::jwt::Claims;
use crate::errors::AppError;
use crate::models::UserRole;

/// Who a write is attributed to, and whether the live ownership/state guards
/// apply. The live route builds this from the caller's JWT; replay builds it from
/// each queued op's embedded `teller_id`.
#[derive(Clone)]
pub struct ActingContext {
    /// The user the write is attributed to (`teller_id` / `moved_by` /
    /// `voided_by` / `created_by`).
    pub teller_id: Uuid,
    /// The org the write is scoped to — every catalog/shift lookup uses this.
    pub org_id: Uuid,
    /// The actor's role; the ownership guards only apply to tellers.
    pub role: UserRole,
    /// `true` when replaying a historical queued op: ownership / drawer-owner /
    /// one-open-per-branch precheck / cash-continuity guards are skipped.
    pub replay: bool,
}

impl ActingContext {
    /// A live action by the JWT principal. Errs if the token carries no org (a
    /// super admin never transacts on the POS).
    pub fn live(claims: &Claims) -> Result<Self, AppError> {
        Ok(Self {
            teller_id: claims.user_id(),
            org_id: claims
                .org_id()
                .ok_or_else(|| AppError::BadRequest("Token has no organization".into()))?,
            role: claims.role.clone(),
            replay: false,
        })
    }

    /// A replay of a historical op, attributed to its embedded teller.
    pub fn replay(teller_id: Uuid, org_id: Uuid) -> Self {
        Self::replay_with_role(teller_id, org_id, UserRole::Teller)
    }

    /// A replay attributed to an embedded actor of a known role. Waiter ops
    /// (fire / round / void) and teller ops (settle, orders, shifts) replay
    /// through this so the ownership/state guards keyed on `role` behave the
    /// same as the live action that originally produced the op.
    pub fn replay_with_role(teller_id: Uuid, org_id: Uuid, role: UserRole) -> Self {
        Self { teller_id, org_id, role, replay: true }
    }
}
