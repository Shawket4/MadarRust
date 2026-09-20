//! Waste from the till: the live route's permission gate, the ledger math, the
//! recipe explosion, and replay's accept-and-flag / approval / idempotency.

use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::{JwtSecret, create_token};
use madar_rust::inventory::handlers::StockMovement;
use madar_rust::inventory::waste::WasteRecorded;
use madar_rust::models::UserRole;
use madar_rust::realtime::hub::BranchEventHub;

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn token(uid: Uuid, org: Uuid, role: UserRole) -> String {
    create_token(&secret(), uid, Some(org), role, None, 24).unwrap()
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Waste Org', $2)")
        .bind(id)
        .bind(format!("waste-{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_branch(pool: &PgPool, org: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(org)
        .bind(format!("Branch {id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_user(pool: &PgPool, org: Uuid, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, $3, $4, 'h', $5::user_role)",
    )
    .bind(id)
    .bind(org)
    .bind(format!("{role}-{id}"))
    .bind(format!("{id}@t.com"))
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// An org ingredient in grams costing `cost` piastres per gram.
async fn seed_ingredient(pool: &PgPool, org: Uuid, name: &str, unit: &str, cost: f64) -> Uuid {
    let id = Uuid::new_v4();
    let cat: Uuid = sqlx::query_scalar("SELECT ingredient_category_id($1, 'general')")
        .bind(org)
        .fetch_one(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO org_ingredients (id, org_id, name, unit, category_id, cost_per_unit) \
         VALUES ($1, $2, $3, $4::inventory_unit, $5, $6)",
    )
    .bind(id)
    .bind(org)
    .bind(name)
    .bind(unit)
    .bind(cat)
    .bind(rust_decimal::Decimal::try_from(cost).unwrap())
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn stock_in(pool: &PgPool, branch: Uuid, ing: Uuid, qty: f64) {
    sqlx::query(
        "INSERT INTO inventory_movements (branch_id, org_ingredient_id, type, quantity, source_type) \
         VALUES ($1, $2, 'purchase_in', $3, 'seed')",
    )
    .bind(branch)
    .bind(ing)
    .bind(qty)
    .execute(pool)
    .await
    .unwrap();
}

async fn on_hand(pool: &PgPool, branch: Uuid, ing: Uuid) -> f64 {
    sqlx::query_scalar(
        "SELECT COALESCE((SELECT on_hand::float8 FROM branch_stock WHERE branch_id = $1 AND org_ingredient_id = $2), 0)",
    )
    .bind(branch)
    .bind(ing)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn waste_movements(pool: &PgPool, id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM inventory_movements WHERE source_type = 'waste' AND source_id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn flags(pool: &PgPool, author: Uuid) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT capability, reason FROM authz_replay_flags WHERE author_id = $1 AND op = 'RecordWaste' ORDER BY id",
    )
    .bind(author)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// `inventory.waste.record` (49) for one person, with an optional value limit.
async fn allow_waste(pool: &PgPool, org: Uuid, user: Uuid, max_value: Option<i64>) {
    let limits = max_value.map(|v| json!({ "max_value": v }));
    sqlx::query(
        "INSERT INTO user_overrides (org_id, user_id, capability_id, effect, limits, reason) \
         VALUES ($1, $2, 49, 'allow', $3, 'test')",
    )
    .bind(org)
    .bind(user)
    .bind(limits)
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
                .app_data(web::Data::new(BranchEventHub::new()))
                .configure(madar_rust::inventory::routes::configure)
                .configure(madar_rust::sync::routes::configure),
        )
        .await
    };
}

fn waste_body(
    id: Uuid,
    branch: Uuid,
    kind: &str,
    subject: Uuid,
    qty: f64,
    unit: Option<&str>,
) -> Value {
    json!({
        "id": id, "branch_id": branch, "subject_kind": kind, "subject_id": subject,
        "quantity": qty, "unit": unit, "reason": "spoiled", "note": "dropped",
        "occurred_at": "2026-09-17T08:00:00Z"
    })
}

#[sqlx::test]
async fn the_live_route_refuses_without_the_capability_before_reading_the_body(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let ing = seed_ingredient(&pool, org, "Milk", "ml", 0.05).await;

    // Off for tellers by default: 403, and a nonsense body is not a 400 first.
    for body in [
        json!({}),
        waste_body(Uuid::new_v4(), branch, "ingredient", ing, 100.0, None),
    ] {
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/inventory/waste")
                .insert_header((
                    "Authorization",
                    format!("Bearer {}", token(teller, org, UserRole::Teller)),
                ))
                .set_json(&body)
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 403);
    }
    assert_eq!(on_hand(&pool, branch, ing).await, 0.0);
}

#[sqlx::test]
async fn an_ingredient_waste_moves_the_ledger_once_in_its_own_unit(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let admin = seed_user(&pool, org, "org_admin").await;
    let beans = seed_ingredient(&pool, org, "Beans", "g", 2.0).await;
    stock_in(&pool, branch, beans, 2000.0).await;
    let bearer = token(admin, org, UserRole::OrgAdmin);
    let id = Uuid::new_v4();
    let body = waste_body(id, branch, "ingredient", beans, 0.5, Some("kg"));

    let post = |b: Value| {
        test::TestRequest::post()
            .uri("/inventory/waste")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(b)
            .to_request()
    };
    let resp = test::call_service(&app, post(body.clone())).await;
    assert_eq!(resp.status(), 201);
    let w: WasteRecorded = test::read_body_json(resp).await;
    assert_eq!(w.source, "dashboard");
    assert_eq!(w.lines.len(), 1);
    assert_eq!(
        w.lines[0].quantity, -500.0,
        "0.5 kg is 500 g of a gram ingredient"
    );
    assert_eq!(w.value_minor, Some(1000), "500 g × 2 piastres");
    assert_eq!(on_hand(&pool, branch, beans).await, 1500.0);

    // The same id again: nothing new is posted.
    let resp = test::call_service(&app, post(body)).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(on_hand(&pool, branch, beans).await, 1500.0);
    assert_eq!(waste_movements(&pool, id).await, 1);

    // A family that does not convert is refused.
    let resp = test::call_service(
        &app,
        post(waste_body(
            Uuid::new_v4(),
            branch,
            "ingredient",
            beans,
            1.0,
            Some("ml"),
        )),
    )
    .await;
    assert_eq!(resp.status(), 400);
}

#[sqlx::test]
async fn a_menu_item_waste_explodes_through_its_recipe(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let admin = seed_user(&pool, org, "org_admin").await;
    let beans = seed_ingredient(&pool, org, "Beans", "g", 2.0).await;
    let milk = seed_ingredient(&pool, org, "Milk", "ml", 0.1).await;
    stock_in(&pool, branch, beans, 1000.0).await;
    stock_in(&pool, branch, milk, 5000.0).await;
    let item = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO menu_items (id, org_id, name, base_price) VALUES ($1, $2, 'Latte', 5000)",
    )
    .bind(item)
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    for (ing, name, unit, qty) in [(beans, "Beans", "g", 18.0), (milk, "Milk", "ml", 200.0)] {
        sqlx::query(
            "INSERT INTO menu_item_recipes (menu_item_id, size_label, quantity_used, ingredient_name, ingredient_unit, org_ingredient_id) \
             VALUES ($1, 'one_size', $2, $3, $4, $5)",
        )
        .bind(item)
        .bind(rust_decimal::Decimal::try_from(qty).unwrap())
        .bind(name)
        .bind(unit)
        .bind(ing)
        .execute(&pool)
        .await
        .unwrap();
    }
    let bearer = token(admin, org, UserRole::OrgAdmin);
    let id = Uuid::new_v4();
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/inventory/waste")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(waste_body(id, branch, "menu_item", item, 3.0, None))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let w: WasteRecorded = test::read_body_json(resp).await;
    assert_eq!(w.subject_name, "Latte");
    assert_eq!(w.lines.len(), 2);
    // 3 × (18 g × 2 + 200 ml × 0.1) = 3 × 56 = 168
    assert_eq!(w.value_minor, Some(168));
    assert_eq!(on_hand(&pool, branch, beans).await, 1000.0 - 54.0);
    assert_eq!(on_hand(&pool, branch, milk).await, 5000.0 - 600.0);

    // An item with no recipe has nothing to waste.
    let bare = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO menu_items (id, org_id, name, base_price) VALUES ($1, $2, 'Water', 500)",
    )
    .bind(bare)
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/inventory/waste")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(waste_body(
                Uuid::new_v4(),
                branch,
                "menu_item",
                bare,
                1.0,
                None,
            ))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400);

    // The log shows who, what and where from.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/branches/{branch}/waste"))
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let log: Vec<StockMovement> = test::read_body_json(resp).await;
    assert_eq!(log.len(), 2);
    assert!(
        log.iter()
            .all(|m| m.waste_subject_name.as_deref() == Some("Latte")
                && m.waste_source.as_deref() == Some("dashboard")
                && m.waste_value_minor == Some(168))
    );
}

