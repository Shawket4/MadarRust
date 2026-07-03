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
    assert!(channel_open(
        true,
        "open",
        Some(t(9, 0)),
        Some(t(17, 0)),
        t(23, 0),
        true
    ));
    // 'auto' obeys the window.
    assert!(!channel_open(
        true,
        "auto",
        Some(t(9, 0)),
        Some(t(17, 0)),
        t(23, 0),
        true
    ));
    assert!(channel_open(
        true,
        "auto",
        Some(t(9, 0)),
        Some(t(17, 0)),
        t(12, 0),
        true
    ));
}

#[test]
fn normalize_phone_egypt() {
    assert_eq!(normalize_phone("01012345678").unwrap(), "201012345678");
    assert_eq!(normalize_phone("201012345678").unwrap(), "201012345678");
    assert_eq!(normalize_phone("+20 101 234 5678").unwrap(), "201012345678");
    assert_eq!(normalize_phone("0020 1012345678").unwrap(), "201012345678");
    // Bare national mobile typed without the leading 0 still resolves to +20.
    assert_eq!(normalize_phone("1012345678").unwrap(), "201012345678");
    assert_eq!(normalize_phone("1 012 345 678").unwrap(), "201012345678");
    assert!(normalize_phone("123").is_err());
    // Bounded on both ends: an over-long raw string and an implausibly long
    // normalised number (> 15 digits, beyond E.164) are both rejected.
    assert!(normalize_phone(&"0".repeat(40)).is_err());
    assert!(normalize_phone("2010123456789012").is_err());
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

#[test]
fn validate_field_helpers() {
    // Required text: non-empty after trim, bounded length (chars, not bytes).
    assert!(validate_required_text("Name", "Sara", 120).is_ok());
    assert!(validate_required_text("Name", "   ", 120).is_err());
    assert!(validate_required_text("Name", &"x".repeat(121), 120).is_err());
    // Multi-byte chars counted as chars, not bytes.
    assert!(validate_required_text("Name", &"أ".repeat(120), 120).is_ok());

    // Optional text: None is fine; bounded when present.
    assert!(validate_optional_text("Floor", None, 120).is_ok());
    assert!(validate_optional_text("Floor", Some("3"), 120).is_ok());
    assert!(validate_optional_text("Floor", Some(&"x".repeat(121)), 120).is_err());

    // Payment hint is constrained to the documented set.
    assert!(validate_payment_hint("cash").is_ok());
    assert!(validate_payment_hint("card").is_ok());
    assert!(validate_payment_hint("crypto").is_err());
    assert!(validate_payment_hint("").is_err());

    // Coordinates must be finite and within WGS84 bounds.
    assert!(validate_coords(30.0, 31.0).is_ok());
    assert!(validate_coords(0.0, 0.0).is_ok());
    assert!(validate_coords(91.0, 0.0).is_err());
    assert!(validate_coords(0.0, 181.0).is_err());
    assert!(validate_coords(f64::NAN, 0.0).is_err());
    assert!(validate_coords(0.0, f64::INFINITY).is_err());
}

// ── Pure zone/fee math (no OSRM, no DB) ───────────────────────

#[cfg(test)]
mod zone_fee {
    use crate::delivery::public::{FeeOutcome, ZoneRow, select_zone_fee};
    use uuid::Uuid;

    fn zone(max: i32, fee: i32) -> ZoneRow {
        ZoneRow {
            id: Uuid::new_v4(),
            name: format!("ring-{max}"),
            fee,
            max_road_distance_meters: max,
        }
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
        assert!(matches!(
            select_zone_fee(600, None, &zones),
            FeeOutcome::OutOfRange
        ));
    }

    #[test]
    fn branch_hard_cap_forces_out_of_range() {
        let zones = [zone(5000, 1000)];
        assert!(matches!(
            select_zone_fee(700, Some(600), &zones),
            FeeOutcome::OutOfRange
        ));
    }

    #[test]
    fn no_zones_is_out_of_range() {
        assert!(matches!(
            select_zone_fee(100, None, &[]),
            FeeOutcome::OutOfRange
        ));
    }

    #[test]
    fn distance_exactly_at_branch_cap_is_allowed() {
        // distance == max_dist is WITHIN range (the cap is inclusive). Mutating the
        // cap check `distance > max` to `>=` would wrongly reject this exact-boundary
        // case — flagged by mutation testing at delivery/public.rs:557.
        let zones = [zone(1000, 1500)];
        assert_eq!(fee_of(select_zone_fee(1000, Some(1000), &zones)), 1500);
    }
}

// ── Broadcast hub (pure, no DB) ───────────────────────────────

#[cfg(test)]
mod hub_tests {
    use crate::delivery::staff::DeliveryOrder;
    use crate::realtime::event::{BranchEvent, Topic};
    use crate::realtime::hub::BranchEventHub;
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

    fn event(branch_id: Uuid) -> BranchEvent {
        BranchEvent::new(Topic::Delivery, "delivery.created", &make_order(branch_id))
    }

    #[test]
    fn publish_reaches_only_its_branch() {
        let hub = BranchEventHub::new();
        let branch_a = Uuid::new_v4();
        let branch_b = Uuid::new_v4();
        let mut rx_a = hub.subscribe(branch_a);
        let mut rx_b = hub.subscribe(branch_b);

        hub.publish(branch_a, event(branch_a));

        let ev = rx_a
            .try_recv()
            .expect("branch A subscriber should receive its event");
        assert_eq!(ev.event_type, "delivery.created");
        assert_eq!(ev.topic, Topic::Delivery);
        assert_eq!(
            ev.data["branch_id"],
            serde_json::Value::String(branch_a.to_string())
        );

        // Tenant isolation: branch B must NEVER see branch A's event.
        assert!(matches!(
            rx_b.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn publish_with_no_subscribers_is_noop() {
        let hub = BranchEventHub::new();
        // No channel exists for this branch yet — must not panic.
        hub.publish(Uuid::new_v4(), event(Uuid::new_v4()));
    }

    #[test]
    fn multiple_subscribers_on_a_branch_all_receive() {
        let hub = BranchEventHub::new();
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
    use actix_web::{App, http::StatusCode, test, web};
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use uuid::Uuid;

    use crate::auth::jwt::{JwtSecret, create_token};
    use crate::models::UserRole;

    fn get_secret() -> JwtSecret {
        JwtSecret("secret".into())
    }
    fn teller_token(uid: Uuid, org: Uuid, branch: Uuid) -> String {
        create_token(
            &get_secret(),
            uid,
            Some(org),
            UserRole::Teller,
            Some(branch),
            24,
        )
        .unwrap()
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
                    .app_data(web::Data::new(crate::realtime::hub::BranchEventHub::new()))
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
        seed_branch_named(pool, org, "Br").await
    }
    async fn seed_branch_named(pool: &PgPool, org: Uuid, name: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO branches (id, org_id, name, latitude, longitude) VALUES ($1,$2,$3,30.0,31.0)")
            .bind(id)
            .bind(org)
            .bind(name)
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
    async fn seed_settings(
        pool: &PgPool,
        branch: Uuid,
        in_mall: bool,
        outside: bool,
        in_mall_fee: i32,
    ) {
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
    async fn seed_discount(pool: &PgPool, org: Uuid, dtype: &str, value: i32) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO discounts (id, org_id, name, type, value, is_active) VALUES ($1,$2,'D',$3::discount_type,$4,true)")
            .bind(id)
            .bind(org)
            .bind(dtype)
            .bind(value)
            .execute(pool)
            .await
            .unwrap();
        id
    }
    async fn set_in_mall_discount(pool: &PgPool, branch: Uuid, discount: Option<Uuid>) {
        sqlx::query(
            "UPDATE branch_delivery_settings SET in_mall_discount_id = $2 WHERE branch_id = $1",
        )
        .bind(branch)
        .bind(discount)
        .execute(pool)
        .await
        .unwrap();
    }
    async fn set_in_mall_require_location(pool: &PgPool, branch: Uuid, required: bool) {
        sqlx::query("UPDATE branch_delivery_settings SET in_mall_require_location = $2 WHERE branch_id = $1")
            .bind(branch)
            .bind(required)
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
    async fn seed_recipe(
        pool: &PgPool,
        org: Uuid,
        branch: Uuid,
        item: Uuid,
        qty: f64,
        stock: f64,
    ) -> Uuid {
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
    async fn seed_channel_override(
        pool: &PgPool,
        branch: Uuid,
        item: Uuid,
        channel: &str,
        price: Option<i32>,
        available: Option<bool>,
    ) {
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
        crate::permissions::seeder::seed_role_permissions(pool)
            .await
            .unwrap();
    }

    const PHONE: &str = "01000000000";

    fn intake_body(branch: Uuid, channel: &str, items: Value) -> Value {
        // Destination details satisfy both channels' required-field rules:
        // in-mall needs shop/company + floor + unit; outside needs an address line.
        // Coordinates (at the seeded branch) satisfy the location requirement for
        // both channels (outside: the delivery pin; in-mall: the at-branch GPS
        // check, which is on by default).
        json!({
            "branch_id": branch, "channel": channel,
            "customer_name": "Sara", "customer_phone": PHONE,
            "place_name": "Shop 12", "floor": "2", "unit_number": "B4",
            "address_line": "12 Main St, Bldg 3",
            "customer_lat": 30.0, "customer_lng": 31.0,
            "payment_method_hint": "cash", "device_token": device_token(PHONE),
            "items": items,
        })
    }

    async fn place_in_mall_order(pool: &PgPool, branch: Uuid, item: Uuid, qty: i32) -> Uuid {
        let app = app!(pool);
        let body = intake_body(
            branch,
            "in_mall",
            json!([{ "menu_item_id": item, "quantity": qty }]),
        );
        let (st, b) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
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
        let body = intake_body(
            branch,
            "in_mall",
            json!([{ "menu_item_id": item, "quantity": 2 }]),
        );
        let (st, b) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
        assert_eq!(b["status"], "received");
        assert_eq!(b["subtotal"], 1000);
        assert_eq!(b["delivery_fee"], 300);
        assert_eq!(b["total"], 1300);
        assert!(b["delivery_ref"].as_str().unwrap().starts_with("D-"));
    }

    /// By default in-mall requires the device GPS "confirm you're at the branch"
    /// location — an order with no coordinates is rejected.
    #[sqlx::test]
    async fn intake_in_mall_requires_location_by_default(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 300).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 20.0, 1000.0).await;

        let app = app!(&pool);
        let mut body = intake_body(
            branch,
            "in_mall",
            json!([{ "menu_item_id": item, "quantity": 1 }]),
        );
        body["customer_lat"] = json!(null);
        body["customer_lng"] = json!(null);
        let (st, b) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    }

    /// When a manager turns the in-mall location requirement off, an order with no
    /// coordinates is accepted (the fee is still the flat in-mall fee).
    #[sqlx::test]
    async fn intake_in_mall_allows_missing_location_when_toggled_off(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 300).await;
        set_in_mall_require_location(&pool, branch, false).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 20.0, 1000.0).await;

        let app = app!(&pool);
        let mut body = intake_body(
            branch,
            "in_mall",
            json!([{ "menu_item_id": item, "quantity": 1 }]),
        );
        body["customer_lat"] = json!(null);
        body["customer_lng"] = json!(null);
        let (st, b) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
        assert_eq!(b["status"], "received");
        assert_eq!(b["delivery_fee"], 300);
    }

    // ── Strict field validation (untrusted public surface) ──

    /// A quantity far above the cap is rejected (and never reaches the
    /// integer-piastre money math where it could overflow / wrap).
    #[sqlx::test]
    async fn intake_rejects_excessive_quantity(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 0).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 20.0, 1000.0).await;

        let app = app!(&pool);
        let body = intake_body(
            branch,
            "in_mall",
            json!([{ "menu_item_id": item, "quantity": 1_000_000 }]),
        );
        let (st, _) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    /// A cart with more lines than the cap is rejected before any per-line work.
    #[sqlx::test]
    async fn intake_rejects_too_many_lines(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 0).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;

        let lines: Vec<Value> = (0..101)
            .map(|_| json!({ "menu_item_id": item, "quantity": 1 }))
            .collect();
        let app = app!(&pool);
        let body = intake_body(branch, "in_mall", Value::Array(lines));
        let (st, _) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    /// The payment-method hint is constrained to the documented set.
    #[sqlx::test]
    async fn intake_rejects_bad_payment_hint(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 0).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;

        let app = app!(&pool);
        let mut body = intake_body(
            branch,
            "in_mall",
            json!([{ "menu_item_id": item, "quantity": 1 }]),
        );
        body["payment_method_hint"] = json!("bitcoin");
        let (st, _) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    /// An over-long free-text field (here: customer name) is rejected.
    #[sqlx::test]
    async fn intake_rejects_oversized_name(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 0).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;

        let app = app!(&pool);
        let mut body = intake_body(
            branch,
            "in_mall",
            json!([{ "menu_item_id": item, "quantity": 1 }]),
        );
        body["customer_name"] = json!("x".repeat(200));
        let (st, _) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    /// A size the item does not offer is a clean 400 (not a Postgres enum-cast 500).
    #[sqlx::test]
    async fn intake_rejects_unknown_size(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 0).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 20.0, 1000.0).await;

        let app = app!(&pool);
        let body = intake_body(
            branch,
            "in_mall",
            json!([{ "menu_item_id": item, "quantity": 1, "size_label": "ginormous" }]),
        );
        let (st, _) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    /// In-mall orders must carry the shop/company (here: omitted) so the runner
    /// can find the customer inside the mall.
    #[sqlx::test]
    async fn intake_in_mall_requires_destination(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 0).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;

        let app = app!(&pool);
        let mut body = intake_body(
            branch,
            "in_mall",
            json!([{ "menu_item_id": item, "quantity": 1 }]),
        );
        body["place_name"] = json!(null);
        let (st, _) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    /// Outside orders must carry a written address line (the pin alone is the
    /// route, not the doorstep).
    #[sqlx::test]
    async fn intake_outside_requires_address(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, false, true, 0).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;

        let app = app!(&pool);
        let mut body = intake_body(
            branch,
            "outside",
            json!([{ "menu_item_id": item, "quantity": 1 }]),
        );
        body["address_line"] = json!(null);
        body["customer_lat"] = json!(30.05);
        body["customer_lng"] = json!(31.23);
        let (st, _) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    /// Out-of-range coordinates are rejected on the public delivery quote.
    #[sqlx::test]
    async fn quote_rejects_invalid_coords(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let app = app!(&pool);
        let uri = format!("/public/branches/{branch}/delivery-quote?lat=999&lng=0&channel=outside");
        let (st, _) = send(&app, test::TestRequest::get().uri(&uri)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
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
        let (st, b) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
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
        let body = intake_body(
            branch,
            "in_mall",
            json!([{ "menu_item_id": item, "quantity": 1 }]),
        );
        let (st, _) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn intake_blocked_without_open_shift(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        seed_settings(&pool, branch, true, false, 0).await; // enabled but NO shift
        let item = seed_item(&pool, org, 500).await;

        let app = app!(&pool);
        let body = intake_body(
            branch,
            "in_mall",
            json!([{ "menu_item_id": item, "quantity": 1 }]),
        );
        let (st, _) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
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
        let body = intake_body(
            branch,
            "in_mall",
            json!([{ "menu_item_id": item, "quantity": 1 }]),
        );
        let (st, _) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
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
        let mut body = intake_body(
            branch,
            "in_mall",
            json!([{ "menu_item_id": item, "quantity": 1 }]),
        );
        body["device_token"] = json!("not-a-real-token");
        let (st, _) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
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
        let (st_bad, _) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/otp/verify")
                .set_json(json!({"phone":PHONE,"code":"0000"})),
        )
        .await;
        assert_eq!(st_bad, StatusCode::BAD_REQUEST);

        let (st_ok, b) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/otp/verify")
                .set_json(json!({"phone":PHONE,"code":"1234"})),
        )
        .await;
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
                auth(
                    test::TestRequest::post().uri(&format!("/delivery-orders/{id}/status")),
                    &token,
                )
                .set_json(json!({ "status": status })),
            )
            .await;
            assert_eq!(st, StatusCode::OK, "status {status}: {b}");
        }
        // receipt printed once, at confirm
        let printed: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT receipt_printed_at FROM delivery_orders WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(printed.is_some());

        let (st, b) = send(
            &app,
            auth(
                test::TestRequest::post().uri(&format!("/delivery-orders/{id}/finalize")),
                &token,
            )
            .set_json(json!({ "shift_id": shift, "payment_method": "cash" })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "finalize: {b}");
        let order_id = Uuid::parse_str(b["order_id"].as_str().unwrap()).unwrap();

        // delivery order linked + delivered
        let (status, linked): (String, Option<Uuid>) =
            sqlx::query_as("SELECT status::text, order_id FROM delivery_orders WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
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
        let moves: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM inventory_movements WHERE source_id=$1 AND type='sale'",
        )
        .bind(order_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(moves, 1);
    }

    #[sqlx::test]
    async fn status_jump_clears_skipped_and_rejects_non_steps(pool: PgPool) {
        // Jumping to any step stamps only the landed step and clears the rest, so
        // the recorded position is exactly where the teller jumped to.
        perms(&pool).await;
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        assign(&pool, teller, branch).await;
        seed_shift(&pool, branch, teller).await;
        seed_settings(&pool, branch, true, false, 300).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 20.0, 1000.0).await;

        let id = place_in_mall_order(&pool, branch, item, 1).await;
        let token = teller_token(teller, org, branch);
        let app = app!(&pool);

        let set = |status: &'static str| {
            let app = &app;
            let token = &token;
            async move {
                send(
                    app,
                    auth(
                        test::TestRequest::post().uri(&format!("/delivery-orders/{id}/status")),
                        token,
                    )
                    .set_json(json!({ "status": status })),
                )
                .await
            }
        };

        // Forward jump received → out_for_delivery (skips confirmed/preparing/ready).
        let (st, b) = set("out_for_delivery").await;
        assert_eq!(st, StatusCode::OK, "forward jump: {b}");
        let (status, c_at, p_at, r_at, o_at): (
            String,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<chrono::DateTime<chrono::Utc>>,
        ) = sqlx::query_as(
            "SELECT status::text, confirmed_at, preparing_at, ready_at, out_for_delivery_at
             FROM delivery_orders WHERE id=$1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "out_for_delivery");
        assert!(o_at.is_some(), "landed step stamped");
        assert!(
            c_at.is_none() && p_at.is_none() && r_at.is_none(),
            "skipped steps cleared"
        );

        // Backward jump out_for_delivery → confirmed clears the later stamp.
        let (st, b) = set("confirmed").await;
        assert_eq!(st, StatusCode::OK, "backward jump: {b}");
        let (status, c_at, o_at): (
            String,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<chrono::DateTime<chrono::Utc>>,
        ) = sqlx::query_as(
            "SELECT status::text, confirmed_at, out_for_delivery_at FROM delivery_orders WHERE id=$1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "confirmed");
        assert!(c_at.is_some());
        assert!(o_at.is_none(), "later step cleared on backward jump");

        // Non-settable targets are rejected (delivered = finalize; received = intake).
        let (st, _) = set("delivered").await;
        assert_eq!(
            st,
            StatusCode::BAD_REQUEST,
            "delivered is not settable here"
        );
        let (st, _) = set("received").await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "received is not settable here");
    }

    #[sqlx::test]
    async fn finalize_surfaces_delivery_on_orders_api(pool: PgPool) {
        // After a delivery order is finalized into a real sale, the standard
        // /orders API must expose the delivery charge, order_type, the link to
        // the delivery order, and (on the detail view) the customer/address
        // context — so the dashboard and POS can show the fee SEPARATELY
        // instead of silently baking it into the total. This is the keystone
        // of the cross-repo delivery integration.
        perms(&pool).await;
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        assign(&pool, teller, branch).await;
        let shift = seed_shift(&pool, branch, teller).await;
        seed_settings(&pool, branch, true, false, 300).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 20.0, 1000.0).await;

        let id = place_in_mall_order(&pool, branch, item, 2).await;
        let token = teller_token(teller, org, branch);

        // Combined app: delivery routes (to finalize) + orders routes (to read).
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool.clone()))
                .app_data(web::Data::new(get_secret()))
                .app_data(web::Data::new(crate::realtime::hub::BranchEventHub::new()))
                .configure(crate::delivery::routes::configure)
                .configure(crate::orders::routes::configure),
        )
        .await;

        // Finalize the delivery order → a real completed sale.
        let (st, b) = send(
            &app,
            auth(
                test::TestRequest::post().uri(&format!("/delivery-orders/{id}/finalize")),
                &token,
            )
            .set_json(json!({ "shift_id": shift, "payment_method": "cash" })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "finalize: {b}");
        let order_id = b["order_id"].as_str().unwrap().to_string();

        // ── Detail view (GET /orders/{id}) ──
        let (st, o) = send(
            &app,
            auth(
                test::TestRequest::get().uri(&format!("/orders/{order_id}")),
                &token,
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "get order: {o}");
        assert_eq!(o["order_type"], "delivery");
        assert_eq!(o["delivery_fee"], 300);
        assert_eq!(o["subtotal"], 1000);
        assert_eq!(o["total_amount"], 1300);
        assert_eq!(o["delivery_order_id"].as_str().unwrap(), id.to_string());
        // Subtotal + fee must reconcile to the total (the math the old API broke).
        assert_eq!(
            o["subtotal"].as_i64().unwrap() + o["delivery_fee"].as_i64().unwrap(),
            o["total_amount"].as_i64().unwrap(),
        );
        // Nested delivery context for the dashboard order-detail "Delivery" card.
        let d = &o["delivery"];
        assert!(d.is_object(), "delivery block missing: {o}");
        assert_eq!(d["channel"], "in_mall");
        let norm = crate::delivery::normalize_phone(PHONE).unwrap();
        assert_eq!(d["customer_phone"], norm);
        assert_eq!(d["payment_method_hint"], "cash");
        assert!(d["delivery_ref"].as_str().unwrap().starts_with("D-"));

        // ── List view (GET /orders?shift_id=…) summary breaks out fees ──
        let (st, list) = send(
            &app,
            auth(
                test::TestRequest::get().uri(&format!("/orders?shift_id={shift}")),
                &token,
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "list: {list}");
        let summary = &list["summary"];
        assert_eq!(summary["delivery_fees"], 300);
        // Channel-split KPIs (computed server-side over the whole filtered set).
        assert_eq!(summary["delivery_orders"], 1);
        assert_eq!(summary["delivery_revenue"], 1300);
        assert_eq!(summary["in_mall_orders"], 1);
        assert_eq!(summary["in_mall_revenue"], 1300);
        assert_eq!(summary["in_mall_fees"], 300);
        assert_eq!(summary["outside_orders"], 0);
        assert_eq!(summary["outside_revenue"], 0);
        let row = &list["data"][0];
        assert_eq!(row["order_type"], "delivery");
        assert_eq!(row["delivery_fee"], 300);
        // The list row carries the lightweight channel flag (for badges + KPIs)…
        assert_eq!(row["delivery_channel"], "in_mall");
        // …but NOT the full address block (detail-only).
        assert!(row.get("delivery").map_or(true, |v| v.is_null()));
    }

    #[sqlx::test]
    async fn dine_in_order_defaults_have_no_delivery(pool: PgPool) {
        // A plain POS sale must report order_type="dine_in", a zero delivery
        // fee, no delivery link, and no nested delivery block — so the UIs can
        // safely branch on order_type without showing a phantom delivery row.
        perms(&pool).await;
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        assign(&pool, teller, branch).await;
        let shift = seed_shift(&pool, branch, teller).await;
        let token = teller_token(teller, org, branch);

        // Insert a minimal dine-in order directly so order_type/delivery_fee
        // take their DB column defaults (the path POST /orders exercises).
        let oid = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO orders (id, branch_id, shift_id, teller_id, order_number, \
             payment_method, subtotal, total_amount, status, order_ref) \
             VALUES ($1,$2,$3,$4,1,'cash',500,500,'completed','DT-000000-0001')",
        )
        .bind(oid)
        .bind(branch)
        .bind(shift)
        .bind(teller)
        .execute(&pool)
        .await
        .unwrap();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool.clone()))
                .app_data(web::Data::new(get_secret()))
                .configure(crate::orders::routes::configure),
        )
        .await;

        let (st, o) = send(
            &app,
            auth(
                test::TestRequest::get().uri(&format!("/orders/{oid}")),
                &token,
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "get order: {o}");
        assert_eq!(o["order_type"], "dine_in");
        assert_eq!(o["delivery_fee"], 0);
        assert!(o["delivery_order_id"].is_null());
        assert!(
            o["delivery_channel"].is_null(),
            "dine-in must have no channel flag"
        );
        assert!(
            o.get("delivery").map_or(true, |v| v.is_null()),
            "dine-in must have no delivery block"
        );
    }

    // ── Per-channel discounts ─────────────────────────────────────

    #[sqlx::test]
    async fn intake_freezes_percentage_discount(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 300).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 20.0, 1000.0).await;
        let disc = seed_discount(&pool, org, "percentage", 10).await;
        set_in_mall_discount(&pool, branch, Some(disc)).await;

        let app = app!(&pool);
        let body = intake_body(
            branch,
            "in_mall",
            json!([{ "menu_item_id": item, "quantity": 2 }]),
        );
        let (st, b) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
        assert_eq!(b["subtotal"], 1000);
        assert_eq!(b["discount_amount"], 100); // 10% of 1000
        assert_eq!(b["discount_type"], "percentage");
        assert_eq!(b["discount_value"], 10);
        assert_eq!(b["delivery_fee"], 300); // fee always charged in full
        assert_eq!(b["total"], 1200); // 1000 - 100 + 300
        assert_eq!(b["discount_id"].as_str().unwrap(), disc.to_string());
    }

    #[sqlx::test]
    async fn intake_fixed_discount_leaves_fee(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 300).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 20.0, 1000.0).await;
        let disc = seed_discount(&pool, org, "fixed", 150).await;
        set_in_mall_discount(&pool, branch, Some(disc)).await;

        let app = app!(&pool);
        let body = intake_body(
            branch,
            "in_mall",
            json!([{ "menu_item_id": item, "quantity": 2 }]),
        );
        let (st, b) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
        assert_eq!(b["discount_amount"], 150);
        assert_eq!(b["total"], 1150); // 1000 - 150 + 300
    }

    #[sqlx::test]
    async fn inactive_channel_discount_drops_at_intake(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 300).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 20.0, 1000.0).await;
        let disc = seed_discount(&pool, org, "percentage", 10).await;
        set_in_mall_discount(&pool, branch, Some(disc)).await;
        // Deactivate AFTER configuring — intake must honor only active discounts.
        sqlx::query("UPDATE discounts SET is_active=false WHERE id=$1")
            .bind(disc)
            .execute(&pool)
            .await
            .unwrap();

        let app = app!(&pool);
        let body = intake_body(
            branch,
            "in_mall",
            json!([{ "menu_item_id": item, "quantity": 2 }]),
        );
        let (st, b) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
        assert_eq!(b["discount_amount"], 0);
        assert_eq!(b["total"], 1300); // no discount: 1000 + 300
        assert!(b["discount_id"].is_null());
    }

    #[sqlx::test]
    async fn finalize_writes_discount_into_order(pool: PgPool) {
        perms(&pool).await;
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        assign(&pool, teller, branch).await;
        let shift = seed_shift(&pool, branch, teller).await;
        seed_settings(&pool, branch, true, false, 300).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 20.0, 1000.0).await;
        let disc = seed_discount(&pool, org, "percentage", 10).await;
        set_in_mall_discount(&pool, branch, Some(disc)).await;

        let id = place_in_mall_order(&pool, branch, item, 2).await;
        let token = teller_token(teller, org, branch);
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool.clone()))
                .app_data(web::Data::new(get_secret()))
                .app_data(web::Data::new(crate::realtime::hub::BranchEventHub::new()))
                .configure(crate::delivery::routes::configure)
                .configure(crate::orders::routes::configure),
        )
        .await;

        let (st, b) = send(
            &app,
            auth(
                test::TestRequest::post().uri(&format!("/delivery-orders/{id}/finalize")),
                &token,
            )
            .set_json(json!({ "shift_id": shift, "payment_method": "cash" })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "finalize: {b}");
        let order_id = b["order_id"].as_str().unwrap().to_string();

        // The real order carries the frozen discount, fee untouched.
        let (sub, dval, damt, tot, dtype): (i32, i32, i32, i32, Option<String>) = sqlx::query_as(
            "SELECT subtotal, discount_value, discount_amount, total_amount, discount_type::text FROM orders WHERE id=$1",
        )
        .bind(Uuid::parse_str(&order_id).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(sub, 1000);
        assert_eq!(dval, 10);
        assert_eq!(damt, 100);
        assert_eq!(tot, 1200);
        assert_eq!(dtype.as_deref(), Some("percentage"));

        // The orders API surfaces it too.
        let (st, o) = send(
            &app,
            auth(
                test::TestRequest::get().uri(&format!("/orders/{order_id}")),
                &token,
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{o}");
        assert_eq!(o["discount_amount"], 100);
        assert_eq!(o["total_amount"], 1200);
        assert_eq!(o["delivery_fee"], 300);
    }

    #[sqlx::test]
    async fn settings_rejects_inactive_or_cross_org_discount(pool: PgPool) {
        perms(&pool).await;
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let admin = seed_user(&pool, org, "org_admin").await;
        let token = admin_token(admin, org);
        let app = app!(&pool);

        let inactive = seed_discount(&pool, org, "percentage", 10).await;
        sqlx::query("UPDATE discounts SET is_active=false WHERE id=$1")
            .bind(inactive)
            .execute(&pool)
            .await
            .unwrap();
        let body = json!({
            "branch_id": branch, "in_mall_enabled": true, "outside_enabled": false,
            "in_mall_fee": 0, "prep_time_minutes": 20, "in_mall_discount_id": inactive,
        });
        let (st, _) = send(
            &app,
            auth(test::TestRequest::put().uri("/delivery/settings"), &token).set_json(&body),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::BAD_REQUEST,
            "inactive discount must be rejected"
        );

        let org2 = seed_org(&pool).await;
        let foreign = seed_discount(&pool, org2, "fixed", 50).await;
        let body2 = json!({
            "branch_id": branch, "in_mall_enabled": true, "outside_enabled": false,
            "in_mall_fee": 0, "prep_time_minutes": 20, "in_mall_discount_id": foreign,
        });
        let (st2, _) = send(
            &app,
            auth(test::TestRequest::put().uri("/delivery/settings"), &token).set_json(&body2),
        )
        .await;
        assert_eq!(
            st2,
            StatusCode::BAD_REQUEST,
            "cross-org discount must be rejected"
        );
    }

    #[sqlx::test]
    async fn public_menu_surfaces_channel_discount(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 0).await;
        seed_shift(&pool, branch, teller).await;
        let _item = seed_item(&pool, org, 500).await;
        let disc = seed_discount(&pool, org, "percentage", 15).await;
        set_in_mall_discount(&pool, branch, Some(disc)).await;

        let app = app!(&pool);
        let (st, b) = send(
            &app,
            test::TestRequest::get()
                .uri(&format!("/public/branches/{branch}/menu?channel=in_mall")),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["discount"]["dtype"], "percentage");
        assert_eq!(b["discount"]["value"], 15);
        assert_eq!(b["discount"]["id"].as_str().unwrap(), disc.to_string());
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
        sqlx::query("UPDATE menu_items SET base_price = 999 WHERE id=$1")
            .bind(item)
            .execute(&pool)
            .await
            .unwrap();

        let token = teller_token(teller, org, branch);
        let app = app!(&pool);
        let (st, b) = send(
            &app,
            auth(
                test::TestRequest::post().uri(&format!("/delivery-orders/{id}/finalize")),
                &token,
            )
            .set_json(json!({ "shift_id": shift, "payment_method": "cash" })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let order_id = Uuid::parse_str(b["order_id"].as_str().unwrap()).unwrap();

        let total: i32 = sqlx::query_scalar("SELECT total_amount FROM orders WHERE id=$1")
            .bind(order_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(total, 500, "must use the frozen price, not the edited 999");
        let unit: i32 = sqlx::query_scalar("SELECT unit_price FROM order_items WHERE order_id=$1")
            .bind(order_id)
            .fetch_one(&pool)
            .await
            .unwrap();
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
        send(
            &app,
            auth(
                test::TestRequest::post().uri(&format!("/delivery-orders/{id}/status")),
                &token,
            )
            .set_json(json!({"status":"confirmed"})),
        )
        .await;
        let (st, _) = send(
            &app,
            auth(
                test::TestRequest::post().uri(&format!("/delivery-orders/{id}/cancel")),
                &token,
            )
            .set_json(json!({ "reason": "test", "restore_inventory": true })),
        )
        .await;
        assert_eq!(st, StatusCode::OK);

        let stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE branch_id=$1 AND org_ingredient_id=$2")
            .bind(branch).bind(ing).fetch_one(&pool).await.unwrap();
        assert!((stock - 1000.0).abs() < 1e-6);
        let waste: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM inventory_movements WHERE source_id=$1 AND type='waste'",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(waste, 0);
        let status: String =
            sqlx::query_scalar("SELECT status::text FROM delivery_orders WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
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
        let (cst, _) = send(
            &app,
            auth(
                test::TestRequest::post().uri(&format!("/delivery-orders/{id}/status")),
                &token,
            )
            .set_json(json!({"status":"confirmed"})),
        )
        .await;
        assert_eq!(cst, StatusCode::OK);
        let (st, _) = send(
            &app,
            auth(
                test::TestRequest::post().uri(&format!("/delivery-orders/{id}/cancel")),
                &token,
            )
            .set_json(json!({ "reason": "no-show", "restore_inventory": false })),
        )
        .await;
        assert_eq!(st, StatusCode::OK);

        let stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE branch_id=$1 AND org_ingredient_id=$2")
            .bind(branch).bind(ing).fetch_one(&pool).await.unwrap();
        assert!((stock - 980.0).abs() < 1e-6, "stock={stock}");
        let waste: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM inventory_movements WHERE source_id=$1 AND type='waste'",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(waste, 1);
    }

    /// Regression for the double-cancel race: a second cancel hits the in-tx
    /// FOR UPDATE status re-check, returns 409, and does NOT deduct waste again.
    #[sqlx::test]
    async fn cancel_twice_does_not_double_waste(pool: PgPool) {
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
        // Move past received so it's a cancel (not a reject).
        send(
            &app,
            auth(
                test::TestRequest::post().uri(&format!("/delivery-orders/{id}/status")),
                &token,
            )
            .set_json(json!({"status":"confirmed"})),
        )
        .await;

        let body = json!({ "reason": "no-show", "restore_inventory": false });
        let (st1, _) = send(
            &app,
            auth(
                test::TestRequest::post().uri(&format!("/delivery-orders/{id}/cancel")),
                &token,
            )
            .set_json(body.clone()),
        )
        .await;
        assert_eq!(st1, StatusCode::OK);
        let (st2, _) = send(
            &app,
            auth(
                test::TestRequest::post().uri(&format!("/delivery-orders/{id}/cancel")),
                &token,
            )
            .set_json(body),
        )
        .await;
        assert_eq!(st2, StatusCode::CONFLICT, "second cancel must be rejected");

        // Waste posted exactly once; stock deducted exactly once (1000 − 20).
        let waste: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM inventory_movements WHERE source_id=$1 AND type='waste'",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(waste, 1, "waste must not be double-deducted");
        let stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE branch_id=$1 AND org_ingredient_id=$2").bind(branch).bind(ing).fetch_one(&pool).await.unwrap();
        assert!((stock - 980.0).abs() < 1e-6, "stock={stock}");
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
            auth(
                test::TestRequest::post().uri(&format!("/delivery-orders/{id}/cancel")),
                &token,
            )
            .set_json(json!({ "reason": "spam" })),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let status: String =
            sqlx::query_scalar("SELECT status::text FROM delivery_orders WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
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
        let (st1, _) = send(
            &app,
            auth(
                test::TestRequest::post().uri(&format!("/delivery-orders/{id}/finalize")),
                &token,
            )
            .set_json(&fin),
        )
        .await;
        assert_eq!(st1, StatusCode::OK);
        let (st2, _) = send(
            &app,
            auth(
                test::TestRequest::post().uri(&format!("/delivery-orders/{id}/finalize")),
                &token,
            )
            .set_json(&fin),
        )
        .await;
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
        let (st, _) = send(
            &app,
            auth(
                test::TestRequest::post().uri(&format!("/delivery-orders/{id}/status")),
                &token,
            )
            .set_json(json!({"status":"confirmed"})),
        )
        .await;
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
                .set_json(json!({ "branch_id": branch, "channel": "in_mall", "mode": "open" })),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT);

        // enable it, then the POS may pause/open
        sqlx::query("UPDATE branch_delivery_settings SET in_mall_enabled=true WHERE branch_id=$1")
            .bind(branch)
            .execute(&pool)
            .await
            .unwrap();
        let (st2, b) = send(
            &app,
            auth(test::TestRequest::post().uri("/delivery/accepting"), &token)
                .set_json(json!({ "branch_id": branch, "channel": "in_mall", "mode": "open" })),
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
        let (lst, list) = send(
            &app,
            auth(
                test::TestRequest::get().uri(&format!("/delivery/zones?branch_id={branch}")),
                &token,
            ),
        )
        .await;
        assert_eq!(lst, StatusCode::OK);
        assert_eq!(list.as_array().unwrap().len(), 1);

        // delete
        let (del, _) = send(
            &app,
            auth(
                test::TestRequest::delete()
                    .uri(&format!("/delivery/zones/{zid}?branch_id={branch}")),
                &token,
            ),
        )
        .await;
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
        let (_, s) = send(
            &app,
            auth(
                test::TestRequest::get().uri(&format!("/delivery/settings?branch_id={branch}")),
                &token,
            ),
        )
        .await;
        assert_eq!(s["in_mall_enabled"], true);
        assert_eq!(s["outside_enabled"], true);
        assert_eq!(s["in_mall_fee"], 250);
        assert_eq!(s["prep_time_minutes"], 30);
        // Omitted in the PUT above → defaults to true (mandatory in-mall location).
        assert_eq!(s["in_mall_require_location"], true);

        // Now turn the in-mall location requirement off and confirm it round-trips.
        let (sst2, _) = send(
            &app,
            auth(test::TestRequest::put().uri("/delivery/settings"), &token).set_json(json!({
                "branch_id": branch, "in_mall_enabled": true, "outside_enabled": true,
                "in_mall_fee": 250, "prep_time_minutes": 30, "in_mall_require_location": false
            })),
        )
        .await;
        assert_eq!(sst2, StatusCode::OK);
        let (_, s2) = send(
            &app,
            auth(
                test::TestRequest::get().uri(&format!("/delivery/settings?branch_id={branch}")),
                &token,
            ),
        )
        .await;
        assert_eq!(s2["in_mall_require_location"], false);
    }

    /// The public tracking endpoint returns a customer-safe view (status, ref,
    /// totals, org for theming) keyed by the opaque order UUID, exposes no phone,
    /// and 404s for an unknown id.
    #[sqlx::test]
    async fn public_track_returns_customer_safe_view(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 300).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 20.0, 1000.0).await;
        let id = place_in_mall_order(&pool, branch, item, 1).await;

        let app = app!(&pool);
        let (st, b) = send(
            &app,
            test::TestRequest::get().uri(&format!("/public/delivery-orders/{id}/track")),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["status"], "received");
        assert_eq!(b["channel"], "in_mall");
        assert_eq!(b["org_id"], org.to_string());
        assert_eq!(b["branch_name"], "Br");
        assert!(b["delivery_ref"].as_str().unwrap().starts_with("D-"));
        assert_eq!(b["subtotal"], 500);
        assert_eq!(b["delivery_fee"], 300);
        assert_eq!(b["total"], 800);
        assert_eq!(b["estimated_prep_minutes"], 20); // branch default base, no extra yet
        // No phone is ever exposed on the public tracking view.
        assert!(b["customer_phone"].is_null());

        // Unknown id → 404.
        let unknown = Uuid::new_v4();
        let (st2, _) = send(
            &app,
            test::TestRequest::get().uri(&format!("/public/delivery-orders/{unknown}/track")),
        )
        .await;
        assert_eq!(st2, StatusCode::NOT_FOUND);
    }

    /// A teller has `delivery_orders:read` (so they can flip the POS open/close
    /// override) but NOT `delivery_settings:read`. get_branch_settings must still
    /// let them read the channel state — otherwise the toggle dead-ends on a 403.
    #[sqlx::test]
    async fn teller_can_read_settings_via_delivery_orders_perm(pool: PgPool) {
        perms(&pool).await; // seeds defaults: teller gets delivery_orders:read, NOT delivery_settings:read
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        assign(&pool, teller, branch).await;
        seed_settings(&pool, branch, true, false, 250).await;
        let token = teller_token(teller, org, branch);
        let app = app!(&pool);

        let (st, s) = send(
            &app,
            auth(
                test::TestRequest::get().uri(&format!("/delivery/settings?branch_id={branch}")),
                &token,
            ),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::OK,
            "teller with delivery_orders:read may read settings: {s}"
        );
        assert_eq!(s["in_mall_enabled"], true);
        assert_eq!(s["outside_enabled"], false);
        assert_eq!(s["in_mall_fee"], 250);
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
        let body = intake_body(
            branch,
            "in_mall",
            json!([{ "menu_item_id": item, "quantity": 1 }]),
        );
        let app = app!(&pool);
        let (s1, b1) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .insert_header(("Idempotency-Key", key.clone()))
                .set_json(&body),
        )
        .await;
        assert_eq!(s1, StatusCode::CREATED);
        let (_s2, b2) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .insert_header(("Idempotency-Key", key))
                .set_json(&body),
        )
        .await;
        assert_eq!(b1["id"], b2["id"], "same key must replay the same order");
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM delivery_orders WHERE branch_id=$1")
                .bind(branch)
                .fetch_one(&pool)
                .await
                .unwrap();
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

    async fn seed_addon_typed(
        pool: &PgPool,
        org: Uuid,
        name: &str,
        addon_type: &str,
        price: i32,
    ) -> Uuid {
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
    async fn seed_milk_addon_for_ingredient(
        pool: &PgPool,
        org: Uuid,
        ing: Uuid,
        name: &str,
    ) -> Uuid {
        let addon = seed_addon_typed(pool, org, name, "milk_type", 0).await;
        sqlx::query("INSERT INTO addon_item_ingredients (addon_item_id, org_ingredient_id, quantity_used, ingredient_name, ingredient_unit) VALUES ($1,$2,200,'Milk','ml')")
            .bind(addon)
            .bind(ing)
            .execute(pool)
            .await
            .unwrap();
        addon
    }

    async fn seed_optional(
        pool: &PgPool,
        item: Uuid,
        name: &str,
        price: i32,
        size_label: Option<&str>,
    ) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO menu_item_optional_fields (id, menu_item_id, name, price, size_label, is_active) \
             VALUES ($1,$2,$3,$4,$5,true)",
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
        // The public menu is gated on the channel being open right now, which
        // requires an open shift.
        let teller = seed_user(&pool, org, "teller").await;
        seed_shift(&pool, branch, teller).await;
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
        let (st, b) = send(
            &app,
            test::TestRequest::get()
                .uri(&format!("/public/branches/{branch}/menu?channel=in_mall")),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");

        // Top-level global addon catalog (one per request, applies to every item).
        let addons = b["addons"].as_array().unwrap();
        assert_eq!(
            addons.len(),
            3,
            "channel-disabled cream must be excluded: {b}"
        );

        let oat_opt = addons
            .iter()
            .find(|o| o["addon_item_id"] == oat.to_string())
            .unwrap();
        assert_eq!(
            oat_opt["price"], 130,
            "channel-effective price, not default 100"
        );
        assert_eq!(oat_opt["type"], "milk_type");
        assert_eq!(oat_opt["is_available"], true);

        let reg_opt = addons
            .iter()
            .find(|o| o["addon_item_id"] == regular.to_string())
            .unwrap();
        assert_eq!(reg_opt["price"], 0);
        assert_eq!(reg_opt["type"], "milk_type");

        let shot_opt = addons
            .iter()
            .find(|o| o["addon_item_id"] == shot.to_string())
            .unwrap();
        assert_eq!(shot_opt["price"], 150);
        assert_eq!(shot_opt["type"], "extra");

        // Channel-disabled cream is absent entirely.
        assert!(
            addons
                .iter()
                .all(|o| o["addon_item_id"] != cream.to_string()),
            "channel-disabled cream must not appear: {b}"
        );

        // Per-item structures unchanged: no addon_slots field, optionals kept.
        let items = b["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        let it = &items[0];
        assert!(
            it.get("addon_slots").is_none(),
            "per-item addon_slots must be gone: {it}"
        );
        let opts = it["optionals"].as_array().unwrap();
        assert_eq!(opts.len(), 1);
        assert_eq!(opts[0]["name"], "Extra Hot");

        // No unified rows seeded → modifier_groups is present but empty (the
        // customizer's fallback signal).
        assert_eq!(it["modifier_groups"].as_array().unwrap().len(), 0);
    }

    /// The unified per-item modifier groups (menu-unification): constraints come
    /// from the attachment overrides, options honour `included_option_ids`, the
    /// price resolves branch_channel → branch → channel → catalog, and
    /// effectively-unavailable options are excluded entirely.
    #[sqlx::test]
    async fn public_menu_exposes_unified_modifier_groups(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        seed_settings(&pool, branch, true, false, 0).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;

        // A reusable "Milk" group (single-select) with three options.
        let gid = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO modifier_groups (id, org_id, name, selection_type, min_selections, max_selections, is_required, legacy_addon_type) \
             VALUES ($1,$2,'Milk','single',0,1,false,'milk_type')",
        )
        .bind(gid).bind(org).execute(&pool).await.unwrap();
        let (oat, almond, soy) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        for (id, name, price, sort) in [
            (oat, "Oat", 100, 0),
            (almond, "Almond", 150, 1),
            (soy, "Soy", 200, 2),
        ] {
            sqlx::query(
                "INSERT INTO modifier_options (id, group_id, name, price, sort) VALUES ($1,$2,$3,$4,$5)",
            )
            .bind(id).bind(gid).bind(name).bind(price).bind(sort)
            .execute(&pool).await.unwrap();
        }
        // Attach to the item: required + min 1 (overrides beat the group's 0/false)
        // and an allowlist of oat+almond (soy excluded).
        sqlx::query(
            "INSERT INTO menu_item_modifier_groups \
                 (menu_item_id, group_id, sort, min_override, is_required_override, included_option_ids) \
             VALUES ($1,$2,0,1,true,ARRAY[$3,$4]::uuid[])",
        )
        .bind(item).bind(gid).bind(oat).bind(almond)
        .execute(&pool).await.unwrap();
        // Channel-scoped price for oat (beats the 100 catalog default)…
        sqlx::query(
            "INSERT INTO menu_price_overrides (scope, branch_id, channel, target_type, target_id, price) \
             VALUES ('branch_channel',$1,'in_mall'::delivery_channel,'modifier_option',$2,130)",
        )
        .bind(branch).bind(oat).execute(&pool).await.unwrap();
        // …and a branch-scoped kill-switch for almond → excluded entirely.
        sqlx::query(
            "INSERT INTO menu_price_overrides (scope, branch_id, target_type, target_id, is_available) \
             VALUES ('branch',$1,'modifier_option',$2,false)",
        )
        .bind(branch).bind(almond).execute(&pool).await.unwrap();

        let app = app!(&pool);
        let (st, b) = send(
            &app,
            test::TestRequest::get()
                .uri(&format!("/public/branches/{branch}/menu?channel=in_mall")),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");

        let groups = b["items"][0]["modifier_groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "{b}");
        let g = &groups[0];
        assert_eq!(g["group_id"], gid.to_string());
        assert_eq!(g["name"], "Milk");
        assert_eq!(g["selection_type"], "single");
        assert_eq!(g["addon_type"], "milk_type");
        assert_eq!(g["min_selections"], 1, "attachment override beats group 0");
        assert_eq!(g["max_selections"], 1);
        assert_eq!(
            g["is_required"], true,
            "attachment override beats group false"
        );

        let options = g["options"].as_array().unwrap();
        assert_eq!(
            options.len(),
            1,
            "soy allowlisted-out, almond branch-unavailable: {b}"
        );
        assert_eq!(options[0]["option_id"], oat.to_string());
        assert_eq!(options[0]["price"], 130, "branch_channel beats catalog 100");
    }

    #[sqlx::test]
    async fn public_menu_preview_returns_menu_when_channel_closed(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        // in_mall enabled, outside NOT enabled. Critically: NO open shift is
        // seeded, so the in_mall channel is closed right now.
        seed_settings(&pool, branch, true, false, 0).await;
        let item = seed_item(&pool, org, 500).await;

        let app = app!(&pool);

        // Without preview: a closed channel still 409s (unchanged behavior).
        let (st, _b) = send(
            &app,
            test::TestRequest::get()
                .uri(&format!("/public/branches/{branch}/menu?channel=in_mall")),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::CONFLICT,
            "closed channel must 409 without preview"
        );

        // With preview=true: read-only browse returns the menu though it's closed.
        let (st, b) = send(
            &app,
            test::TestRequest::get().uri(&format!(
                "/public/branches/{branch}/menu?channel=in_mall&preview=true"
            )),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::OK,
            "preview must return the menu for a closed channel: {b}"
        );
        let items = b["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "preview menu should list the item: {b}");
        assert_eq!(items[0]["id"], item.to_string());

        // Preview never relaxes the channel-*enabled* check: a non-enabled channel
        // 404s even with preview.
        let (st, _b) = send(
            &app,
            test::TestRequest::get().uri(&format!(
                "/public/branches/{branch}/menu?channel=outside&preview=true"
            )),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::NOT_FOUND,
            "preview must not bypass the channel-enabled check"
        );
    }

    #[sqlx::test]
    async fn public_menu_exposes_default_milk_addon(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        seed_settings(&pool, branch, true, false, 0).await;
        // The public menu is gated on the channel being open (needs an open shift).
        let teller = seed_user(&pool, org, "teller").await;
        seed_shift(&pool, branch, teller).await;

        // Latte: recipe milk ingredient + a milk_type addon bound to that ingredient
        // → that addon is the item's base/default milk.
        let latte = seed_item(&pool, org, 600).await;
        let milk_ing = seed_milk_recipe(&pool, org, latte).await;
        let regular_milk =
            seed_milk_addon_for_ingredient(&pool, org, milk_ing, "Regular Milk").await;
        // A second, unrelated milk addon (different ingredient) must NOT be picked.
        let other_milk_ing = Uuid::new_v4();
        sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category) VALUES ($1,$2,'OatMilk','ml'::inventory_unit,90,'milk')")
            .bind(other_milk_ing).bind(org).execute(&pool).await.unwrap();
        seed_milk_addon_for_ingredient(&pool, org, other_milk_ing, "Oat Milk").await;

        // Cookie: a non-milk item (general recipe ingredient only) → no default milk.
        let cookie = seed_item(&pool, org, 200).await;
        seed_recipe(&pool, org, branch, cookie, 1.0, 100.0).await;

        let app = app!(&pool);
        let (st, b) = send(
            &app,
            test::TestRequest::get()
                .uri(&format!("/public/branches/{branch}/menu?channel=in_mall")),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");

        let items = b["items"].as_array().unwrap();
        let latte_item = items
            .iter()
            .find(|it| it["id"] == latte.to_string())
            .unwrap();
        assert_eq!(
            latte_item["default_milk_addon_id"],
            regular_milk.to_string(),
            "default milk must be the milk_type addon matching the recipe milk ingredient: {latte_item}"
        );

        let cookie_item = items
            .iter()
            .find(|it| it["id"] == cookie.to_string())
            .unwrap();
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
        let (st, b) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
        // 500 (item) + 300 (channel addon override, NOT the 100 default) = 800
        assert_eq!(b["subtotal"], 800);

        // Mark the addon unavailable for the channel → intake rejects.
        sqlx::query("UPDATE branch_channel_addon_overrides SET is_available = false WHERE branch_id=$1 AND addon_item_id=$2")
            .bind(branch).bind(addon).execute(&pool).await.unwrap();
        let (st2, _) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
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
            auth(
                test::TestRequest::put().uri("/delivery/channel-addon-overrides"),
                &token,
            )
            .set_json(json!({
                "branch_id": branch, "addon_item_id": addon, "channel": "outside",
                "price_override": 250, "is_available": true
            })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{o}");
        assert_eq!(o["price_override"], 250);

        // list
        let (lst, list) = send(
            &app,
            auth(
                test::TestRequest::get().uri(&format!(
                    "/delivery/channel-addon-overrides?branch_id={branch}&channel=outside"
                )),
                &token,
            ),
        )
        .await;
        assert_eq!(lst, StatusCode::OK);
        assert_eq!(list.as_array().unwrap().len(), 1);

        // delete
        let (del, _) = send(&app, auth(test::TestRequest::delete().uri(&format!("/delivery/channel-addon-overrides?branch_id={branch}&addon_item_id={addon}&channel=outside")), &token)).await;
        assert_eq!(del, StatusCode::NO_CONTENT);
        let (_, list2) = send(
            &app,
            auth(
                test::TestRequest::get().uri(&format!(
                    "/delivery/channel-addon-overrides?branch_id={branch}&channel=outside"
                )),
                &token,
            ),
        )
        .await;
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
        let (bad, _) = send(
            &app,
            auth(
                test::TestRequest::post().uri(&format!("/delivery-orders/{id}/prep-time")),
                &token,
            )
            .set_json(json!({"extra_prep_minutes": 7})),
        )
        .await;
        assert_eq!(bad, StatusCode::BAD_REQUEST);

        // +15 ok
        let (ok, b) = send(
            &app,
            auth(
                test::TestRequest::post().uri(&format!("/delivery-orders/{id}/prep-time")),
                &token,
            )
            .set_json(json!({"extra_prep_minutes": 15})),
        )
        .await;
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
        // The quote is gated on the channel being open (needs an open shift).
        let teller = seed_user(&pool, org, "teller").await;
        seed_shift(&pool, branch, teller).await;
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
            test::TestRequest::get().uri(&format!(
                "/public/branches/{branch}/delivery-quote?lat=30.01&lng=31.0&channel=outside"
            )),
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
                test::TestRequest::get()
                    .uri(&format!("/delivery-orders/stream?branch_id={branch}")),
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
                test::TestRequest::get()
                    .uri(&format!("/delivery-orders/stream?branch_id={branch}")),
                &token,
            )
            .to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
        // Must opt out of compression so the Compress middleware can't buffer
        // SSE frames (it skips any response that already has Content-Encoding).
        assert_eq!(resp.headers().get("content-encoding").unwrap(), "identity");
    }

    // ── WhatsApp gateway relay (super-admin only) ────────────────

    /// super_admin rows must have NULL org_id (chk_super_admin_no_org), so this
    /// can't reuse `seed_user`.
    async fn seed_super_admin(pool: &PgPool) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO users (id, org_id, name, email, password_hash, role) \
             VALUES ($1, NULL, 'SA', $2, 'h', 'super_admin')",
        )
        .bind(id)
        .bind(format!("sa-{id}@t.com"))
        .execute(pool)
        .await
        .unwrap();
        id
    }
    fn super_admin_token(uid: Uuid) -> String {
        create_token(&get_secret(), uid, None, UserRole::SuperAdmin, None, 24).unwrap()
    }

    #[sqlx::test]
    async fn whatsapp_relay_is_super_admin_only(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        assign(&pool, teller, branch).await;
        let admin = seed_user(&pool, org, "org_admin").await;
        let sa = seed_super_admin(&pool).await;
        let app = app!(&pool);

        // Teller and org-admin are rejected on EVERY relay route.
        for tok in [teller_token(teller, org, branch), admin_token(admin, org)] {
            let (st, _) = send(
                &app,
                auth(test::TestRequest::get().uri("/whatsapp/status"), &tok),
            )
            .await;
            assert_eq!(st, StatusCode::FORBIDDEN);
            let (st, _) = send(
                &app,
                auth(test::TestRequest::post().uri("/whatsapp/pair"), &tok),
            )
            .await;
            assert_eq!(st, StatusCode::FORBIDDEN);
            let (st, _) = send(
                &app,
                auth(test::TestRequest::post().uri("/whatsapp/logout"), &tok),
            )
            .await;
            assert_eq!(st, StatusCode::FORBIDDEN);
            let (st, _) = send(
                &app,
                auth(test::TestRequest::post().uri("/whatsapp/pause"), &tok)
                    .set_json(json!({ "paused": true })),
            )
            .await;
            assert_eq!(st, StatusCode::FORBIDDEN);
        }

        // Super-admin reaches status (gateway unconfigured in tests → safe,
        // no network call; just reports reachable=false).
        let (st, body) = send(
            &app,
            auth(
                test::TestRequest::get().uri("/whatsapp/status"),
                &super_admin_token(sa),
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(body["paused"], false);
    }

    #[sqlx::test]
    async fn whatsapp_pause_persists_and_gates_sending(pool: PgPool) {
        let sa = seed_super_admin(&pool).await;
        let token = super_admin_token(sa);
        let app = app!(&pool);

        assert!(!crate::delivery::gateway::is_paused(&pool).await);

        // Pause → persisted; reflected in status and the send-path gate.
        let (st, body) = send(
            &app,
            auth(test::TestRequest::post().uri("/whatsapp/pause"), &token)
                .set_json(json!({ "paused": true })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(body["paused"], true);
        assert!(crate::delivery::gateway::is_paused(&pool).await);

        // Resume.
        let (st, body) = send(
            &app,
            auth(test::TestRequest::post().uri("/whatsapp/pause"), &token)
                .set_json(json!({ "paused": false })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(body["paused"], false);
        assert!(!crate::delivery::gateway::is_paused(&pool).await);
    }

    // ── Public branch selector ────────────────────────────────────────────────

    /// GET /public/branches?org_id=... returns branches where at least one
    /// delivery channel is enabled, with correct open-now / enabled fields.
    #[sqlx::test]
    async fn public_branches_returns_delivery_enabled_branches(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 300).await;
        seed_shift(&pool, branch, teller).await;

        let app = app!(&pool);
        let (st, body) = send(
            &app,
            test::TestRequest::get().uri(&format!("/public/branches?org_id={org}")),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let list = body.as_array().expect("array");
        assert_eq!(list.len(), 1, "one delivery-enabled branch");
        let b = &list[0];
        assert_eq!(b["id"].as_str().unwrap(), branch.to_string()); // branch id comes back
        // in_mall is enabled and has an open shift with no time window → open now
        assert_eq!(b["in_mall_enabled"], true);
        assert_eq!(b["in_mall_open_now"], true);
        assert_eq!(b["outside_enabled"], false);
    }

    /// Branches without any delivery settings (or all channels disabled) must
    /// not appear in the public branch list.
    #[sqlx::test]
    async fn public_branches_hides_non_delivery_branches(pool: PgPool) {
        let org = seed_org(&pool).await;
        // Branch with no delivery settings at all.
        let _b1 = seed_branch_named(&pool, org, "BrA").await;
        // Branch with both channels explicitly disabled.
        let b2 = seed_branch_named(&pool, org, "BrB").await;
        seed_settings(&pool, b2, false, false, 0).await;

        let app = app!(&pool);
        let (st, body) = send(
            &app,
            test::TestRequest::get().uri(&format!("/public/branches?org_id={org}")),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(
            body.as_array().unwrap().len(),
            0,
            "no delivery-enabled branches should appear"
        );
    }

    // ── OTP cooldown ─────────────────────────────────────────────────────────

    /// Requesting a second OTP for the same phone within 60 seconds returns 409.
    #[sqlx::test]
    async fn otp_rapid_resend_rejected(pool: PgPool) {
        // Seed an unconsumed OTP that was just created.
        let phone = "201000000001";
        sqlx::query(
            "INSERT INTO delivery_otp (phone, code_hash, expires_at) \
             VALUES ($1, 'x', now() + interval '5 minutes')",
        )
        .bind(phone)
        .execute(&pool)
        .await
        .unwrap();

        let app = app!(&pool);
        let (st, body) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/otp/request")
                .set_json(json!({ "phone": phone })),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "rapid resend must be 409: {body}");
    }

    // ── Guest order history ───────────────────────────────────────────────────

    #[sqlx::test]
    async fn guest_order_history_shows_own_orders(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 300).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 10.0, 1000.0).await;

        let _order = place_in_mall_order(&pool, branch, item, 1).await;

        let app = app!(&pool);
        let (st, body) = send(
            &app,
            test::TestRequest::get().uri(&format!(
                "/public/delivery-orders/history?phone={PHONE}&org_id={org}"
            )),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let list = body.as_array().expect("array");
        assert_eq!(list.len(), 1, "customer should see their own order");
        assert_eq!(
            list[0]["branch_id"],
            branch.to_string().as_str().to_string()
        );
        assert_eq!(list[0]["channel"], "in_mall");
    }

    /// History for a different phone returns an empty list, not another
    /// customer's orders (tenant isolation at the phone level).
    #[sqlx::test]
    async fn guest_order_history_isolated_by_phone(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 300).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 10.0, 1000.0).await;
        place_in_mall_order(&pool, branch, item, 1).await;

        let app = app!(&pool);
        // A different phone number should get zero results.
        let (st, body) = send(
            &app,
            test::TestRequest::get().uri(&format!(
                "/public/delivery-orders/history?phone=01099999999&org_id={org}"
            )),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(body.as_array().unwrap().len(), 0);
    }

    /// Supplying an invalid device token for the history endpoint → 401.
    #[sqlx::test]
    async fn guest_order_history_invalid_device_token_rejected(pool: PgPool) {
        let org = seed_org(&pool).await;
        let app = app!(&pool);

        let (st, _) = send(
            &app,
            test::TestRequest::get().uri(&format!(
                "/public/delivery-orders/history?phone={PHONE}&org_id={org}&device_token=garbage"
            )),
        )
        .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }

    // ── Guest past locations ──────────────────────────────────────────────────

    /// Two orders placed to the same in-mall location appear as a single entry
    /// in the past-locations list.
    #[sqlx::test]
    async fn guest_past_locations_deduplicates_same_address(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, true, false, 300).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 10.0, 1000.0).await;

        // Two orders with identical place_name/floor/unit — should collapse.
        place_in_mall_order(&pool, branch, item, 1).await;
        place_in_mall_order(&pool, branch, item, 1).await;

        let app = app!(&pool);
        let (st, body) = send(
            &app,
            test::TestRequest::get().uri(&format!(
                "/public/delivery-orders/past-locations?phone={PHONE}&org_id={org}"
            )),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let list = body.as_array().expect("array");
        assert_eq!(
            list.len(),
            1,
            "two orders to the same location must deduplicate to one past-location entry"
        );
    }

    // ── Outside delivery ──────────────────────────────────────────────────────

    /// Outside-channel order without coordinates → 400 Bad Request.
    #[sqlx::test]
    async fn outside_intake_requires_coordinates(pool: PgPool) {
        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await;
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, false, true, 0).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 10.0, 1000.0).await;

        let app = app!(&pool);
        let mut body = intake_body(
            branch,
            "outside",
            json!([{ "menu_item_id": item, "quantity": 1 }]),
        );
        body["customer_lat"] = json!(null);
        body["customer_lng"] = json!(null);
        let (st, b) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::BAD_REQUEST,
            "outside without coords must be 400: {b}"
        );
    }

    /// Outside delivery with a zone covering the customer's location applies the
    /// zone fee (haversine fallback used when OSRM is not configured).
    #[sqlx::test]
    async fn outside_intake_zone_fee_applied(pool: PgPool) {
        // Unset OSRM so haversine is used as fallback.
        unsafe { std::env::remove_var("OSRM_URL") };

        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await; // at (30.0, 31.0)
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, false, true, 0).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 10.0, 1000.0).await;
        perms(&pool).await;

        // Seed a delivery zone that covers any address within 50 km.
        let admin_uid = seed_user(&pool, org, "org_admin").await;
        let admin_tok = admin_token(admin_uid, org);
        let app = app!(&pool);
        let (st, _) = send(
            &app,
            auth(
                test::TestRequest::post().uri("/delivery/zones").set_json(
                    json!({ "branch_id": branch, "name": "City", "fee": 750, "max_road_distance_meters": 50_000 }),
                ),
                &admin_tok,
            ),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "zone seed failed");

        // Customer is ~160m from the branch — well within the 50 km zone.
        let mut body = intake_body(
            branch,
            "outside",
            json!([{ "menu_item_id": item, "quantity": 1 }]),
        );
        body["customer_lat"] = json!(30.001);
        body["customer_lng"] = json!(31.001);
        let (st, b) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::CREATED,
            "outside order should be accepted: {b}"
        );
        assert_eq!(b["delivery_fee"], 750, "zone fee must be applied");
        assert_eq!(b["subtotal"], 500);
        assert_eq!(b["total"], 1250);
    }

    /// Outside delivery when the address falls outside all configured zones → 400.
    #[sqlx::test]
    async fn outside_intake_out_of_range_rejected(pool: PgPool) {
        unsafe { std::env::remove_var("OSRM_URL") };

        let org = seed_org(&pool).await;
        let branch = seed_branch(&pool, org).await; // at (30.0, 31.0)
        let teller = seed_user(&pool, org, "teller").await;
        seed_settings(&pool, branch, false, true, 0).await;
        seed_shift(&pool, branch, teller).await;
        let item = seed_item(&pool, org, 500).await;
        seed_recipe(&pool, org, branch, item, 10.0, 1000.0).await;
        perms(&pool).await;

        // Zone only covers up to 1 km; customer is ~110 km away.
        let admin_uid = seed_user(&pool, org, "org_admin").await;
        let admin_tok = admin_token(admin_uid, org);
        let app = app!(&pool);
        let (st, _) = send(
            &app,
            auth(
                test::TestRequest::post().uri("/delivery/zones").set_json(
                    json!({ "branch_id": branch, "name": "Near", "fee": 200, "max_road_distance_meters": 1_000 }),
                ),
                &admin_tok,
            ),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);

        // Cairo to Alexandria is ~110 km — way outside the 1 km zone.
        let mut body = intake_body(
            branch,
            "outside",
            json!([{ "menu_item_id": item, "quantity": 1 }]),
        );
        body["customer_lat"] = json!(31.2);
        body["customer_lng"] = json!(29.9);
        let (st, b) = send(
            &app,
            test::TestRequest::post()
                .uri("/public/delivery-orders")
                .set_json(&body),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::BAD_REQUEST,
            "out-of-range address must be 400: {b}"
        );
    }
}
