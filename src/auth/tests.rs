use actix_web::{test, App, web};
use sqlx::PgPool;
use uuid::Uuid;
use serde_json::json;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::auth::routes;
use crate::auth::handlers::{LoginResponse, MeResponse, AuthPermissionsResponse, ResolveBranchResponse};

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn generate_token(user_id: Uuid, org_id: Option<Uuid>, role: UserRole) -> String {
    crate::auth::jwt::create_token(&get_secret(), user_id, org_id, role, None, 24).unwrap()
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let org_id = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO organizations (id, name, slug) VALUES ($1, 'Test Org', $2)",
        org_id,
        format!("test-auth-org-{}", org_id)
    )
    .execute(pool)
    .await
    .unwrap();
    org_id
}

async fn seed_branch(pool: &PgPool, org_id: Uuid) -> Uuid {
    let branch_id = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO branches (id, org_id, name) VALUES ($1, $2, $3)",
        branch_id,
        org_id,
        format!("Branch {}", branch_id)
    )
    .execute(pool)
    .await
    .unwrap();
    branch_id
}

/// Seed a branch with GPS coordinates for geofence tests.
async fn seed_branch_with_geo(
    pool: &PgPool,
    org_id: Uuid,
    lat: f64,
    lng: f64,
    radius_m: i32,
) -> Uuid {
    let branch_id = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO branches (id, org_id, name, latitude, longitude, geo_radius_meters)
         VALUES ($1, $2, $3, $4, $5, $6)",
        branch_id,
        org_id,
        format!("GeoBranch {}", branch_id),
        lat,
        lng,
        radius_m,
    )
    .execute(pool)
    .await
    .unwrap();
    branch_id
}

async fn assign_teller_to_branch(pool: &PgPool, user_id: Uuid, branch_id: Uuid) {
    sqlx::query!(
        "INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)",
        user_id,
        branch_id
    )
    .execute(pool)
    .await
    .unwrap();
}

