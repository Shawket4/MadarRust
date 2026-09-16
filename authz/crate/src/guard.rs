//! Anti-escalation (PERMISSIONS_ARCHITECTURE §4.10). Every write that changes
//! who can do what passes one of these checks, on the server. They are pure:
//! the caller resolves the effective sets and passes them in.
//!
//! - G1 hold to grant: an allow needs the capability, and its limits no more
//!   generous than the actor's own.
//! - G2 hold to assign: assigning a role needs every grant of the role.
//! - G3 hold to edit a role: the role's grants after the edit are all held.
//! - G4 dominate the target: the target's effective set is within the actor's.
//! - G5 no self-edit of grants, assignments or owner status.
//! - G6 owners: only owners change owners; a protected capability is never
//!   denied to an owner; core grants are never removed.

use serde::{Deserialize, Serialize};

use crate::{is_core_for, Cap, CapSet, EffectiveSet, Kinds, Limits};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "rule", rename_all = "snake_case")]
pub enum GuardError {
    /// G5.
    SelfEdit,
    /// The actor lacks the authority for this kind of write.
    MissingAuthority { cap: String },
    /// G4: the target holds something the actor does not.
    NotDominant { caps: Vec<String> },
    /// G1/G2/G3: granting something the actor does not hold.
    NotHeld { cap: String },
    /// G1: limits more generous than the actor's own.
    LimitAbove { cap: String },
    /// Core grants are always on for the role kind.
    CoreRemoval { cap: String },
    /// G6.
    OwnerProtected,
}

fn key(c: Cap) -> String {
    c.key().to_string()
}

/// G4 + G5 + G6: may `actor` write anything about `target`?
pub fn may_touch(
    actor: &EffectiveSet,
    actor_id: &str,
    target: &EffectiveSet,
    target_id: &str,
) -> Result<(), GuardError> {
    if actor_id == target_id {
        return Err(GuardError::SelfEdit);
    }
    if target.owner && !actor.owner {
        return Err(GuardError::OwnerProtected);
    }
    let above = target.caps.minus(&actor.caps);
    if !above.is_empty() {
        return Err(GuardError::NotDominant {
            caps: above.iter().map(key).collect(),
        });
    }
    Ok(())
}

/// G1 + G6 + core: may `actor` set an override of `cap` on `target`?
pub fn may_set_override(
    actor: &EffectiveSet,
    actor_id: &str,
    target: &EffectiveSet,
    target_id: &str,
    target_kinds: Kinds,
    cap: Cap,
    allow: bool,
    limits: Option<&Limits>,
) -> Result<(), GuardError> {
    if !actor.can(Cap::StaffPermissionsEdit) {
        return Err(GuardError::MissingAuthority {
            cap: key(Cap::StaffPermissionsEdit),
        });
    }
    may_touch(actor, actor_id, target, target_id)?;
    if allow {
        if !actor.can(cap) {
            return Err(GuardError::NotHeld { cap: key(cap) });
        }
        let wanted = limits.copied().unwrap_or_default();
        if !wanted.within(&actor.limits_of(cap)) {
            return Err(GuardError::LimitAbove { cap: key(cap) });
        }
    } else {
        if target.owner && cap.meta().protected {
            return Err(GuardError::OwnerProtected);
        }
        if is_core_for(cap, target_kinds) {
            return Err(GuardError::CoreRemoval { cap: key(cap) });
        }
    }
    Ok(())
}

/// G2: may `actor` give `target` a role whose grants (with core) are `role_caps`?
pub fn may_assign(
    actor: &EffectiveSet,
    actor_id: &str,
    target: &EffectiveSet,
    target_id: &str,
    role_caps: &CapSet,
) -> Result<(), GuardError> {
    if !actor.can(Cap::StaffUsersEdit) {
        return Err(GuardError::MissingAuthority {
            cap: key(Cap::StaffUsersEdit),
        });
    }
    may_touch(actor, actor_id, target, target_id)?;
    held(actor, role_caps)
}

/// G3: may `actor` change a role of kind `kinds` from `before` to `after`?
pub fn may_edit_role(
    actor: &EffectiveSet,
    kinds: Kinds,
    before: &CapSet,
    after: &CapSet,
) -> Result<(), GuardError> {
    if !actor.can(Cap::StaffRolesManage) {
        return Err(GuardError::MissingAuthority {
            cap: key(Cap::StaffRolesManage),
        });
    }
    for c in before.minus(after).iter() {
        if is_core_for(c, kinds) {
            return Err(GuardError::CoreRemoval { cap: key(c) });
        }
    }
    // Only what is ADDED must be held: removing a grant you do not hold is how
    // an owner-granted role gets narrowed by a manager who has roles.manage.
    held(actor, &after.minus(before))
}

fn held(actor: &EffectiveSet, caps: &CapSet) -> Result<(), GuardError> {
    match caps.minus(&actor.caps).iter().next() {
        Some(c) => Err(GuardError::NotHeld { cap: key(c) }),
        None => Ok(()),
    }
}
