//! Rewards by Madar: loyalty sold on its own (Madar REWARDS_DESIGN.md).
//!
//! - The add-on: plan Free + `loyalty` lifts the ceiling for loyalty only;
//!   only a super admin switches it; upgrading to paid ends it; a downgrade
//!   does not bring it back.
//! - The counter: stamps with no order, a full card completed, once per key,
//!   and every refusal (stamps only, card not full, bad counts, key reuse, no
//!   add-on).
//! - PIN sign-in with no registered device, as the teller page does it.
//!
//! Each guard is exercised from both sides, so no test passes by never
//! reaching it.

mod common;

use actix_web::{App, http::StatusCode, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use common::combos::{Shop, secret, shop, token};
use madar_rust::authz::{Cap, require::effective};
use madar_rust::models::UserRole;

macro_rules! app {
    ($pool:expr) => {{
        unsafe {
            std::env::set_var("MADAR_DISABLE_RATE_LIMIT", "1");
            std::env::set_var("MADAR_DISABLE_AUTO_TRANSLATION", "1");
        }
        let read_pool = web::Data::new($pool.clone());
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(
                    madar_rust::menu::cache::MenuCache::from_env(),
                ))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(
                    madar_rust::auth::org_status::OrgStatusCache::new(),
                ))
                .app_data(web::Data::new(
                    madar_rust::realtime::hub::BranchEventHub::new(),
                ))
                .app_data(madar_rust::qr_card::routes::make_provider())
                .app_data(web::Data::new(
                    madar_rust::demo::config::DemoConfig::from_env(),
                ))
                .app_data(web::Data::new(madar_rust::ai::AiState::from_env()))
                .configure(|cfg| madar_rust::app_routes::configure_api(cfg, read_pool.clone())),
        )
        .await
    }};
}

