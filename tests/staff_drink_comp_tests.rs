//! A staff drink is a NORMAL sale whose pooled line is comped — priced by the
//! server, in the order's own transaction.
//!
//! The rule itself is pinned by `staff_comp_vectors_tests.rs`. This suite is
//! about the money around it: that the live path prices the comp and ignores
//! the client's, that tax and an order discount see the charged part only, that
//! replay accepts the till's figure and keeps its own beside it, that nothing
//! is double-counted, that the books count revenue net and cost in full, and
//! that POS v0.5.0–v0.7.12's record-only flow is untouched.

use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::{JwtSecret, create_token};
use madar_rust::models::UserRole;
use madar_rust::realtime::hub::BranchEventHub;

const AT: &str = "2026-09-19T09:00:00Z";
const DAY: &str = "2026-09-19";
const CAP: &str = "orders.staff_drink.record";

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn token(uid: Uuid, org: Uuid, role: UserRole) -> String {
    create_token(&secret(), uid, Some(org), role, None, 24).unwrap()
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(BranchEventHub::new()))
                .configure(madar_rust::orders::routes::configure)
                .configure(madar_rust::tickets::routes::configure)
                .configure(madar_rust::staff_pool::routes::configure)
                .configure(madar_rust::sync::routes::configure)
                .configure(|cfg| {
                    madar_rust::reports::routes::configure(cfg, web::Data::new($pool.clone()))
                }),
        )
        .await
    };
}

/// A shop with one latte: small 60 / medium 75 / large 90, a REQUIRED syrup
/// (vanilla 10 default, hazelnut 15, sugar-free 5) and an OPTIONAL whip 7.
/// Tax 14% exclusive. 10 g of a 1.00/g ingredient per drink.
struct Shop {
    org: Uuid,
    branch: Uuid,
    admin: Uuid,
    teller: Uuid,
    till: Uuid,
    teller_till: Uuid,
    latte: Uuid,
    cake: Uuid,
    vanilla: Uuid,
    hazelnut: Uuid,
    sugarfree: Uuid,
    whip: Uuid,
}