#[sqlx::test]
async fn the_live_route_refuses_a_waste_over_the_persons_limit(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    allow_waste(&pool, org, teller, Some(500)).await;
    let beans = seed_ingredient(&pool, org, "Beans", "g", 2.0).await;
    let bearer = token(teller, org, UserRole::Teller);
    let call = |qty: f64| {
        test::TestRequest::post()
            .uri("/inventory/waste")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(waste_body(
                Uuid::new_v4(),
                branch,
                "ingredient",
                beans,
                qty,
                None,
            ))
            .to_request()
    };
    assert_eq!(
        test::call_service(&app, call(250.0)).await.status(),
        201,
        "500 is within 500"
    );
    assert_eq!(
        test::call_service(&app, call(251.0)).await.status(),
        403,
        "502 is over"
    );
    assert_eq!(
        on_hand(&pool, branch, beans).await,
        -250.0,
        "waste may go below zero"
    );
}

/// The live waste route now takes a manager's one-time PIN unlock (owner,
/// 2026-09-17), the same rule `verify_approval` already checks at replay: a
/// manager holding `inventory.waste.record` unlocks an over-limit waste live;
/// an approver who doesn't hold it does not.
#[sqlx::test]
async fn a_live_waste_over_the_limit_with_a_managers_pin_is_allowed(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let manager = seed_user(&pool, org, "branch_manager").await;
    allow_waste(&pool, org, teller, Some(500)).await;
    allow_waste(&pool, org, manager, None).await;
    let beans = seed_ingredient(&pool, org, "Beans", "g", 2.0).await;
    let bearer = token(teller, org, UserRole::Teller);
    let mut over = waste_body(Uuid::new_v4(), branch, "ingredient", beans, 1000.0, None);
    let approval_id = Uuid::new_v4();

    // A manager who does NOT hold the capability (denied for this one person,
    // over the "om" role default) doesn't unlock it either.
    let other_manager = seed_user(&pool, org, "branch_manager").await;
    sqlx::query(
        "INSERT INTO user_overrides (org_id, user_id, capability_id, effect, reason) \
         VALUES ($1, $2, 49, 'deny', 'test')",
    )
    .bind(org)
    .bind(other_manager)
    .execute(&pool)
    .await
    .unwrap();
    over["live_approval"] = json!({
        "id": approval_id, "capability": "inventory.waste.record",
        "approver_id": other_manager, "value_minor": 2000,
    });
    let r = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/inventory/waste")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(over.clone())
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), 403, "the approver doesn't hold it either");

    over["live_approval"]["approver_id"] = json!(manager);
    let r = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/inventory/waste")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(over)
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), 201, "{}", r.status());
    let approver: Uuid =
        sqlx::query_scalar("SELECT approver_user_id FROM approvals WHERE id = $1")
            .bind(approval_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(approver, manager);
}

