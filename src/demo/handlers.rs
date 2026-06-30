//! `POST /demo/session` — provision a throwaway, isolated demo org + a scoped
//! JWT. No authentication; rate-limited and capacity-capped at the route layer
//! and here. The returned token is a normal org_admin JWT, so the rest of the
//! API treats the visitor exactly like a real tenant — confined to their own
//! demo org by the usual `require_same_org` checks.

use actix_web::{HttpResponse, web};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::{JwtSecret, create_token};
use crate::errors::AppError;
use crate::models::UserRole;

use super::config::DemoConfig;
use super::{seed, sweeper};

/// bcrypt-format placeholder. Demo users never authenticate by password — the
/// session endpoint returns their JWT directly — but `users` requires a login
/// secret (CHECK: password_hash OR pin_hash), so we store an unusable hash.
const DEMO_PASSWORD_HASH: &str = "$2b$12$demodemodemodemodemoduO0000000000000000000000000000000000";

#[derive(Deserialize)]
pub struct SessionQuery {
    /// `full` → seeded café; anything else (default) → empty org → onboarding.
    #[serde(default)]
    pub variant: Option<String>,
}

#[derive(Serialize)]
struct DemoUser {
    id: Uuid,
    name: String,
    email: Option<String>,
    phone: Option<String>,
    role: String,
    org_id: Uuid,
    branch_id: Option<Uuid>,
    is_active: bool,
}

#[derive(Serialize)]
struct SessionResponse {
    token: String,
    org_id: Uuid,
    expires_at: DateTime<Utc>,
    variant: String,
    user: DemoUser,
}

pub async fn create_session(
    pool: web::Data<PgPool>,
    secret: web::Data<JwtSecret>,
    cfg: web::Data<DemoConfig>,
    q: web::Query<SessionQuery>,
) -> Result<HttpResponse, AppError> {
    if !cfg.enabled {
        return Err(AppError::NotFound("Demo mode is not enabled".into()));
    }
    let full = matches!(q.variant.as_deref(), Some("full"));

    // Opportunistic GC, then enforce the live-org ceiling (abuse/cost guard).
    let _ = sweeper::gc_expired(pool.get_ref()).await;
    let live: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM organizations WHERE is_demo AND deleted_at IS NULL",
    )
    .fetch_one(pool.get_ref())
    .await?;
    if live >= cfg.max_orgs {
        return Err(AppError::ServiceUnavailable(
            "The demo is at capacity — please try again in a few minutes.".into(),
        ));
    }

    let org_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let expires_at = Utc::now() + Duration::hours(cfg.ttl_hours);
    let onboarding_at: Option<DateTime<Utc>> = if full { Some(Utc::now()) } else { None };
    let slug = format!("demo-{}", org_id.simple());
    let email = format!("demo-{}@madar.demo", &org_id.simple().to_string()[..8]);

    let mut tx = pool.begin().await?;

    sqlx::query(
        "INSERT INTO organizations \
         (id, name, slug, currency_code, tax_rate, timezone, is_active, is_demo, demo_expires_at, onboarding_completed_at) \
         VALUES ($1, $2, $3, 'EGP', 0.14, 'Africa/Cairo', true, true, $4, $5)",
    )
    .bind(org_id)
    .bind("Demo Café")
    .bind(&slug)
    .bind(expires_at)
    .bind(onboarding_at)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role, is_active) \
         VALUES ($1, $2, $3, $4, $5, 'org_admin'::user_role, true)",
    )
    .bind(user_id)
    .bind(org_id)
    .bind("Demo Owner")
    .bind(&email)
    .bind(DEMO_PASSWORD_HASH)
    .execute(&mut *tx)
    .await?;

    if full {
        seed::seed_full(&mut tx, org_id, user_id).await?;
    }
    // (seed_full borrows the tx connection; it returns before commit below)

    let token = create_token(
        secret.get_ref(),
        user_id,
        Some(org_id),
        UserRole::OrgAdmin,
        None,
        cfg.ttl_hours,
    )
    .map_err(|_| AppError::Internal)?;

    tx.commit().await?;

    Ok(HttpResponse::Ok().json(SessionResponse {
        token,
        org_id,
        expires_at,
        variant: if full { "full".into() } else { "empty".into() },
        user: DemoUser {
            id: user_id,
            name: "Demo Owner".into(),
            email: Some(email),
            phone: None,
            role: "org_admin".into(),
            org_id,
            branch_id: None,
            is_active: true,
        },
    }))
}