async fn user(pool: &PgPool, org: Uuid, role: &str) -> Uuid {
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

async fn till(pool: &PgPool, branch: Uuid, who: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO tills (branch_id, teller_id, status, opening_cash) \
         VALUES ($1, $2, 'open', 0) RETURNING id",
    )
    .bind(branch)
    .bind(who)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn option(
    pool: &PgPool,
    org: Uuid,
    group: Uuid,
    ing: Uuid,
    name: &str,
    price: i32,
    default: bool,
    sort: i32,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO addon_items (id, org_id, name, type, default_price) VALUES ($1, $2, $3, 'extra', $4)")
        .bind(id)
        .bind(org)
        .bind(name)
        .bind(price)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO modifier_options (id, group_id, name, price, sort, is_default) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(group)
    .bind(name)
    .bind(price)
    .bind(sort)
    .bind(default)
    .execute(pool)
    .await
    .unwrap();
    // 1 g of the 1.00/g ingredient, so a line with picks has a KNOWN cost.
    sqlx::query(
        "INSERT INTO addon_item_ingredients (addon_item_id, org_ingredient_id, quantity_used, ingredient_name, ingredient_unit) \
         VALUES ($1, $2, 1, 'Beans', 'g')",
    )
    .bind(id)
    .bind(ing)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn group(pool: &PgPool, org: Uuid, item: Uuid, name: &str, min: i32, required: bool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO modifier_groups (id, org_id, name, selection_type, min_selections, is_required, legacy_addon_type, effect) \
         VALUES ($1, $2, $3, 'multi', $4, $5, 'extra', 'none')",
    )
    .bind(id)
    .bind(org)
    .bind(name)
    .bind(min)
    .bind(required)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO menu_item_modifier_groups (menu_item_id, group_id) VALUES ($1, $2)")
        .bind(item)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn shop(pool: &PgPool) -> Shop {
    let org = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, tax_rate, tax_inclusive) VALUES ($1, 'Comp Org', $2, 0.14, false)",
    )
    .bind(org)
    .bind(format!("comp-{org}"))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, color, icon, is_cash, is_active) \
         VALUES ($1, 'cash', '#000', 'cash', true, true)",
    )
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    let branch = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name, code, timezone) VALUES ($1, $2, 'Comp', 'CMP', 'Africa/Cairo')")
        .bind(branch)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    let admin = user(pool, org, "org_admin").await;
    let teller = user(pool, org, "teller").await;
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(teller)
        .bind(branch)
        .execute(pool)
        .await
        .unwrap();
    for role in ["org_admin", "teller"] {
        for (r, a) in [
            ("orders", "create"),
            ("orders", "read"),
            ("order_items", "create"),
            ("payments", "create"),
            ("open_tickets", "create"),
            ("open_tickets", "read"),
        ] {
            sqlx::query(
                "INSERT INTO role_permissions (role, resource, action, granted) \
                 VALUES ($1::user_role, $2::permission_resource, $3::permission_action, true) ON CONFLICT DO NOTHING",
            )
            .bind(role)
            .bind(r)
            .bind(a)
            .execute(pool)
            .await
            .unwrap();
        }
    }
    let till_id = till(pool, branch, admin).await;
    let teller_till = till(pool, branch, teller).await;

    let cat: Uuid =
        sqlx::query_scalar("INSERT INTO categories (org_id, name) VALUES ($1, 'Hot') RETURNING id")
            .bind(org)
            .fetch_one(pool)
            .await
            .unwrap();
    let ing: Uuid = sqlx::query_scalar(
        "INSERT INTO org_ingredients (org_id, name, unit, cost_per_unit, category_id) \
         VALUES ($1, 'Beans', 'g', 100, ingredient_category_id($1, 'general')) RETURNING id",
    )
    .bind(org)
    .fetch_one(pool)
    .await
    .unwrap();
    let mut items = Vec::new();
    for name in ["Latte", "Cheesecake"] {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO menu_items (org_id, category_id, name, base_price, is_active) \
             VALUES ($1, $2, $3, 6000, true) RETURNING id",
        )
        .bind(org)
        .bind(cat)
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap();
        items.push(id);
    }
    let latte = items[0];
    sqlx::query("DELETE FROM menu_item_sizes WHERE menu_item_id = $1")
        .bind(latte)
        .execute(pool)
        .await
        .ok();
    for (i, (label, price)) in [("small", 6000), ("medium", 7500), ("large", 9000)]
        .iter()
        .enumerate()
    {
        sqlx::query(
            "INSERT INTO menu_item_sizes (menu_item_id, label, price, sort) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (menu_item_id, label) DO UPDATE SET price = EXCLUDED.price, is_active = true",
        )
        .bind(latte)
        .bind(label)
        .bind(price)
        .bind(i as i32)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO menu_item_recipes (menu_item_id, org_ingredient_id, quantity_used, size_label, ingredient_name, ingredient_unit) \
             VALUES ($1, $2, 10, $3, 'Beans', 'g')",
        )
        .bind(latte)
        .bind(ing)
        .bind(label)
        .execute(pool)
        .await
        .unwrap();
    }
    sqlx::query("DELETE FROM menu_item_sizes WHERE menu_item_id = $1 AND label = 'one_size'")
        .bind(latte)
        .execute(pool)
        .await
        .unwrap();

    let syrup = group(pool, org, latte, "Syrup", 1, true).await;
    let vanilla = option(pool, org, syrup, ing, "Vanilla", 1000, true, 0).await;
    let hazelnut = option(pool, org, syrup, ing, "Hazelnut", 1500, false, 1).await;
    let sugarfree = option(pool, org, syrup, ing, "Sugar free", 500, false, 2).await;
    let extras = group(pool, org, latte, "Extras", 0, false).await;
    let whip = option(pool, org, extras, ing, "Whip", 700, false, 0).await;

    Shop {
        org,
        branch,
        admin,
        teller,
        till: till_id,
        teller_till,
        latte,
        cake: items[1],
        vanilla,
        hazelnut,
        sugarfree,
        whip,
    }
}

async fn set_pool(pool: &PgPool, org: Uuid, allowance: i32, items: &[Uuid]) {
    sqlx::query(
        "INSERT INTO staff_pool_settings (org_id, branch_id, enabled, daily_allowance, eligible_item_ids) \
         VALUES ($1, NULL, true, $2, $3)",
    )
    .bind(org)
    .bind(allowance)
    .bind(items)
    .execute(pool)
    .await
    .unwrap();
}

async fn allow_staff_drink(pool: &PgPool, org: Uuid, who: Uuid) {
    sqlx::query(
        "INSERT INTO user_overrides (org_id, user_id, capability_id, effect, reason) \
         VALUES ($1, $2, 223, 'allow', 'test')",
    )
    .bind(org)
    .bind(who)
    .execute(pool)
    .await
    .unwrap();
}

fn staff(id: Uuid, note: &str) -> Value {
    json!({ "id": id, "note": note })
}

/// A large latte with hazelnut and whip: rings at 90 + 15 + 7 = 112.00, of
/// which the pool gives the small's 60 and the default syrup's 10.
fn big_latte(s: &Shop, staff_drink: Option<Value>) -> Value {
    let mut line = json!({
        "menu_item_id": s.latte, "size_label": "large", "quantity": 1,
        "addons": [{ "addon_item_id": s.hazelnut }, { "addon_item_id": s.whip }]
    });
    if let Some(sd) = staff_drink {
        line["staff_drink"] = sd;
    }
    line
}

