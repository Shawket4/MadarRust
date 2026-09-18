//! The phase 5 VOID limits on the LIVE route: a teller may void their OWN sale
//! within their window, anything else goes to a manager.
//!
//! Until `authz::acts` these limits were enforced nowhere — the live route and
//! replay both asked only the legacy `orders:delete` cell. These tests pin the
//! live half, and `an_org_with_no_capability_grants_voids_exactly_as_before`
//! pins the promise made to tills already in the field.

use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::orders::routes;

const CAP_ORDERS_VOID: i32 = 64;

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn token(user_id: Uuid, org_id: Uuid, branch_id: Uuid, role: UserRole) -> String {
    crate::auth::jwt::create_token(&secret(), user_id, Some(org_id), role, Some(branch_id), 24)
        .unwrap()
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Void Org', $2)")
        .bind(org_id)
        .bind(format!("void-org-{org_id}"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, is_active) \
         VALUES ($1, 'cash', '{}', 'emerald', 'payments_outlined', true, true)",
    )
    .bind(org_id)
    .execute(pool)
    .await
    .unwrap();
    org_id
}

async fn seed_branch(pool: &PgPool, org_id: Uuid) -> Uuid {
    let branch_id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(branch_id)
        .bind(org_id)
        .bind(format!("Branch {}", &branch_id.to_string()[..8]))
        .execute(pool)
        .await
        .unwrap();
    branch_id
}

async fn seed_user(pool: &PgPool, org_id: Uuid, role: &str) -> Uuid {
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, $5, $3, 'hash', $4::user_role)",
    )
    .bind(user_id)
    .bind(org_id)
    .bind(format!("user-{user_id}@test.com"))
    .bind(role)
    .bind(format!("Till {}", &user_id.to_string()[..8]))
    .execute(pool)
    .await
    .unwrap();
    user_id
}

async fn grant_legacy(pool: &PgPool, role: &str) {
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) \
         VALUES ($1::user_role, 'orders', 'delete', true) ON CONFLICT DO NOTHING",
    )
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
}

/// The capability itself, with the limits a provisioned org's template writes
/// onto the teller's grant (`own` + `max_age_minutes`). `None` = unrestricted,
/// which is what every role above the teller holds.
async fn grant_void(pool: &PgPool, org: Uuid, user: Uuid, limits: Option<Value>) {
    sqlx::query(
        "INSERT INTO user_overrides (org_id, user_id, capability_id, effect, limits, reason) \
         VALUES ($1, $2, $3, 'allow', $4, 'test')",
    )
    .bind(org)
    .bind(user)
    .bind(CAP_ORDERS_VOID)
    .bind(limits)
    .execute(pool)
    .await
    .unwrap();
}

async fn deny_void(pool: &PgPool, org: Uuid, user: Uuid) {
    sqlx::query(
        "INSERT INTO user_overrides (org_id, user_id, capability_id, effect, reason) \
         VALUES ($1, $2, $3, 'deny', 'test')",
    )
    .bind(org)
    .bind(user)
    .bind(CAP_ORDERS_VOID)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_till(pool: &PgPool, branch_id: Uuid, teller_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO tills (id, branch_id, teller_id, status, opening_cash) \
         VALUES ($1, $2, $3, 'open', 10000)",
    )
    .bind(id)
    .bind(branch_id)
    .bind(teller_id)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// A settled cash sale rung `age_minutes` ago by `teller_id`.
async fn seed_order(
    pool: &PgPool,
    branch_id: Uuid,
    till_id: Uuid,
    teller_id: Uuid,
    age_minutes: i64,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO orders (id, branch_id, teller_id, till_id, idempotency_key, subtotal, \
                             tax_amount, total_amount, status, order_number, payment_method, \
                             order_ref, created_at) \
         VALUES ($1, $2, $3, $4, gen_random_uuid(), 200, 0, 200, 'completed', \
                 (floor(random() * 1000000000)::int), 'cash', \
                 gen_random_uuid()::text, now() - make_interval(mins => $5))",
    )
    .bind(id)
    .bind(branch_id)
    .bind(teller_id)
    .bind(till_id)
    .bind(age_minutes as i32)
    .execute(pool)
    .await
    .unwrap();
    id
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(crate::realtime::hub::BranchEventHub::new()))
                .configure(routes::configure),
        )
        .await
    };
}

fn void_body() -> Value {
    json!({ "reason": "mistake" })
}

