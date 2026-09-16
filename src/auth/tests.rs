use actix_web::{App, test, web};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::handlers::{
    AuthPermissionsResponse, LoginResponse, MeResponse, ResolveBranchResponse,
};
use crate::auth::jwt::JwtSecret;
use crate::auth::org_status::OrgStatusCache;
use crate::auth::routes;
use crate::models::UserRole;

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
        user_id,
        org_id,
        hash
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
        user_id,
        org_id,
        hash
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
        user_id,
        org_id,
        hash
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
        user_id,
        org_id,
        hash
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
    assert_eq!(
        resp.status(),
        200,
        "PIN login with correct credentials should succeed"
    );

    let body: LoginResponse = test::read_body_json(resp).await;
    assert_eq!(body.user.name, "Teller One");
    assert!(!body.token.is_empty());
    assert_eq!(
        body.user.branch_id,
        Some(branch_id),
        "branch_id should be echoed in response"
    );
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
    assert_eq!(
        resp.status(),
        400,
        "PIN login without branch_id must return 400"
    );
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
async fn test_login_pin_teller_refused_at_a_branch_they_are_not_allowed_at(pool: PgPool) {
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
        user_id,
        org_id,
        hash
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
    // D13 ("tellers are org-scoped, no per-branch gate at the till") is
    // SUPERSEDED: POS_SIGNIN_OVERHAUL.md §5.2 "A + B". A person who HAS an
    // explicit branch allow-list is held to it at the till, so the dashboard's
    // per-branch toggles finally bite. 403, not 401: the name and PIN were
    // right, the branch was not.
    assert_eq!(
        resp.status(),
        403,
        "a teller listed only at branch A must not sign in at branch B"
    );

    // The one failed sign-in with an identity (§3.4): counted per person in the
    // owner's review queue. A second try is the same item, attempts = 2.
    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(&json!({"name": "Teller One", "pin": "1234", "branch_id": branch_b}))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 403);
    let (n, attempts): (i64, Option<i32>) = sqlx::query_as(
        "SELECT count(*), max((details->>'attempts')::int) FROM authz_replay_flags
          WHERE author_id = $1 AND branch_id = $2 AND reason = 'pin_wrong_branch'",
    )
    .bind(user_id)
    .bind(branch_b)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((n, attempts), (1, Some(2)), "one owner item, two attempts");

    // ...and at the branch they ARE allowed at, they sign in normally.
    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(&json!({
            "name": "Teller One",
            "pin": "1234",
            "branch_id": branch_a
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "allowed branch should sign in");
}

/// Wrong PINs earn a GROWING DELAY, never a lock (POS_SIGNIN_OVERHAUL.md §3.4).
/// A wrong PIN matches nobody, so there is no account to lock; and the tablet is
/// shared, so a lock would stop the shop. The refusal carries the remaining wait
/// so the PIN pad can count down instead of guessing.
#[sqlx::test(migrations = "./migrations")]
async fn wrong_pins_earn_a_growing_delay_and_a_correct_one_clears_it(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let hash = bcrypt::hash("135790", bcrypt::DEFAULT_COST).unwrap();
    sqlx::query!(
        "INSERT INTO users (org_id, name, role, pin_hash)
         VALUES ($1, 'Patient One', 'teller'::user_role, $2)",
        org_id,
        hash
    )
    .execute(&pool)
    .await
    .unwrap();

    let attempt = |pin: &str| {
        test::TestRequest::post()
            .uri("/auth/login")
            .insert_header((crate::tickets::DEVICE_ID_HEADER, "tablet-1"))
            .set_json(&json!({"name": "Patient One", "pin": pin, "branch_id": branch_id}))
            .to_request()
    };

    // The free misses: an ordinary mistype costs nothing. The miss that trips
    // the delay is itself still a plain refusal — the wait is checked BEFORE the
    // lookup, so it lands on the NEXT attempt.
    for i in 0..=crate::auth::pin_throttle::FREE_ATTEMPTS {
        let resp = test::call_service(&app, attempt("000000")).await;
        assert_eq!(resp.status(), 401, "miss {i} should be a plain refusal");
    }

    // One more, and the wait bites.
    let resp = test::call_service(&app, attempt("000000")).await;
    assert_eq!(resp.status(), 429, "the delay has begun");
    let retry_after = resp
        .headers()
        .get("Retry-After")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok())
        .expect("Retry-After names the wait");
    assert!((1..=5).contains(&retry_after), "{retry_after}");
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["code"], "PIN_THROTTLED");
    assert_eq!(
        body["retry_after_seconds"], retry_after,
        "the POS counts down from this"
    );

    // Even the RIGHT PIN waits: the delay is on the place, not the person.
    let resp = test::call_service(&app, attempt("135790")).await;
    assert_eq!(resp.status(), 429);

    // Once the wait is over, the correct PIN works and clears the run.
    sqlx::query("UPDATE pin_attempts SET blocked_until = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let resp = test::call_service(&app, attempt("135790")).await;
    assert_eq!(resp.status(), 200);
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM pin_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0, "a correct PIN forgets the run");
}