fn order(s: &Shop, till: Uuid, items: Vec<Value>) -> Value {
    json!({
        "branch_id": s.branch, "till_id": till, "payment_method": "cash",
        "items": items, "created_at": AT, "idempotency_key": Uuid::new_v4()
    })
}

macro_rules! post {
    ($app:expr, $uri:expr, $bearer:expr, $body:expr) => {
        test::call_service(
            &$app,
            test::TestRequest::post()
                .uri($uri)
                .insert_header(("Authorization", format!("Bearer {}", $bearer)))
                .set_json($body)
                .to_request(),
        )
        .await
    };
}

macro_rules! get_json {
    ($app:expr, $uri:expr, $bearer:expr) => {{
        let resp = test::call_service(
            &$app,
            test::TestRequest::get()
                .uri($uri)
                .insert_header(("Authorization", format!("Bearer {}", $bearer)))
                .to_request(),
        )
        .await;
        assert!(resp.status().is_success(), "{} → {:?}", $uri, resp.status());
        let v: Value = test::read_body_json(resp).await;
        v
    }};
}

async fn used(pool: &PgPool, branch: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT COALESCE(sum(quantity), 0)::bigint FROM staff_drinks WHERE branch_id = $1 AND business_date = $2::date",
    )
    .bind(branch)
    .bind(DAY)
    .fetch_one(pool)
    .await
    .unwrap()
}

type DrinkRow = (
    Option<Uuid>,
    Option<i32>,
    Option<i32>,
    Option<i32>,
    bool,
    bool,
    Option<i32>,
    String,
);

