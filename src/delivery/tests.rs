//! Delivery tests. Pure-helper unit tests live here; the heavy #[sqlx::test]
//! integration + e2e suite is appended in `integration_tests` below.

use super::*;
use chrono::NaiveTime;

fn t(h: u32, m: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, m, 0).unwrap()
}

#[test]
fn within_window_no_bounds_is_always_open() {
    assert!(within_window(None, None, t(3, 0)));
    assert!(within_window(Some(t(9, 0)), None, t(3, 0)));
    assert!(within_window(None, Some(t(17, 0)), t(3, 0)));
}

#[test]
fn within_window_same_day() {
    assert!(within_window(Some(t(9, 0)), Some(t(17, 0)), t(12, 0)));
    assert!(within_window(Some(t(9, 0)), Some(t(17, 0)), t(9, 0))); // inclusive open
    assert!(!within_window(Some(t(9, 0)), Some(t(17, 0)), t(8, 59)));
    assert!(!within_window(Some(t(9, 0)), Some(t(17, 0)), t(17, 0))); // exclusive close
}

#[test]
fn within_window_overnight() {
    // 18:00 → 02:00 spans midnight.
    assert!(within_window(Some(t(18, 0)), Some(t(2, 0)), t(20, 0)));
    assert!(within_window(Some(t(18, 0)), Some(t(2, 0)), t(1, 0)));
    assert!(!within_window(Some(t(18, 0)), Some(t(2, 0)), t(12, 0)));
    assert!(!within_window(Some(t(18, 0)), Some(t(2, 0)), t(2, 0)));
}

#[test]
fn channel_open_respects_master_and_shift() {
    // Disabled by dashboard → closed regardless of override.
    assert!(!channel_open(false, "open", None, None, t(12, 0), true));
    // No open shift → closed.
    assert!(!channel_open(true, "open", None, None, t(12, 0), false));
    // Paused → closed.
    assert!(!channel_open(true, "closed", None, None, t(12, 0), true));
}

#[test]
fn channel_open_force_open_ignores_window() {
    // 'open' force-accepts even outside the window (shift still required).
    assert!(channel_open(true, "open", Some(t(9, 0)), Some(t(17, 0)), t(23, 0), true));
    // 'auto' obeys the window.
    assert!(!channel_open(true, "auto", Some(t(9, 0)), Some(t(17, 0)), t(23, 0), true));
    assert!(channel_open(true, "auto", Some(t(9, 0)), Some(t(17, 0)), t(12, 0), true));
}

#[test]
fn normalize_phone_egypt() {
    assert_eq!(normalize_phone("01012345678").unwrap(), "201012345678");
    assert_eq!(normalize_phone("201012345678").unwrap(), "201012345678");
    assert_eq!(normalize_phone("+20 101 234 5678").unwrap(), "201012345678");
    assert_eq!(normalize_phone("0020 1012345678").unwrap(), "201012345678");
    assert!(normalize_phone("123").is_err());
}

#[test]
fn validate_helpers() {
    assert!(validate_channel("in_mall").is_ok());
    assert!(validate_channel("outside").is_ok());
    assert!(validate_channel("drive_thru").is_err());
    assert!(validate_override("auto").is_ok());
    assert!(validate_override("open").is_ok());
    assert!(validate_override("closed").is_ok());
    assert!(validate_override("paused").is_err());
}

// ── Pure zone/fee math (no OSRM, no DB) ───────────────────────

#[cfg(test)]
mod zone_fee {
    use crate::delivery::public::{select_zone_fee, FeeOutcome, ZoneRow};
    use uuid::Uuid;

    fn zone(max: i32, fee: i32) -> ZoneRow {
        ZoneRow { id: Uuid::new_v4(), name: format!("ring-{max}"), fee, max_road_distance_meters: max }
    }
    fn fee_of(o: FeeOutcome) -> i32 {
        match o {
            FeeOutcome::Ok { fee, .. } => fee,
            _ => panic!("expected Ok"),
        }
    }

    #[test]
    fn smallest_covering_ring_fee_wins() {
        let zones = [zone(500, 1000), zone(2000, 2500)];
        assert_eq!(fee_of(select_zone_fee(400, None, &zones)), 1000);
        assert_eq!(fee_of(select_zone_fee(1500, None, &zones)), 2500);
        assert_eq!(fee_of(select_zone_fee(500, None, &zones)), 1000); // inclusive boundary
    }

    #[test]
    fn out_of_range_when_no_zone_covers() {
        let zones = [zone(500, 1000)];
        assert!(matches!(select_zone_fee(600, None, &zones), FeeOutcome::OutOfRange));
    }

    #[test]
    fn branch_hard_cap_forces_out_of_range() {
        let zones = [zone(5000, 1000)];
        assert!(matches!(select_zone_fee(700, Some(600), &zones), FeeOutcome::OutOfRange));
    }

    #[test]
    fn no_zones_is_out_of_range() {
        assert!(matches!(select_zone_fee(100, None, &[]), FeeOutcome::OutOfRange));
    }
}

// ── Broadcast hub (pure, no DB) ───────────────────────────────

#[cfg(test)]
mod hub_tests {
    use crate::delivery::hub::{DeliveryEvent, DeliveryHub};
    use crate::delivery::staff::DeliveryOrder;
    use tokio::sync::broadcast;
    use uuid::Uuid;

    fn make_order(branch_id: Uuid) -> DeliveryOrder {
        // Option fields default to None when absent; only the required columns
        // need to be present for a valid read model.
        serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(),
            "org_id": Uuid::new_v4(),
            "branch_id": branch_id,
            "channel": "in_mall",
            "status": "received",
            "customer_name": "Test",
            "customer_phone": "201000000000",
            "subtotal": 1000,
            "delivery_fee": 0,
            "total": 1000,
            "extra_prep_minutes": 0,
            "cart": { "lines": [] },
            "otp_verified": true,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap()
    }

    fn event(branch_id: Uuid) -> DeliveryEvent {
        DeliveryEvent { event_type: "created".into(), order: make_order(branch_id) }
    }

