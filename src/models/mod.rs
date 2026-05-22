use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Type;
use uuid::Uuid;

// ── Enums ────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[sqlx(type_name = "user_role", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum UserRole {
    SuperAdmin,
    OrgAdmin,
    BranchManager,
    Teller,
}

// ── User ─────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
    pub id:            Uuid,
    pub org_id:        Option<Uuid>,
    pub name:          String,
    pub email:         Option<String>,
    pub phone:         Option<String>,
    pub password_hash: Option<String>,
    pub pin_hash:      Option<String>,
    pub role:          UserRole,
    pub is_active:     bool,
    pub last_login_at: Option<DateTime<Utc>>,
    pub created_at:    DateTime<Utc>,
    pub updated_at:    DateTime<Utc>,
    pub deleted_at:    Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UserPublic {
    pub id:        Uuid,
    pub org_id:    Option<Uuid>,
    pub branch_id: Option<Uuid>,
    pub name:      String,
    pub email:     Option<String>,
    pub phone:     Option<String>,
    pub role:      UserRole,
    pub is_active: bool,
}

impl From<User> for UserPublic {
    fn from(u: User) -> Self {
        Self {
            id:        u.id,
            org_id:    u.org_id,
            branch_id: None,
            name:      u.name,
            email:     u.email,
            phone:     u.phone,
            role:      u.role,
            is_active: u.is_active,
        }
    }
}

// ── Discount ──────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct Discount {
    pub id:         Uuid,
    pub org_id:     Uuid,
    pub name:       String,
    #[serde(rename = "type")]
    pub dtype:      String,   // "percentage" | "fixed"
    pub value:      i32,
    pub is_active:  bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
