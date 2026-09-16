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
//!
//! WHO MAY DO WHAT is decided in exactly one place: the permission tables
//! (`role_permissions` + the per-user `permissions` overrides), consulted through
//! `permissions::checker::check_permission_for` against the op's EMBEDDED actor.
//! Replay asks the table the same `(resource, action)` question the live route
//! asks (`ReplayOp::required_permissions`), so a grant made in the dashboard works
//! offline and a revocation stops a queued op the same as a live one. The only
//! thing replay decides on its own is ATTRIBUTION — whether the embedded actor is
//! a real, active member of the bearer's org who may sign in at a till, which
//! architecture E asks as the `pos.sign_in` capability at the branch.
//! There used to be a third answer, a hard-coded role → op table in the replay
//! path; it is gone, because three answers to one question is how a per-user
//! grant came to work online and be ignored offline.

pub mod handlers;
pub mod pull;
pub mod routes;

#[cfg(test)]
mod tests;

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
    /// Live staff who may act only on their OWN till.
    ///
    /// Architecture E replaced "is this a teller?" with a capability: anyone
    /// who can see the branch's tills (`till.read.branch`) may ring up on, or
    /// correct, another person's shift. `live()` starts restricted and
    /// [`ActingContext::scoped`] lifts it after the capability lookup, so a
    /// call site that forgets the lookup fails closed. Guests and replays are
    /// unrestricted, exactly as before.
    pub own_till_only: bool,
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
            own_till_only: true,
        })
    }

    /// A customer acting for themselves — the code on a table, not a member of
    /// staff and not a queued op.
    ///
    /// `replay: false` is the load-bearing part. It was briefly `true`, chosen
    /// for the one thing replay skips that a guest needs skipped (the
    /// ownership guards, which a guest can satisfy none of), and it quietly
    /// brought the OTHER thing replay means with it: that the prices on the
    /// request are history and should be recorded as sent. A customer's phone
    /// then got to name its own prices. The branch-open gate that `replay`
    /// also skips is re-checked by the caller, so nothing is lost by being
    /// honest about what this is.
    pub fn guest(user_id: Uuid, org_id: Uuid) -> Self {
        Self {
            teller_id: user_id,
            org_id,
            role: UserRole::Waiter,
            replay: false,
            own_till_only: false,
        }
    }

    /// A replay of a historical op, attributed to its embedded teller.
    pub fn replay(teller_id: Uuid, org_id: Uuid) -> Self {
        Self::replay_with_role(teller_id, org_id, UserRole::Teller)
    }

    /// A replay attributed to an embedded actor of a known role. Waiter ops
    /// (fire / round / void), teller ops (settle, orders, shifts) and a branch
    /// manager working the till all replay through this so the ownership/state
    /// guards keyed on `role` behave the same as the live action that
    /// originally produced the op.
    pub fn replay_with_role(teller_id: Uuid, org_id: Uuid, role: UserRole) -> Self {
        Self {
            teller_id,
            org_id,
            role,
            replay: true,
            own_till_only: false,
        }
    }

    /// Resolve the till scope from the actor's capabilities. Call this on every
    /// live action guarded by "your own till".
    pub async fn scoped(mut self, pool: &sqlx::PgPool) -> Result<Self, AppError> {
        if self.own_till_only
            && crate::authz::require::effective(pool, self.teller_id, None)
                .await?
                .can(crate::authz::Cap::TillReadBranch)
        {
            self.own_till_only = false;
        }
        Ok(self)
    }
}