async fn drink(pool: &PgPool, id: Uuid) -> DrinkRow {
    sqlx::query_as(
        "SELECT order_id, comp_minor, extras_minor, comp_minor_reported, overspent, overspent_on_replay, \
                cost_minor, note FROM staff_drinks WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn flags(pool: &PgPool, author: Uuid) -> Vec<(String, String, Option<Uuid>)> {
    sqlx::query_as(
        "SELECT op, capability, subject_id FROM authz_replay_flags WHERE author_id = $1 ORDER BY id",
    )
    .bind(author)
    .fetch_all(pool)
    .await
    .unwrap()
}

fn replay_order(teller: Uuid, request: Value) -> Value {
    json!({ "op": "create_order", "teller_id": teller, "request": request })
}

// ── Live ────────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn a_live_pooled_line_is_priced_by_the_server_and_a_client_comp_is_ignored(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    set_pool(&pool, s.org, 5, &[s.latte]).await;
    let bearer = token(s.admin, s.org, UserRole::OrgAdmin);

    let id = Uuid::new_v4();
    // The client claims the whole drink is free and states a total to match.
    let mut body = order(
        &s,
        s.till,
        vec![big_latte(
            &s,
            Some(json!({ "id": id, "note": " for Sara ", "comp_minor": 11200 })),
        )],
    );
    body["subtotal"] = json!(0);
    body["total_amount"] = json!(0);
    let resp = post!(app, "/orders", bearer, &body);
    assert_eq!(resp.status(), 201);
    let o: Value = test::read_body_json(resp).await;

    // Charged: 30 (large over small) + 5 (hazelnut over vanilla) + 7 (whip).
    assert_eq!(o["subtotal"], 4200);
    assert_eq!(o["tax_amount"], 588, "14% of the CHARGED part only");
    assert_eq!(o["total_amount"], 4788);
    let line = &o["items"][0];
    assert_eq!(
        line["unit_price"], 9000,
        "the normal price stays on the line"
    );
    assert_eq!(line["staff_comp_minor"], 7000);
    assert_eq!(line["staff_drink_id"], json!(id));
    assert_eq!(
        line["line_total"], 3000,
        "the size part of the comp is off the line"
    );
    let addons = line["addons"].as_array().unwrap();
    assert_eq!(addons[0]["unit_price"], 1500);
    assert_eq!(
        addons[0]["staff_comp_minor"], 1000,
        "the default's price, off the pricier pick"
    );
    assert_eq!(addons[0]["line_total"], 500);
    assert_eq!(
        addons[1]["staff_comp_minor"], 0,
        "an optional add-on is never free"
    );
    assert_eq!(addons[1]["line_total"], 700);

    let order_id: Uuid = serde_json::from_value(o["id"].clone()).unwrap();
    let (on, comp, extras, reported, over, _, cost, note) = drink(&pool, id).await;
    assert_eq!(
        on,
        Some(order_id),
        "written with its order, in one transaction"
    );
    assert_eq!((comp, extras, reported), (Some(7000), Some(4200), None));
    assert!(!over);
    assert_eq!(
        cost,
        Some(1200),
        "the drink was made: its cost is recorded, picks included"
    );
    assert_eq!(note, "for Sara");
    // The payment is the charged total, nothing else.
    let paid: i64 =
        sqlx::query_scalar("SELECT sum(amount)::bigint FROM order_payments WHERE order_id = $1")
            .bind(order_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(paid, 4788);
    assert!(
        flags(&pool, s.admin).await.is_empty(),
        "nothing is flagged on the live path"
    );
}

#[sqlx::test]
async fn the_base_configuration_rings_at_zero_and_stays_a_visible_sale(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    set_pool(&pool, s.org, 5, &[s.latte]).await;
    let bearer = token(s.admin, s.org, UserRole::OrgAdmin);

    let body = order(
        &s,
        s.till,
        vec![json!({
            "menu_item_id": s.latte, "size_label": "small", "quantity": 2,
            "addons": [{ "addon_item_id": s.sugarfree }],
            "staff_drink": staff(Uuid::new_v4(), "two for the openers")
        })],
    );
    let resp = post!(app, "/orders", bearer, &body);
    assert_eq!(resp.status(), 201);
    let o: Value = test::read_body_json(resp).await;
    assert_eq!(o["subtotal"], 0);
    assert_eq!(o["total_amount"], 0);
    assert!(
        o["order_number"].as_i64().unwrap() >= 1,
        "a normal order number"
    );
    // A cheaper pick is free and earns no credit: 2 × (60 + 5), not 2 × (60 + 10).
    assert_eq!(o["items"][0]["staff_comp_minor"], 13000);
    assert_eq!(
        used(&pool, s.branch).await,
        2,
        "two units are two drinks off the allowance"
    );
    // Stock still came off: the drink was made.
    let moved: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM inventory_movements WHERE source_type = 'order' AND type = 'sale'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(moved >= 1);
}

#[sqlx::test]
async fn an_order_discount_applies_after_the_comp_to_what_remains(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    set_pool(&pool, s.org, 5, &[s.latte]).await;
    let bearer = token(s.admin, s.org, UserRole::OrgAdmin);

    let mut body = order(
        &s,
        s.till,
        vec![big_latte(&s, Some(staff(Uuid::new_v4(), "Omar")))],
    );
    body["discount_type"] = json!("percentage");
    body["discount_value"] = json!(0.1);
    // A till that took 10% of the UNCOMPED basket: not believed.
    body["discount_amount"] = json!(1120);
    let resp = post!(app, "/orders", bearer, &body);
    assert_eq!(resp.status(), 201, "{:?}", test::read_body(resp).await);
    let o: Value = test::read_body_json(post!(app, "/orders", bearer, &body)).await;
    assert_eq!(o["subtotal"], 4200);
    assert_eq!(
        o["discount_amount"], 420,
        "10% of what remained after the comp"
    );
    assert_eq!(o["tax_amount"], 529, "14% of 37.80");
    assert_eq!(o["total_amount"], 4309);
}

#[sqlx::test]
async fn the_live_path_refuses_by_the_pools_own_tokens(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let bearer = token(s.admin, s.org, UserRole::OrgAdmin);
    let plain = |item: Uuid, note: &str| json!({ "menu_item_id": item, "quantity": 1, "staff_drink": staff(Uuid::new_v4(), note) });

    // No settings at all: the pool is off.
    let resp = post!(
        app,
        "/orders",
        bearer,
        &order(&s, s.till, vec![plain(s.latte, "Sara")])
    );
    assert_eq!(resp.status(), 400);
    let e: Value = test::read_body_json(resp).await;
    assert_eq!(e["code"], "pool_off", "{e}");

    set_pool(&pool, s.org, 5, &[s.latte]).await;
    for (line, code) in [
        (plain(s.cake, "Sara"), "item_not_eligible"),
        (plain(s.latte, "   "), "note_required"),
    ] {
        let resp = post!(app, "/orders", bearer, &order(&s, s.till, vec![line]));
        assert_eq!(resp.status(), 400);
        let e: Value = test::read_body_json(resp).await;
        assert_eq!(e["code"], code, "{e}");
    }

    // A bundle is never on the pool.
    let bundle: Uuid = sqlx::query_scalar(
        "INSERT INTO bundles (org_id, name, price, status) VALUES ($1, 'Duo', 2000, 'active') RETURNING id",
    )
    .bind(s.org)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO bundle_components (bundle_id, item_id) VALUES ($1, $2)")
        .bind(bundle)
        .bind(s.cake)
        .execute(&pool)
        .await
        .unwrap();
    let line =
        json!({ "bundle_id": bundle, "quantity": 1, "staff_drink": staff(Uuid::new_v4(), "Sara") });
    let resp = post!(app, "/orders", bearer, &order(&s, s.till, vec![line]));
    assert_eq!(resp.status(), 400);
    let e: Value = test::read_body_json(resp).await;
    assert_eq!(e["code"], "item_not_eligible", "{e}");

    let orders: i64 = sqlx::query_scalar("SELECT count(*) FROM orders")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        orders, 0,
        "a refused staff line refuses the sale: nothing was rung"
    );
    assert_eq!(used(&pool, s.branch).await, 0);
}

