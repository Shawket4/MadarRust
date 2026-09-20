//! Phase 0 security hotfixes (ORG_ONBOARDING_PERMISSIONS_AUDIT.md S1-S8, B1),
//! each pinned against the live routes.

use actix_web::{App, http::StatusCode, test, web};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::JwtSecret;
use madar_rust::models::UserRole;

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn token(user: Uuid, org: Option<Uuid>, role: UserRole) -> String {
    madar_rust::auth::jwt::create_token(&secret(), user, org, role, None, 24).unwrap()
}

async fn org(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(id)
        .bind(format!("o-{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn branch(pool: &PgPool, org: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(org)
        .bind(format!("B {id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn user(pool: &PgPool, org: Option<Uuid>, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, role, email, password_hash, pin_hash)
         VALUES ($1, $2, $3, $4::user_role, $5, 'h', 'h')",
    )
    .bind(id)
    .bind(org)
    .bind(format!("{role}-{}", &id.to_string()[..6]))
    .bind(role)
    .bind(format!("{id}@t.com"))
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn assign(pool: &PgPool, user: Uuid, branch: Uuid) {
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(user)
        .bind(branch)
        .execute(pool)
        .await
        .unwrap();
}

async fn override_grant(pool: &PgPool, user: Uuid, resource: &str, action: &str) {
    sqlx::query(
        "INSERT INTO permissions (user_id, resource, action, granted)
         VALUES ($1, $2::permission_resource, $3::permission_action, true)",
    )
    .bind(user)
    .bind(resource)
    .bind(action)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed(pool: &PgPool) {
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .configure(madar_rust::users::routes::configure)
                .configure(madar_rust::permissions::routes::configure)
                .configure(madar_rust::orgs::routes::configure)
                .configure(madar_rust::devices::routes::configure)
                .configure(madar_rust::payment_methods::routes::configure),
        )
        .await
    };
}

fn create_body(org: Uuid, role: &str) -> serde_json::Value {
    json!({
        "org_id": org, "name": format!("New {}", Uuid::new_v4()), "role": role,
        "email": format!("{}@n.com", Uuid::new_v4()), "password": "secret-pass-1",
        // PINs are unique across an org and six digits when newly issued
        // (POS_SIGNIN_OVERHAUL.md §3), so every fixture account needs its own.
        "pin": format!("{:06}", Uuid::new_v4().as_u128() % 1_000_000)
    })
}

// ── S1 ──────────────────────────────────────────────────────────────────

/// Architecture E replaced the phase-0 rank rule ("strictly downward") with G2:
/// you can only create an account whose role gives it nothing you do not
/// already hold. A stray `users:create` on a teller therefore still creates
/// nobody *above* them — the S4 hole — but it does let them create their own
/// kind, which is not an escalation and which the owner asked for by granting
/// the capability in the first place.
#[sqlx::test]
async fn s1_a_teller_holding_users_create_creates_nobody_above_them(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool).await;
    let teller = user(&pool, Some(o), "teller").await;
    override_grant(&pool, teller, "users", "create").await;
    let t = token(teller, Some(o), UserRole::Teller);
    let call = |role: &'static str| {
        test::TestRequest::post()
            .uri("/users")
            .insert_header(("Authorization", format!("Bearer {t}")))
            .set_json(create_body(o, role))
            .to_request()
    };
    for role in ["org_admin", "branch_manager"] {
        let resp = test::call_service(&app, call(role)).await;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "teller created {role}"
        );
    }
    // Owner decision 2026-09-16, no peer writes: not even their own kind.
    assert_eq!(
        test::call_service(&app, call("teller")).await.status(),
        StatusCode::FORBIDDEN,
        "a teller creates no peer"
    );
}