/// Old tablets (v0.5–v0.7) send a name and no device id: the new place-delay
/// never touches them, however many misses (§3.4, "leave them alone").
#[sqlx::test(migrations = "./migrations")]
async fn old_tablets_without_a_device_id_are_never_delayed(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    // Ten: the per-address governor on /auth/login allows a burst of ten, and
    // "no pin_attempts row at all" below proves neither bucket was touched.
    for _ in 0..10 {
        let req = test::TestRequest::post()
            .uri("/auth/login")
            .set_json(&json!({"name": "Nobody", "pin": "000000", "branch_id": branch_id}))
            .to_request();
        assert_eq!(test::call_service(&app, req).await.status(), 401);
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM pin_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

/// The keyed PIN fingerprint (POS_SIGNIN_OVERHAUL.md §2, §6). Salted hashes
/// cannot be fingerprinted by a migration, so the column fills in at the one
/// moment the plaintext exists: a successful sign-in.
#[sqlx::test(migrations = "./migrations")]
async fn a_successful_pin_login_backfills_the_fingerprint(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = Uuid::new_v4();
    let hash = bcrypt::hash("1234", bcrypt::DEFAULT_COST).unwrap();
    sqlx::query!(
        "INSERT INTO users (id, org_id, name, role, pin_hash)
         VALUES ($1, $2, 'Fingerprint Me', 'teller'::user_role, $3)",
        user_id,
        org_id,
        hash
    )
    .execute(&pool)
    .await
    .unwrap();

    let before: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT pin_fingerprint FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(before.is_none(), "nothing to fingerprint before a sign-in");

    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(&json!({"name": "Fingerprint Me", "pin": "1234", "branch_id": branch_id}))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 200);

    let after: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT pin_fingerprint FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        after,
        Some(crate::auth::pin_fingerprint::fingerprint(org_id, "1234")),
        "the fingerprint is HMAC(key, org || pin) under the current key"
    );
    // It is a LOOKUP key, never something a client sees.
    assert!(!after.unwrap().is_empty());
}