#[sqlx::test]
async fn a_teller_without_the_grant_is_refused_live_and_a_managers_pin_unlocks_it(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    set_pool(&pool, s.org, 5, &[s.latte]).await;
    let bearer = token(s.teller, s.org, UserRole::Teller);

    let mut body = order(
        &s,
        s.teller_till,
        vec![big_latte(&s, Some(staff(Uuid::new_v4(), "Sara")))],
    );
    let resp = post!(app, "/orders", bearer, &body);
    assert_eq!(resp.status(), 403, "the same 403 the record endpoint gives");

    // The same sale WITHOUT the staff line is an ordinary sale.
    let resp = post!(
        app,
        "/orders",
        bearer,
        &order(&s, s.teller_till, vec![big_latte(&s, None)])
    );
    assert_eq!(resp.status(), 201);

    // approval = true: a manager who holds the act unlocks it on the spot.
    body["live_approval"] =
        json!({ "id": Uuid::new_v4(), "capability": CAP, "approver_id": s.admin });
    let resp = post!(app, "/orders", bearer, &body);
    assert_eq!(resp.status(), 201, "{:?}", test::read_body(resp).await);
    let approvals: i64 =
        sqlx::query_scalar("SELECT count(*) FROM approvals WHERE capability = $1 AND verified")
            .bind(CAP)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(approvals, 1);
}