#[sqlx::test]
async fn s1_a_manager_creates_nobody_above_themselves(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool).await;
    let mgr = user(&pool, Some(o), "branch_manager").await;
    let t = token(mgr, Some(o), UserRole::BranchManager);
    let call = |role: &'static str| {
        test::TestRequest::post()
            .uri("/users")
            .insert_header(("Authorization", format!("Bearer {t}")))
            .set_json(create_body(o, role))
            .to_request()
    };
    assert_eq!(
        test::call_service(&app, call("org_admin")).await.status(),
        StatusCode::FORBIDDEN
    );
    // Owner decision 2026-09-16, no peer writes: a manager creates no other
    // manager, even though the role gives nothing they don't already hold.
    assert_eq!(
        test::call_service(&app, call("branch_manager"))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        test::call_service(&app, call("teller")).await.status(),
        StatusCode::CREATED
    );
}

#[sqlx::test]
async fn s8_a_foreign_branch_writes_no_user(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool).await;
    let other = org(&pool).await;
    let foreign = branch(&pool, other).await;
    let admin = user(&pool, Some(o), "org_admin").await;
    let mut body = create_body(o, "teller");
    body["branch_ids"] = json!([foreign]);
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/users")
            .insert_header((
                "Authorization",
                format!("Bearer {}", token(admin, Some(o), UserRole::OrgAdmin)),
            ))
            .set_json(&body)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE org_id = $1")
        .bind(o)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1, "only the admin exists");
}

// ── S5 / owners ─────────────────────────────────────────────────────────