// ── Email / password login ────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn test_login_email_password_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let user_id = Uuid::new_v4();
    let hash = bcrypt::hash("password123", bcrypt::DEFAULT_COST).unwrap();

    sqlx::query!(
        "INSERT INTO users (id, org_id, name, role, email, password_hash)
         VALUES ($1, $2, 'Admin', 'org_admin'::user_role, 'admin@test.com', $3)",
        user_id, org_id, hash
    )
    .execute(&pool)
    .await
    .unwrap();

    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(&json!({ "email": "admin@test.com", "password": "password123" }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: LoginResponse = test::read_body_json(resp).await;
    assert_eq!(body.user.email.as_deref(), Some("admin@test.com"));
    assert!(!body.token.is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn test_login_email_wrong_password(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let user_id = Uuid::new_v4();
    let hash = bcrypt::hash("password123", bcrypt::DEFAULT_COST).unwrap();

    sqlx::query!(
        "INSERT INTO users (id, org_id, name, role, email, password_hash)
         VALUES ($1, $2, 'Admin', 'org_admin'::user_role, 'admin@test.com', $3)",
        user_id, org_id, hash
    )
    .execute(&pool)
    .await
    .unwrap();

    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(&json!({ "email": "admin@test.com", "password": "wrongpassword" }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_login_disabled_account(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let user_id = Uuid::new_v4();
    let hash = bcrypt::hash("password123", bcrypt::DEFAULT_COST).unwrap();

    sqlx::query!(
        "INSERT INTO users (id, org_id, name, role, email, password_hash, is_active)
         VALUES ($1, $2, 'Admin', 'org_admin'::user_role, 'dis@test.com', $3, false)",
        user_id, org_id, hash
    )
    .execute(&pool)
    .await
    .unwrap();

    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(&json!({ "email": "dis@test.com", "password": "password123" }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_login_missing_both_email_and_pin(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(&json!({ "org_id": Uuid::new_v4() }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);
}

// ── PIN login (branch-scoped) ─────────────────────────────────

/// Helper: seed org + branch + teller + branch assignment, return IDs.
async fn seed_pin_login_setup(pool: &PgPool, pin: &str) -> (Uuid, Uuid, Uuid) {
    let org_id = seed_org(pool).await;
    let branch_id = seed_branch(pool, org_id).await;
    let user_id = Uuid::new_v4();
    let hash = bcrypt::hash(pin, bcrypt::DEFAULT_COST).unwrap();
    sqlx::query!(
        "INSERT INTO users (id, org_id, name, role, pin_hash)
         VALUES ($1, $2, 'Teller One', 'teller'::user_role, $3)",
        user_id, org_id, hash
    )
    .execute(pool)
    .await
    .unwrap();
    assign_teller_to_branch(pool, user_id, branch_id).await;
    (org_id, branch_id, user_id)
}

#[sqlx::test(migrations = "./migrations")]
async fn test_login_pin_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let (_org_id, branch_id, _user_id) = seed_pin_login_setup(&pool, "1234").await;

    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(&json!({
            "name": "Teller One",
            "pin": "1234",
            "branch_id": branch_id
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "PIN login with correct credentials should succeed");

    let body: LoginResponse = test::read_body_json(resp).await;
    assert_eq!(body.user.name, "Teller One");
    assert!(!body.token.is_empty());
    assert_eq!(body.user.branch_id, Some(branch_id), "branch_id should be echoed in response");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_login_pin_wrong_pin(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let (_org_id, branch_id, _user_id) = seed_pin_login_setup(&pool, "1234").await;

    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(&json!({
            "name": "Teller One",
            "pin": "0000",
            "branch_id": branch_id
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_login_pin_missing_branch_id_returns_400(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(&json!({ "name": "Teller One", "pin": "1234" }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400, "PIN login without branch_id must return 400");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_login_pin_invalid_branch_returns_401(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    // branch_id that doesn't exist in DB
    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(&json!({
            "name": "Teller One",
            "pin": "1234",
            "branch_id": Uuid::new_v4()
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401, "Non-existent branch should return 401");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_login_pin_unassigned_org_teller_allowed(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    let branch_b = seed_branch(&pool, org_id).await;
    let user_id = Uuid::new_v4();
    let hash = bcrypt::hash("1234", bcrypt::DEFAULT_COST).unwrap();

    sqlx::query!(
        "INSERT INTO users (id, org_id, name, role, pin_hash)
         VALUES ($1, $2, 'Teller One', 'teller'::user_role, $3)",
        user_id, org_id, hash
    )
    .execute(&pool)
    .await
    .unwrap();
    // teller is assigned to branch_a but login attempts branch_b
    assign_teller_to_branch(&pool, user_id, branch_a).await;

    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(&json!({
            "name": "Teller One",
            "pin": "1234",
            "branch_id": branch_b
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    // D13: tellers are ORG-scoped — a valid teller in the branch's org may sign
    // in at ANY branch in that org, even one they're not explicitly assigned to.
    assert_eq!(resp.status(), 200, "org teller should be allowed at any org branch");
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert!(body["token"].as_str().map(|t| !t.is_empty()).unwrap_or(false),
        "successful login should return a token, got {body:?}");
}

// Layer 3: a successful PIN login silently derives + stores the teller's
// argon2id OFFLINE verifier (so the org bundle can later let them unlock offline).
#[sqlx::test(migrations = "./migrations")]
async fn test_pin_login_derives_offline_pin_hash(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch = seed_branch(&pool, org_id).await;
    let user_id = Uuid::new_v4();
    let hash = bcrypt::hash("1234", bcrypt::DEFAULT_COST).unwrap();
    sqlx::query!(
        "INSERT INTO users (id, org_id, name, role, pin_hash)
         VALUES ($1, $2, 'Teller One', 'teller'::user_role, $3)",
        user_id, org_id, hash
    )
    .execute(&pool)
    .await
    .unwrap();

    let before: Option<String> =
        sqlx::query_scalar("SELECT offline_pin_hash FROM users WHERE id = $1")
            .bind(user_id).fetch_one(&pool).await.unwrap();
    assert!(before.is_none(), "no offline hash before first login");

    let resp = test::call_service(&app, test::TestRequest::post()
        .uri("/auth/login")
        .set_json(&json!({ "name": "Teller One", "pin": "1234", "branch_id": branch }))
        .to_request()).await;
    assert!(resp.status().is_success(), "login should succeed, got {:?}", resp.status());

    let after: Option<String> =
        sqlx::query_scalar("SELECT offline_pin_hash FROM users WHERE id = $1")
            .bind(user_id).fetch_one(&pool).await.unwrap();
    let phc = after.expect("offline_pin_hash must be derived on PIN login");
    assert!(crate::auth::offline::verify_offline_pin("1234", &phc),
        "stored argon2id verifier must match the PIN");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_login_pin_cross_org_isolation(pool: PgPool) {
    // Org A teller cannot log in using Org B's branch
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_a = seed_org(&pool).await;
    let org_b_id = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org B', $2)",
        org_b_id,
        format!("org-b-{}", org_b_id)
    )
    .execute(&pool)
    .await
    .unwrap();

    let branch_a = seed_branch(&pool, org_a).await;
    let branch_b = seed_branch(&pool, org_b_id).await;

    let user_id = Uuid::new_v4();
    let hash = bcrypt::hash("1234", bcrypt::DEFAULT_COST).unwrap();
    sqlx::query!(
        "INSERT INTO users (id, org_id, name, role, pin_hash)
         VALUES ($1, $2, 'Teller One', 'teller'::user_role, $3)",
        user_id, org_a, hash
    )
    .execute(&pool)
    .await
    .unwrap();
    assign_teller_to_branch(&pool, user_id, branch_a).await;

    // Org B also has a teller with the SAME name (names are unique only per org)
    // but a different PIN and not assigned to branch_b — a deliberate collision.
    let user_b = Uuid::new_v4();
    let hash_b = bcrypt::hash("9999", bcrypt::DEFAULT_COST).unwrap();
    sqlx::query!(
        "INSERT INTO users (id, org_id, name, role, pin_hash)
         VALUES ($1, $2, 'Teller One', 'teller'::user_role, $3)",
        user_b, org_b_id, hash_b
    )
    .execute(&pool)
    .await
    .unwrap();

    // Org A's teller signing in at Org B's branch with Org A's PIN → the org-scoped
    // lookup only sees Org B's "Teller One" (PIN 9999), which 1234 doesn't match →
    // 401 invalid credentials. It must NOT be the "not assigned to this branch"
    // (403) message — that would leak that the credentials are valid somewhere.
    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(&json!({
            "name": "Teller One",
            "pin": "1234",
            "branch_id": branch_b
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401, "cross-org PIN login must return 401 (invalid credentials)");
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert!(!body["error"].as_str().unwrap_or("").to_lowercase().contains("branch"),
        "cross-org login must NOT leak the branch-access message, got {body:?}");
}

// ── GET /auth/me ──────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn test_me_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let user_id = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO users (id, org_id, name, role, email, password_hash)
         VALUES ($1, $2, 'Me User', 'org_admin'::user_role, 'me@test.com', 'h')",
        user_id, org_id
    )
    .execute(&pool)
    .await
    .unwrap();

    let token = generate_token(user_id, Some(org_id), UserRole::OrgAdmin);

    let req = test::TestRequest::get()
        .uri("/auth/me")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: MeResponse = test::read_body_json(resp).await;
    assert_eq!(body.user.name, "Me User");
}

/// A teller assigned to MULTIPLE branches must have /auth/me report the branch
/// their TOKEN is bound to — not an arbitrary `LIMIT 1` assignment. Otherwise the
/// POS adopts the wrong branch as `user.branchId` and every branch-scoped call
/// 403s on the token-branch binding check while /auth/me itself returns 200.
#[sqlx::test(migrations = "./migrations")]
async fn test_me_returns_token_branch_for_multi_branch_teller(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    let branch_b = seed_branch(&pool, org_id).await;
    let teller = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO users (id, org_id, name, role, email, password_hash)
         VALUES ($1, $2, 'Multi Teller', 'teller'::user_role, 'mt@test.com', 'h')",
        teller, org_id
    )
    .execute(&pool)
    .await
    .unwrap();
    // Assigned to BOTH branches; branch_a is inserted first, so a naive LIMIT-1
    // would tend to pick it — but the token is bound to branch_b.
    assign_teller_to_branch(&pool, teller, branch_a).await;
    assign_teller_to_branch(&pool, teller, branch_b).await;

    let token = crate::auth::jwt::create_token(
        &get_secret(), teller, Some(org_id), UserRole::Teller, Some(branch_b), 24,
    )
    .unwrap();

    let req = test::TestRequest::get()
        .uri("/auth/me")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body: MeResponse = test::read_body_json(resp).await;
    assert_eq!(
        body.user.branch_id,
        Some(branch_b),
        "/auth/me must report the token's branch, not an arbitrary assignment"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn test_me_no_token_returns_401(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let req = test::TestRequest::get().uri("/auth/me").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}

// ── GET /auth/permissions ─────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn test_permissions_super_admin_all_granted(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let user_id = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO users (id, name, role, email, password_hash)
         VALUES ($1, 'Super Admin', 'super_admin'::user_role, 'super@test.com', 'h')",
        user_id
    )
    .execute(&pool)
    .await
    .unwrap();

    let token = generate_token(user_id, None, UserRole::SuperAdmin);

    let req = test::TestRequest::get()
        .uri("/auth/permissions")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: AuthPermissionsResponse = test::read_body_json(resp).await;
    assert!(!body.permissions.is_empty());
    assert!(
        body.permissions.iter().all(|p| p.granted),
        "SuperAdmin should have all permissions granted"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn test_permissions_with_user_override(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let user_id = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO users (id, org_id, name, role, pin_hash)
         VALUES ($1, $2, 'Teller Perm', 'teller'::user_role, 'h')",
        user_id, org_id
    )
    .execute(&pool)
    .await
    .unwrap();

    // Role default: teller cannot create orgs
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted)
         VALUES ('teller'::user_role, 'orgs'::permission_resource, 'create'::permission_action, false)
         ON CONFLICT DO NOTHING",
    )
    .execute(&pool)
    .await
    .unwrap();

    // User-level override: grant create-orgs to this specific teller
    sqlx::query!(
        "INSERT INTO permissions (user_id, resource, action, granted)
         VALUES ($1, 'orgs'::permission_resource, 'create'::permission_action, true)",
        user_id
    )
    .execute(&pool)
    .await
    .unwrap();

    let token = generate_token(user_id, Some(org_id), UserRole::Teller);

    let req = test::TestRequest::get()
        .uri("/auth/permissions")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: AuthPermissionsResponse = test::read_body_json(resp).await;
    let perm = body
        .permissions
        .iter()
        .find(|p| p.resource == "orgs" && p.action == "create")
        .expect("orgs:create permission must be in the list");
    assert!(perm.granted, "User-level override should make orgs:create granted");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_permissions_no_token_returns_401(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let req = test::TestRequest::get().uri("/auth/permissions").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}

// ── POST /auth/resolve-branch ─────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn test_resolve_branch_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    // Cairo: 30.0444° N, 31.2357° E — 200 m radius
    let branch_id = seed_branch_with_geo(&pool, org_id, 30.0444, 31.2357, 200).await;

    // Request from effectively the same point (< 1 m away)
    let req = test::TestRequest::post()
        .uri("/auth/resolve-branch")
        .set_json(&json!({
            "org_id": org_id,
            "latitude": 30.0444,
            "longitude": 31.2357
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: ResolveBranchResponse = test::read_body_json(resp).await;
    assert_eq!(body.branch_id, branch_id);
    assert!(body.distance_meters < 1.0, "distance should be near 0");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_resolve_branch_picks_nearest(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    // Two branches; the device is very close to branch_near
    let branch_near = seed_branch_with_geo(&pool, org_id, 30.0444, 31.2357, 500).await;
    // ~22 km away — still within a 25 000 m radius but farther
    let _branch_far = seed_branch_with_geo(&pool, org_id, 30.2444, 31.2357, 25_000).await;

    let req = test::TestRequest::post()
        .uri("/auth/resolve-branch")
        .set_json(&json!({
            "org_id": org_id,
            "latitude": 30.0444,
            "longitude": 31.2357
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: ResolveBranchResponse = test::read_body_json(resp).await;
    assert_eq!(body.branch_id, branch_near, "nearest branch should win");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_resolve_branch_outside_radius_returns_404(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    // Branch in Cairo with 200 m radius
    seed_branch_with_geo(&pool, org_id, 30.0444, 31.2357, 200).await;

    // Device is ~22 km away (Alexandria direction) — outside 200 m radius
    let req = test::TestRequest::post()
        .uri("/auth/resolve-branch")
        .set_json(&json!({
            "org_id": org_id,
            "latitude": 30.2444,
            "longitude": 31.2357
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404, "device outside branch radius should return 404");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_resolve_branch_no_geo_branches_returns_404(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    // Branch exists but has no lat/lng configured
    seed_branch(&pool, org_id).await;

    let req = test::TestRequest::post()
        .uri("/auth/resolve-branch")
        .set_json(&json!({
            "org_id": org_id,
            "latitude": 30.0444,
            "longitude": 31.2357
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404, "branch without geo coordinates should not match");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_resolve_branch_wrong_org_returns_404(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    // Branch belongs to org_id, but request uses a different org
    seed_branch_with_geo(&pool, org_id, 30.0444, 31.2357, 200).await;

    let req = test::TestRequest::post()
        .uri("/auth/resolve-branch")
        .set_json(&json!({
            "org_id": Uuid::new_v4(),
            "latitude": 30.0444,
            "longitude": 31.2357
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_pin_login_same_branch_allowed_different_branch_blocked(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    // Teller assigned to two branches, with an OPEN shift at branch A.
    let org_id = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    let branch_b = seed_branch(&pool, org_id).await;
    let user_id = Uuid::new_v4();
    let hash = bcrypt::hash("1234", bcrypt::DEFAULT_COST).unwrap();
    sqlx::query("INSERT INTO users (id, org_id, name, role, pin_hash) VALUES ($1,$2,'Teller One','teller'::user_role,$3)")
        .bind(user_id).bind(org_id).bind(&hash).execute(&pool).await.unwrap();
    assign_teller_to_branch(&pool, user_id, branch_a).await;
    assign_teller_to_branch(&pool, user_id, branch_b).await;
    let shift_id = Uuid::new_v4();
    sqlx::query("INSERT INTO shifts (id, branch_id, teller_id, opening_cash) VALUES ($1,$2,$3,0)")
        .bind(shift_id).bind(branch_a).bind(user_id).execute(&pool).await.unwrap();

    let login = |branch: Uuid| test::TestRequest::post()
        .uri("/auth/login")
        .set_json(&json!({"name":"Teller One","pin":"1234","branch_id": branch}))
        .to_request();

    // Signing in at a DIFFERENT branch while the shift is open elsewhere is blocked.
    assert_eq!(test::call_service(&app, login(branch_b)).await.status(), 409,
        "login at a different branch while a shift is open must be blocked");
    // Re-signing in at the SAME branch as the open shift is ALLOWED — the teller
    // must be able to resume their own shift (e.g. after a token expiry),
    // otherwise an expired token locks them out of the shift they need to close.
    assert_eq!(test::call_service(&app, login(branch_a)).await.status(), 200,
        "login at the same branch as the open shift must be allowed (resume)");

    // Once the shift is closed, any branch is available again.
    sqlx::query("UPDATE shifts SET status='closed', closed_at=now() WHERE id=$1")
        .bind(shift_id).execute(&pool).await.unwrap();
    assert_eq!(test::call_service(&app, login(branch_b)).await.status(), 200,
        "login must succeed at any branch once the open shift is closed");
}

/// /auth/me exposes the org tax_rate + currency so the POS can compute a
/// tax-inclusive cart total client-side.
#[sqlx::test(migrations = "./migrations")]
async fn test_me_returns_org_tax_rate(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    sqlx::query("UPDATE organizations SET tax_rate = 0.14, currency_code = 'EGP' WHERE id = $1")
        .bind(org_id).execute(&pool).await.unwrap();
    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, org_id, name, role, email, password_hash) VALUES ($1,$2,'U','org_admin'::user_role,'tx@test.com','h')")
        .bind(user_id).bind(org_id).execute(&pool).await.unwrap();
    let token = generate_token(user_id, Some(org_id), UserRole::OrgAdmin);

    let resp = test::call_service(&app, test::TestRequest::get().uri("/auth/me")
        .insert_header(("Authorization", format!("Bearer {}", token))).to_request()).await;
    assert_eq!(resp.status(), 200);
    let body: MeResponse = test::read_body_json(resp).await;
    assert!((body.tax_rate - 0.14).abs() < 1e-9, "me must expose org tax_rate, got {}", body.tax_rate);
    assert_eq!(body.currency_code, "EGP");
}

/// Open-shift login rule (different teller, same branch → reject): a teller may
/// not sign in at a branch that already holds another teller's open shift.
#[sqlx::test(migrations = "./migrations")]
async fn test_pin_login_blocked_when_branch_has_other_tellers_open_shift(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    let branch = seed_branch(&pool, org_id).await;
    let other_branch = seed_branch(&pool, org_id).await;

    // Alice has an OPEN shift at `branch`.
    let alice = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, org_id, name, role, pin_hash) VALUES ($1,$2,'Alice','teller'::user_role,$3)")
        .bind(alice).bind(org_id).bind(bcrypt::hash("1111", bcrypt::DEFAULT_COST).unwrap()).execute(&pool).await.unwrap();
    assign_teller_to_branch(&pool, alice, branch).await;
    sqlx::query("INSERT INTO shifts (branch_id, teller_id, opening_cash) VALUES ($1,$2,0)")
        .bind(branch).bind(alice).execute(&pool).await.unwrap();

    // Bob (no open shift) is assigned to both branches.
    let bob = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, org_id, name, role, pin_hash) VALUES ($1,$2,'Bob','teller'::user_role,$3)")
        .bind(bob).bind(org_id).bind(bcrypt::hash("2222", bcrypt::DEFAULT_COST).unwrap()).execute(&pool).await.unwrap();
    assign_teller_to_branch(&pool, bob, branch).await;
    assign_teller_to_branch(&pool, bob, other_branch).await;

    let login = |b: Uuid| test::TestRequest::post().uri("/auth/login")
        .set_json(&json!({"name":"Bob","pin":"2222","branch_id": b})).to_request();

    // Bob at Alice's open-shift branch → blocked.
    assert_eq!(test::call_service(&app, login(branch)).await.status(), 409,
        "a different teller must not sign in at a branch with someone else's open shift");
    // Bob at a branch with no open shift → allowed.
    assert_eq!(test::call_service(&app, login(other_branch)).await.status(), 200,
        "a branch with no open shift accepts a fresh teller login");
}
