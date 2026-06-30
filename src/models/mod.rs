use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Type;
use utoipa::ToSchema;
use uuid::Uuid;

// ── Enums ────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type, ToSchema)]
#[sqlx(type_name = "user_role", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum UserRole {
    SuperAdmin,
    OrgAdmin,
    BranchManager,
    Teller,
    /// Org-scoped, device-bound (PIN) like a teller, but holds NO shift/cash.
    /// Takes dine-in orders and fires them to the kitchen as open tickets.
    Waiter,
    /// Org-scoped, device-bound (PIN) for a Kitchen Display device. Reads the
    /// kitchen feed + bumps lines; holds NO shift/cash and CANNOT touch the POS
    /// (orders/payments) or settle tickets. The KDS device signs in as this.
    Kitchen,
}

// ── User ─────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
    pub id: Uuid,
    pub org_id: Option<Uuid>,
    pub name: String,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub password_hash: Option<String>,
    pub pin_hash: Option<String>,
    pub role: UserRole,
    pub is_active: bool,
    pub last_login_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UserPublic {
    #[schema(example = "550e8400-e29b-41d4-a716-446655440000")]
    pub id: Uuid,

    #[schema(example = "a20e8400-e29b-41d4-a716-446655440011")]
    pub org_id: Option<Uuid>,

    #[schema(example = "b30e8400-e29b-41d4-a716-446655440022")]
    pub branch_id: Option<Uuid>,

    #[schema(example = "Ahmad Ghazal")]
    pub name: String,

    #[schema(example = "ahmad@madar.com")]
    pub email: Option<String>,

    #[schema(example = "+201001234567")]
    pub phone: Option<String>,

    #[schema(example = "branch_manager")]
    pub role: UserRole,

    #[schema(example = true)]
    pub is_active: bool,
}

impl From<User> for UserPublic {
    fn from(u: User) -> Self {
        Self {
            id: u.id,
            org_id: u.org_id,
            branch_id: None,
            name: u.name,
            email: u.email,
            phone: u.phone,
            role: u.role,
            is_active: u.is_active,
        }
    }
}

// ── Discount ──────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Discount {
    #[schema(example = "770e8400-e29b-41d4-a716-446655440033")]
    pub id: Uuid,

    #[schema(example = "a20e8400-e29b-41d4-a716-446655440011")]
    pub org_id: Uuid,

    #[schema(example = "Summer Promo")]
    pub name: String,

    #[serde(rename = "type")]
    #[schema(example = "percentage")]
    pub dtype: String, // "percentage" | "fixed"

    #[schema(example = 15)]
    pub value: i32,

    #[schema(example = true)]
    pub is_active: bool,

    pub created_at: DateTime<Utc>,

    pub updated_at: DateTime<Utc>,
}
