//! Dawam employees for the suites that exercise staff routes (Phase A: an
//! employee is its own entity, optionally linked to a Madar user).
//!
//! Every helper gives an employee a FRESH id, never the linked user's: a
//! handler that confuses a user id with an employee id fails these suites.

#![allow(dead_code)]

use actix_web::test::TestRequest;
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::JwtSecret;
use madar_rust::models::UserRole;

pub fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

/// A Madar user's session (dashboard or POS).
pub fn user_token(user: Uuid, org: Uuid, role: UserRole) -> String {
    madar_rust::auth::jwt::create_token(&secret(), user, Some(org), role, None, 24).unwrap()
}

/// Switch an org's modules (`pos`, `dawam`).
pub async fn set_modules(pool: &PgPool, org: Uuid, modules: &[&str]) {
    sqlx::query("UPDATE organizations SET modules = $2 WHERE id = $1")
        .bind(org)
        .bind(modules.iter().map(|m| m.to_string()).collect::<Vec<_>>())
        .execute(pool)
        .await
        .unwrap();
}

/// An employee of `org` at `branches`. `user` links them to a Madar account;
/// `phone` + `app` give them the staff app.
#[allow(clippy::too_many_arguments)]
pub async fn employee(
    pool: &PgPool,
    org: Uuid,
    name: &str,
    user: Option<Uuid>,
    phone: Option<&str>,
    app: bool,
    branches: &[Uuid],
    salary_piastres: i64,
) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO employees (org_id, user_id, name, phone, app_access, base_salary_piastres, \
             hire_date) VALUES ($1, $2, $3, $4, $5, $6, CURRENT_DATE - 365) RETURNING id",
    )
    .bind(org)
    .bind(user)
    .bind(name)
    .bind(phone)
    .bind(app)
    .bind(salary_piastres)
    .fetch_one(pool)
    .await
    .unwrap();
    for b in branches {
        sqlx::query(
            "INSERT INTO employee_branches (employee_id, branch_id, org_id) VALUES ($1, $2, $3)",
        )
        .bind(id)
        .bind(b)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    }
    id
}

/// A signed-in staff-app phone.
#[derive(Clone, Debug)]
pub struct Session {
    pub token: String,
    pub device: String,
    pub device_id: Uuid,
}

impl Session {
    /// Attach this session to a request: the staff token and the device.
    pub fn on(&self, req: TestRequest) -> TestRequest {
        req.insert_header(("Authorization", format!("Bearer {}", self.token)))
            .insert_header(("X-Staff-Device", self.device.clone()))
    }
}

/// Sign `employee` in on a new phone, as `POST /auth/staff/otp/verify` does,
/// without the WhatsApp round trip: one live device and a staff token.
pub async fn session(pool: &PgPool, employee: Uuid) -> Session {
    let (org, user): (Uuid, Option<Uuid>) =
        sqlx::query_as("SELECT org_id, user_id FROM employees WHERE id = $1")
            .bind(employee)
            .fetch_one(pool)
            .await
            .unwrap();
    sqlx::query(
        "UPDATE staff_devices SET revoked_at = now() WHERE employee_id = $1 AND revoked_at IS NULL",
    )
    .bind(employee)
    .execute(pool)
    .await
    .unwrap();
    let device = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let device_id: Uuid = sqlx::query_scalar(
        "INSERT INTO staff_devices (org_id, employee_id, token_hash, platform, model) \
         VALUES ($1, $2, $3, 'test', 'Test phone') RETURNING id",
    )
    .bind(org)
    .bind(employee)
    .bind(madar_rust::staff::principal::hash_device_token(&device))
    .fetch_one(pool)
    .await
    .unwrap();
    let (token, _) =
        madar_rust::staff::principal::mint(&secret(), employee, org, user, device_id).unwrap();
    Session {
        token,
        device,
        device_id,
    }
}

/// Give an employee the app (a number and access) for a sign-in test.
pub async fn give_app(pool: &PgPool, employee: Uuid, phone: &str) {
    sqlx::query("UPDATE employees SET phone = $2, app_access = true WHERE id = $1")
        .bind(employee)
        .bind(phone)
        .execute(pool)
        .await
        .unwrap();
}

/// A staff-app phone as one string, `token|device`, for suites whose request
/// macros take a single token (see [`authed`]).
pub async fn phone_token(pool: &PgPool, employee: Uuid) -> String {
    let s = session(pool, employee).await;
    format!("{}|{}", s.token, s.device)
}

/// Authenticate a request with a user's JWT, or with a phone's
/// `token|device` (the staff token plus `X-Staff-Device`).
pub fn authed(req: TestRequest, token: &str) -> TestRequest {
    match token.split_once('|') {
        Some((t, device)) => req
            .insert_header(("Authorization", format!("Bearer {t}")))
            .insert_header(("X-Staff-Device", device.to_string())),
        None => req.insert_header(("Authorization", format!("Bearer {token}"))),
    }
}

/// `.auth(&token)` on a test request: [`authed`] as a method.
pub trait Auth {
    fn auth(self, token: &str) -> TestRequest;
}

impl Auth for TestRequest {
    fn auth(self, token: &str) -> TestRequest {
        authed(self, token)
    }
}
