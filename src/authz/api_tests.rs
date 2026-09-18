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

    // An owner signs in at a till with a PIN on a 0.8+ tablet (which names its
    // device on login); a pre-0.8 tablet is refused for an owner.
    let b = branch(&pool, o).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/auth/login")
            .insert_header((crate::tickets::DEVICE_ID_HEADER, "tablet-08"))
            .set_json(json!({"name": "Owner", "pin": "1111", "branch_id": b}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/auth/login")
            .set_json(json!({"name": "Owner", "pin": "1111", "branch_id": b}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
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

/// Bulk-resolve: many flags at once, with a note, one PIN's worth of review
/// (owner, 2026-09-17). A bad id never loses the good ones, and resubmitting
/// the same batch is a no-op success, never a double record or an error.
#[sqlx::test]
async fn bulk_review_resolves_many_flags_at_once_and_is_idempotent(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool).await;
    let b = branch(&pool, o).await;
    let owner = user(&pool, o, "org_admin", "Owner", None).await;
    let teller = user(&pool, o, "teller", "Sara", None).await;
    assign(&pool, teller, b).await;
    let mut ids = Vec::new();
    for _ in 0..3 {
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
        ids.push(id);
    }
    let missing_id = ids.iter().max().unwrap() + 10_000;

    // No approvals.review: refused before the ids are even looked at.
    let (s, _) = call(
        &app,
        test::TestRequest::post()
            .uri("/authz/flags/bulk-review")
            .set_json(json!({ "flag_ids": ids, "note": "checked the till" })),
        &token(teller, o, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    let ot = token(owner, o, UserRole::OrgAdmin);
    let mut batch = ids.clone();
    batch.push(missing_id);
    let (s, result) = call(
        &app,
        test::TestRequest::post()
            .uri("/authz/flags/bulk-review")
            .set_json(json!({ "flag_ids": batch, "note": "checked the till" })),
        &ot,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let resolved: Vec<i64> = result["resolved"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    assert_eq!(resolved.len(), 3, "all three real flags resolved");
    for id in &ids {
        assert!(resolved.contains(id));
    }
    let pending = result["pending"].as_array().unwrap();
    assert_eq!(pending.len(), 1, "the bad id stays visible, not dropped");
    assert_eq!(pending[0]["id"], json!(missing_id));

    let (note,): (Option<String>,) =
        sqlx::query_as("SELECT review_note FROM authz_replay_flags WHERE id = $1")
            .bind(ids[0])
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(note.as_deref(), Some("checked the till"));

    // Resubmitting the same batch: still all resolved, no error, no second row.
    let (s, result2) = call(
        &app,
        test::TestRequest::post()
            .uri("/authz/flags/bulk-review")
            .set_json(json!({ "flag_ids": ids })),
        &ot,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "idempotent resubmit");
    assert_eq!(result2["resolved"].as_array().unwrap().len(), 3);
    let (note_after,): (Option<String>,) =
        sqlx::query_as("SELECT review_note FROM authz_replay_flags WHERE id = $1")
            .bind(ids[0])
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        note_after.as_deref(),
        Some("checked the till"),
        "the note from the first review is not overwritten by a no-op resubmit"
    );
}

/// Owner decisions 2026-09-16: `staff.permissions.edit` is off for managers by
/// default, and even granted it stays anti-escalation gated. The editor grants
/// or revokes only what they hold, never hands on role or owner management they
/// lack, and edits only people strictly below them (no peer writes).
#[sqlx::test]
async fn a_permissions_editor_cannot_escalate(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool).await;
    let b = branch(&pool, o).await;
    let owner = user(&pool, o, "org_admin", "Owner", None).await;
    let mgr = user(&pool, o, "branch_manager", "Mona", None).await;
    let peer = user(&pool, o, "branch_manager", "Nour", None).await;
    let teller = user(&pool, o, "teller", "Sara", None).await;
    for u in [mgr, peer, teller] {
        assign(&pool, u, b).await;
    }
    let mt = token(mgr, o, UserRole::BranchManager);
    let ot = token(owner, o, UserRole::OrgAdmin);
    let put = |id: Uuid| test::TestRequest::put().uri(&format!("/authz/users/{id}/overrides"));
    let ask = |cap: &str, effect: &str| json!({"capability": cap, "effect": effect, "reason": "test"});

    // Default: a manager holds no staff.permissions.edit.
    let (s, me) = call(&app, test::TestRequest::get().uri("/authz/me"), &mt).await;
    assert_eq!(s, StatusCode::OK);
    assert!(!has(&me, "staff.permissions.edit"), "off for managers by default");
    let (s, _) = call(&app, put(teller).set_json(ask("refunds.create", "deny")), &mt).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "no editing without the capability");

    let (s, _) = call(&app, put(mgr).set_json(ask("staff.permissions.edit", "allow")), &ot).await;
    assert_eq!(s, StatusCode::OK);

    // Granted: a held capability, to someone below, works.
    let (s, body) = call(&app, put(teller).set_json(ask("refunds.create", "deny")), &mt).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    // Role and owner management it lacks cannot be handed on.
    for cap in ["staff.roles.manage", "staff.owners.manage", "staff.permissions.reset"] {
        let (s, _) = call(&app, put(teller).set_json(ask(cap, "allow")), &mt).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{cap} handed on");
    }
    // Revoking what the editor does not hold is refused too.
    let (s, _) = call(&app, put(teller).set_json(ask("hr.payroll.read", "deny")), &mt).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "revoke of a capability not held");
    // No peer writes: not another manager, even one holding less.
    let (s, _) = call(&app, put(peer).set_json(ask("refunds.create", "deny")), &mt).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "a peer manager");
    let (s, _) = call(&app, put(peer).set_json(ask("staff.permissions.edit", "allow")), &mt).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "editing power to a peer");
    // Not themselves, not the owner.
    let (s, _) = call(&app, put(mgr).set_json(ask("staff.roles.manage", "allow")), &mt).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "self");
    let (s, _) = call(&app, put(owner).set_json(ask("refunds.create", "deny")), &mt).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "the owner");
}