    #[test]
    fn publish_reaches_only_its_branch() {
        let hub = DeliveryHub::new();
        let branch_a = Uuid::new_v4();
        let branch_b = Uuid::new_v4();
        let mut rx_a = hub.subscribe(branch_a);
        let mut rx_b = hub.subscribe(branch_b);

        hub.publish(branch_a, event(branch_a));

        let ev = rx_a.try_recv().expect("branch A subscriber should receive its event");
        assert_eq!(ev.event_type, "created");
        assert_eq!(ev.order.branch_id, branch_a);

        // Tenant isolation: branch B must NEVER see branch A's event.
        assert!(matches!(rx_b.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
    }

    #[test]
    fn publish_with_no_subscribers_is_noop() {
        let hub = DeliveryHub::new();
        // No channel exists for this branch yet — must not panic.
        hub.publish(Uuid::new_v4(), event(Uuid::new_v4()));
    }

    #[test]
    fn multiple_subscribers_on_a_branch_all_receive() {
        let hub = DeliveryHub::new();
        let branch = Uuid::new_v4();
        let mut rx1 = hub.subscribe(branch);
        let mut rx2 = hub.subscribe(branch);
        hub.publish(branch, event(branch));
        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_ok());
    }
}

// ── Integration / e2e (#[sqlx::test]) ─────────────────────────

#[cfg(test)]
mod it {
    use actix_http::Request;
    use actix_web::dev::{Service, ServiceResponse};
    use actix_web::{http::StatusCode, test, web, App};
    use serde_json::{json, Value};
    use sqlx::PgPool;
    use uuid::Uuid;

    use crate::auth::jwt::{create_token, JwtSecret};
    use crate::models::UserRole;

    fn get_secret() -> JwtSecret {
        JwtSecret("secret".into())
    }
    fn teller_token(uid: Uuid, org: Uuid, branch: Uuid) -> String {
        create_token(&get_secret(), uid, Some(org), UserRole::Teller, Some(branch), 24).unwrap()
    }
    fn admin_token(uid: Uuid, org: Uuid) -> String {
        create_token(&get_secret(), uid, Some(org), UserRole::OrgAdmin, None, 24).unwrap()
    }
    fn device_token(raw_phone: &str) -> String {
        let norm = crate::delivery::normalize_phone(raw_phone).unwrap();
        crate::delivery::whatsapp::issue_device_token(&get_secret().0, &norm).unwrap()
    }

    macro_rules! app {
        ($pool:expr) => {
            test::init_service(
                App::new()
                    .app_data(web::Data::new($pool.clone()))
                    .app_data(web::Data::new(get_secret()))
                    .app_data(web::Data::new(crate::delivery::hub::DeliveryHub::new()))
                    .configure(crate::delivery::routes::configure),
            )
            .await
        };
    }