async fn replay(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    bearer: &str,
    op: &Value,
) -> actix_web::dev::ServiceResponse {
    test::call_service(
        app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .insert_header(("X-Madar-Device", Uuid::nil().to_string()))
            .set_json(op)
            .to_request(),
    )
    .await
}

#[sqlx::test]
async fn a_queued_waste_by_someone_without_the_capability_is_accepted_and_flagged(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let beans = seed_ingredient(&pool, org, "Beans", "g", 2.0).await;
    stock_in(&pool, branch, beans, 100.0).await;
    let bearer = token(teller, org, UserRole::Teller);
    let id = Uuid::new_v4();
    let op = json!({
        "op": "record_waste", "teller_id": teller,
        "request": waste_body(id, branch, "ingredient", beans, 40.0, None)
    });

    let r = replay(&app, &bearer, &op).await;
    assert_eq!(r.status(), 201, "the beans are in the bin: accepted");
    assert_eq!(on_hand(&pool, branch, beans).await, 60.0);
    assert_eq!(
        flags(&pool, teller).await,
        vec![(
            "inventory_waste:create".to_string(),
            "unauthorized_offline".to_string()
        )]
    );

    // Idempotent: a re-flushed queue posts nothing and flags nothing more.
    let r = replay(&app, &bearer, &op).await;
    assert_eq!(r.status(), 200);
    assert_eq!(on_hand(&pool, branch, beans).await, 60.0);
    assert_eq!(waste_movements(&pool, id).await, 1);
    let source: String = sqlx::query_scalar("SELECT source FROM waste_events WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(source, "pos");
}

#[sqlx::test]
async fn a_queued_waste_over_the_limit_needs_a_valid_approval_to_be_clean(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let manager = seed_user(&pool, org, "branch_manager").await;
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(manager)
        .bind(branch)
        .execute(&pool)
        .await
        .unwrap();
    allow_waste(&pool, org, teller, Some(100)).await;
    let beans = seed_ingredient(&pool, org, "Beans", "g", 2.0).await;
    let bearer = token(teller, org, UserRole::Teller);

    // Within the limit: clean.
    let small = json!({ "op": "record_waste", "teller_id": teller,
        "request": waste_body(Uuid::new_v4(), branch, "ingredient", beans, 50.0, None) });
    assert_eq!(replay(&app, &bearer, &small).await.status(), 201);
    assert!(flags(&pool, teller).await.is_empty());

    // Over it, with the manager's approval (and a till figure that lies low —
    // the server's own value is what the approver is checked against).
    let approval_id = Uuid::new_v4();
    let approved = json!({ "op": "record_waste", "teller_id": teller,
        "request": waste_body(Uuid::new_v4(), branch, "ingredient", beans, 500.0, None),
        "approval": { "id": approval_id, "capability": "inventory.waste.record",
                      "approver_id": manager, "value_minor": 1 } });
    assert_eq!(replay(&app, &bearer, &approved).await.status(), 201);
    assert!(flags(&pool, teller).await.is_empty(), "approved: clean");
    let (verified, amount): (bool, Option<i64>) =
        sqlx::query_as("SELECT verified, amount_minor FROM approvals WHERE id = $1")
            .bind(approval_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(verified);
    assert_eq!(amount, Some(1));
    let linked: Option<Uuid> =
        sqlx::query_scalar("SELECT approval_id FROM waste_events WHERE approval_id = $1")
            .bind(approval_id)
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert_eq!(linked, Some(approval_id));

    // Over it with an approval by the teller themself: accepted, flagged.
    let self_approved = json!({ "op": "record_waste", "teller_id": teller,
        "request": waste_body(Uuid::new_v4(), branch, "ingredient", beans, 500.0, None),
        "approval": { "id": Uuid::new_v4(), "capability": "inventory.waste.record",
                      "approver_id": teller } });
    assert_eq!(replay(&app, &bearer, &self_approved).await.status(), 201);
    assert_eq!(
        flags(&pool, teller).await,
        vec![(
            "inventory.waste.record:max_value".to_string(),
            "unauthorized_offline".to_string()
        )]
    );
    assert_eq!(on_hand(&pool, branch, beans).await, -1050.0);
}

/// The log is ordered by when the waste HAPPENED, not when the server got it:
/// a waste queued offline at 07:00 and posted after one made at 09:00 lists
/// below it, and carries its receive time separately.
#[sqlx::test]
async fn the_log_orders_by_when_the_waste_happened(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let admin = seed_user(&pool, org, "org_admin").await;
    let beans = seed_ingredient(&pool, org, "Beans", "g", 2.0).await;
    stock_in(&pool, branch, beans, 2000.0).await;
    let bearer = token(admin, org, UserRole::OrgAdmin);

    let mut ids = Vec::new();
    // Posted first, happened later.
    for at in ["2026-09-17T09:00:00Z", "2026-09-17T07:00:00Z"] {
        let id = Uuid::new_v4();
        let mut body = waste_body(id, branch, "ingredient", beans, 10.0, None);
        body["occurred_at"] = json!(at);
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/inventory/waste")
                .insert_header(("Authorization", format!("Bearer {bearer}")))
                .set_json(body)
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 201);
        ids.push(id);
    }
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/branches/{branch}/waste"))
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let log: Vec<StockMovement> = test::read_body_json(resp).await;
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].source_id, Some(ids[0]), "09:00 first");
    assert_eq!(
        log[1].source_id,
        Some(ids[1]),
        "07:00 below it, though posted later"
    );
    assert_eq!(
        log[1].occurred_at.unwrap().to_rfc3339(),
        "2026-09-17T07:00:00+00:00"
    );
    assert!(log[1].received_at.unwrap() > log[1].occurred_at.unwrap());
}

