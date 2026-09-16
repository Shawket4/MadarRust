use crate::auth::jwt::Claims;
use crate::{errors::AppError, models::UserRole};

/// Ensure the caller is a super_admin
pub fn require_super_admin(claims: &Claims) -> Result<(), AppError> {
    if claims.role == UserRole::SuperAdmin {
        Ok(())
    } else {
        Err(AppError::Forbidden("Super admin access required".into()))
    }
}

/// Ensure the caller belongs to the org they're trying to manage
pub fn require_same_org(claims: &Claims, org_id: Option<uuid::Uuid>) -> Result<(), AppError> {
    if claims.role == UserRole::SuperAdmin {
        return Ok(()); // super_admin can access any org
    }
    match (claims.org_id(), org_id) {
        (Some(claims_org), Some(target_org)) if claims_org == target_org => Ok(()),
        _ => Err(AppError::Forbidden(
            "Access to this org is not allowed".into(),
        )),
    }
}