/// The migration rule (POS_SIGNIN_OVERHAUL.md §5.3): a person with NO explicit
/// branches keeps working everywhere in their org. Ten PIN holders in prod have
/// no allow-list row; reading "listed nowhere" as "allowed nowhere" would lock
/// them all out overnight.
#[sqlx::test(migrations = "./migrations")]
async fn test_login_pin_teller_with_no_branch_list_works_anywhere(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let _branch_a = seed_branch(&pool, org_id).await;
    let branch_b = seed_branch(&pool, org_id).await;
    let user_id = Uuid::new_v4();
    let hash = bcrypt::hash("1234", bcrypt::DEFAULT_COST).unwrap();
    sqlx::query!(
        "INSERT INTO users (id, org_id, name, role, pin_hash)
         VALUES ($1, $2, 'Teller Free', 'teller'::user_role, $3)",
        user_id,
        org_id,
        hash
    )
    .execute(&pool)
    .await
    .unwrap();
    // Deliberately NO user_branch_assignments row.

    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(&json!({
            "name": "Teller Free",
            "pin": "1234",
            "branch_id": branch_b
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let raw = test::read_body(resp).await;
    let text = String::from_utf8_lossy(&raw).to_string();
    assert_eq!(
        status, 200,
        "a teller with no explicit branches works everywhere in their org, got {text}"
    );
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(
        body["token"]
            .as_str()
            .map(|t| !t.is_empty())
            .unwrap_or(false),
        "successful login should return a token, got {body:?}"
    );
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
        user_id,
        org_id,
        hash
    )
    .execute(&pool)
    .await
    .unwrap();

    let before: Option<String> =
        sqlx::query_scalar("SELECT offline_pin_hash FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(before.is_none(), "no offline hash before first login");

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/auth/login")
            .set_json(&json!({ "name": "Teller One", "pin": "1234", "branch_id": branch }))
            .to_request(),
    )
    .await;
    assert!(
        resp.status().is_success(),
        "login should succeed, got {:?}",
        resp.status()
    );

    let after: Option<String> =
        sqlx::query_scalar("SELECT offline_pin_hash FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let phc = after.expect("offline_pin_hash must be derived on PIN login");
    assert!(
        crate::auth::offline::verify_offline_pin("1234", &phc),
        "stored argon2id verifier must match the PIN"
    );
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
        user_id,
        org_a,
        hash
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
        user_b,
        org_b_id,
        hash_b
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
    assert_eq!(
        resp.status(),
        401,
        "cross-org PIN login must return 401 (invalid credentials)"
    );
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert!(
        !body["error"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .contains("branch"),
        "cross-org login must NOT leak the branch-access message, got {body:?}"
    );
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
        user_id,
        org_id
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
        teller,
        org_id
    )
    .execute(&pool)
    .await
    .unwrap();
    // Assigned to BOTH branches; branch_a is inserted first, so a naive LIMIT-1
    // would tend to pick it — but the token is bound to branch_b.
    assign_teller_to_branch(&pool, teller, branch_a).await;
    assign_teller_to_branch(&pool, teller, branch_b).await;

    let token = crate::auth::jwt::create_token(
        &get_secret(),
        teller,
        Some(org_id),
        UserRole::Teller,
        Some(branch_b),
        24,
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
        user_id,
        org_id
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
    assert!(
        perm.granted,
        "User-level override should make orgs:create granted"
    );
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

    let req = test::TestRequest::get()
        .uri("/auth/permissions")
        .to_request();
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
    assert_eq!(
        resp.status(),
        404,
        "device outside branch radius should return 404"
    );
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
    assert_eq!(
        resp.status(),
        404,
        "branch without geo coordinates should not match"
    );
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
    sqlx::query("INSERT INTO tills (id, branch_id, teller_id, opening_cash) VALUES ($1,$2,$3,0)")
        .bind(shift_id)
        .bind(branch_a)
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();

    let login = |branch: Uuid| {
        test::TestRequest::post()
            .uri("/auth/login")
            .set_json(&json!({"name":"Teller One","pin":"1234","branch_id": branch}))
            .to_request()
    };

    // Signing in at a DIFFERENT branch while the shift is open elsewhere is blocked.
    assert_eq!(
        test::call_service(&app, login(branch_b)).await.status(),
        409,
        "login at a different branch while a shift is open must be blocked"
    );
    // Re-signing in at the SAME branch as the open shift is ALLOWED — the teller
    // must be able to resume their own shift (e.g. after a token expiry),
    // otherwise an expired token locks them out of the shift they need to close.
    assert_eq!(
        test::call_service(&app, login(branch_a)).await.status(),
        200,
        "login at the same branch as the open shift must be allowed (resume)"
    );

    // Once the shift is closed, any branch is available again.
    sqlx::query("UPDATE tills SET status='closed', closed_at=now() WHERE id=$1")
        .bind(shift_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        test::call_service(&app, login(branch_b)).await.status(),
        200,
        "login must succeed at any branch once the open shift is closed"
    );
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
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();
    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, org_id, name, role, email, password_hash) VALUES ($1,$2,'U','org_admin'::user_role,'tx@test.com','h')")
        .bind(user_id).bind(org_id).execute(&pool).await.unwrap();
    let token = generate_token(user_id, Some(org_id), UserRole::OrgAdmin);

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/auth/me")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: MeResponse = test::read_body_json(resp).await;
    assert!(
        (body.tax_rate - 0.14).abs() < 1e-9,
        "me must expose org tax_rate, got {}",
        body.tax_rate
    );
    assert_eq!(body.currency_code, "EGP");
}

/// Multi-teller: a different teller MAY sign in at a branch that already holds
/// another teller's open shift — they'll operate their own till. (Pre-multi-teller
/// this was rejected; the one-open-per-till index, not login, now guards a drawer.)
#[sqlx::test(migrations = "./migrations")]
async fn test_pin_login_allowed_when_branch_has_other_tellers_open_shift(pool: PgPool) {
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
    sqlx::query("INSERT INTO tills (branch_id, teller_id, opening_cash) VALUES ($1,$2,0)")
        .bind(branch)
        .bind(alice)
        .execute(&pool)
        .await
        .unwrap();

    // Bob (no open shift) is assigned to both branches.
    let bob = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, org_id, name, role, pin_hash) VALUES ($1,$2,'Bob','teller'::user_role,$3)")
        .bind(bob).bind(org_id).bind(bcrypt::hash("2222", bcrypt::DEFAULT_COST).unwrap()).execute(&pool).await.unwrap();
    assign_teller_to_branch(&pool, bob, branch).await;
    assign_teller_to_branch(&pool, bob, other_branch).await;

    let login = |b: Uuid| {
        test::TestRequest::post()
            .uri("/auth/login")
            .set_json(&json!({"name":"Bob","pin":"2222","branch_id": b}))
            .to_request()
    };

    // Bob at Alice's open-shift branch → now ALLOWED (his own till).
    assert_eq!(
        test::call_service(&app, login(branch)).await.status(),
        200,
        "multi-teller: a fresh teller may sign in alongside another teller's open shift"
    );
    // Bob at a branch with no open shift → allowed.
    assert_eq!(
        test::call_service(&app, login(other_branch)).await.status(),
        200,
        "a branch with no open shift accepts a fresh teller login"
    );
}

// ── Org-suspension kill-switch ────────────────────────────────

/// Login must refuse to issue a token to a suspended org, with the stable
/// `ORG_SUSPENDED` code so the client can show the right message.
#[sqlx::test(migrations = "./migrations")]
async fn test_login_suspended_org_rejected(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    sqlx::query("UPDATE organizations SET is_active = false WHERE id = $1")
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();

    let user_id = Uuid::new_v4();
    let hash = bcrypt::hash("password123", bcrypt::DEFAULT_COST).unwrap();
    sqlx::query!(
        "INSERT INTO users (id, org_id, name, role, email, password_hash)
         VALUES ($1, $2, 'Admin', 'org_admin'::user_role, 'sus@test.com', $3)",
        user_id,
        org_id,
        hash
    )
    .execute(&pool)
    .await
    .unwrap();

    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(&json!({ "email": "sus@test.com", "password": "password123" }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        403,
        "login into a suspended org must be rejected"
    );
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(
        body["code"].as_str(),
        Some("ORG_SUSPENDED"),
        "rejection must carry the ORG_SUSPENDED code, got {body:?}"
    );
}

/// A soft-deleted org (deleted_at set) is also refused at login.
#[sqlx::test(migrations = "./migrations")]
async fn test_login_soft_deleted_org_rejected(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    sqlx::query("UPDATE organizations SET deleted_at = NOW() WHERE id = $1")
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();

    let user_id = Uuid::new_v4();
    let hash = bcrypt::hash("password123", bcrypt::DEFAULT_COST).unwrap();
    sqlx::query!(
        "INSERT INTO users (id, org_id, name, role, email, password_hash)
         VALUES ($1, $2, 'Admin', 'org_admin'::user_role, 'del@test.com', $3)",
        user_id,
        org_id,
        hash
    )
    .execute(&pool)
    .await
    .unwrap();

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/auth/login")
            .set_json(&json!({ "email": "del@test.com", "password": "password123" }))
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        403,
        "login into a soft-deleted org must be rejected"
    );
}

/// With the cache registered (enforcement armed), an authenticated request
/// scoped to a suspended org is rejected by the middleware — even on a token
/// that was issued while the org was still active.
#[sqlx::test(migrations = "./migrations")]
async fn test_middleware_blocks_request_for_suspended_org(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .app_data(web::Data::new(OrgStatusCache::new()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let user_id = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO users (id, org_id, name, role, email, password_hash)
         VALUES ($1, $2, 'Admin', 'org_admin'::user_role, 'mw@test.com', 'h')",
        user_id,
        org_id
    )
    .execute(&pool)
    .await
    .unwrap();

    // Token minted while the org is healthy.
    let token = generate_token(user_id, Some(org_id), UserRole::OrgAdmin);

    // Suspend the org out from under the live token.
    sqlx::query("UPDATE organizations SET is_active = false WHERE id = $1")
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/auth/me")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        403,
        "a live token for a suspended org must be rejected"
    );
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["code"].as_str(), Some("ORG_SUSPENDED"));
}

/// The allow path with enforcement armed: an active org passes through.
#[sqlx::test(migrations = "./migrations")]
async fn test_middleware_allows_request_for_active_org(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .app_data(web::Data::new(OrgStatusCache::new()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let user_id = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO users (id, org_id, name, role, email, password_hash)
         VALUES ($1, $2, 'Admin', 'org_admin'::user_role, 'ok@test.com', 'h')",
        user_id,
        org_id
    )
    .execute(&pool)
    .await
    .unwrap();

    let token = generate_token(user_id, Some(org_id), UserRole::OrgAdmin);
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/auth/me")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        200,
        "an active org must pass the kill-switch"
    );
}

/// Super admins carry no org_id, so the kill-switch never applies to them —
/// this is what keeps the reactivation path usable against a down org.
#[sqlx::test(migrations = "./migrations")]
async fn test_middleware_super_admin_bypasses_kill_switch(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .app_data(web::Data::new(OrgStatusCache::new()))
            .configure(routes::configure),
    )
    .await;

    let user_id = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO users (id, name, role, email, password_hash)
         VALUES ($1, 'Super Admin', 'super_admin'::user_role, 'sa@test.com', 'h')",
        user_id
    )
    .execute(&pool)
    .await
    .unwrap();

    let token = generate_token(user_id, None, UserRole::SuperAdmin);
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/auth/permissions")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        200,
        "super admin (no org_id) must bypass the kill-switch"
    );
}