#[sqlx::test]
async fn an_overspend_lands_live_marked_and_never_refused(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    set_pool(&pool, s.org, 1, &[s.latte]).await;
    let bearer = token(s.admin, s.org, UserRole::OrgAdmin);

    let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
    for id in [first, second] {
        let resp = post!(
            app,
            "/orders",
            bearer,
            &order(&s, s.till, vec![big_latte(&s, Some(staff(id, "Sara")))])
        );
        assert_eq!(resp.status(), 201);
        let o: Value = test::read_body_json(resp).await;
        assert_eq!(
            o["items"][0]["staff_comp_minor"], 7000,
            "an overspent drink is still comped"
        );
        let warned = o["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("allowance"));
        assert_eq!(warned, id == second);
    }
    assert!(!drink(&pool, first).await.4);
    assert!(
        drink(&pool, second).await.4,
        "the second is past an allowance of one"
    );
    let day = get_json!(
        app,
        &format!(
            "/staff-pool/today?branch_id={}&business_date={DAY}",
            s.branch
        ),
        bearer
    );
    assert_eq!(
        (day["used"].as_i64(), day["over"].as_i64()),
        (Some(2), Some(1))
    );
}

#[sqlx::test]
async fn a_line_of_three_is_three_off_the_allowance(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    set_pool(&pool, s.org, 2, &[s.latte]).await;
    let bearer = token(s.admin, s.org, UserRole::OrgAdmin);
    let id = Uuid::new_v4();
    let line = json!({ "menu_item_id": s.latte, "size_label": "medium", "quantity": 3, "staff_drink": staff(id, "the morning shift") });
    let o: Value = test::read_body_json(post!(
        app,
        "/orders",
        bearer,
        &order(&s, s.till, vec![line])
    ))
    .await;
    assert_eq!(
        o["items"][0]["staff_comp_minor"], 18000,
        "3 × the small's 60"
    );
    assert_eq!(o["subtotal"], 4500, "3 × the 15 a medium is over a small");
    assert!(
        drink(&pool, id).await.4,
        "three against an allowance of two"
    );
    assert_eq!(used(&pool, s.branch).await, 3);
}

// ── Idempotency ─────────────────────────────────────────────────────────────

#[sqlx::test]
async fn a_retry_never_double_counts_and_an_earlier_record_only_row_is_reused(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    set_pool(&pool, s.org, 5, &[s.latte]).await;
    let bearer = token(s.admin, s.org, UserRole::OrgAdmin);

    // The same sale sent twice is one order and one drink.
    let id = Uuid::new_v4();
    let body = order(&s, s.till, vec![big_latte(&s, Some(staff(id, "Sara")))]);
    assert_eq!(post!(app, "/orders", bearer, &body).status(), 201);
    assert_eq!(post!(app, "/orders", bearer, &body).status(), 200);
    assert_eq!(used(&pool, s.branch).await, 1);

    // An older flow recorded the drink FIRST; the sale then names the same id.
    let early = Uuid::new_v4();
    let resp = post!(
        app,
        "/staff-pool/drinks",
        bearer,
        &json!({
            "id": early, "branch_id": s.branch, "menu_item_id": s.latte, "item_name": "Latte",
            "quantity": 1, "note": "Omar", "recorded_at": AT
        })
    );
    assert_eq!(resp.status(), 201);
    assert_eq!(used(&pool, s.branch).await, 2);
    let resp = post!(
        app,
        "/orders",
        bearer,
        &order(&s, s.till, vec![big_latte(&s, Some(staff(early, "Omar")))])
    );
    assert_eq!(resp.status(), 201);
    let o: Value = test::read_body_json(resp).await;
    assert_eq!(
        used(&pool, s.branch).await,
        2,
        "reused, not spent a second time"
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM staff_drinks")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 2);
    let (on, comp, extras, ..) = drink(&pool, early).await;
    assert_eq!(
        on,
        serde_json::from_value(o["id"].clone()).unwrap(),
        "the order is attached to it"
    );
    assert_eq!((comp, extras), (Some(7000), Some(4200)));

    // The same drink on a SECOND sale would be a comp nobody counted.
    let resp = post!(
        app,
        "/orders",
        bearer,
        &order(&s, s.till, vec![big_latte(&s, Some(staff(early, "Omar")))])
    );
    assert_eq!(resp.status(), 409);
}

// ── Replay ──────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn replay_accepts_the_tills_comp_keeps_the_servers_beside_it_and_flags(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    set_pool(&pool, s.org, 5, &[s.latte]).await;
    allow_staff_drink(&pool, s.org, s.teller).await;
    let bearer = token(s.teller, s.org, UserRole::Teller);

    // This till believed the WHOLE large latte was free and charged nothing.
    let id = Uuid::new_v4();
    let key = Uuid::new_v4();
    let request = json!({
        "branch_id": s.branch, "till_id": s.teller_till, "payment_method": "cash",
        "created_at": AT, "idempotency_key": key,
        "items": [{
            "menu_item_id": s.latte, "size_label": "large", "quantity": 1, "unit_price": 9000,
            "staff_drink": { "id": id, "note": "Sara", "comp_minor": 9000 }
        }],
        "subtotal": 0, "tax_amount": 0, "total_amount": 0
    });
    let resp = post!(
        app,
        "/sync/replay",
        bearer,
        &replay_order(s.teller, request.clone())
    );
    assert!(
        resp.status().is_success(),
        "a sale that happened is never refused: {:?}",
        resp.status()
    );
    let o: Value = test::read_body_json(resp).await;
    assert_eq!(
        o["total_amount"], 0,
        "what the till charged is what the books say was charged"
    );
    assert_eq!(o["items"][0]["staff_comp_minor"], 9000);
    assert_eq!(o["items"][0]["line_total"], 0);
    assert_eq!(o["price_flagged"], true);

    let (_, comp, extras, reported, ..) = drink(&pool, id).await;
    assert_eq!(
        comp,
        Some(6000),
        "the SERVER's verdict: only the small is free"
    );
    assert_eq!(reported, Some(9000), "the till's claim, kept beside it");
    assert_eq!(extras, Some(0));
    let order_id: Uuid = serde_json::from_value(o["id"].clone()).unwrap();
    assert_eq!(
        flags(&pool, s.teller).await,
        vec![(
            "CreateOrder".to_string(),
            format!("{CAP}:comp_mismatch"),
            Some(order_id)
        )]
    );

    // Flushing the same op again is the same sale: no second row, flag or drink.
    let resp = post!(
        app,
        "/sync/replay",
        bearer,
        &replay_order(s.teller, request)
    );
    assert!(resp.status().is_success());
    assert_eq!(used(&pool, s.branch).await, 1);
    assert_eq!(flags(&pool, s.teller).await.len(), 1);
}

#[sqlx::test]
async fn a_replayed_comp_the_server_agrees_with_lands_clean(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    set_pool(&pool, s.org, 5, &[s.latte]).await;
    allow_staff_drink(&pool, s.org, s.teller).await;
    let bearer = token(s.teller, s.org, UserRole::Teller);

    let id = Uuid::new_v4();
    let mut line = big_latte(
        &s,
        Some(json!({ "id": id, "note": "Sara", "comp_minor": 7000, "overspent": false })),
    );
    line["unit_price"] = json!(9000);
    let mut request = order(&s, s.teller_till, vec![line]);
    request["subtotal"] = json!(4200);
    request["total_amount"] = json!(4788);
    let resp = post!(
        app,
        "/sync/replay",
        bearer,
        &replay_order(s.teller, request)
    );
    assert!(resp.status().is_success());
    let o: Value = test::read_body_json(resp).await;
    assert_eq!(o["total_amount"], 4788);
    assert_eq!(
        o["price_flagged"], false,
        "a comp the menu explains is not a price anomaly"
    );
    let (_, comp, _, reported, ..) = drink(&pool, id).await;
    assert_eq!((comp, reported), (Some(7000), Some(7000)));
    assert!(flags(&pool, s.teller).await.is_empty());
}

#[sqlx::test]
async fn replay_flags_a_missing_grant_a_refusal_and_an_overspend_and_still_lands(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    set_pool(&pool, s.org, 0, &[s.latte]).await;
    // No grant for this teller, an item off the list, a blank note, allowance 0.
    let bearer = token(s.teller, s.org, UserRole::Teller);

    let (off_list, blank, over) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let request = json!({
        "branch_id": s.branch, "till_id": s.teller_till, "payment_method": "cash",
        "created_at": AT, "idempotency_key": Uuid::new_v4(),
        "items": [
            { "menu_item_id": s.cake, "quantity": 1, "unit_price": 6000,
              "staff_drink": { "id": off_list, "note": "Sara", "comp_minor": 6000 } },
            { "menu_item_id": s.latte, "size_label": "small", "quantity": 1, "unit_price": 6000,
              "staff_drink": { "id": blank, "note": "  ", "comp_minor": 6000 } },
            { "menu_item_id": s.latte, "size_label": "small", "quantity": 1, "unit_price": 6000,
              "staff_drink": { "id": over, "note": "Omar", "comp_minor": 6000 } }
        ],
        "subtotal": 0, "total_amount": 0
    });
    let resp = post!(
        app,
        "/sync/replay",
        bearer,
        &replay_order(s.teller, request)
    );
    assert!(resp.status().is_success(), "{:?}", resp.status());

    let mut caps: Vec<String> = flags(&pool, s.teller)
        .await
        .into_iter()
        .map(|f| f.1)
        .collect();
    caps.sort();
    caps.dedup();
    assert_eq!(
        caps,
        vec![
            CAP.to_string(),
            format!("{CAP}:comp_mismatch"),
            format!("{CAP}:item_not_eligible"),
            format!("{CAP}:note_required"),
            format!("{CAP}:overspent"),
        ]
    );
    assert_eq!(
        drink(&pool, off_list).await.1,
        Some(0),
        "the server's verdict: nothing of it was free"
    );
    assert_eq!(drink(&pool, blank).await.7, "(no note given)");
    let (_, comp, _, _, overspent, on_replay, ..) = drink(&pool, over).await;
    assert_eq!(comp, Some(6000));
    assert!(overspent && on_replay);
    assert_eq!(
        used(&pool, s.branch).await,
        3,
        "all three were made, all three count"
    );
}

#[sqlx::test]
async fn the_record_only_op_of_older_tills_still_works_untouched(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    set_pool(&pool, s.org, 5, &[s.latte]).await;
    allow_staff_drink(&pool, s.org, s.teller).await;
    let bearer = token(s.teller, s.org, UserRole::Teller);

    // v0.5.0–v0.7.12: a plain sale with no `staff_drink`, then the side record.
    let resp = post!(
        app,
        "/sync/replay",
        bearer,
        &replay_order(
            s.teller,
            order(&s, s.teller_till, vec![big_latte(&s, None)])
        )
    );
    assert!(resp.status().is_success());
    let o: Value = test::read_body_json(resp).await;
    assert_eq!(
        o["subtotal"], 11200,
        "nothing is comped unless the line asks"
    );
    assert_eq!(o["items"][0]["staff_comp_minor"], 0);
    assert!(o["items"][0]["staff_drink_id"].is_null());

    let id = Uuid::new_v4();
    let resp = post!(
        app,
        "/sync/replay",
        bearer,
        &json!({
            "op": "record_staff_drink", "teller_id": s.teller,
            "request": { "id": id, "branch_id": s.branch, "order_id": o["id"], "menu_item_id": s.latte,
                         "item_name": "Latte", "quantity": 1, "note": "Sara", "recorded_at": AT }
        })
    );
    assert_eq!(resp.status(), 201);
    let d: Value = test::read_body_json(resp).await;
    assert!(
        d["comp_minor"].is_null() && d["extras_minor"].is_null(),
        "a record-only drink priced nothing"
    );
    assert_eq!(used(&pool, s.branch).await, 1);

    let summary = get_json!(
        app,
        &format!(
            "/staff-pool/drinks/summary?branch_id={}&from={DAY}&to={DAY}",
            s.branch
        ),
        bearer
    );
    assert_eq!(summary["unpriced"], 1);
    assert_eq!(summary["comp_minor"], 0);
}

// ── The books ───────────────────────────────────────────────────────────────

#[sqlx::test]
async fn revenue_counts_only_the_charged_part_and_cost_counts_in_full(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    set_pool(&pool, s.org, 5, &[s.latte]).await;
    let bearer = token(s.admin, s.org, UserRole::OrgAdmin);

    // One paid and one pooled line of the SAME drink, on one sale.
    let body = order(
        &s,
        s.till,
        vec![
            big_latte(&s, None),
            big_latte(&s, Some(staff(Uuid::new_v4(), "Sara"))),
        ],
    );
    let o: Value = test::read_body_json(post!(app, "/orders", bearer, &body)).await;
    assert_eq!(o["subtotal"], 11200 + 4200);
    let (paid, pooled) = (&o["items"][0], &o["items"][1]);
    assert_eq!(
        paid["line_cost"], pooled["line_cost"],
        "the drink was made either way"
    );
    assert!(pooled["line_cost"].as_i64().unwrap() > 0);

    // The figures every revenue report sums.
    let (items, addons): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT sum(line_total) FROM order_items)::bigint, (SELECT sum(line_total) FROM order_item_addons)::bigint",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(items, 9000 + 3000);
    assert_eq!(addons, (1500 + 700) + (500 + 700));
    assert_eq!(
        items + addons,
        11200 + 4200,
        "lines and add-ons add up to the subtotal"
    );

    let sales = get_json!(
        app,
        &format!("/reports/branches/{}/sales", s.branch),
        bearer
    );
    assert_eq!(sales["subtotal"], 15400);
    assert_eq!(
        sales["total_revenue"],
        15400 + 2156,
        "charged part + its 14%"
    );
    assert_eq!(
        sales["top_items"][0]["quantity_sold"], 2,
        "the staff drink is a visible sale"
    );
    assert_eq!(sales["top_items"][0]["revenue"], 12000);

    // The drawer expects the cash that was actually taken.
    let mut conn = pool.acquire().await.unwrap();
    let cash = madar_rust::tills::handlers::compute_system_cash(&mut *conn, s.till)
        .await
        .unwrap();
    assert_eq!(cash, 15400 + 2156);

    let summary = get_json!(
        app,
        &format!(
            "/staff-pool/drinks/summary?branch_id={}&from={DAY}&to={DAY}",
            s.branch
        ),
        bearer
    );
    assert_eq!(summary["drinks"], 1);
    assert_eq!(summary["comp_minor"], 7000);
    assert_eq!(summary["extras_minor"], 4200);
    assert_eq!(summary["cost_minor"], 1200);
    let list = get_json!(
        app,
        &format!(
            "/staff-pool/drinks?branch_id={}&from={DAY}&to={DAY}",
            s.branch
        ),
        bearer
    );
    assert_eq!(list[0]["comp_minor"], 7000);
    assert_eq!(list[0]["extras_minor"], 4200);
}

