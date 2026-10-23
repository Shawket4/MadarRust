use chrono::{Duration, Utc};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::models::UserRole;

pub struct JwtSecret(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub org_id: Option<String>, // None for super_admin
    pub role: UserRole,
    pub branch_id: Option<String>,
    pub exp: usize,
    pub iat: usize,
    /// The server-side session this token belongs to (`auth_sessions.id`,
    /// see `auth::sessions`). Absent on tokens issued before sessions existed,
    /// which are adopted as a session on first use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
}

impl Claims {
    pub fn user_id(&self) -> Uuid {
        Uuid::parse_str(&self.sub)
            .expect("JWT sub field is not a valid UUID — token was issued incorrectly")
    }
    pub fn user_id_safe(&self) -> Result<Uuid, crate::errors::AppError> {
        Uuid::parse_str(&self.sub)
            .map_err(|_| crate::errors::AppError::Unauthorized("Invalid user ID in token".into()))
    }
    pub fn org_id(&self) -> Option<Uuid> {
        self.org_id.as_deref().and_then(|s| Uuid::parse_str(s).ok())
    }
    /// The branch this token was issued for. Tellers always carry one (the
    /// branch they signed into); other roles are not branch-bound (`None`).
    pub fn session_id(&self) -> Option<Uuid> {
        self.sid.as_deref().and_then(|s| Uuid::parse_str(s).ok())
    }
    pub fn branch_id(&self) -> Option<Uuid> {
        self.branch_id
            .as_deref()
            .and_then(|s| Uuid::parse_str(s).ok())
    }

    /// The org an "all branches" / org-wide scope rolls up over. Org-bound
    /// tokens carry it directly; a super admin's token does not, so callers pass
    /// the org the dashboard pinned via the `X-Org-Id` header — honoured ONLY
    /// for super admins (who may read any org anyway), ignored for every other
    /// role so the token's own org always wins.
    pub fn scope_org(&self, header_org: Option<Uuid>) -> Option<Uuid> {
        match self.org_id() {
            Some(org) => Some(org),
            None if self.role == UserRole::SuperAdmin => header_org,
            None => None,
        }
    }
}

pub fn create_token(
    secret: &JwtSecret,
    user_id: Uuid,
    org_id: Option<Uuid>,
    role: UserRole,
    branch_id: Option<Uuid>,
    hours: i64,
) -> Result<String, jsonwebtoken::errors::Error> {
    create_session_token(secret, user_id, org_id, role, branch_id, hours, None)
}

/// [`create_token`] naming its session. Every sign-in goes through
/// `auth::sessions::issue`, which inserts the row and calls this.
pub fn create_session_token(
    secret: &JwtSecret,
    user_id: Uuid,
    org_id: Option<Uuid>,
    role: UserRole,
    branch_id: Option<Uuid>,
    hours: i64,
    sid: Option<Uuid>,
) -> Result<String, jsonwebtoken::errors::Error> {
    let now = Utc::now();
    let claims = Claims {
        sub: user_id.to_string(),
        org_id: org_id.map(|o| o.to_string()),
        role,
        branch_id: branch_id.map(|b| b.to_string()),
        iat: now.timestamp() as usize,
        exp: (now + Duration::hours(hours)).timestamp() as usize,
        sid: sid.map(|s| s.to_string()),
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.0.as_bytes()),
    )
}

pub fn verify_token(
    secret: &JwtSecret,
    token: &str,
) -> Result<Claims, jsonwebtoken::errors::Error> {
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.0.as_bytes()),
        &Validation::default(),
    )?;
    Ok(data.claims)
}
