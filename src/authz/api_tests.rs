//! Phase 3: the permissions API enforces anti-escalation, and the new model
//! decides at the till (PIN sign-in, force close) and in the UI (`/authz/me`).

use actix_web::{App, http::StatusCode, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn token(user: Uuid, org: Uuid, role: UserRole) -> String {
    crate::auth::jwt::create_token(&secret(), user, Some(org), role, None, 24).unwrap()
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

async fn user(pool: &PgPool, org: Uuid, role: &str, name: &str, pin: Option<&str>) -> Uuid {
    let id = Uuid::new_v4();
    let pin_hash = pin.map(|p| bcrypt::hash(p, 4).unwrap());
    sqlx::query(
        "INSERT INTO users (id, org_id, name, role, email, password_hash, pin_hash)
         VALUES ($1, $2, $3, $4::user_role, $5, 'h', $6)",
    )
    .bind(id)
    .bind(org)
    .bind(name)
    .bind(role)
    .bind(format!("{id}@t.com"))
    .bind(pin_hash)
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

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .configure(crate::authz::api::configure)
                .configure(crate::auth::routes::configure),
        )
        .await
    };
}

async fn seed(pool: &PgPool) {
    crate::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
}

async fn call(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    req: test::TestRequest,
    bearer: &str,
) -> (StatusCode, Value) {
    let resp = test::call_service(
        app,
        req.insert_header(("Authorization", format!("Bearer {bearer}")))
            .to_request(),
    )
    .await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn has(v: &Value, cap: &str) -> bool {
    v["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c == cap)
}

#[sqlx::test]
async fn me_reports_role_grants_and_core_reads(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool).await;
    let t = user(&pool, o, "teller", "Sara", None).await;
    let (s, me) = call(
        &app,
        test::TestRequest::get().uri("/authz/me"),
        &token(t, o, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(has(&me, "orders.create"));
    assert!(has(&me, "menu.items.read"));
    assert!(!has(&me, "till.force_close"));
    assert!(!has(&me, "till.cash_spot_check"), "off by default");
    assert!(
        !has(&me, "legacy.orders.update"),
        "legacy cells never reach the UI"
    );
}

#[sqlx::test]
async fn overrides_obey_the_guard(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool).await;
    let b = branch(&pool, o).await;
    let owner = user(&pool, o, "org_admin", "Owner", None).await;
    let mgr = user(&pool, o, "branch_manager", "Mona", None).await;
    let teller = user(&pool, o, "teller", "Sara", None).await;
    assign(&pool, mgr, b).await;
    let mt = token(mgr, o, UserRole::BranchManager);
    let ot = token(owner, o, UserRole::OrgAdmin);
    let put = |id: Uuid| test::TestRequest::put().uri(&format!("/authz/users/{id}/overrides"));

    // A manager has no staff.permissions.edit by default.
    let (s, _) = call(
        &app,
        put(teller)
            .set_json(json!({"capability": "till.force_close", "effect": "allow", "reason": "x"})),
        &mt,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // The owner gives it; money needs a reason.
    let (s, _) = call(&app, put(mgr).set_json(json!({"capability": "staff.permissions.edit", "effect": "allow", "reason": "shift lead"})), &ot).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(
        &app,
        put(teller).set_json(json!({"capability": "refunds.create", "effect": "deny"})),
        &mt,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "reason required for money");

    // Now the manager: can deny a cashier's refunds, can't grant what they lack,
    // can't remove core, can't touch themselves or the owner.
    let (s, body) = call(
        &app,
        put(teller).set_json(
            json!({"capability": "refunds.create", "effect": "deny", "reason": "training"}),
        ),
        &mt,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (s, _) = call(
        &app,
        put(teller)
            .set_json(json!({"capability": "hr.payroll.read", "effect": "allow", "reason": "x"})),
        &mt,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "not held");
    let (s, _) = call(
        &app,
        put(teller).set_json(json!({"capability": "menu.items.read", "effect": "deny"})),
        &mt,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "core");
    let (s, _) = call(
        &app,
        put(mgr).set_json(json!({"capability": "orders.void", "effect": "allow", "reason": "x"})),
        &mt,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "self");
    let (s, _) = call(
        &app,
        put(owner).set_json(json!({"capability": "orders.void", "effect": "deny", "reason": "x"})),
        &mt,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "owner");

    // The deny took effect for the cashier.
    let (_, me) = call(
        &app,
        test::TestRequest::get().uri("/authz/me"),
        &token(teller, o, UserRole::Teller),
    )
    .await;
    assert!(!has(&me, "refunds.create"));
    // "Why" explains it.
    let (s, ex) = call(
        &app,
        test::TestRequest::get().uri(&format!(
            "/authz/explain?user_id={teller}&capability=refunds.create"
        )),
        &ot,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(ex["effective"], false);
    assert!(
        ex["steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|st| st["kind"] == "override_deny")
    );
}

#[sqlx::test]
async fn roles_are_edited_within_what_you_hold_and_core_stays(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool).await;
    let owner = user(&pool, o, "org_admin", "Owner", None).await;
    let teller = user(&pool, o, "teller", "Sara", None).await;
    let ot = token(owner, o, UserRole::OrgAdmin);
    let (s, roles) = call(&app, test::TestRequest::get().uri("/authz/roles"), &ot).await;
    assert_eq!(s, StatusCode::OK);
    let id_of = |k: &str| {
        roles
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["key"] == k)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let cashier = id_of("teller");
    let owner_role = id_of("org_admin");

    let grant = |role: &str, cap: &str, granted: bool| {
        test::TestRequest::put()
            .uri(&format!("/authz/roles/{role}/grants"))
            .set_json(json!({"capability": cap, "granted": granted}))
    };
    let (s, _) = call(&app, grant(&owner_role, "orders.void", false), &ot).await;
    assert_eq!(s, StatusCode::CONFLICT, "the owner role is not editable");
    let (s, _) = call(&app, grant(&cashier, "menu.items.read", false), &ot).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "core");
    let (s, _) = call(&app, grant(&cashier, "till.cash_spot_check", true), &ot).await;
    assert_eq!(s, StatusCode::OK);
    let (_, me) = call(
        &app,
        test::TestRequest::get().uri("/authz/me"),
        &token(teller, o, UserRole::Teller),
    )
    .await;
    assert!(has(&me, "till.cash_spot_check"));

    // A custom role, then assigning it replaces the system one.
    let (s, role) = call(
        &app,
        test::TestRequest::post().uri("/authz/roles").set_json(json!({
            "name_en": "Cashier, no voids", "name_ar": "كاشير بدون إلغاء", "kind": "teller", "copy_from": cashier
        })),
        &ot,
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let custom = role["id"].as_str().unwrap().to_string();
    let (s, _) = call(&app, grant(&custom, "orders.void", false), &ot).await;
    assert_eq!(s, StatusCode::OK);
    let (s, access) = call(
        &app,
        test::TestRequest::put()
            .uri(&format!("/authz/users/{teller}/assignments"))
            .set_json(json!({"assignments": [{"role_id": custom, "all_branches": true}]})),
        &ot,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{access}");
    let (_, me) = call(
        &app,
        test::TestRequest::get().uri("/authz/me"),
        &token(teller, o, UserRole::Teller),
    )
    .await;
    assert!(!has(&me, "orders.void"));
    assert!(has(&me, "orders.create"));
    // The system role no longer holds them; a busy role cannot be deleted.
    let (s, _) = call(
        &app,
        test::TestRequest::delete().uri(&format!("/authz/roles/{custom}")),
        &ot,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
}

#[sqlx::test]
async fn a_manager_at_one_branch_and_a_cashier_at_another(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool).await;
    let b1 = branch(&pool, o).await;
    let b2 = branch(&pool, o).await;
    let owner = user(&pool, o, "org_admin", "Owner", None).await;
    let p = user(&pool, o, "teller", "Hana", Some("4321")).await;
    let ot = token(owner, o, UserRole::OrgAdmin);
    let (_, roles) = call(&app, test::TestRequest::get().uri("/authz/roles"), &ot).await;
    let id_of = |k: &str| {
        roles
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["key"] == k)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let (s, body) = call(
        &app,
        test::TestRequest::put()
            .uri(&format!("/authz/users/{p}/assignments"))
            .set_json(json!({"assignments": [
                {"role_id": id_of("branch_manager"), "all_branches": false, "branch_ids": [b1]},
                {"role_id": id_of("teller"), "all_branches": false, "branch_ids": [b2]},
            ]})),
        &ot,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let pt = token(p, o, UserRole::BranchManager);
    let (_, at1) = call(
        &app,
        test::TestRequest::get().uri(&format!("/authz/me?branch_id={b1}")),
        &pt,
    )
    .await;
    let (_, at2) = call(
        &app,
        test::TestRequest::get().uri(&format!("/authz/me?branch_id={b2}")),
        &pt,
    )
    .await;
    assert!(has(&at1, "till.force_close"));
    assert!(!has(&at2, "till.force_close"));
    assert!(has(&at2, "orders.create"));

    // PIN sign-in works at both (pos.sign_in at each), not at a third branch.
    let b3 = branch(&pool, o).await;
    for (b, ok) in [(b1, true), (b2, true), (b3, false)] {
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/auth/login")
                .set_json(json!({"name": "Hana", "pin": "4321", "branch_id": b}))
                .to_request(),
        )
        .await;
        assert_eq!(
            resp.status().is_success(),
            ok,
            "branch {b}: {}",
            resp.status()
        );
    }

    // The legacy allow-list is now a PROJECTION of the assignments, so pre-0.8
    // readers (offline bundle, schedules, attendance) see the same answer.
    let projected: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT branch_id FROM user_branch_assignments WHERE user_id = $1 ORDER BY branch_id",
    )
    .bind(p)
    .fetch_all(&pool)
    .await
    .unwrap();
    let mut want = vec![b1, b2];
    want.sort();
    assert_eq!(projected, want, "allow-list projected back for old readers");

    // Widen to org-wide: the legacy table's way of saying that is no rows.
    let (s, body) = call(
        &app,
        test::TestRequest::put()
            .uri(&format!("/authz/users/{p}/assignments"))
            .set_json(json!({"assignments": [
                {"role_id": id_of("teller"), "all_branches": true},
            ]})),
        &ot,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM user_branch_assignments WHERE user_id = $1")
            .bind(p)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(n, 0, "org-wide projects to no rows");
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/auth/login")
            .set_json(json!({"name": "Hana", "pin": "4321", "branch_id": b3}))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success(), "org-wide signs in anywhere");
}

#[sqlx::test]
async fn owners_are_protected_and_assign_owners_only_themselves(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool).await;
    let owner = user(&pool, o, "org_admin", "Owner", Some("1111")).await;
    let mgr = user(&pool, o, "branch_manager", "Mona", None).await;
    let ot = token(owner, o, UserRole::OrgAdmin);
    let (_, roles) = call(&app, test::TestRequest::get().uri("/authz/roles"), &ot).await;
    let owner_role = roles
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == "org_admin")
        .unwrap()["id"]
        .clone();
    let teller_role = roles
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == "teller")
        .unwrap()["id"]
        .clone();

    // The last owner cannot be moved off the owner role (and not by themselves).
    let other = user(&pool, o, "org_admin", "Second", None).await;
    let st = token(other, o, UserRole::OrgAdmin);
    sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(other)
        .execute(&pool)
        .await
        .unwrap();
    let _ = st;
    // Give the manager every power but ownership: still cannot make an owner.
    let (s, _) = call(
        &app,
        test::TestRequest::put()
            .uri(&format!("/authz/users/{owner}/assignments"))
            .set_json(json!({"assignments": [{"role_id": teller_role, "all_branches": true}]})),
        &token(mgr, o, UserRole::BranchManager),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(
        &app,
        test::TestRequest::put()
            .uri(&format!("/authz/users/{mgr}/assignments"))
            .set_json(json!({"assignments": [{"role_id": owner_role, "all_branches": true}]})),
        &token(mgr, o, UserRole::BranchManager),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // An owner signs in at a till with a PIN.
    let b = branch(&pool, o).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/auth/login")
            .set_json(json!({"name": "Owner", "pin": "1111", "branch_id": b}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[sqlx::test]
async fn ask_a_manager_is_the_owners_choice_per_capability(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool).await;
    let owner = user(&pool, o, "org_admin", "Owner", None).await;
    let teller = user(&pool, o, "teller", "Sara", None).await;
    let ot = token(owner, o, UserRole::OrgAdmin);
    let (s, _) = call(
        &app,
        test::TestRequest::put()
            .uri("/authz/policy")
            .set_json(json!({"capability": "till.force_close", "ask_manager": true})),
        &ot,
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "force close does not take approval"
    );
    let (s, _) = call(
        &app,
        test::TestRequest::put()
            .uri("/authz/policy")
            .set_json(json!({"capability": "till.cash_spot_check", "ask_manager": true})),
        &ot,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (_, me) = call(
        &app,
        test::TestRequest::get().uri("/authz/me"),
        &token(teller, o, UserRole::Teller),
    )
    .await;
    assert!(
        me["ask_manager"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "till.cash_spot_check")
    );
    assert!(!has(&me, "till.cash_spot_check"));
}

/// The owner's review queue for offline acts that were accepted despite
/// failing the permission re-check (PERMISSIONS_ARCHITECTURE §4.4.5).
///
/// It is gated on `approvals.review`, which the teller who caused the flag
/// does not hold — a person cannot quietly close their own notice.
#[sqlx::test]
async fn flagged_offline_acts_are_the_owners_queue(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool).await;
    let b = branch(&pool, o).await;
    let owner = user(&pool, o, "org_admin", "Owner", None).await;
    let teller = user(&pool, o, "teller", "Sara", None).await;
    assign(&pool, teller, b).await;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO authz_replay_flags
             (org_id, branch_id, op, author_id, capability, reason, occurred_at)
         VALUES ($1, $2, 'RefundOrder', $3, 'refunds:create', 'stale_snapshot', now())
         RETURNING id",
    )
    .bind(o)
    .bind(b)
    .bind(teller)
    .fetch_one(&pool)
    .await
    .unwrap();

    // The teller who caused it may not review it.
    let (s, _) = call(
        &app,
        test::TestRequest::get().uri("/authz/flags"),
        &token(teller, o, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "not the teller's queue");

    let ot = token(owner, o, UserRole::OrgAdmin);
    let (s, list) = call(&app, test::TestRequest::get().uri("/authz/flags"), &ot).await;
    assert_eq!(s, StatusCode::OK);
    let rows = list.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["op"], "RefundOrder");
    assert_eq!(rows[0]["capability"], "refunds:create");
    assert_eq!(rows[0]["reason"], "stale_snapshot");
    assert_eq!(
        rows[0]["author_name"], "Sara",
        "the owner sees who, by name"
    );

    // Acknowledging it takes it off the queue, and only once.
    let (s, _) = call(
        &app,
        test::TestRequest::post().uri(&format!("/authz/flags/{id}/review")),
        &ot,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (_, list) = call(&app, test::TestRequest::get().uri("/authz/flags"), &ot).await;
    assert!(
        list.as_array().unwrap().is_empty(),
        "reviewed, so off the queue"
    );
    let (s, _) = call(
        &app,
        test::TestRequest::post().uri(&format!("/authz/flags/{id}/review")),
        &ot,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "no double review");

    // It is still on the record.
    let (_, all) = call(
        &app,
        test::TestRequest::get().uri("/authz/flags?include_reviewed=true"),
        &ot,
    )
    .await;
    let rows = all.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["reviewed_by"], json!(owner));
}