// ── The feed ────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn the_sync_projections_carry_the_comp(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    set_pool(&pool, s.org, 5, &[s.latte]).await;
    let bearer = token(s.admin, s.org, UserRole::OrgAdmin);
    let id = Uuid::new_v4();
    let o: Value = test::read_body_json(post!(
        app,
        "/orders",
        bearer,
        &order(&s, s.till, vec![big_latte(&s, Some(staff(id, "Sara")))])
    ))
    .await;
    let order_id: Uuid = serde_json::from_value(o["id"].clone()).unwrap();

    let mut conn = pool.acquire().await.unwrap();
    let orders = madar_rust::sync::pull::projection::project(
        &mut conn,
        s.org,
        s.branch,
        "order",
        &[order_id],
    )
    .await
    .unwrap();
    let line = &orders[&order_id]["items"][0];
    assert_eq!(line["staff_comp_minor"], 7000);
    assert_eq!(line["staff_drink_id"], json!(id));
    assert_eq!(line["addons"][0]["staff_comp_minor"], 1000);

    let drinks = madar_rust::sync::pull::projection::project(
        &mut conn,
        s.org,
        s.branch,
        "staff_drink",
        &[id],
    )
    .await
    .unwrap();
    assert_eq!(drinks[&id]["comp_minor"], 7000);
    assert_eq!(drinks[&id]["extras_minor"], 4200);
    assert_eq!(drinks[&id]["order_id"], json!(order_id));
    assert_eq!(
        drinks[&id]["quantity"], 1,
        "the fields old tablets read are still there"
    );
}

// ── Tickets ─────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn a_tables_bill_never_carries_a_staff_drink(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    set_pool(&pool, s.org, 5, &[s.latte]).await;
    let bearer = token(s.admin, s.org, UserRole::OrgAdmin);
    let resp = post!(
        app,
        "/open-tickets",
        bearer,
        &json!({
            "branch_id": s.branch,
            "items": [{ "menu_item_id": s.latte, "size_label": "small", "quantity": 1, "staff_drink": staff(Uuid::new_v4(), "Sara") }]
        })
    );
    assert_eq!(resp.status(), 400);
    let e: Value = test::read_body_json(resp).await;
    assert_eq!(e["code"], "staff_drink_not_on_ticket", "{e}");
    assert_eq!(used(&pool, s.branch).await, 0);
}