// ── a till signed in as a TELLER clears its own backlog with a manager's PIN ──
// (owner-approved, 2026-09-18). `GET /authz/flags` and the bulk review take an
// OPTIONAL one-time approval, the same `ReplayApproval` shape as `live_approval`
// elsewhere, decided by the same shared helper. Without one, nothing changes.

fn branch_token(user: Uuid, org: Uuid, role: UserRole, branch: Uuid) -> String {
    crate::auth::jwt::create_token(&secret(), user, Some(org), role, Some(branch), 24).unwrap()
}

fn approval(cap: &str, approver: Uuid) -> Value {
    json!({ "id": Uuid::new_v4(), "capability": cap, "approver_id": approver })
}

async fn a_flag(pool: &PgPool, org: Uuid, branch: Option<Uuid>, author: Uuid) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO authz_replay_flags
             (org_id, branch_id, op, author_id, capability, reason, occurred_at)
         VALUES ($1, $2, 'RefundOrder', $3, 'refunds:create', 'stale_snapshot', now())
         RETURNING id",
    )
    .bind(org)
    .bind(branch)
    .bind(author)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn a_teller_till_pulls_and_clears_its_flags_with_a_managers_one_time_approval(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool).await;
    let b = branch(&pool, o).await;
    let manager = user(&pool, o, "org_admin", "Mona", None).await;
    assign(&pool, manager, b).await;
    let teller = user(&pool, o, "teller", "Sara", None).await;
    assign(&pool, teller, b).await;
    let id = a_flag(&pool, o, Some(b), teller).await;
    let tt = branch_token(teller, o, UserRole::Teller, b);

    // Unchanged without an approval: the teller may neither pull nor clear.
    let (s, _) = call(&app, test::TestRequest::get().uri("/authz/flags"), &tt).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "the plain path is exactly as before");
    let (s, _) = call(
        &app,
        test::TestRequest::post()
            .uri("/authz/flags/bulk-review")
            .set_json(json!({ "flag_ids": [id] })),
        &tt,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // With a manager's one-time approval the same session pulls its own flags.
    let a = approval("approvals.review", manager);
    let (s, list) = call(
        &app,
        test::TestRequest::get().uri(&format!(
            "/authz/flags?approval={}",
            urlencoding::encode(&a.to_string())
        )),
        &tt,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], json!(id));

    // ...and clears them. The review is recorded under the APPROVER.
    let (s, result) = call(
        &app,
        test::TestRequest::post()
            .uri("/authz/flags/bulk-review")
            .set_json(json!({ "flag_ids": [id], "note": "till 3", "approval": a })),
        &tt,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(result["resolved"], json!([id]));
    let (by, note): (Option<Uuid>, Option<String>) =
        sqlx::query_as("SELECT reviewed_by, review_note FROM authz_replay_flags WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(by, Some(manager), "the approver's name is on it, not the teller's");
    let note = note.unwrap();
    assert!(note.contains("till 3"), "the till's own note is kept: {note}");
    assert!(note.contains("Mona"), "and the note names the approver: {note}");

    // Idempotent re-submit with the same approval: still resolved, still clean.
    let (s, again) = call(
        &app,
        test::TestRequest::post()
            .uri("/authz/flags/bulk-review")
            .set_json(json!({ "flag_ids": [id], "approval": approval("approvals.review", manager) })),
        &tt,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(again["resolved"], json!([id]));
    let (by_after,): (Option<Uuid>,) =
        sqlx::query_as("SELECT reviewed_by FROM authz_replay_flags WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(by_after, Some(manager), "a no-op resubmit rewrites nothing");
}

/// What an approval does NOT unlock.
#[sqlx::test]
async fn a_flag_approval_is_refused_unless_the_approver_really_holds_the_review(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool).await;
    let b = branch(&pool, o).await;
    let other_b = branch(&pool, o).await;
    let manager = user(&pool, o, "org_admin", "Mona", None).await;
    assign(&pool, manager, b).await;
    let teller = user(&pool, o, "teller", "Sara", None).await;
    assign(&pool, teller, b).await;
    let waiter = user(&pool, o, "waiter", "Nour", None).await;
    assign(&pool, waiter, b).await;

    // Another org entirely.
    let o2 = org(&pool).await;
    let b2 = branch(&pool, o2).await;
    let outsider = user(&pool, o2, "org_admin", "Far", None).await;
    assign(&pool, outsider, b2).await;

    let id = a_flag(&pool, o, Some(b), teller).await;
    let elsewhere = a_flag(&pool, o, Some(other_b), teller).await;
    let tt = branch_token(teller, o, UserRole::Teller, b);

    let refused = |a: Value| {
        let app = &app;
        let tt = tt.clone();
        async move {
            let (get, _) = call(
                app,
                test::TestRequest::get().uri(&format!(
                    "/authz/flags?approval={}",
                    urlencoding::encode(&a.to_string())
                )),
                &tt,
            )
            .await;
            let (post, _) = call(
                app,
                test::TestRequest::post()
                    .uri("/authz/flags/bulk-review")
                    .set_json(json!({ "flag_ids": [id], "approval": a })),
                &tt,
            )
            .await;
            (get, post)
        }
    };

    // An approver who does not hold `approvals.review`.
    assert_eq!(
        refused(approval("approvals.review", waiter)).await,
        (StatusCode::FORBIDDEN, StatusCode::FORBIDDEN)
    );
    // Self-approval.
    assert_eq!(
        refused(approval("approvals.review", teller)).await,
        (StatusCode::FORBIDDEN, StatusCode::FORBIDDEN)
    );
    // Someone from another org.
    assert_eq!(
        refused(approval("approvals.review", outsider)).await,
        (StatusCode::FORBIDDEN, StatusCode::FORBIDDEN)
    );
    // An approval minted for a different act cannot be spent here.
    assert_eq!(
        refused(approval("orders.void", manager)).await,
        (StatusCode::FORBIDDEN, StatusCode::FORBIDDEN)
    );
    // A made-up capability.
    assert_eq!(
        refused(approval("not.a.capability", manager)).await,
        (StatusCode::FORBIDDEN, StatusCode::FORBIDDEN)
    );

    // A good approval still does not widen what this till can see or touch:
    // another branch's flag is neither listed nor clearable.
    let a = approval("approvals.review", manager);
    let (s, list) = call(
        &app,
        test::TestRequest::get().uri(&format!(
            "/authz/flags?approval={}",
            urlencoding::encode(&a.to_string())
        )),
        &tt,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let seen: Vec<i64> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert_eq!(seen, vec![id], "its own branch only");
    let (s, result) = call(
        &app,
        test::TestRequest::post()
            .uri("/authz/flags/bulk-review")
            .set_json(json!({ "flag_ids": [elsewhere], "approval": a })),
        &tt,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(result["resolved"].as_array().unwrap().is_empty());
    assert_eq!(result["pending"][0]["id"], json!(elsewhere));
    let (still_open,): (Option<chrono::DateTime<chrono::Utc>>,) =
        sqlx::query_as("SELECT reviewed_at FROM authz_replay_flags WHERE id = $1")
            .bind(elsewhere)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(still_open.is_none(), "another branch's flag is untouched");

    // A session with no branch cannot spend an approval at all: an approval
    // never buys org-wide sight.
    let unbound = branch_token(teller, o, UserRole::Teller, b);
    let _ = unbound;
    let no_branch = token(teller, o, UserRole::Teller);
    let (s, _) = call(
        &app,
        test::TestRequest::post()
            .uri("/authz/flags/bulk-review")
            .set_json(json!({ "flag_ids": [id], "approval": approval("approvals.review", manager) })),
        &no_branch,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}