    async fn send<S>(app: &S, req: test::TestRequest) -> (StatusCode, Value)
    where
        S: Service<Request, Response = ServiceResponse, Error = actix_web::Error>,
    {
        let resp = test::call_service(app, req.to_request()).await;
        let status = resp.status();
        let bytes = test::read_body(resp).await;
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    fn auth(req: test::TestRequest, token: &str) -> test::TestRequest {
        req.insert_header(("Authorization", format!("Bearer {token}")))
    }

    // ── seed helpers ──
    async fn seed_org(pool: &PgPool) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
            .bind(id)
            .bind(format!("org-{id}"))
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, is_active) VALUES \
             ($1,'cash','{}','e','i',true,true),($1,'card','{}','b','c',false,true)",
        )
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
        id
    }
    async fn seed_branch(pool: &PgPool, org: Uuid) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO branches (id, org_id, name, latitude, longitude) VALUES ($1,$2,'Br',30.0,31.0)")
            .bind(id)
            .bind(org)
            .execute(pool)
            .await
            .unwrap();
        id
    }
    async fn seed_user(pool: &PgPool, org: Uuid, role: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES ($1,$2,'U',$3,'h',$4::user_role)")
            .bind(id)
            .bind(org)
            .bind(format!("u-{id}@t.com"))
            .bind(role)
            .execute(pool)
            .await
            .unwrap();
        id
    }
    async fn assign(pool: &PgPool, user: Uuid, branch: Uuid) {
        sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1,$2)")
            .bind(user)
            .bind(branch)
            .execute(pool)
            .await
            .unwrap();
    }
    async fn seed_shift(pool: &PgPool, branch: Uuid, user: Uuid) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO shifts (id, branch_id, teller_id, status, opening_cash) VALUES ($1,$2,$3,'open',10000)")
            .bind(id)
            .bind(branch)
            .bind(user)
            .execute(pool)
            .await
            .unwrap();
        id
    }
    async fn seed_settings(pool: &PgPool, branch: Uuid, in_mall: bool, outside: bool, in_mall_fee: i32) {
        sqlx::query(
            "INSERT INTO branch_delivery_settings (branch_id, in_mall_enabled, outside_enabled, in_mall_fee) \
             VALUES ($1,$2,$3,$4)",
        )
        .bind(branch)
        .bind(in_mall)
        .bind(outside)
        .bind(in_mall_fee)
        .execute(pool)
        .await
        .unwrap();
    }
    async fn seed_item(pool: &PgPool, org: Uuid, price: i32) -> Uuid {
        let cat = Uuid::new_v4();
        sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1,$2,$3)")
            .bind(cat)
            .bind(org)
            .bind(format!("C-{cat}"))
            .execute(pool)
            .await
            .unwrap();
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) VALUES ($1,$2,$3,'Item',$4,true)")
            .bind(id)
            .bind(org)
            .bind(cat)
            .bind(price)
            .execute(pool)
            .await
            .unwrap();
        id
    }
    async fn seed_recipe(pool: &PgPool, org: Uuid, branch: Uuid, item: Uuid, qty: f64, stock: f64) -> Uuid {
        let ing = Uuid::new_v4();
        sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category) VALUES ($1,$2,'Ing','g'::inventory_unit,100,'general')")
            .bind(ing)
            .bind(org)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO branch_inventory (branch_id, org_ingredient_id, current_stock) VALUES ($1,$2,$3)")
            .bind(branch)
            .bind(ing)
            .bind(stock)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO menu_item_recipes (menu_item_id, org_ingredient_id, quantity_used, size_label, ingredient_name, ingredient_unit) VALUES ($1,$2,$3,'one_size','Ing','g')")
            .bind(item)
            .bind(ing)
            .bind(qty)
            .execute(pool)
            .await
            .unwrap();
        ing
    }
    async fn seed_branch_override(pool: &PgPool, branch: Uuid, item: Uuid, price: i32) {
        sqlx::query("INSERT INTO branch_menu_overrides (branch_id, menu_item_id, price_override, is_available) VALUES ($1,$2,$3,true)")
            .bind(branch)
            .bind(item)
            .bind(price)
            .execute(pool)
            .await
            .unwrap();
    }
    async fn seed_channel_override(pool: &PgPool, branch: Uuid, item: Uuid, channel: &str, price: Option<i32>, available: Option<bool>) {
        sqlx::query("INSERT INTO branch_channel_menu_overrides (branch_id, menu_item_id, channel, price_override, is_available) VALUES ($1,$2,$3::delivery_channel,$4,$5)")
            .bind(branch)
            .bind(item)
            .bind(channel)
            .bind(price)
            .bind(available)
            .execute(pool)
            .await
            .unwrap();
    }
    async fn perms(pool: &PgPool) {
        crate::permissions::seeder::seed_role_permissions(pool).await.unwrap();
    }

    const PHONE: &str = "01000000000";

    fn intake_body(branch: Uuid, channel: &str, items: Value) -> Value {
        json!({
            "branch_id": branch, "channel": channel,
            "customer_name": "Sara", "customer_phone": PHONE,
            "payment_method_hint": "cash", "device_token": device_token(PHONE),
            "items": items,
        })
    }

    async fn place_in_mall_order(pool: &PgPool, branch: Uuid, item: Uuid, qty: i32) -> Uuid {
        let app = app!(pool);
        let body = intake_body(branch, "in_mall", json!([{ "menu_item_id": item, "quantity": qty }]));
        let (st, b) = send(&app, test::TestRequest::post().uri("/public/delivery-orders").set_json(&body)).await;
        assert_eq!(st, StatusCode::CREATED, "intake failed: {b}");
        Uuid::parse_str(b["id"].as_str().unwrap()).unwrap()
    }

    // ── tests ──

    #[sqlx::test]
    async fn intake_in_mall_happy(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 300).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 20.0, 1000.0).await;

        let app = app!(&pool);
        let body = intake_body(branch, "in_mall", json!([{ "menu_item_id": item, "quantity": 2 }]));
        let (st, b) = send(&app, test::TestRequest::post().uri("/public/delivery-orders").set_json(&body)).await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
        assert_eq!(b["status"], "received");
        assert_eq!(b["subtotal"], 1000);
        assert_eq!(b["delivery_fee"], 300);
        assert_eq!(b["total"], 1300);
        assert!(b["delivery_ref"].as_str().unwrap().starts_with("D-"));
    }

    #[sqlx::test]
    async fn pricing_precedence_org_branch_channel(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 0).await;
        seed_shift(&pool, branch, teller).await;

        let chan_item = seed_item(&pool, org, 500).await;
        seed_branch_override(&pool, branch, chan_item, 400).await;
        seed_channel_override(&pool, branch, chan_item, "in_mall", Some(300), None).await;

        let branch_item = seed_item(&pool, org, 500).await;
        seed_branch_override(&pool, branch, branch_item, 400).await;

        let app = app!(&pool);
        let body = intake_body(
            branch,
            "in_mall",
            json!([{ "menu_item_id": chan_item, "quantity": 1 }, { "menu_item_id": branch_item, "quantity": 1 }]),
        );
        let (st, b) = send(&app, test::TestRequest::post().uri("/public/delivery-orders").set_json(&body)).await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
        // 300 (channel) + 400 (branch) = 700
        assert_eq!(b["subtotal"], 700);
    }

    #[sqlx::test]
    async fn channel_disabled_item_rejected(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 0).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;
        seed_channel_override(&pool, branch, item, "in_mall", None, Some(false)).await;

        let app = app!(&pool);
        let body = intake_body(branch, "in_mall", json!([{ "menu_item_id": item, "quantity": 1 }]));
        let (st, _) = send(&app, test::TestRequest::post().uri("/public/delivery-orders").set_json(&body)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn intake_blocked_without_open_shift(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        seed_settings(&pool, branch, true, false, 0).await; // enabled but NO shift
        let item = seed_item(&pool, org, 500).await;

        let app = app!(&pool);
        let body = intake_body(branch, "in_mall", json!([{ "menu_item_id": item, "quantity": 1 }]));
        let (st, _) = send(&app, test::TestRequest::post().uri("/public/delivery-orders").set_json(&body)).await;
        assert_eq!(st, StatusCode::CONFLICT);
    }

    #[sqlx::test]
    async fn intake_blocked_when_channel_disabled(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, false, false, 0).await; // in_mall disabled
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;

        let app = app!(&pool);
        let body = intake_body(branch, "in_mall", json!([{ "menu_item_id": item, "quantity": 1 }]));
        let (st, _) = send(&app, test::TestRequest::post().uri("/public/delivery-orders").set_json(&body)).await;
        assert_eq!(st, StatusCode::CONFLICT);
    }

    #[sqlx::test]
    async fn intake_requires_verified_device(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 0).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;

        let app = app!(&pool);
        let mut body = intake_body(branch, "in_mall", json!([{ "menu_item_id": item, "quantity": 1 }]));
        body["device_token"] = json!("not-a-real-token");
        let (st, _) = send(&app, test::TestRequest::post().uri("/public/delivery-orders").set_json(&body)).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    async fn otp_verify_roundtrip(pool: PgPool) {
        let norm = crate::delivery::normalize_phone(PHONE).unwrap();
        let hash = bcrypt::hash("1234", bcrypt::DEFAULT_COST).unwrap();
        sqlx::query("INSERT INTO delivery_otp (phone, code_hash, expires_at) VALUES ($1,$2, now()+interval '5 minutes')")
            .bind(&norm)
            .bind(hash)
            .execute(&pool)
            .await
            .unwrap();

        let app = app!(&pool);
        let (st_bad, _) = send(&app, test::TestRequest::post().uri("/public/otp/verify").set_json(json!({"phone":PHONE,"code":"0000"}))).await;
        assert_eq!(st_bad, StatusCode::BAD_REQUEST);

        let (st_ok, b) = send(&app, test::TestRequest::post().uri("/public/otp/verify").set_json(json!({"phone":PHONE,"code":"1234"}))).await;
        assert_eq!(st_ok, StatusCode::OK, "{b}");
        assert!(b["device_token"].as_str().unwrap().len() > 20);
    }

    #[sqlx::test]
    async fn full_lifecycle_to_finalized_sale(pool: PgPool) {
        perms(&pool).await;
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        assign(&pool, teller, branch).await;
        let shift = seed_shift(&pool, branch, teller).await;
        seed_settings(&pool, branch, true, false, 300).await;
        let item = seed_item(&pool, org, 500).await;
        let ing = seed_recipe(&pool, org, branch, item, 20.0, 1000.0).await;

        let id = place_in_mall_order(&pool, branch, item, 2).await;
        let token = teller_token(teller, org, branch);
        let app = app!(&pool);

        for status in ["confirmed", "preparing", "ready", "out_for_delivery"] {
            let (st, b) = send(
                &app,
                auth(test::TestRequest::post().uri(&format!("/delivery-orders/{id}/status")), &token)
                    .set_json(json!({ "status": status })),
            )
            .await;
            assert_eq!(st, StatusCode::OK, "status {status}: {b}");
        }
        // receipt printed once, at confirm
        let printed: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT receipt_printed_at FROM delivery_orders WHERE id=$1").bind(id).fetch_one(&pool).await.unwrap();
        assert!(printed.is_some());

        let (st, b) = send(
            &app,
            auth(test::TestRequest::post().uri(&format!("/delivery-orders/{id}/finalize")), &token)
                .set_json(json!({ "shift_id": shift, "payment_method": "cash" })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "finalize: {b}");
        let order_id = Uuid::parse_str(b["order_id"].as_str().unwrap()).unwrap();

        // delivery order linked + delivered
        let (status, linked): (String, Option<Uuid>) =
            sqlx::query_as("SELECT status::text, order_id FROM delivery_orders WHERE id=$1").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(status, "delivered");
        assert_eq!(linked, Some(order_id));

        // a normal completed delivery sale
        let (ostatus, otype, total, fee, oref): (String, String, i32, i32, Option<String>) = sqlx::query_as(
            "SELECT status::text, order_type, total_amount, delivery_fee, order_ref FROM orders WHERE id=$1",
        )
        .bind(order_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(ostatus, "completed");
        assert_eq!(otype, "delivery");
        assert_eq!(total, 1300);
        assert_eq!(fee, 300);
        assert!(oref.is_some());

        // inventory deducted (20g × 2) + a sale movement recorded
        let stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE branch_id=$1 AND org_ingredient_id=$2")
            .bind(branch).bind(ing).fetch_one(&pool).await.unwrap();
        assert!((stock - 960.0).abs() < 1e-6, "stock={stock}");
        let moves: i64 = sqlx::query_scalar("SELECT count(*) FROM inventory_movements WHERE source_id=$1 AND type='sale'")
            .bind(order_id).fetch_one(&pool).await.unwrap();
        assert_eq!(moves, 1);
    }

    #[sqlx::test]
    async fn finalize_replays_frozen_snapshot_not_live_price(pool: PgPool) {
        perms(&pool).await;
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        assign(&pool, teller, branch).await;
        let shift = seed_shift(&pool, branch, teller).await;
        seed_settings(&pool, branch, true, false, 0).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 10.0, 1000.0).await;

        let id = place_in_mall_order(&pool, branch, item, 1).await;

        // Dashboard edits the price AFTER the order is placed — must NOT affect it.
        sqlx::query("UPDATE menu_items SET base_price = 999 WHERE id=$1").bind(item).execute(&pool).await.unwrap();

        let token = teller_token(teller, org, branch);
        let app = app!(&pool);
        let (st, b) = send(
            &app,
            auth(test::TestRequest::post().uri(&format!("/delivery-orders/{id}/finalize")), &token)
                .set_json(json!({ "shift_id": shift, "payment_method": "cash" })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let order_id = Uuid::parse_str(b["order_id"].as_str().unwrap()).unwrap();

        let total: i32 = sqlx::query_scalar("SELECT total_amount FROM orders WHERE id=$1").bind(order_id).fetch_one(&pool).await.unwrap();
        assert_eq!(total, 500, "must use the frozen price, not the edited 999");
        let unit: i32 = sqlx::query_scalar("SELECT unit_price FROM order_items WHERE order_id=$1").bind(order_id).fetch_one(&pool).await.unwrap();
        assert_eq!(unit, 500);
    }

    #[sqlx::test]
    async fn cancel_restock_true_leaves_inventory(pool: PgPool) {
        perms(&pool).await;
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        assign(&pool, teller, branch).await;
        seed_shift(&pool, branch, teller).await;
        seed_settings(&pool, branch, true, false, 0).await;
        let item = seed_item(&pool, org, 500).await;
        let ing = seed_recipe(&pool, org, branch, item, 20.0, 1000.0).await;
        let id = place_in_mall_order(&pool, branch, item, 1).await;

        let token = teller_token(teller, org, branch);
        let app = app!(&pool);
        // advance past received so cancel yields "cancelled" (received → "rejected")
        send(&app, auth(test::TestRequest::post().uri(&format!("/delivery-orders/{id}/status")), &token).set_json(json!({"status":"confirmed"}))).await;
        let (st, _) = send(
            &app,
            auth(test::TestRequest::post().uri(&format!("/delivery-orders/{id}/cancel")), &token)
                .set_json(json!({ "reason": "test", "restore_inventory": true })),
        )
        .await;
        assert_eq!(st, StatusCode::OK);

        let stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE branch_id=$1 AND org_ingredient_id=$2")
            .bind(branch).bind(ing).fetch_one(&pool).await.unwrap();
        assert!((stock - 1000.0).abs() < 1e-6);
        let waste: i64 = sqlx::query_scalar("SELECT count(*) FROM inventory_movements WHERE source_id=$1 AND type='waste'").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(waste, 0);
        let status: String = sqlx::query_scalar("SELECT status::text FROM delivery_orders WHERE id=$1").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(status, "cancelled");
    }

    #[sqlx::test]
    async fn cancel_restock_false_wastes_inventory(pool: PgPool) {
        perms(&pool).await;
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        assign(&pool, teller, branch).await;
        seed_shift(&pool, branch, teller).await;
        seed_settings(&pool, branch, true, false, 0).await;
        let item = seed_item(&pool, org, 500).await;
        let ing = seed_recipe(&pool, org, branch, item, 20.0, 1000.0).await;
        let id = place_in_mall_order(&pool, branch, item, 1).await;

        let token = teller_token(teller, org, branch);
        let app = app!(&pool);
        // move past received so it's a cancel, not a reject
        let (cst, _) = send(&app, auth(test::TestRequest::post().uri(&format!("/delivery-orders/{id}/status")), &token).set_json(json!({"status":"confirmed"}))).await;
        assert_eq!(cst, StatusCode::OK);
        let (st, _) = send(
            &app,
            auth(test::TestRequest::post().uri(&format!("/delivery-orders/{id}/cancel")), &token)
                .set_json(json!({ "reason": "no-show", "restore_inventory": false })),
        )
        .await;
        assert_eq!(st, StatusCode::OK);

        let stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE branch_id=$1 AND org_ingredient_id=$2")
            .bind(branch).bind(ing).fetch_one(&pool).await.unwrap();
        assert!((stock - 980.0).abs() < 1e-6, "stock={stock}");
        let waste: i64 = sqlx::query_scalar("SELECT count(*) FROM inventory_movements WHERE source_id=$1 AND type='waste'").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(waste, 1);
    }

    #[sqlx::test]
    async fn reject_from_received(pool: PgPool) {
        perms(&pool).await;
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        assign(&pool, teller, branch).await;
        seed_shift(&pool, branch, teller).await;
        seed_settings(&pool, branch, true, false, 0).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 5.0, 100.0).await;
        let id = place_in_mall_order(&pool, branch, item, 1).await;

        let token = teller_token(teller, org, branch);
        let app = app!(&pool);
        let (st, _) = send(
            &app,
            auth(test::TestRequest::post().uri(&format!("/delivery-orders/{id}/cancel")), &token)
                .set_json(json!({ "reason": "spam" })),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let status: String = sqlx::query_scalar("SELECT status::text FROM delivery_orders WHERE id=$1").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(status, "rejected");
    }

    #[sqlx::test]
    async fn finalize_twice_conflicts(pool: PgPool) {
        perms(&pool).await;
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        assign(&pool, teller, branch).await;
        let shift = seed_shift(&pool, branch, teller).await;
        seed_settings(&pool, branch, true, false, 0).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 5.0, 100.0).await;
        let id = place_in_mall_order(&pool, branch, item, 1).await;

        let token = teller_token(teller, org, branch);
        let app = app!(&pool);
        let fin = json!({ "shift_id": shift, "payment_method": "cash" });
        let (st1, _) = send(&app, auth(test::TestRequest::post().uri(&format!("/delivery-orders/{id}/finalize")), &token).set_json(&fin)).await;
        assert_eq!(st1, StatusCode::OK);
        let (st2, _) = send(&app, auth(test::TestRequest::post().uri(&format!("/delivery-orders/{id}/finalize")), &token).set_json(&fin)).await;
        assert_eq!(st2, StatusCode::CONFLICT);
    }

    #[sqlx::test]
    async fn staff_action_requires_permission(pool: PgPool) {
        // NB: perms NOT seeded → teller has no delivery_orders grant.
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        assign(&pool, teller, branch).await;
        seed_shift(&pool, branch, teller).await;
        seed_settings(&pool, branch, true, false, 0).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 5.0, 100.0).await;
        let id = place_in_mall_order(&pool, branch, item, 1).await;

        let token = teller_token(teller, org, branch);
        let app = app!(&pool);
        let (st, _) = send(&app, auth(test::TestRequest::post().uri(&format!("/delivery-orders/{id}/status")), &token).set_json(json!({"status":"confirmed"}))).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn pos_override_cannot_open_disabled_channel(pool: PgPool) {
        perms(&pool).await;
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        assign(&pool, teller, branch).await;
        seed_settings(&pool, branch, false, false, 0).await; // in_mall disabled by dashboard
        let token = teller_token(teller, org, branch);
        let app = app!(&pool);

        let (st, _) = send(
            &app,
            auth(test::TestRequest::post().uri("/delivery/accepting"), &token)
                .set_json(json!({ "branch_id": branch, "channel": "in_mall", "override": "open" })),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT);

        // enable it, then the POS may pause/open
        sqlx::query("UPDATE branch_delivery_settings SET in_mall_enabled=true WHERE branch_id=$1").bind(branch).execute(&pool).await.unwrap();
        let (st2, b) = send(
            &app,
            auth(test::TestRequest::post().uri("/delivery/accepting"), &token)
                .set_json(json!({ "branch_id": branch, "channel": "in_mall", "override": "open" })),
        )
        .await;
        assert_eq!(st2, StatusCode::OK, "{b}");
        assert_eq!(b["in_mall_override"], "open");
    }

    #[sqlx::test]
    async fn zones_crud(pool: PgPool) {
        perms(&pool).await;
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let admin = seed_user(&pool, org, "org_admin").await;
        let token = admin_token(admin, org);
        let app = app!(&pool);

        // create a ring with a flat fee
        let (st, z) = send(
            &app,
            auth(test::TestRequest::post().uri("/delivery/zones"), &token).set_json(json!({
                "branch_id": branch, "name": "Inner",
                "max_road_distance_meters": 2000, "fee": 1500
            })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{z}");
        assert_eq!(z["fee"], 1500);
        let zid = z["id"].as_str().unwrap().to_string();

        // duplicate distance → conflict (one ring per distance per branch)
        let (dup, _) = send(
            &app,
            auth(test::TestRequest::post().uri("/delivery/zones"), &token).set_json(json!({
                "branch_id": branch, "name": "Dup",
                "max_road_distance_meters": 2000, "fee": 100
            })),
        )
        .await;
        assert_eq!(dup, StatusCode::CONFLICT);

        // negative fee → bad request
        let (bad, _) = send(
            &app,
            auth(test::TestRequest::post().uri("/delivery/zones"), &token).set_json(json!({
                "branch_id": branch, "name": "M",
                "max_road_distance_meters": 4000, "fee": -5
            })),
        )
        .await;
        assert_eq!(bad, StatusCode::BAD_REQUEST);

        // list
        let (lst, list) = send(&app, auth(test::TestRequest::get().uri(&format!("/delivery/zones?branch_id={branch}")), &token)).await;
        assert_eq!(lst, StatusCode::OK);
        assert_eq!(list.as_array().unwrap().len(), 1);

        // delete
        let (del, _) = send(&app, auth(test::TestRequest::delete().uri(&format!("/delivery/zones/{zid}?branch_id={branch}")), &token)).await;
        assert_eq!(del, StatusCode::NO_CONTENT);
    }

    #[sqlx::test]
    async fn settings_roundtrip(pool: PgPool) {
        perms(&pool).await;
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let admin = seed_user(&pool, org, "org_admin").await;
        let token = admin_token(admin, org);
        let app = app!(&pool);

        let (sst, _) = send(
            &app,
            auth(test::TestRequest::put().uri("/delivery/settings"), &token).set_json(json!({
                "branch_id": branch, "in_mall_enabled": true, "outside_enabled": true,
                "in_mall_fee": 250, "prep_time_minutes": 30
            })),
        )
        .await;
        assert_eq!(sst, StatusCode::OK);
        let (_, s) = send(&app, auth(test::TestRequest::get().uri(&format!("/delivery/settings?branch_id={branch}")), &token)).await;
        assert_eq!(s["in_mall_enabled"], true);
        assert_eq!(s["outside_enabled"], true);
        assert_eq!(s["in_mall_fee"], 250);
        assert_eq!(s["prep_time_minutes"], 30);
    }

    #[sqlx::test]
    async fn intake_idempotency_replays(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 0).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 5.0, 100.0).await;

        let key = Uuid::new_v4().to_string();
        let body = intake_body(branch, "in_mall", json!([{ "menu_item_id": item, "quantity": 1 }]));
        let app = app!(&pool);
        let (s1, b1) = send(&app, test::TestRequest::post().uri("/public/delivery-orders").insert_header(("Idempotency-Key", key.clone())).set_json(&body)).await;
        assert_eq!(s1, StatusCode::CREATED);
        let (_s2, b2) = send(&app, test::TestRequest::post().uri("/public/delivery-orders").insert_header(("Idempotency-Key", key)).set_json(&body)).await;
        assert_eq!(b1["id"], b2["id"], "same key must replay the same order");
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM delivery_orders WHERE branch_id=$1").bind(branch).fetch_one(&pool).await.unwrap();
        assert_eq!(count, 1);
    }

    async fn seed_addon(pool: &PgPool, org: Uuid, price: i32) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO addon_items (id, org_id, name, type, default_price) VALUES ($1,$2,'Extra','extra',$3)")
            .bind(id)
            .bind(org)
            .bind(price)
            .execute(pool)
            .await
            .unwrap();
        id
    }

    async fn seed_addon_typed(pool: &PgPool, org: Uuid, name: &str, addon_type: &str, price: i32) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO addon_items (id, org_id, name, type, default_price) VALUES ($1,$2,$3,$4,$5)")
            .bind(id)
            .bind(org)
            .bind(name)
            .bind(addon_type)
            .bind(price)
            .execute(pool)
            .await
            .unwrap();
        id
    }

    /// Give `item` a recipe milk ingredient (org_ingredients.category='milk') and
    /// return that ingredient id. Mirrors the POS default-milk base ingredient.
    async fn seed_milk_recipe(pool: &PgPool, org: Uuid, item: Uuid) -> Uuid {
        let ing = Uuid::new_v4();
        sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category) VALUES ($1,$2,'Milk','ml'::inventory_unit,50,'milk')")
            .bind(ing)
            .bind(org)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO menu_item_recipes (menu_item_id, org_ingredient_id, quantity_used, size_label, ingredient_name, ingredient_unit) VALUES ($1,$2,200,'one_size','Milk','ml')")
            .bind(item)
            .bind(ing)
            .execute(pool)
            .await
            .unwrap();
        ing
    }

    /// Bind a milk_type addon to `ing` (addon_item_ingredients), making it the
    /// base/default milk for any item whose recipe uses `ing`.
    async fn seed_milk_addon_for_ingredient(pool: &PgPool, org: Uuid, ing: Uuid, name: &str) -> Uuid {
        let addon = seed_addon_typed(pool, org, name, "milk_type", 0).await;
        sqlx::query("INSERT INTO addon_item_ingredients (addon_item_id, org_ingredient_id, quantity_used, ingredient_name, ingredient_unit) VALUES ($1,$2,200,'Milk','ml')")
            .bind(addon)
            .bind(ing)
            .execute(pool)
            .await
            .unwrap();
        addon
    }

    async fn seed_optional(pool: &PgPool, item: Uuid, name: &str, price: i32, size_label: Option<&str>) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO menu_item_optional_fields (id, menu_item_id, name, price, size_label, is_active) \
             VALUES ($1,$2,$3,$4,$5::item_size,true)",
        )
        .bind(id)
        .bind(item)
        .bind(name)
        .bind(price)
        .bind(size_label)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    #[sqlx::test]
    async fn public_menu_exposes_global_addon_catalog_and_channel_price(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        seed_settings(&pool, branch, true, false, 0).await;
        let item = seed_item(&pool, org, 500).await;

        // Org-wide global addon catalog (POS model): no per-item slots involved.
        let regular = seed_addon_typed(&pool, org, "Regular Milk", "milk_type", 0).await;
        let oat = seed_addon_typed(&pool, org, "Oat Milk", "milk_type", 100).await;
        let shot = seed_addon_typed(&pool, org, "Extra Shot", "extra", 150).await;
        let cream = seed_addon_typed(&pool, org, "Whipped Cream", "extra", 80).await;

        // Channel override: oat milk costs 130 on in_mall (channel beats default 100).
        sqlx::query("INSERT INTO branch_channel_addon_overrides (branch_id, addon_item_id, channel, price_override) VALUES ($1,$2,'in_mall'::delivery_channel,$3)")
            .bind(branch).bind(oat).bind(130)
            .execute(&pool).await.unwrap();
        // Channel disables whipped cream on in_mall → excluded from the catalog.
        sqlx::query("INSERT INTO branch_channel_addon_overrides (branch_id, addon_item_id, channel, is_available) VALUES ($1,$2,'in_mall'::delivery_channel,false)")
            .bind(branch).bind(cream)
            .execute(&pool).await.unwrap();

        // A per-item optional field (still item-scoped, kept).
        seed_optional(&pool, item, "Extra Hot", 0, None).await;

        let app = app!(&pool);
        let (st, b) = send(&app, test::TestRequest::get().uri(&format!("/public/branches/{branch}/menu?channel=in_mall"))).await;
        assert_eq!(st, StatusCode::OK, "{b}");

        // Top-level global addon catalog (one per request, applies to every item).
        let addons = b["addons"].as_array().unwrap();
        assert_eq!(addons.len(), 3, "channel-disabled cream must be excluded: {b}");

        let oat_opt = addons.iter().find(|o| o["addon_item_id"] == oat.to_string()).unwrap();
        assert_eq!(oat_opt["price"], 130, "channel-effective price, not default 100");
        assert_eq!(oat_opt["type"], "milk_type");
        assert_eq!(oat_opt["is_available"], true);

        let reg_opt = addons.iter().find(|o| o["addon_item_id"] == regular.to_string()).unwrap();
        assert_eq!(reg_opt["price"], 0);
        assert_eq!(reg_opt["type"], "milk_type");

        let shot_opt = addons.iter().find(|o| o["addon_item_id"] == shot.to_string()).unwrap();
        assert_eq!(shot_opt["price"], 150);
        assert_eq!(shot_opt["type"], "extra");

        // Channel-disabled cream is absent entirely.
        assert!(
            addons.iter().all(|o| o["addon_item_id"] != cream.to_string()),
            "channel-disabled cream must not appear: {b}"
        );

        // Per-item structures unchanged: no addon_slots field, optionals kept.
        let items = b["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        let it = &items[0];
        assert!(it.get("addon_slots").is_none(), "per-item addon_slots must be gone: {it}");
        let opts = it["optionals"].as_array().unwrap();
        assert_eq!(opts.len(), 1);
        assert_eq!(opts[0]["name"], "Extra Hot");
    }

    #[sqlx::test]
    async fn public_menu_exposes_default_milk_addon(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        seed_settings(&pool, branch, true, false, 0).await;

        // Latte: recipe milk ingredient + a milk_type addon bound to that ingredient
        // → that addon is the item's base/default milk.
        let latte = seed_item(&pool, org, 600).await;
        let milk_ing = seed_milk_recipe(&pool, org, latte).await;
        let regular_milk = seed_milk_addon_for_ingredient(&pool, org, milk_ing, "Regular Milk").await;
        // A second, unrelated milk addon (different ingredient) must NOT be picked.
        let other_milk_ing = Uuid::new_v4();
        sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category) VALUES ($1,$2,'OatMilk','ml'::inventory_unit,90,'milk')")
            .bind(other_milk_ing).bind(org).execute(&pool).await.unwrap();
        seed_milk_addon_for_ingredient(&pool, org, other_milk_ing, "Oat Milk").await;

        // Cookie: a non-milk item (general recipe ingredient only) → no default milk.
        let cookie = seed_item(&pool, org, 200).await;
        seed_recipe(&pool, org, branch, cookie, 1.0, 100.0).await;

        let app = app!(&pool);
        let (st, b) = send(&app, test::TestRequest::get().uri(&format!("/public/branches/{branch}/menu?channel=in_mall"))).await;
        assert_eq!(st, StatusCode::OK, "{b}");

        let items = b["items"].as_array().unwrap();
        let latte_item = items.iter().find(|it| it["id"] == latte.to_string()).unwrap();
        assert_eq!(
            latte_item["default_milk_addon_id"], regular_milk.to_string(),
            "default milk must be the milk_type addon matching the recipe milk ingredient: {latte_item}"
        );

        let cookie_item = items.iter().find(|it| it["id"] == cookie.to_string()).unwrap();
        assert!(
            cookie_item["default_milk_addon_id"].is_null(),
            "non-milk item must have no default milk: {cookie_item}"
        );
    }

    #[sqlx::test]
    async fn channel_addon_override_applied_at_intake(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 0).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;
        let addon = seed_addon(&pool, org, 100).await; // branch-effective default 100

        // Channel addon override: 300 for in_mall.
        sqlx::query("INSERT INTO branch_channel_addon_overrides (branch_id, addon_item_id, channel, price_override) VALUES ($1,$2,'in_mall'::delivery_channel,$3)")
            .bind(branch).bind(addon).bind(300)
            .execute(&pool).await.unwrap();

        let app = app!(&pool);
        let body = intake_body(
            branch,
            "in_mall",
            json!([{ "menu_item_id": item, "quantity": 1, "addons": [{ "addon_item_id": addon, "quantity": 1 }] }]),
        );
        let (st, b) = send(&app, test::TestRequest::post().uri("/public/delivery-orders").set_json(&body)).await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
        // 500 (item) + 300 (channel addon override, NOT the 100 default) = 800
        assert_eq!(b["subtotal"], 800);

        // Mark the addon unavailable for the channel → intake rejects.
        sqlx::query("UPDATE branch_channel_addon_overrides SET is_available = false WHERE branch_id=$1 AND addon_item_id=$2")
            .bind(branch).bind(addon).execute(&pool).await.unwrap();
        let (st2, _) = send(&app, test::TestRequest::post().uri("/public/delivery-orders").set_json(&body)).await;
        assert_eq!(st2, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn channel_addon_overrides_crud(pool: PgPool) {
        perms(&pool).await;
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let admin = seed_user(&pool, org, "org_admin").await;
        let addon = seed_addon(&pool, org, 100).await;
        let token = admin_token(admin, org);
        let app = app!(&pool);

        // upsert
        let (st, o) = send(
            &app,
            auth(test::TestRequest::put().uri("/delivery/channel-addon-overrides"), &token).set_json(json!({
                "branch_id": branch, "addon_item_id": addon, "channel": "outside",
                "price_override": 250, "is_available": true
            })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{o}");
        assert_eq!(o["price_override"], 250);

        // list
        let (lst, list) = send(&app, auth(test::TestRequest::get().uri(&format!("/delivery/channel-addon-overrides?branch_id={branch}&channel=outside")), &token)).await;
        assert_eq!(lst, StatusCode::OK);
        assert_eq!(list.as_array().unwrap().len(), 1);

        // delete
        let (del, _) = send(&app, auth(test::TestRequest::delete().uri(&format!("/delivery/channel-addon-overrides?branch_id={branch}&addon_item_id={addon}&channel=outside")), &token)).await;
        assert_eq!(del, StatusCode::NO_CONTENT);
        let (_, list2) = send(&app, auth(test::TestRequest::get().uri(&format!("/delivery/channel-addon-overrides?branch_id={branch}&channel=outside")), &token)).await;
        assert_eq!(list2.as_array().unwrap().len(), 0);
    }

    #[sqlx::test]
    async fn set_prep_time_increments(pool: PgPool) {
        perms(&pool).await;
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        assign(&pool, teller, branch).await;
        seed_shift(&pool, branch, teller).await;
        seed_settings(&pool, branch, true, false, 0).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 5.0, 100.0).await;
        let id = place_in_mall_order(&pool, branch, item, 1).await;
        let token = teller_token(teller, org, branch);
        let app = app!(&pool);

        // not a multiple of 5 → bad request
        let (bad, _) = send(&app, auth(test::TestRequest::post().uri(&format!("/delivery-orders/{id}/prep-time")), &token).set_json(json!({"extra_prep_minutes": 7}))).await;
        assert_eq!(bad, StatusCode::BAD_REQUEST);

        // +15 ok
        let (ok, b) = send(&app, auth(test::TestRequest::post().uri(&format!("/delivery-orders/{id}/prep-time")), &token).set_json(json!({"extra_prep_minutes": 15}))).await;
        assert_eq!(ok, StatusCode::OK, "{b}");
        assert_eq!(b["extra_prep_minutes"], 15);
    }

    #[sqlx::test]
    async fn quote_falls_back_to_haversine_when_osrm_down(pool: PgPool) {
        // OSRM_URL is unset in the test process → the quote must fall back to the
        // straight-line (haversine) distance and still resolve the covering zone.
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await; // seeded at lat 30.0, lng 31.0
        seed_settings(&pool, branch, false, true, 0).await; // outside enabled
        // A wide ring (50 km) so the ~1 km straight-line distance is covered.
        sqlx::query("INSERT INTO delivery_zones (branch_id, name, max_road_distance_meters, fee) VALUES ($1,'Zone 1',50000,2500)")
            .bind(branch)
            .execute(&pool)
            .await
            .unwrap();

        let app = app!(&pool);
        // Customer ~1.1 km north of the branch.
        let (st, b) = send(
            &app,
            test::TestRequest::get()
                .uri(&format!("/public/branches/{branch}/delivery-quote?lat=30.01&lng=31.0&channel=outside")),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["status"], "ok");
        assert_eq!(b["fee"], 2500);
    }

    // ── SSE stream endpoint (auth contract only; body is infinite) ──

    #[sqlx::test]
    async fn stream_requires_auth(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let app = app!(&pool);
        // No bearer → JwtMiddleware rejects before the stream opens.
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/delivery-orders/stream?branch_id={branch}"))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    async fn stream_requires_permission(pool: PgPool) {
        // perms NOT seeded → teller has no delivery_orders:read grant.
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        assign(&pool, teller, branch).await;
        let token = teller_token(teller, org, branch);
        let app = app!(&pool);
        let resp = test::call_service(
            &app,
            auth(
                test::TestRequest::get().uri(&format!("/delivery-orders/stream?branch_id={branch}")),
                &token,
            )
            .to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn stream_opens_for_authorized_teller(pool: PgPool) {
        perms(&pool).await;
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        assign(&pool, teller, branch).await;
        let token = teller_token(teller, org, branch);
        let app = app!(&pool);
        // Do NOT read the body — it's an infinite event-stream. Assert the
        // response head only (status + content-type).
        let resp = test::call_service(
            &app,
            auth(
                test::TestRequest::get().uri(&format!("/delivery-orders/stream?branch_id={branch}")),
                &token,
            )
            .to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("content-type").unwrap(), "text/event-stream");
        // Must opt out of compression so the Compress middleware can't buffer
        // SSE frames (it skips any response that already has Content-Encoding).
        assert_eq!(resp.headers().get("content-encoding").unwrap(), "identity");
    }
}