async fn post_void<S>(app: &S, token: &str, order: Uuid, body: &Value) -> u16
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let resp = test::call_service(
        app,
        test::TestRequest::post()
            .uri(&format!("/orders/{order}/void"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(body)
            .to_request(),
    )
    .await;
    let status = resp.status().as_u16();
    if status >= 400 {
        let body = test::read_body(resp).await;
        eprintln!("VOID {order} -> {status}: {}", String::from_utf8_lossy(&body));
    }
    status
}

/// A teller limited to their own sale within 10 minutes: inside the window is
/// theirs to void, outside it is not, and neither is somebody else's sale.
#[sqlx::test]
async fn the_live_route_enforces_own_only_and_the_age_window(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let other = seed_user(&pool, org, "teller").await;
    grant_legacy(&pool, "teller").await;
    grant_void(
        &pool,
        org,
        teller,
        Some(json!({ "own": true, "max_age_minutes": 10 })),
    )
    .await;
    let till = seed_till(&pool, branch, teller).await;
    let bearer = token(teller, org, branch, UserRole::Teller);

    let fresh = seed_order(&pool, branch, till, teller, 3).await;
    assert_eq!(
        post_void(&app, &bearer, fresh, &void_body()).await,
        200,
        "own sale, 3 minutes old"
    );

    let stale = seed_order(&pool, branch, till, teller, 40).await;
    assert_eq!(
        post_void(&app, &bearer, stale, &void_body()).await,
        403,
        "own sale, but 40 minutes old"
    );

    let theirs = seed_order(&pool, branch, till, other, 1).await;
    assert_eq!(
        post_void(&app, &bearer, theirs, &void_body()).await,
        403,
        "somebody else's sale, however fresh"
    );

    // Refused means refused: neither sale moved.
    let voided: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM orders WHERE id = ANY($1) AND status = 'voided'",
    )
    .bind(vec![stale, theirs])
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(voided, 0);
}

/// A manager's one-time PIN unlock carries an over-limit void live, exactly as
/// it does on replay — and only a real approver's.
#[sqlx::test]
async fn a_managers_live_approval_carries_an_over_limit_void(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let manager = seed_user(&pool, org, "branch_manager").await;
    let bystander = seed_user(&pool, org, "branch_manager").await;
    grant_legacy(&pool, "teller").await;
    grant_void(
        &pool,
        org,
        teller,
        Some(json!({ "own": true, "max_age_minutes": 10 })),
    )
    .await;
    grant_void(&pool, org, manager, None).await;
    deny_void(&pool, org, bystander).await;
    let till = seed_till(&pool, branch, teller).await;
    let bearer = token(teller, org, branch, UserRole::Teller);

    let approval = |approver: Uuid, id: Uuid| {
        json!({ "reason": "mistake",
                "live_approval": { "id": id, "capability": "orders.void",
                                   "approver_id": approver } })
    };

    // An approver who does not hold the act unlocks nothing.
    let stale = seed_order(&pool, branch, till, teller, 40).await;
    assert_eq!(
        post_void(&app, &bearer, stale, &approval(bystander, Uuid::new_v4())).await,
        403,
        "the approver doesn't hold the void either"
    );

    // Neither does approving yourself.
    assert_eq!(
        post_void(&app, &bearer, stale, &approval(teller, Uuid::new_v4())).await,
        403,
        "self-approval"
    );

    let approval_id = Uuid::new_v4();
    assert_eq!(
        post_void(&app, &bearer, stale, &approval(manager, approval_id)).await,
        200,
        "a manager who holds it carries the void"
    );
    let (approver, op): (Uuid, String) =
        sqlx::query_as("SELECT approver_user_id, op FROM approvals WHERE id = $1")
            .bind(approval_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(approver, manager);
    assert_eq!(op, "void_order_live");
}

/// A grant with no limits is unrestricted — a manager voids anybody's sale of
/// any age, with no approval and no new call.
#[sqlx::test]
async fn an_unlimited_grant_is_untouched(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let manager = seed_user(&pool, org, "branch_manager").await;
    grant_legacy(&pool, "branch_manager").await;
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(manager)
        .bind(branch)
        .execute(&pool)
        .await
        .unwrap();
    grant_void(&pool, org, manager, None).await;
    let till = seed_till(&pool, branch, teller).await;
    let old = seed_order(&pool, branch, till, teller, 500).await;
    let bearer = token(manager, org, branch, UserRole::BranchManager);
    assert_eq!(post_void(&app, &bearer, old, &void_body()).await, 200);
}

/// THE OLD-CLIENT / OLD-ORG GOLDEN CHECK. The backend ships before the POS
/// release, so every till in the field hits this code with no `live_approval`
/// to send. An org that predates capability grants holds none of them, and a
/// void that worked yesterday — somebody else's sale, hours old, from a till
/// that never heard of a limit — must still work today.
#[sqlx::test]
async fn an_org_with_no_capability_grants_voids_exactly_as_before(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let other = seed_user(&pool, org, "teller").await;
    // Legacy only: the role matrix says yes, and nothing else is written down.
    grant_legacy(&pool, "teller").await;
    let till = seed_till(&pool, branch, teller).await;
    let old_and_not_theirs = seed_order(&pool, branch, till, other, 600).await;
    let bearer = token(teller, org, branch, UserRole::Teller);
    // The pre-0.7.9 body: no `live_approval` field at all, and the fields an
    // old till still sends.
    let legacy_body = json!({ "reason": "customer_request", "restore_inventory": false });
    assert_eq!(
        post_void(&app, &bearer, old_and_not_theirs, &legacy_body).await,
        200,
        "an org with no capability grants must be unaffected"
    );
}