async fn call<S, B>(app: &S, req: test::TestRequest, bearer: Option<&str>) -> (StatusCode, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    let req = match bearer {
        Some(b) => req.insert_header(("Authorization", format!("Bearer {b}"))),
        None => req,
    };
    let resp = test::call_service(app, req.to_request()).await;
    let status = resp.status();
    let bytes = test::read_body(resp).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn super_admin() -> String {
    madar_rust::auth::jwt::create_token(
        &secret(),
        Uuid::new_v4(),
        None,
        UserRole::SuperAdmin,
        None,
        24,
    )
    .unwrap()
}

async fn setup(pool: &PgPool) -> Shop {
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    madar_rust::authz::sync_catalogue(pool).await.unwrap();
    shop(pool).await
}

async fn set_plan(pool: &PgPool, org: Uuid, plan: &str) {
    sqlx::query("UPDATE organizations SET plan = $2 WHERE id = $1")
        .bind(org)
        .bind(plan)
        .execute(pool)
        .await
        .unwrap();
}

async fn addon_row(pool: &PgPool, org: Uuid) -> Option<(String, bool)> {
    sqlx::query_as(
        "SELECT status, ends_at IS NOT NULL FROM org_addons WHERE org_id = $1 AND feature = 'loyalty'",
    )
    .bind(org)
    .fetch_optional(pool)
    .await
    .unwrap()
}

/// A stamp card: `size` stamps fill it; `per_item` counts items, not visits.
async fn stamp_card(pool: &PgPool, org: Uuid, mode: &str, size: i32, per_item: bool) {
    sqlx::query(
        "INSERT INTO loyalty_settings \
            (org_id, branch_id, enabled, mode, earn_piastres_per_point, default_reward_cost, \
             require_otp, stamp_per_line_item, reward_any_item) \
         VALUES ($1, NULL, true, $2, 1000, $3, false, $4, true)",
    )
    .bind(org)
    .bind(mode)
    .bind(size)
    .bind(per_item)
    .execute(pool)
    .await
    .unwrap();
}

async fn member(pool: &PgPool, org: Uuid, phone: &str) -> Uuid {
    let tok = format!("M{}", &Uuid::new_v4().simple().to_string()[..22]);
    common::members::seed_loyalty_member(pool, org, phone, "Mona", &tok).await
}

async fn visits(pool: &PgPool, m: Uuid) -> i32 {
    sqlx::query_scalar("SELECT visits_balance FROM loyalty_memberships WHERE id = $1")
        .bind(m)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A loyalty-only shop: plan Free + the add-on, granted the way the super
/// admin does it.
async fn rewards_shop<S, B>(app: &S, pool: &PgPool, s: &Shop)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    set_plan(pool, s.org, "free").await;
    let (st, body) = call(
        app,
        test::TestRequest::put()
            .uri(&format!("/orgs/{}/addons/loyalty", s.org))
            .set_json(json!({"note": "Rewards test"})),
        Some(&super_admin()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
}

fn teller(s: &Shop) -> String {
    token(s.teller, s.org, UserRole::Teller)
}

// ── The add-on ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn the_loyalty_addon_lifts_the_free_ceiling_for_loyalty_and_nothing_else(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    set_plan(&pool, s.org, "free").await;

    let before = effective(&pool, s.teller, None).await.unwrap();
    assert!(!before.can(Cap::LoyaltyUse), "Free alone has no loyalty");

    rewards_shop(&app, &pool, &s).await;
    let after = effective(&pool, s.teller, None).await.unwrap();
    assert!(
        after.can(Cap::LoyaltyUse),
        "the add-on grants the teller loyalty.use"
    );
    assert!(after.can(Cap::LoyaltyRead));
    assert!(!after.can(Cap::InventoryRead), "and nothing else paid");
    assert!(after.can(Cap::PosSignIn), "core stays");

    // The wire: the plan stays `free`; the add-on rides beside it.
    let (_, me) = call(
        &app,
        test::TestRequest::get().uri("/auth/me"),
        Some(&teller(&s)),
    )
    .await;
    assert_eq!(me["plan"], "free");
    assert_eq!(me["addons"], json!(["loyalty"]));
    let (st, plan) = call(
        &app,
        test::TestRequest::get().uri(&format!("/orgs/{}/plan", s.org)),
        Some(&super_admin()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{plan}");
    assert_eq!(
        plan["entitlements"]["features"],
        json!(["floor", "loyalty"])
    );
    assert_eq!(
        plan["entitlements"]["limits"]["branches"], 1,
        "limits stay Free's"
    );
    assert_eq!(plan["addons"], json!(["loyalty"]));
}

#[sqlx::test]
async fn only_a_super_admin_grants_an_addon_and_only_on_free(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    let uri = format!("/orgs/{}/addons/loyalty", s.org);

    // Paid already includes it: refused, no row.
    let (st, body) = call(
        &app,
        test::TestRequest::put().uri(&uri),
        Some(&super_admin()),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "ADDON_ON_PAID");
    assert!(addon_row(&pool, s.org).await.is_none());

    set_plan(&pool, s.org, "free").await;
    // The shop's own owner cannot sell itself an add-on.
    let (st, _) = call(
        &app,
        test::TestRequest::put().uri(&uri),
        Some(&s.admin_token()),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // Only loyalty is sold as an add-on.
    let (st, _) = call(
        &app,
        test::TestRequest::put().uri(&format!("/orgs/{}/addons/inventory", s.org)),
        Some(&super_admin()),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(addon_row(&pool, s.org).await.is_none());

    let (st, _) = call(
        &app,
        test::TestRequest::put().uri(&uri),
        Some(&super_admin()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        addon_row(&pool, s.org).await,
        Some(("active".into(), false))
    );

    // Ending it keeps the row and switches loyalty off.
    let (st, _) = call(
        &app,
        test::TestRequest::delete().uri(&uri),
        Some(&super_admin()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(addon_row(&pool, s.org).await, Some(("ended".into(), true)));
    assert!(
        !effective(&pool, s.teller, None)
            .await
            .unwrap()
            .can(Cap::LoyaltyUse)
    );
}

#[sqlx::test]
async fn upgrading_ends_the_addon_and_a_downgrade_does_not_bring_it_back(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    rewards_shop(&app, &pool, &s).await;

    // Upgrade through the super admin's PATCH, the route the dashboard uses.
    let (st, body) = call(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/orgs/{}", s.org))
            .set_json(json!({"plan": "paid"})),
        Some(&super_admin()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(addon_row(&pool, s.org).await, Some(("ended".into(), true)));
    assert!(
        effective(&pool, s.teller, None)
            .await
            .unwrap()
            .can(Cap::LoyaltyUse),
        "paid has it"
    );

    set_plan(&pool, s.org, "free").await;
    assert_eq!(addon_row(&pool, s.org).await, Some(("ended".into(), true)));
    assert!(
        !effective(&pool, s.teller, None)
            .await
            .unwrap()
            .can(Cap::LoyaltyUse),
        "back on Free the add-on must be granted again"
    );
}

#[sqlx::test]
async fn a_free_shops_card_page_exists_only_with_the_addon(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    stamp_card(&pool, s.org, "visits", 5, true).await;
    let m = member(&pool, s.org, "01001234567").await;
    let tok: String =
        sqlx::query_scalar("SELECT member_token FROM loyalty_memberships WHERE id = $1")
            .bind(m)
            .fetch_one(&pool)
            .await
            .unwrap();
    let uri = format!("/public/loyalty/card/{tok}");

    set_plan(&pool, s.org, "free").await;
    let (st, _) = call(&app, test::TestRequest::get().uri(&uri), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "Free alone has no card page");

    rewards_shop(&app, &pool, &s).await;
    let (st, card) = call(&app, test::TestRequest::get().uri(&uri), None).await;
    assert_eq!(st, StatusCode::OK, "{card}");
}

// ── The counter ─────────────────────────────────────────────────────────────

fn stamp_req(s: &Shop, m: Uuid, items: i32, key: &str) -> test::TestRequest {
    test::TestRequest::post()
        .uri("/loyalty/counter/stamp")
        .set_json(json!({
            "branch_id": s.branch, "customer_id": m, "items": items, "idempotency_key": key,
        }))
}

fn redeem_req(s: &Shop, m: Uuid, key: &str) -> test::TestRequest {
    test::TestRequest::post()
        .uri("/loyalty/counter/redeem")
        .set_json(json!({
            "branch_id": s.branch, "customer_id": m, "idempotency_key": key,
        }))
}

#[sqlx::test]
async fn a_counter_stamps_completes_and_carries_over(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    rewards_shop(&app, &pool, &s).await;
    // A card of 5, a stamp per item.
    stamp_card(&pool, s.org, "visits", 5, true).await;
    let m = member(&pool, s.org, "01001234567").await;
    let t = teller(&s);

    // 3 items -> 3 stamps (0 + 3 = 3 of 5).
    let (st, r) = call(&app, stamp_req(&s, m, 3, "key-aaaa-0001"), Some(&t)).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["stamps"], 3);
    assert_eq!(r["already"], false);
    assert_eq!(r["member"]["balance"], 3);
    assert_eq!(r["member"]["next_reward_cost"], 5);
    assert_eq!(visits(&pool, m).await, 3);

    // The same key again: nothing new, the first answer.
    let (st, r) = call(&app, stamp_req(&s, m, 3, "key-aaaa-0001"), Some(&t)).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(
        (r["stamps"].as_i64(), r["already"].as_bool()),
        (Some(3), Some(true))
    );
    assert_eq!(visits(&pool, m).await, 3);

    // Not full (3 < 5): refused, nothing moves.
    let (st, r) = call(&app, redeem_req(&s, m, "key-bbbb-0001"), Some(&t)).await;
    assert_eq!(st, StatusCode::CONFLICT, "{r}");
    assert_eq!(r["code"], "LOYALTY_CARD_NOT_FULL");
    assert_eq!(visits(&pool, m).await, 3);

    // 4 more -> 7; complete -> 7 - 5 = 2 carried over.
    call(&app, stamp_req(&s, m, 4, "key-aaaa-0002"), Some(&t)).await;
    assert_eq!(visits(&pool, m).await, 7);
    let (st, r) = call(&app, redeem_req(&s, m, "key-bbbb-0002"), Some(&t)).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["stamps"], -5);
    assert_eq!(r["member"]["balance"], 2);
    // A retried completion does not take a second card.
    let (_, r) = call(&app, redeem_req(&s, m, "key-bbbb-0002"), Some(&t)).await;
    assert_eq!(
        (r["stamps"].as_i64(), r["already"].as_bool()),
        (Some(-5), Some(true))
    );
    assert_eq!(visits(&pool, m).await, 2);

    // The ledger: counter rows, no order, the teller's branch, one per key.
    let rows: Vec<(String, String, i32, Option<Uuid>, Uuid)> = sqlx::query_as(
        "SELECT kind::text, source, points, order_id, branch_id FROM loyalty_transactions
          WHERE customer_id = $1 ORDER BY created_at, points DESC",
    )
    .bind(m)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            ("earn".into(), "counter".into(), 3, None, s.branch),
            ("earn".into(), "counter".into(), 4, None, s.branch),
            ("redeem".into(), "counter".into(), -5, None, s.branch),
        ]
    );

    // History reads them (old tills decode `source` as a string, branch as a uuid).
    let (st, h) = call(
        &app,
        test::TestRequest::get().uri(&format!("/loyalty/members/{m}")),
        Some(&s.admin_token()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{h}");
}

#[sqlx::test]
async fn a_visit_card_counts_one_stamp_per_visit_when_items_are_not_counted(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    rewards_shop(&app, &pool, &s).await;
    stamp_card(&pool, s.org, "visits", 5, false).await;
    let m = member(&pool, s.org, "01001234567").await;
    let (st, r) = call(
        &app,
        stamp_req(&s, m, 3, "key-cccc-0001"),
        Some(&teller(&s)),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["stamps"], 1, "per-visit card: 3 items are one visit");
}

#[sqlx::test]
async fn the_counter_refuses_what_it_must(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    let m = member(&pool, s.org, "01001234567").await;
    let other = member(&pool, s.org, "01001234568").await;
    let t = teller(&s);

    // Free without the add-on: no loyalty at all.
    set_plan(&pool, s.org, "free").await;
    stamp_card(&pool, s.org, "points", 100, false).await;
    let (st, r) = call(&app, stamp_req(&s, m, 1, "key-dddd-0001"), Some(&t)).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{r}");

    rewards_shop(&app, &pool, &s).await;
    // A points programme: the counter stamps cards only.
    let (st, r) = call(&app, stamp_req(&s, m, 1, "key-dddd-0002"), Some(&t)).await;
    assert_eq!(st, StatusCode::CONFLICT, "{r}");
    assert_eq!(r["code"], "LOYALTY_COUNTER_STAMPS_ONLY");

    sqlx::query(
        "UPDATE loyalty_settings SET mode = 'visits', default_reward_cost = 5 WHERE org_id = $1",
    )
    .bind(s.org)
    .execute(&pool)
    .await
    .unwrap();
    // Counts outside 1..=50, and a malformed key.
    for (items, key) in [
        (0, "key-dddd-0003"),
        (51, "key-dddd-0004"),
        (-2, "key-dddd-0005"),
        (1, "short"),
    ] {
        let (st, r) = call(&app, stamp_req(&s, m, items, key), Some(&t)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{items} {key}: {r}");
    }
    let (st, _) = call(&app, stamp_req(&s, m, 50, "key-dddd-0006"), Some(&t)).await;
    assert_eq!(st, StatusCode::OK, "50 is the documented bound");

    // One key, another member: a client bug, refused rather than answered.
    let (st, r) = call(&app, stamp_req(&s, other, 50, "key-dddd-0006"), Some(&t)).await;
    assert_eq!(st, StatusCode::CONFLICT, "{r}");
    assert_eq!(r["code"], "LOYALTY_KEY_REUSED");
    assert_eq!(visits(&pool, other).await, 0);

    // A member of another shop is not found here.
    let s2 = shop(&pool).await;
    let stranger = member(&pool, s2.org, "01001234569").await;
    let (st, _) = call(&app, stamp_req(&s, stranger, 1, "key-dddd-0007"), Some(&t)).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

// ── PIN sign-in with no device ──────────────────────────────────────────────

#[sqlx::test]
async fn a_teller_signs_in_with_a_pin_and_no_registered_device(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    rewards_shop(&app, &pool, &s).await;
    sqlx::query("UPDATE users SET pin_hash = $2 WHERE id = $1")
        .bind(s.teller)
        .bind(bcrypt::hash("4821", 4).unwrap())
        .execute(&pool)
        .await
        .unwrap();

    // The teller page sends a per-browser id that no device row knows.
    let (st, r) = call(
        &app,
        test::TestRequest::post()
            .uri("/auth/login")
            .insert_header(("X-Madar-Device-Id", Uuid::new_v4().to_string()))
            .set_json(json!({"pin": "4821", "branch_id": s.branch})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["plan"], "free");
    assert_eq!(r["addons"], json!(["loyalty"]));
    assert_eq!(r["user"]["branch_id"], json!(s.branch));

    // A wrong PIN is still refused.
    let (st, _) = call(
        &app,
        test::TestRequest::post()
            .uri("/auth/login")
            .set_json(json!({"pin": "1111", "branch_id": s.branch})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

// ── Upgrade: same ledger, and orders earn into it ───────────────────────────

#[sqlx::test]
async fn after_upgrading_the_counters_stamps_stay_and_orders_earn_on_top(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    rewards_shop(&app, &pool, &s).await;
    stamp_card(&pool, s.org, "visits", 5, true).await;
    let m = member(&pool, s.org, "01001234567").await;
    let t = teller(&s);
    call(&app, stamp_req(&s, m, 2, "key-eeee-0001"), Some(&t)).await;
    assert_eq!(visits(&pool, m).await, 2);

    // The super admin moves the shop to paid: the add-on ends, nothing moves.
    let (st, _) = call(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/orgs/{}", s.org))
            .set_json(json!({"plan": "paid"})),
        Some(&super_admin()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(addon_row(&pool, s.org).await, Some(("ended".into(), true)));
    assert_eq!(visits(&pool, m).await, 2, "balance intact");

    // A till's sale now earns into the same card. An order with no lines in a
    // per-item programme earns one stamp (earn::points_for_order's floor):
    // 2 + 1 = 3.
    let order = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO orders (id, branch_id, teller_id, till_id, idempotency_key, customer_name, subtotal, \
             discount_amount, tax_amount, total_amount, status, order_number, payment_method, order_ref) \
         VALUES ($1, $2, $3, $4, gen_random_uuid(), 'Customer', 500, 0, 0, 500, 'completed', 1, 'cash', gen_random_uuid()::text)",
    )
    .bind(order)
    .bind(s.branch)
    .bind(s.teller)
    .bind(s.teller_till)
    .execute(&pool)
    .await
    .unwrap();
    let (st, r) = call(
        &app,
        test::TestRequest::post().uri("/loyalty/award").set_json(json!({
            "branch_id": s.branch, "order_id": order, "customer_id": m,
        })),
        Some(&t),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["points_awarded"], 1);
    assert_eq!(visits(&pool, m).await, 3);
    let sources: Vec<String> = sqlx::query_scalar(
        "SELECT source FROM loyalty_transactions WHERE customer_id = $1 ORDER BY created_at",
    )
    .bind(m)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(sources, vec!["counter", "sale"]);
}