/// Set an ingredient's org cost directly, including the values a well-behaved
/// UI would never send (unknown, negative).
async fn set_cost(pool: &PgPool, ing: Uuid, cost: Option<f64>) {
    sqlx::query("UPDATE org_ingredients SET cost_per_unit = $2 WHERE id = $1")
        .bind(ing)
        .bind(cost.map(|c| rust_decimal::Decimal::try_from(c).unwrap()))
        .execute(pool)
        .await
        .unwrap();
}

/// A waste whose worth cannot be worked out (no cost on file) used to be judged
/// as ZERO, which is under every ceiling — so the most expensive kind of waste,
/// the one nobody can put a number on, was the one that recorded itself with no
/// approval at all. It must ask a manager instead. Live and replay, one rule.
#[sqlx::test]
async fn a_waste_whose_value_is_unknown_needs_approval_instead_of_counting_as_zero(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    allow_waste(&pool, org, teller, Some(500)).await;
    let beans = seed_ingredient(&pool, org, "Beans", "g", 2.0).await;
    set_cost(&pool, beans, None).await;
    let bearer = token(teller, org, UserRole::Teller);

    // Live: no approval offered, so it is refused rather than waved through.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/inventory/waste")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(waste_body(
                Uuid::new_v4(),
                branch,
                "ingredient",
                beans,
                10_000.0,
                None,
            ))
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        403,
        "an unjudgeable value must not bypass the cap"
    );
    assert_eq!(on_hand(&pool, branch, beans).await, 0.0, "nothing recorded");

    // Replay: the food is already in the bin, so it is accepted AND flagged.
    let op = json!({ "op": "record_waste", "teller_id": teller,
        "request": waste_body(Uuid::new_v4(), branch, "ingredient", beans, 10_000.0, None) });
    assert_eq!(replay(&app, &bearer, &op).await.status(), 201);
    assert_eq!(
        flags(&pool, teller).await,
        vec![(
            "inventory.waste.record:max_value".to_string(),
            "unauthorized_offline".to_string()
        )],
        "replay judges it by the same rule as the live route"
    );
}

