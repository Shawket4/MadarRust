use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::models::UserRole;

pub struct JwtSecret(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub:       String,
    pub org_id:    Option<String>, // None for super_admin
    pub role:      UserRole,
    pub branch_id: Option<String>,
    pub exp:       usize,
    pub iat:       usize,
}

impl Claims {
    pub fn user_id(&self) -> Uuid {
        Uuid::parse_str(&self.sub).expect("JWT sub field is not a valid UUID — token was issued incorrectly")
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
    pub fn branch_id(&self) -> Option<Uuid> {
        self.branch_id.as_deref().and_then(|s| Uuid::parse_str(s).ok())
    }
}

pub fn create_token(
    secret:    &JwtSecret,
    user_id:   Uuid,
    org_id:    Option<Uuid>,
    role:      UserRole,
    branch_id: Option<Uuid>,
    hours:     i64,
) -> Result<String, jsonwebtoken::errors::Error> {
    let now = Utc::now();
    let claims = Claims {
        sub:       user_id.to_string(),
        org_id:    org_id.map(|o| o.to_string()),
        role,
        branch_id: branch_id.map(|b| b.to_string()),
        iat:       now.timestamp() as usize,
        exp:       (now + Duration::hours(hours)).timestamp() as usize,
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.0.as_bytes()),
    )
}

pub fn verify_token(secret: &JwtSecret, token: &str) -> Result<Claims, jsonwebtoken::errors::Error> {
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.0.as_bytes()),
        &Validation::default(),
    )?;
    Ok(data.claims)
}