#[sqlx::test]
async fn s5_nobody_escalates_a_peer_or_themselves(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool).await;
    let b = branch(&pool, o).await;
    let m1 = user(&pool, Some(o), "branch_manager").await;
    let m2 = user(&pool, Some(o), "branch_manager").await;
    assign(&pool, m1, b).await;
    assign(&pool, m2, b).await;
    let t1 = token(m1, Some(o), UserRole::BranchManager);
    let patch = |id: Uuid, body: serde_json::Value| {
        test::TestRequest::patch()
            .uri(&format!("/users/{id}"))
            .insert_header(("Authorization", format!("Bearer {t1}")))
            .set_json(body)
            .to_request()
    };
    // Promote a peer to owner: G2 refuses, the manager is no owner.
    assert_eq!(
        test::call_service(&app, patch(m2, json!({"role": "org_admin"})))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    // Promote yourself: G5, whatever you hold.
    assert_eq!(
        test::call_service(&app, patch(m1, json!({"role": "org_admin"})))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    // Deactivate yourself out of the way of your own guards: also G5.
    assert_eq!(
        test::call_service(&app, patch(m1, json!({"is_active": false})))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    // Owner decision 2026-09-16, no peer writes: on top of G4
    // (`E(target) ⊆ E(actor)`) a non-owner must rank strictly above the
    // target, so a manager resets, deactivates or deletes no other manager.
    // (Owners are unchanged: one owner still resets another's password.)
    assert_eq!(
        test::call_service(&app, patch(m2, json!({"password": "x-new-pass"})))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        test::call_service(&app, patch(m2, json!({"is_active": false})))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri(&format!("/users/{m2}"))
            .insert_header(("Authorization", format!("Bearer {t1}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    // A teller below them is still within reach.
    let t = user(&pool, Some(o), "teller").await;
    assign(&pool, t, b).await;
    assert_eq!(
        test::call_service(&app, patch(t, json!({"password": "x-new-pass"})))
            .await
            .status(),
        StatusCode::OK
    );
}

#[sqlx::test]
async fn the_last_owner_cannot_be_removed_or_demoted(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool).await;
    let owner = user(&pool, Some(o), "org_admin").await;
    let second = user(&pool, Some(o), "org_admin").await;
    let t2 = token(second, Some(o), UserRole::OrgAdmin);

    // Two owners: the second may demote the first.
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/users/{owner}"))
            .insert_header(("Authorization", format!("Bearer {t2}")))
            .set_json(json!({"role": "branch_manager"}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Now `second` is the last owner; nobody (not even a platform user) removes it.
    let sa = user(&pool, None, "super_admin").await;
    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri(&format!("/users/{second}"))
            .insert_header((
                "Authorization",
                format!("Bearer {}", token(sa, None, UserRole::SuperAdmin)),
            ))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    // And they cannot deactivate themselves.
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/users/{second}"))
            .insert_header(("Authorization", format!("Bearer {t2}")))
            .set_json(json!({"is_active": false}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ── S2 / S10 ────────────────────────────────────────────────────────────

#[sqlx::test]
async fn s2_override_writes_are_guarded(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool).await;
    let b = branch(&pool, o).await;
    let owner = user(&pool, Some(o), "org_admin").await;
    let mgr = user(&pool, Some(o), "branch_manager").await;
    let teller = user(&pool, Some(o), "teller").await;
    assign(&pool, mgr, b).await;
    assign(&pool, teller, b).await;
    // A teller holding permissions:update (the prod S4 shape).
    override_grant(&pool, teller, "permissions", "update").await;

    let put = |actor: Uuid, role: UserRole, target: Uuid, body: serde_json::Value| {
        test::TestRequest::put()
            .uri(&format!("/permissions/user/{target}"))
            .insert_header((
                "Authorization",
                format!("Bearer {}", token(actor, Some(o), role)),
            ))
            .set_json(body)
            .to_request()
    };
    let grant = |r: &str, a: &str| json!({"resource": r, "action": a, "granted": true});

    // Self-grant.
    let s = test::call_service(
        &app,
        put(teller, UserRole::Teller, teller, grant("orgs", "update")),
    )
    .await
    .status();
    assert_eq!(s, StatusCode::FORBIDDEN, "teller self-grant");
    // A teller editing a manager.
    let s = test::call_service(
        &app,
        put(teller, UserRole::Teller, mgr, grant("orders", "read")),
    )
    .await
    .status();
    assert_eq!(s, StatusCode::FORBIDDEN, "teller edits manager");
    // A manager denying the owner.
    let deny = json!({"resource": "users", "action": "update", "granted": false});
    let s = test::call_service(&app, put(mgr, UserRole::BranchManager, owner, deny))
        .await
        .status();
    assert_eq!(s, StatusCode::FORBIDDEN, "manager locks out owner");
    // A manager granting what they don't hold (orgs:update).
    let s = test::call_service(
        &app,
        put(
            mgr,
            UserRole::BranchManager,
            teller,
            grant("orgs", "update"),
        ),
    )
    .await
    .status();
    assert_eq!(s, StatusCode::FORBIDDEN, "grant not held");
    // A nonsense pair.
    let s = test::call_service(
        &app,
        put(
            owner,
            UserRole::OrgAdmin,
            teller,
            grant("menu_items", "waive_service"),
        ),
    )
    .await
    .status();
    assert_eq!(s, StatusCode::BAD_REQUEST, "S10");
    // The legitimate case still works.
    let s = test::call_service(
        &app,
        put(owner, UserRole::OrgAdmin, teller, grant("orders", "read")),
    )
    .await
    .status();
    assert_eq!(s, StatusCode::OK);
}

// ── S3 ──────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn s3_bundle_needs_a_device_or_a_till_worker_and_is_branch_scoped(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool).await;
    let b1 = branch(&pool, o).await;
    let b2 = branch(&pool, o).await;
    let kitchen = user(&pool, Some(o), "kitchen").await;
    let t1 = user(&pool, Some(o), "teller").await;
    let t2 = user(&pool, Some(o), "teller").await;
    assign(&pool, t1, b1).await;
    assign(&pool, t2, b2).await;
    let kt = token(kitchen, Some(o), UserRole::Kitchen);

    // A kitchen token with no device: refused.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/orgs/{o}/offline-auth-bundle"))
            .insert_header(("Authorization", format!("Bearer {kt}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // A registered device at branch 1: served, branch-1 people only.
    let dev = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO devices (id, org_id, branch_id, code, kind) VALUES ($1, $2, $3, 'K1', 'kds')",
    )
    .bind(dev)
    .bind(o)
    .bind(b1)
    .execute(&pool)
    .await
    .unwrap();
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/orgs/{o}/offline-auth-bundle"))
            .insert_header(("Authorization", format!("Bearer {kt}")))
            .insert_header(("X-Madar-Device", dev.to_string()))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = test::read_body_json(resp).await;
    let ids: Vec<String> = body["tellers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["user_id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.contains(&t1.to_string()));
    assert!(ids.contains(&kitchen.to_string()), "unassigned floor staff");
    assert!(!ids.contains(&t2.to_string()), "another branch's teller");
}

// ── S6 ──────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn s6_device_registration_is_gated_and_never_rehomes_across_orgs(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool).await;
    let b = branch(&pool, o).await;
    let kitchen = user(&pool, Some(o), "kitchen").await;
    let teller = user(&pool, Some(o), "teller").await;
    let reg = |actor: Uuid, role: UserRole, org: Uuid, branch: Uuid, id: Uuid, kind: &str| {
        test::TestRequest::post()
            .uri("/devices/register")
            .insert_header((
                "Authorization",
                format!("Bearer {}", token(actor, Some(org), role)),
            ))
            .set_json(json!({"id": id, "branch_id": branch, "code": "A1", "kind": kind}))
            .to_request()
    };
    let d = Uuid::new_v4();
    let s = test::call_service(&app, reg(kitchen, UserRole::Kitchen, o, b, d, "pos"))
        .await
        .status();
    assert_eq!(s, StatusCode::FORBIDDEN, "kitchen registers a POS");
    let s = test::call_service(&app, reg(teller, UserRole::Teller, o, b, d, "pos"))
        .await
        .status();
    assert_eq!(s, StatusCode::OK);

    // Another org's teller cannot take the device over.
    let o2 = org(&pool).await;
    let b2 = branch(&pool, o2).await;
    let t2 = user(&pool, Some(o2), "teller").await;
    let s = test::call_service(&app, reg(t2, UserRole::Teller, o2, b2, d, "pos"))
        .await
        .status();
    assert!(s.is_client_error(), "cross-org rehome: {s}");
    let still: Uuid = sqlx::query_scalar("SELECT branch_id FROM devices WHERE id = $1")
        .bind(d)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(still, b);
}

// ── B1 ──────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn b1_a_super_admin_sees_the_selected_orgs_payment_methods(pool: PgPool) {
    let app = app!(pool);
    let o = org(&pool).await;
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash)
         VALUES ($1, 'cash', '{\"en\":\"Cash\",\"ar\":\"نقدي\"}', '#10B981', 'money', true)",
    )
    .bind(o)
    .execute(&pool)
    .await
    .unwrap();
    let sa = user(&pool, None, "super_admin").await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/payment-methods")
            .insert_header((
                "Authorization",
                format!("Bearer {}", token(sa, None, UserRole::SuperAdmin)),
            ))
            .insert_header(("X-Org-Id", o.to_string()))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body.as_array().unwrap().len(), 1);
}

// ── Revocable sessions ──────────────────────────────────────────────────

#[sqlx::test]
async fn a_password_change_ends_older_sessions(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool).await;
    let owner = user(&pool, Some(o), "org_admin").await;
    let mgr = user(&pool, Some(o), "branch_manager").await;
    let old = token(mgr, Some(o), UserRole::BranchManager);
    // Make the old token strictly older than the bump.
    sqlx::query(
        "UPDATE users SET sessions_valid_after = now() + interval '2 seconds' WHERE id = $1",
    )
    .bind(mgr)
    .execute(&pool)
    .await
    .unwrap();
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/users")
            .insert_header(("Authorization", format!("Bearer {old}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // And the handler bumps it.
    sqlx::query("UPDATE users SET sessions_valid_after = NULL WHERE id = $1")
        .bind(mgr)
        .execute(&pool)
        .await
        .unwrap();
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/users/{mgr}"))
            .insert_header((
                "Authorization",
                format!("Bearer {}", token(owner, Some(o), UserRole::OrgAdmin)),
            ))
            .set_json(json!({"password": "brand-new-pass"}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bumped: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT sessions_valid_after FROM users WHERE id = $1")
            .bind(mgr)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(bumped.is_some());
}