/// The cache caches within its TTL, and `invalidate` forces an immediate
/// re-read — the mechanism the org-mutating handlers rely on for instant effect.
#[sqlx::test(migrations = "./migrations")]
async fn test_org_status_cache_caches_and_invalidates(pool: PgPool) {
    let cache = OrgStatusCache::new();
    let org_id = seed_org(&pool).await;

    assert!(
        cache.is_allowed(&pool, org_id).await.unwrap(),
        "a fresh active org is allowed"
    );

    // Suspend in the DB; the cached "allowed" verdict survives the TTL window.
    sqlx::query("UPDATE organizations SET is_active = false WHERE id = $1")
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        cache.is_allowed(&pool, org_id).await.unwrap(),
        "within the TTL the cached allow verdict is still served"
    );

    // Invalidation forces a re-read, which now sees the suspension.
    cache.invalidate(org_id);
    assert!(
        !cache.is_allowed(&pool, org_id).await.unwrap(),
        "after invalidation the suspension is observed"
    );
}

/// A token referencing an org that does not exist resolves to "not allowed".
#[sqlx::test(migrations = "./migrations")]
async fn test_org_status_unknown_org_not_allowed(pool: PgPool) {
    let cache = OrgStatusCache::new();
    assert!(
        !cache.is_allowed(&pool, Uuid::new_v4()).await.unwrap(),
        "an unknown org id must not be allowed"
    );
}