/// A negative unit cost is bad data, and it used to make `value_minor` NEGATIVE
/// — which compares as under every `max_value` ceiling, so an unlimited-size
/// waste recorded itself with no approval. It is refused outright now, on both
/// halves, and nothing reaches the ledger.
#[sqlx::test]
async fn a_waste_with_a_negative_unit_cost_is_refused_and_never_recorded(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    allow_waste(&pool, org, teller, Some(500)).await;
    let beans = seed_ingredient(&pool, org, "Beans", "g", 2.0).await;
    set_cost(&pool, beans, Some(-50.0)).await;
    let bearer = token(teller, org, UserRole::Teller);
    let id = Uuid::new_v4();

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/inventory/waste")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(waste_body(id, branch, "ingredient", beans, 1_000.0, None))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400, "a negative value is not a waste at all");
    assert_eq!(waste_movements(&pool, id).await, 0);
    assert_eq!(on_hand(&pool, branch, beans).await, 0.0);

    // The replay half refuses it too, by the same shared rule.
    let op = json!({ "op": "record_waste", "teller_id": teller,
        "request": waste_body(Uuid::new_v4(), branch, "ingredient", beans, 1_000.0, None) });
    assert_eq!(replay(&app, &bearer, &op).await.status(), 400);
    assert_eq!(on_hand(&pool, branch, beans).await, 0.0);
}
