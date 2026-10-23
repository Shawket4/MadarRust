//! The Rewards dashboard (Madar REWARDS_DASHBOARD_DESIGN.md): stamp tracks
//! (earned by the one "Add points" press, redeemed per track at tender, at the
//! counter), the card designer, messages to cards, the CRM and the figures.
//!
//! Every figure is worked by hand in the test; every guard is exercised from
//! both sides (allowed and refused), with its stable code.

mod common;

use actix_web::{App, http::StatusCode, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use common::combos::{Shop, secret, shop};

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

async fn call<S, B>(app: &S, req: test::TestRequest, bearer: &str) -> (StatusCode, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    let req = req.insert_header(("Authorization", format!("Bearer {bearer}")));
    let resp = test::call_service(app, req.to_request()).await;
    let status = resp.status();
    let bytes = test::read_body(resp).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn raw<S, B>(app: &S, req: test::TestRequest) -> (StatusCode, Vec<u8>)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    let resp = test::call_service(app, req.to_request()).await;
    let status = resp.status();
    (status, test::read_body(resp).await.to_vec())
}

async fn setup(pool: &PgPool) -> Shop {
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    madar_rust::authz::sync_catalogue(pool).await.unwrap();
    shop(pool).await
}

/// A stamp programme: `size` stamps fill track 1, a stamp per item.
async fn stamps(pool: &PgPool, org: Uuid, size: i32) {
    sqlx::query(
        "INSERT INTO loyalty_settings \
            (org_id, branch_id, enabled, mode, earn_piastres_per_point, default_reward_cost, \
             require_otp, stamp_per_line_item, reward_any_item) \
         VALUES ($1, NULL, true, 'visits', 1000, $2, false, true, false)",
    )
    .bind(org)
    .bind(size)
    .execute(pool)
    .await
    .unwrap();
}

async fn member(pool: &PgPool, org: Uuid, phone: &str, name: &str) -> Uuid {
    let tok = format!("M{}", &Uuid::new_v4().simple().to_string()[..22]);
    common::members::seed_loyalty_member(pool, org, phone, name, &tok).await
}

async fn visits(pool: &PgPool, m: Uuid) -> i32 {
    sqlx::query_scalar("SELECT visits_balance FROM loyalty_memberships WHERE id = $1")
        .bind(m)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn track_balance(pool: &PgPool, m: Uuid, t: i16) -> i32 {
    sqlx::query_scalar(
        "SELECT balance FROM loyalty_track_balances WHERE customer_id = $1 AND position = $2",
    )
    .bind(m)
    .bind(t)
    .fetch_optional(pool)
    .await
    .unwrap()
    .unwrap_or(0)
}

/// Track 1 named; track 2 "Sandwich" (4 stamps, croissant + cookie); track 3
/// "Juice" (5 stamps, counter-only).
fn three_tracks(s: &Shop) -> Value {
    json!({"tracks": [
        {"position": 1, "name_en": "Coffee", "name_ar": "قهوة", "shape": "cup"},
        {"position": 2, "name_en": "Sandwich", "name_ar": "ساندوتش", "size": 4,
         "items": [s.croissant.to_string(), s.cookie.to_string()], "colour": "#F2B33D"},
        {"position": 3, "name_en": "Juice", "name_ar": "عصير", "size": 5}
    ]})
}

async fn put_tracks<S, B>(app: &S, s: &Shop, body: Value) -> (StatusCode, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    call(
        app,
        test::TestRequest::put()
            .uri("/loyalty/tracks")
            .set_json(body),
        &s.admin_token(),
    )
    .await
}

async fn sell<S, B>(
    app: &S,
    s: &Shop,
    items: Value,
    member: Option<Uuid>,
    redemptions: Value,
) -> (StatusCode, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    let mut body = s.order_body(items);
    body["idempotency_key"] = json!(Uuid::new_v4());
    if let Some(m) = member {
        body["loyalty_customer_id"] = json!(m);
        body["loyalty_redemptions"] = redemptions;
    }
    call(
        app,
        test::TestRequest::post().uri("/orders").set_json(body),
        &s.admin_token(),
    )
    .await
}

/// Run a broadcast to the end (the request also starts it in the background;
/// both runners share the work safely).
async fn finish(pool: &PgPool, id: Uuid) {
    for _ in 0..50 {
        madar_rust::loyalty::broadcasts::run_one(pool, id)
            .await
            .unwrap();
        let status: String =
            sqlx::query_scalar("SELECT status FROM loyalty_broadcasts WHERE id = $1")
                .bind(id)
                .fetch_one(pool)
                .await
                .unwrap();
        if status == "sent" {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("broadcast {id} never finished");
}

async fn award<S, B>(app: &S, s: &Shop, order: &str, m: Uuid) -> (StatusCode, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    call(
        app,
        test::TestRequest::post()
            .uri("/loyalty/award")
            .set_json(json!({"branch_id": s.branch, "order_id": order, "customer_id": m})),
        &s.teller_token(),
    )
    .await
}

// ── Tracks: the definition ──────────────────────────────────────────────────

#[sqlx::test]
async fn tracks_are_saved_by_a_designer_and_refused_to_anyone_else(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    stamps(&pool, s.org, 8).await;

    // A teller holds loyalty.use, not loyalty.card.design.
    let (st, _) = call(
        &app,
        test::TestRequest::put()
            .uri("/loyalty/tracks")
            .set_json(three_tracks(&s)),
        &s.teller_token(),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    let (st, body) = put_tracks(&app, &s, three_tracks(&s)).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["mode"], "visits");
    assert_eq!(body["track_one_size"], 8, "track 1 is the programme's card");
    assert_eq!(body["tracks"].as_array().unwrap().len(), 3);
    assert_eq!(body["tracks"][1]["items"].as_array().unwrap().len(), 2);
    assert_eq!(
        body["tracks"][2]["items"],
        json!([]),
        "a counter-only track"
    );

    // Anyone who may read the programme may read them (the teller too).
    let (st, got) = call(
        &app,
        test::TestRequest::get().uri("/loyalty/tracks"),
        &s.teller_token(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(got["tracks"][1]["name_ar"], "ساندوتش");

    // madar-card's rules: a size out of range, a gap, no name.
    for bad in [
        json!({"tracks": [{"position": 1}, {"position": 2, "name_en": "X", "size": 51}]}),
        json!({"tracks": [{"position": 1}, {"position": 3, "name_en": "X", "size": 5}]}),
        json!({"tracks": [{"position": 1}, {"position": 2, "size": 5}]}),
    ] {
        let (st, body) = put_tracks(&app, &s, bad).await;
        assert_eq!(st, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "LOYALTY_TRACKS_INVALID");
    }
    // Another shop's menu item.
    let stranger = Uuid::new_v4();
    let (st, _) = put_tracks(
        &app,
        &s,
        json!({"tracks": [{"position": 1}, {"position": 2, "name_en": "X", "size": 5, "items": [stranger.to_string()]}]}),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // A points programme keeps one track.
    sqlx::query("UPDATE loyalty_settings SET mode = 'points' WHERE org_id = $1")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let (st, body) = put_tracks(&app, &s, three_tracks(&s)).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("tracks_points_single"),
        "{body}"
    );
    let (_, got) = call(
        &app,
        test::TestRequest::get().uri("/loyalty/tracks"),
        &s.admin_token(),
    )
    .await;
    assert_eq!(
        got["tracks"].as_array().unwrap().len(),
        1,
        "tracks 2-3 are inactive in points mode"
    );
}

// ── Tracks: earning with the one press ──────────────────────────────────────

#[sqlx::test]
async fn one_press_earns_every_track_the_sale_qualifies_for_once(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    stamps(&pool, s.org, 8).await;
    put_tracks(&app, &s, three_tracks(&s)).await;
    let m = member(&pool, s.org, "201000000101", "Mona").await;

    // 2 colas + 3 croissants. Track 1 (whole menu, a stamp per item): 2 + 3 = 5.
    // Track 2 (croissant, cookie): 3. Track 3 (no items): nothing from a sale.
    let (st, order) = sell(
        &app,
        &s,
        json!([{"menu_item_id": s.cola, "quantity": 2}, {"menu_item_id": s.croissant, "quantity": 3}]),
        None,
        json!([]),
    )
    .await;
    assert!(st.is_success(), "{st} {order}");
    let oid = order["id"].as_str().unwrap().to_string();

    let (st, a) = award(&app, &s, &oid, m).await;
    assert_eq!(st, StatusCode::OK, "{a}");
    assert_eq!(a["points_awarded"], 5);
    assert_eq!(a["already_awarded"], false);
    // Only tracks bound to items earn from a sale (track 3 is counter-only).
    assert_eq!(a["tracks"], json!([{"position": 2, "points_awarded": 3}]));
    assert_eq!(visits(&pool, m).await, 5);
    assert_eq!(track_balance(&pool, m, 2).await, 3);
    // The member carries every track; track 1's figures are the top-level ones.
    let tracks = a["member"]["tracks"].as_array().unwrap();
    assert_eq!(tracks[0]["balance"], 5);
    assert_eq!(tracks[1]["balance"], 3);
    assert_eq!(tracks[1]["points_to_next_reward"], 1, "4 - 3");
    assert_eq!(tracks[2]["earns_from_sales"], false);

    // A second press: nothing moves, on any track.
    let (_, again) = award(&app, &s, &oid, m).await;
    assert_eq!(again["already_awarded"], true);
    assert_eq!(again["points_awarded"], 0);
    assert_eq!(visits(&pool, m).await, 5);
    assert_eq!(track_balance(&pool, m, 2).await, 3);

    // A void reverses every track's earn (the void trigger, per ledger row).
    let (st, body) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/orders/{oid}/void"))
            .set_json(json!({"reason": "customer_changed_mind"})),
        &s.admin_token(),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
    assert_eq!(visits(&pool, m).await, 0);
    assert_eq!(track_balance(&pool, m, 2).await, 0);
    let reversed: Vec<(String, Option<i16>, i32)> = sqlx::query_as(
        "SELECT kind::text, track, points FROM loyalty_transactions WHERE order_id = $1::uuid AND kind = 'reverse_earn' ORDER BY track NULLS FIRST",
    )
    .bind(&oid)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        reversed,
        vec![
            ("reverse_earn".into(), None, -5),
            ("reverse_earn".into(), Some(2), -3)
        ]
    );
}

#[sqlx::test]
async fn a_track_earns_per_sale_when_asked_and_its_ceiling_trims(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    stamps(&pool, s.org, 8).await;
    let mut t = three_tracks(&s);
    t["tracks"][1]["per_item"] = json!(false);
    put_tracks(&app, &s, t).await;
    let m = member(&pool, s.org, "201000000102", "Omar").await;
    let (_, order) = sell(
        &app,
        &s,
        json!([{"menu_item_id": s.croissant, "quantity": 3}]),
        None,
        json!([]),
    )
    .await;
    let (_, a) = award(&app, &s, order["id"].as_str().unwrap(), m).await;
    assert_eq!(
        a["tracks"][0],
        json!({"position": 2, "points_awarded": 1}),
        "one per sale"
    );

    // A ceiling of 4 with 3 on the card: three more croissants (per item) earn 1.
    let mut t = three_tracks(&s);
    t["tracks"][1]["cap"] = json!(4);
    put_tracks(&app, &s, t).await;
    sqlx::query(
        "INSERT INTO loyalty_transactions (org_id, customer_id, branch_id, kind, currency, points, track, note) \
         VALUES ($1,$2,$3,'adjust','visits',2,2,'test')",
    )
    .bind(s.org)
    .bind(m)
    .bind(s.branch)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(track_balance(&pool, m, 2).await, 3);
    let (_, order) = sell(
        &app,
        &s,
        json!([{"menu_item_id": s.croissant, "quantity": 3}]),
        None,
        json!([]),
    )
    .await;
    let (_, a) = award(&app, &s, order["id"].as_str().unwrap(), m).await;
    assert_eq!(
        a["tracks"][0],
        json!({"position": 2, "points_awarded": 1}),
        "trimmed to the ceiling: 4 - 3"
    );
    assert_eq!(track_balance(&pool, m, 2).await, 4);
}

// ── Tracks: redemption at tender ────────────────────────────────────────────

#[sqlx::test]
async fn a_basket_takes_a_reward_from_each_track_with_its_own_balance(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    stamps(&pool, s.org, 8).await;
    // Track 1's catalogue: a cola for 5 stamps.
    sqlx::query(
        "INSERT INTO loyalty_reward_items (org_id, menu_item_id, cost_currency, cost_amount) VALUES ($1,$2,'visits',5)",
    )
    .bind(s.org)
    .bind(s.cola)
    .execute(&pool)
    .await
    .unwrap();
    put_tracks(&app, &s, three_tracks(&s)).await;
    let m = member(&pool, s.org, "201000000103", "Laila").await;
    for (n, track) in [(6, None), (9, Some(2i16))] {
        sqlx::query(
            "INSERT INTO loyalty_transactions (org_id, customer_id, branch_id, kind, currency, points, track, note) \
             VALUES ($1,$2,$3,'adjust','visits',$4,$5,'test')",
        )
        .bind(s.org)
        .bind(m)
        .bind(s.branch)
        .bind(n)
        .bind(track)
        .execute(&pool)
        .await
        .unwrap();
    }

    // The scan: `rewards` stays track 1's (a released till offers only those);
    // track 2's are beside it, each of its items at its size.
    let (_, scan) = call(
        &app,
        test::TestRequest::post()
            .uri("/loyalty/lookup")
            .set_json(json!({"branch_id": s.branch, "customer_id": m})),
        &s.teller_token(),
    )
    .await;
    let rewards: Vec<&str> = scan["rewards"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(rewards, ["Cola"]);
    assert_eq!(scan["track_rewards"][0]["position"], 2);
    assert_eq!(
        scan["track_rewards"][0]["rewards"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(scan["track_rewards"][0]["rewards"][0]["cost_amount"], 4);
    assert_eq!(
        scan["track_rewards"][1]["rewards"],
        json!([]),
        "track 3 has no items"
    );

    // Refusals with their codes: an item that is not track 2's; a track that
    // does not exist.
    let (st, body) = sell(
        &app,
        &s,
        json!([{"menu_item_id": s.cola, "quantity": 1}]),
        Some(m),
        json!([{"item_index": 0, "track": 2}]),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "LOYALTY_TRACK_NOT_ON_OFFER");
    let (st, body) = sell(
        &app,
        &s,
        json!([{"menu_item_id": s.croissant, "quantity": 1}]),
        Some(m),
        json!([{"item_index": 0, "track": 4}]),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "LOYALTY_TRACK_UNKNOWN");
    // Two croissants on track 2 cost 8 of its 9 — fine; three cost 12 > 9.
    let (st, body) = sell(
        &app,
        &s,
        json!([{"menu_item_id": s.croissant, "quantity": 3}]),
        Some(m),
        json!([{"item_index": 0, "units": 3, "track": 2}]),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "LOYALTY_TRACK_BALANCE_SHORT");

    // The mixed basket: a cola on track 1 (5 of 6) and a croissant on track 2
    // (4 of 9). Each track spends its own balance.
    let (st, order) = sell(
        &app,
        &s,
        json!([{"menu_item_id": s.cola, "quantity": 1}, {"menu_item_id": s.croissant, "quantity": 1}]),
        Some(m),
        json!([{"item_index": 0}, {"item_index": 1, "track": 2}]),
    )
    .await;
    assert!(st.is_success(), "{st} {order}");
    assert_eq!(visits(&pool, m).await, 1, "6 - 5");
    assert_eq!(track_balance(&pool, m, 2).await, 5, "9 - 4");
    let lines: Vec<(bool, Option<i16>)> = sqlx::query_as(
        "SELECT is_reward, reward_track FROM order_items WHERE order_id = $1::uuid ORDER BY item_name",
    )
    .bind(order["id"].as_str().unwrap())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        lines,
        vec![(true, None), (true, Some(2))],
        "Cola then Croissant"
    );

    // One shared ceiling: with one reward per order, the two together are refused.
    sqlx::query("UPDATE loyalty_settings SET max_rewards_per_order = 1 WHERE org_id = $1")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO loyalty_transactions (org_id, customer_id, branch_id, kind, currency, points, note) \
         VALUES ($1,$2,$3,'adjust','visits',5,'test')",
    )
    .bind(s.org)
    .bind(m)
    .bind(s.branch)
    .execute(&pool)
    .await
    .unwrap();
    let (st, body) = sell(
        &app,
        &s,
        json!([{"menu_item_id": s.cola, "quantity": 1}, {"menu_item_id": s.croissant, "quantity": 1}]),
        Some(m),
        json!([{"item_index": 0}, {"item_index": 1, "track": 2}]),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert!(
        body.to_string().contains("Only one reward per order"),
        "{body}"
    );

    // A released till sends no `track`: track 1, exactly as before.
    let (st, body) = sell(
        &app,
        &s,
        json!([{"menu_item_id": s.cola, "quantity": 1}]),
        Some(m),
        json!([{"item_index": 0, "units": 1}]),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
    assert_eq!(visits(&pool, m).await, 1, "6 - 5 again");
    assert_eq!(track_balance(&pool, m, 2).await, 5, "untouched");
}

// ── Tracks: the counter ─────────────────────────────────────────────────────

#[sqlx::test]
async fn the_counter_stamps_and_completes_any_track_once_per_key(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    stamps(&pool, s.org, 8).await;
    put_tracks(&app, &s, three_tracks(&s)).await;
    let m = member(&pool, s.org, "201000000104", "Hady").await;
    let stamp = |items: i32, track: u8, key: &str| {
        test::TestRequest::post().uri("/loyalty/counter/stamp").set_json(json!({
            "branch_id": s.branch, "customer_id": m, "items": items, "idempotency_key": key, "track": track
        }))
    };
    let (st, r) = call(&app, stamp(2, 3, "key-juice-0001"), &s.teller_token()).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["stamps"], 2);
    assert_eq!(track_balance(&pool, m, 3).await, 2);
    assert_eq!(visits(&pool, m).await, 0, "track 1 untouched");
    // The same key again: the first outcome. On another track: refused.
    let (_, r) = call(&app, stamp(2, 3, "key-juice-0001"), &s.teller_token()).await;
    assert_eq!(r["already"], true);
    let (st, r) = call(&app, stamp(2, 2, "key-juice-0001"), &s.teller_token()).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(r["code"], "LOYALTY_KEY_REUSED");
    // Completing a card that is not full: 2 of 5.
    let redeem = |track: u8, key: &str| {
        test::TestRequest::post()
            .uri("/loyalty/counter/redeem")
            .set_json(json!({
                "branch_id": s.branch, "customer_id": m, "idempotency_key": key, "track": track
            }))
    };
    let (st, r) = call(&app, redeem(3, "key-juice-0002"), &s.teller_token()).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(r["code"], "LOYALTY_CARD_NOT_FULL");
    // Four more: 6 of 5, completed: 6 - 5 = 1 carries over.
    call(&app, stamp(4, 3, "key-juice-0003"), &s.teller_token()).await;
    let (st, r) = call(&app, redeem(3, "key-juice-0004"), &s.teller_token()).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["stamps"], -5);
    assert_eq!(track_balance(&pool, m, 3).await, 1);
    assert_eq!(r["member"]["tracks"][2]["balance"], 1);
    // A track the card does not have.
    let (st, r) = call(&app, stamp(1, 4, "key-juice-0005"), &s.teller_token()).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(r["code"], "LOYALTY_TRACK_UNKNOWN");
}

// ── Old clients: a one-track shop answers exactly as before ─────────────────

#[sqlx::test]
async fn a_one_track_shop_sends_no_new_fields(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    stamps(&pool, s.org, 8).await;
    let m = member(&pool, s.org, "201000000105", "Sara").await;
    let (_, scan) = call(
        &app,
        test::TestRequest::post()
            .uri("/loyalty/lookup")
            .set_json(json!({"branch_id": s.branch, "customer_id": m})),
        &s.teller_token(),
    )
    .await;
    assert!(scan.get("track_rewards").is_none(), "{scan}");
    assert!(scan["member"].get("tracks").is_none(), "{scan}");
    let (_, order) = sell(
        &app,
        &s,
        json!([{"menu_item_id": s.cola, "quantity": 1}]),
        None,
        json!([]),
    )
    .await;
    let (_, a) = award(&app, &s, order["id"].as_str().unwrap(), m).await;
    assert!(a.get("tracks").is_none(), "{a}");
    assert!(a["member"].get("tracks").is_none());
    let (_, detail) = call(
        &app,
        test::TestRequest::get().uri(&format!("/loyalty/members/{m}")),
        &s.admin_token(),
    )
    .await;
    assert!(detail["ledger"][0].get("track").is_none(), "{detail}");
}

// ── The card designer ───────────────────────────────────────────────────────

fn design(elements: Value) -> Value {
    json!({
        "version": 1, "template": "classic",
        "colours": {"background": "#0D6273", "foreground": "#EFF3F4", "label": "#B9DCE3"},
        "ground": {"kind": "colour", "pattern": null},
        "elements": elements,
        "stamp": {"shape": "circle", "empty": "outline"},
        "apple": {"header": [{"type": "balance", "track": 1}], "primary": null,
                  "secondary": [{"type": "progress", "track": 1}], "auxiliary": []},
        "google": {"points": {"type": "progress", "track": 1}, "secondary": null}
    })
}

#[sqlx::test]
async fn a_design_is_checked_saved_published_and_rendered(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    stamps(&pool, s.org, 8).await;
    let m = member(&pool, s.org, "201000000106", "Nour").await;

    let (st, v) = call(
        &app,
        test::TestRequest::get().uri("/loyalty/card-design"),
        &s.teller_token(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["default_design"]["template"], "classic");
    assert!(v["published"].is_null());
    assert_eq!(v["ctx"]["tracks"], json!([{"position": 1, "size": 8}]));

    // Nothing to publish yet.
    let (st, body) = call(
        &app,
        test::TestRequest::post().uri("/loyalty/card-design/publish"),
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(body["code"], "LOYALTY_DESIGN_NO_DRAFT");

    // A bar over the footer cannot render: refused by madar-card's checks.
    let bad = design(
        json!([{"kind": {"type": "bar", "track": 1}, "cell": {"col": 0, "row": 5, "w": 12, "h": 1}}]),
    );
    let (st, body) = call(
        &app,
        test::TestRequest::put()
            .uri("/loyalty/card-design")
            .set_json(json!({"design": bad})),
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(body["code"], "LOYALTY_DESIGN_INVALID");
    assert!(
        body["error"].as_str().unwrap().contains("covers_footer"),
        "{body}"
    );
    // A teller may not design.
    let good = design(
        json!([{"kind": {"type": "stamps", "track": 1}, "cell": {"col": 0, "row": 0, "w": 12, "h": 3}}]),
    );
    let (st, _) = call(
        &app,
        test::TestRequest::put()
            .uri("/loyalty/card-design")
            .set_json(json!({"design": good.clone()})),
        &s.teller_token(),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // A card in a wallet, current: publishing makes it stale for the sweep.
    sqlx::query("UPDATE loyalty_memberships SET pass_updated_at = now(), google_object_id = 'x.y' WHERE id = $1")
        .bind(m)
        .execute(&pool)
        .await
        .unwrap();
    let (st, _) = call(
        &app,
        test::TestRequest::put()
            .uri("/loyalty/card-design")
            .set_json(json!({"design": good})),
        &s.manager_token(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "a manager may design");
    let (st, v) = call(
        &app,
        test::TestRequest::post().uri("/loyalty/card-design/publish"),
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["revision"], 1);
    assert_eq!(v["published"]["elements"][0]["kind"]["type"], "stamps");
    let stale: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT pass_updated_at FROM loyalty_memberships WHERE id = $1")
            .bind(m)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        stale.is_none(),
        "every installed card is rebuilt by the sweep"
    );

    // The exact render: Apple's strip at 2x, Google's hero.
    let preview = |store: &str| {
        test::TestRequest::post()
            .uri("/loyalty/card-design/preview")
            .insert_header(("Authorization", format!("Bearer {}", s.admin_token())))
            .set_json(
                json!({"design": v["published"], "store": store, "arabic": true,
                             "balances": [{"position": 1, "balance": 3}]}),
            )
    };
    let (st, png) = raw(&app, preview("apple")).await;
    assert_eq!(st, StatusCode::OK);
    let img = image::load_from_memory(&png).unwrap();
    assert_eq!((img.width(), img.height()), (750, 288));
    let (_, png) = raw(&app, preview("google")).await;
    let img = image::load_from_memory(&png).unwrap();
    assert_eq!((img.width(), img.height()), (1032, 336));

    // Google's hero, public: the published revision at a progress the card can
    // show. Anything else is a 404.
    let hero = |path: String| test::TestRequest::get().uri(&path);
    let (st, png) = raw(
        &app,
        hero(format!("/public/loyalty/brand/{}/card/1/3-en.png", s.org)),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(&png[1..4], b"PNG");
    for bad in ["8-en", "3-fr", "3-1-en", "x-en"] {
        let (st, _) = raw(
            &app,
            hero(format!("/public/loyalty/brand/{}/card/1/{bad}.png", s.org)),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{bad}");
    }
    let (st, _) = raw(
        &app,
        hero(format!("/public/loyalty/brand/{}/card/2/3-en.png", s.org)),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "an old revision is gone");

    // Back to the classic look.
    let (st, v) = call(
        &app,
        test::TestRequest::delete().uri("/loyalty/card-design/published"),
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(v["published"].is_null());
    assert_eq!(v["revision"], 2);
}

// ── Messages to cards ───────────────────────────────────────────────────────

#[sqlx::test]
async fn a_message_reaches_its_audience_within_its_limits(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    stamps(&pool, s.org, 8).await;
    let a = member(&pool, s.org, "201000000107", "Aya").await;
    let b = member(&pool, s.org, "201000000108", "Badr").await;
    let c = member(&pool, s.org, "201000000109", "Cyrine").await;
    sqlx::query("UPDATE customers SET marketing_opt_out = true WHERE id = $1")
        .bind(c)
        .execute(&pool)
        .await
        .unwrap();

    let (st, n) = call(
        &app,
        test::TestRequest::post()
            .uri("/loyalty/broadcasts/audience")
            .set_json(json!({"audience": {"rules": []}})),
        &s.manager_token(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{n}");
    assert_eq!(
        n,
        json!({"members": 3, "opted_out": 1, "apple": 0, "google": 0, "no_card": 2})
    );

    let send = |body: Value| {
        test::TestRequest::post()
            .uri("/loyalty/broadcasts")
            .set_json(body)
    };
    // A teller may not message every member.
    let (st, _) = call(
        &app,
        send(json!({"message": {"en": "Hi"}})),
        &s.teller_token(),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // No links on a card.
    let (st, body) = call(
        &app,
        send(json!({"message": {"en": "Order at drops.com"}})),
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(body["code"], "LOYALTY_MESSAGE_INVALID");
    // Too soon to schedule.
    let soon = chrono::Utc::now() + chrono::Duration::minutes(2);
    let (st, body) = call(
        &app,
        send(json!({"message": {"en": "Hi"}, "scheduled_for": soon})),
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(body["code"], "LOYALTY_MESSAGE_SCHEDULE");

    let (st, sent) = call(
        &app,
        send(json!({"message": {"en": "Double stamps on Friday!", "ar": "طوابع مضاعفة يوم الجمعة!"}})),
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{sent}");
    let id: Uuid = sent["id"].as_str().unwrap().parse().unwrap();
    finish(&pool, id).await;
    let (_, got) = call(
        &app,
        test::TestRequest::get().uri(&format!("/loyalty/broadcasts/{id}")),
        &s.admin_token(),
    )
    .await;
    assert_eq!(got["status"], "sent", "{got}");
    assert_eq!(got["recipients"], 3);
    assert_eq!(got["sent"], 2, "Aya and Badr");
    assert_eq!(got["skipped_opted_out"], 1, "Cyrine opted out");
    assert_eq!(
        got["unreachable"], 2,
        "no card in a wallet, and WhatsApp was not asked for"
    );

    // A second message the same day: both were messaged under 20 hours ago.
    let (_, second) = call(
        &app,
        send(json!({"message": {"en": "See you soon"}})),
        &s.admin_token(),
    )
    .await;
    let id2: Uuid = second["id"].as_str().unwrap().parse().unwrap();
    finish(&pool, id2).await;
    let (_, got) = call(
        &app,
        test::TestRequest::get().uri(&format!("/loyalty/broadcasts/{id2}")),
        &s.admin_token(),
    )
    .await;
    assert_eq!(got["skipped_recent"], 2, "{got}");
    assert_eq!(got["sent"], 0);

    // The third is allowed, the fourth is over the day's limit of 3.
    let (st, _) = call(
        &app,
        send(json!({"message": {"en": "Third"}})),
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, body) = call(
        &app,
        send(json!({"message": {"en": "Fourth"}})),
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(body["code"], "LOYALTY_MESSAGE_DAILY_LIMIT");

    // A scheduled one can be cancelled once.
    let later = chrono::Utc::now() + chrono::Duration::days(2);
    let (st, sched) = call(
        &app,
        send(json!({"message": {"en": "Later"}, "scheduled_for": later})),
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{sched}");
    assert_eq!(sched["status"], "scheduled");
    let cancel = format!(
        "/loyalty/broadcasts/{}/cancel",
        sched["id"].as_str().unwrap()
    );
    let (st, c1) = call(
        &app,
        test::TestRequest::post().uri(&cancel),
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(c1["status"], "cancelled");
    let (st, c2) = call(
        &app,
        test::TestRequest::post().uri(&cancel),
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(c2["code"], "LOYALTY_MESSAGE_NOT_CANCELLABLE");

    // The log and the quota.
    let (_, page) = call(
        &app,
        test::TestRequest::get().uri("/loyalty/broadcasts"),
        &s.manager_token(),
    )
    .await;
    assert_eq!(page["total"], 4);
    assert_eq!(page["sent_last_24h"], 3);
    assert_eq!(page["limits"]["daily_limit"], 3);

    // Seen: the card's device fetched the pass after the push.
    sqlx::query(
        "UPDATE loyalty_memberships SET pass_notice = 'x', pass_notice_at = now(), pass_notice_broadcast_id = $2 WHERE id = $1",
    )
    .bind(a)
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    let row = madar_rust::loyalty::model::find_by_id(&pool, a)
        .await
        .unwrap()
        .unwrap();
    madar_rust::loyalty::wallet::notices::mark_seen(&pool, &row).await;
    let (_, got) = call(
        &app,
        test::TestRequest::get().uri(&format!("/loyalty/broadcasts/{id}")),
        &s.admin_token(),
    )
    .await;
    assert_eq!(got["seen"], 1);
    let _ = b;
}

// ── The CRM and the figures ─────────────────────────────────────────────────

#[sqlx::test]
async fn segments_filter_the_members_and_the_figures_add_up(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    stamps(&pool, s.org, 5).await;
    put_tracks(&app, &s, three_tracks(&s)).await;
    let a = member(&pool, s.org, "201000000110", "Amal").await;
    let b = member(&pool, s.org, "201000000111", "Basma").await;
    let _c = member(&pool, s.org, "201000000112", "Cherif").await;
    // Amal: 6 stamps (a reward ready on a 5-stamp card); Basma: 2, and 4 on track 2.
    for (m, n, t) in [(a, 6, None), (b, 2, None), (b, 4, Some(2i16))] {
        sqlx::query(
            "INSERT INTO loyalty_transactions (org_id, customer_id, branch_id, kind, currency, points, track, note) \
             VALUES ($1,$2,$3,'adjust','visits',$4,$5,'test')",
        )
        .bind(s.org)
        .bind(m)
        .bind(s.branch)
        .bind(n)
        .bind(t)
        .execute(&pool)
        .await
        .unwrap();
    }
    let q = |body: Value| {
        test::TestRequest::post()
            .uri("/loyalty/members/query")
            .set_json(body)
    };
    let (st, page) = call(
        &app,
        q(json!({"audience": {"rules": [{"type": "reward_ready", "track": 1}]}})),
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{page}");
    assert_eq!(page["total"], 1);
    assert_eq!(page["rows"][0]["name"], "Amal");
    assert_eq!(page["rows"][0]["rewards_ready"], 1, "6 / 5");
    let (_, page) = call(
        &app,
        q(json!({"audience": {"rules": [{"type": "reward_ready", "track": 2}]}})),
        &s.admin_token(),
    )
    .await;
    assert_eq!(page["rows"][0]["name"], "Basma", "4 of 4 on track 2");
    assert_eq!(page["rows"][0]["tracks"][1]["rewards_ready"], 1);
    let (_, page) = call(
        &app,
        q(json!({"sort": "balance", "desc": true})),
        &s.admin_token(),
    )
    .await;
    let names: Vec<&str> = page["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Amal", "Basma", "Cherif"]);
    // A contradictory segment is refused with its code; a teller may not list.
    let (st, body) = call(
        &app,
        q(json!({"audience": {"rules": [{"type": "visited_within", "days": 7}, {"type": "not_visited_for", "days": 30}]}})),
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(body["code"], "LOYALTY_AUDIENCE_INVALID");
    let (st, _) = call(&app, q(json!({})), &s.teller_token()).await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // The figures over today: 3 joined; 6 + 2 + 4 = 12 earned... as
    // adjustments, which are not earns: earned counts `earn` rows only, so 0;
    // active = the 2 members with a ledger row today.
    let (st, stats) = call(
        &app,
        test::TestRequest::get().uri("/loyalty/stats"),
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{stats}");
    assert_eq!(stats["members_total"], 3);
    assert_eq!(stats["new_members"], 3);
    assert_eq!(stats["active_members"], 2);
    assert_eq!(stats["earned"], 0);
    assert_eq!(stats["daily"].as_array().unwrap().len(), 30);
    // The dashboard sends instants (its date scope): a one-day range in Cairo.
    let (st, one) = call(
        &app,
        test::TestRequest::get().uri("/loyalty/stats?from=2026-10-22T21:00:00Z&to=2026-10-23T20:59:59.999Z"),
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{one}");
    assert_eq!(one["from"], "2026-10-23");
    assert_eq!(one["to"], "2026-10-23");

    // A member's wallet.
    let (st, w) = call(
        &app,
        test::TestRequest::get().uri(&format!("/loyalty/members/{a}/wallet")),
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{w}");
    assert_eq!(w["apple_devices"], 0);
    assert_eq!(w["google_card"], false);
}

#[sqlx::test]
async fn a_tracks_earn_and_redeem_show_in_the_figures_per_track(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    stamps(&pool, s.org, 8).await;
    put_tracks(&app, &s, three_tracks(&s)).await;
    let m = member(&pool, s.org, "201000000113", "Dina").await;
    // A sale of 1 cola + 4 croissants, awarded: track 1 = 5, track 2 = 4.
    let (_, order) = sell(
        &app,
        &s,
        json!([{"menu_item_id": s.cola, "quantity": 1}, {"menu_item_id": s.croissant, "quantity": 4}]),
        None,
        json!([]),
    )
    .await;
    award(&app, &s, order["id"].as_str().unwrap(), m).await;
    // Then a cookie paid with track 2's 4 stamps.
    let (st, body) = sell(
        &app,
        &s,
        json!([{"menu_item_id": s.cookie, "quantity": 1}]),
        Some(m),
        json!([{"item_index": 0, "track": 2}]),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
    let (_, stats) = call(
        &app,
        test::TestRequest::get().uri("/loyalty/stats"),
        &s.admin_token(),
    )
    .await;
    assert_eq!(stats["earned"], 9, "5 + 4");
    assert_eq!(stats["redemptions"], 1);
    assert_eq!(
        stats["per_track"],
        json!([{"position": 1, "earned": 5, "redemptions": 0}, {"position": 2, "earned": 4, "redemptions": 1}])
    );
}

/// Real responses from a three-track shop, for the old-client decode check
/// (released POS madar-api models must read them). Written only when asked:
///       MADAR_WRITE_TRACK_CAPTURES=/some/dir scripts/run_tests.sh rewards_dashboard
#[sqlx::test]
async fn captures_for_old_clients(pool: PgPool) {
    let Ok(dir) = std::env::var("MADAR_WRITE_TRACK_CAPTURES") else {
        return;
    };
    let s = setup(&pool).await;
    let app = app!(pool);
    stamps(&pool, s.org, 8).await;
    sqlx::query(
        "INSERT INTO loyalty_reward_items (org_id, menu_item_id, cost_currency, cost_amount) VALUES ($1,$2,'visits',5)",
    )
    .bind(s.org)
    .bind(s.cola)
    .execute(&pool)
    .await
    .unwrap();
    put_tracks(&app, &s, three_tracks(&s)).await;
    let m = member(&pool, s.org, "201000000199", "Capture").await;
    let (_, order) = sell(
        &app,
        &s,
        json!([{"menu_item_id": s.cola, "quantity": 6}, {"menu_item_id": s.croissant, "quantity": 5}]),
        None,
        json!([]),
    )
    .await;
    let (_, award_r) = award(&app, &s, order["id"].as_str().unwrap(), m).await;
    let (_, scan) = call(
        &app,
        test::TestRequest::post()
            .uri("/loyalty/lookup")
            .set_json(json!({"branch_id": s.branch, "customer_id": m})),
        &s.teller_token(),
    )
    .await;
    let (_, sale) = sell(
        &app,
        &s,
        json!([{"menu_item_id": s.cola, "quantity": 1}, {"menu_item_id": s.cookie, "quantity": 1}]),
        Some(m),
        json!([{"item_index": 0}, {"item_index": 1, "track": 2}]),
    )
    .await;
    let (_, read_back) = call(
        &app,
        test::TestRequest::get().uri(&format!("/orders/{}", sale["id"].as_str().unwrap())),
        &s.admin_token(),
    )
    .await;
    let (_, detail) = call(
        &app,
        test::TestRequest::get().uri(&format!("/loyalty/members/{m}")),
        &s.admin_token(),
    )
    .await;
    let (_, counter) = call(
        &app,
        test::TestRequest::post().uri("/loyalty/counter/stamp").set_json(json!({
            "branch_id": s.branch, "customer_id": m, "items": 1, "idempotency_key": "capture-0001", "track": 3
        })),
        &s.teller_token(),
    )
    .await;
    std::fs::create_dir_all(&dir).unwrap();
    for (name, v) in [
        ("award_result", award_r),
        ("scan_result", scan),
        ("order_full", read_back),
        ("member_detail", detail),
        ("counter_result", counter),
    ] {
        assert!(!v.is_null(), "{name}");
        std::fs::write(
            format!("{dir}/{name}.json"),
            serde_json::to_string_pretty(&v).unwrap(),
        )
        .unwrap();
    }
}

#[sqlx::test]
async fn a_refund_claws_back_each_tracks_earn_in_proportion(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    stamps(&pool, s.org, 8).await;
    put_tracks(&app, &s, three_tracks(&s)).await;
    let m = member(&pool, s.org, "201000000114", "Eman").await;
    // 2 colas (30 EGP) + 3 croissants (55 EGP): 6000 + 16500 = 22500 piastres.
    let (_, order) = sell(
        &app,
        &s,
        json!([{"menu_item_id": s.cola, "quantity": 2}, {"menu_item_id": s.croissant, "quantity": 3}]),
        None,
        json!([]),
    )
    .await;
    assert_eq!(order["total_amount"], 22500, "{order}");
    let oid = order["id"].as_str().unwrap().to_string();
    award(&app, &s, &oid, m).await;
    assert_eq!(
        (visits(&pool, m).await, track_balance(&pool, m, 2).await),
        (5, 3)
    );
    // Half the money back: each earn follows it, floor(earned × 11250 / 22500):
    // track 1 floor(5 / 2) = 2, track 2 floor(3 / 2) = 1.
    let (st, body) = call(
        &app,
        test::TestRequest::post().uri("/refunds").set_json(json!({
            "order_id": oid, "amount": 11250, "method": "cash", "reason": "customer_request"
        })),
        &s.admin_token(),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
    assert_eq!(visits(&pool, m).await, 3, "5 - 2");
    assert_eq!(track_balance(&pool, m, 2).await, 2, "3 - 1");
}

/// The read caches (rewards_cache) never serve a write's old value: the next
/// read after a tracks save, a publish or an unpublish is the new one.
#[sqlx::test]
async fn a_write_is_visible_on_the_next_read_through_the_caches(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    stamps(&pool, s.org, 8).await;
    let m = member(&pool, s.org, "201000000115", "Farah").await;
    let lookup = || {
        test::TestRequest::post()
            .uri("/loyalty/lookup")
            .set_json(json!({"branch_id": s.branch, "customer_id": m}))
    };
    // Warm the tracks cache with one track…
    let (_, before) = call(&app, lookup(), &s.teller_token()).await;
    assert!(before["member"].get("tracks").is_none());
    // …save three, and the very next lookup has them.
    put_tracks(&app, &s, three_tracks(&s)).await;
    let (_, after) = call(&app, lookup(), &s.teller_token()).await;
    assert_eq!(after["member"]["tracks"].as_array().unwrap().len(), 3, "{after}");
    // Rename track 2: visible at once.
    let mut t = three_tracks(&s);
    t["tracks"][1]["name_en"] = json!("Bakery");
    put_tracks(&app, &s, t).await;
    let (_, again) = call(&app, lookup(), &s.teller_token()).await;
    assert_eq!(again["member"]["tracks"][1]["name_en"], "Bakery");

    // The published design: the hero answers at the new revision at once, and
    // stops answering at once when unpublished.
    let hero = |rev: i32| test::TestRequest::get().uri(&format!("/public/loyalty/brand/{}/card/{rev}/0-0-0-en.png", s.org));
    let (st, _) = raw(&app, hero(0)).await;
    assert_eq!(st, StatusCode::OK, "the automatic multi-track design (revision 0)");
    let good = design(json!([{"kind": {"type": "bar", "track": 1}, "cell": {"col": 0, "row": 4, "w": 12, "h": 1}}]));
    call(&app, test::TestRequest::put().uri("/loyalty/card-design").set_json(json!({"design": good})), &s.admin_token()).await;
    call(&app, test::TestRequest::post().uri("/loyalty/card-design/publish"), &s.admin_token()).await;
    let (st, _) = raw(&app, hero(1)).await;
    assert_eq!(st, StatusCode::OK, "revision 1, right after publishing");
    let (st, _) = raw(&app, hero(0)).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "the automatic one is gone");
    call(&app, test::TestRequest::delete().uri("/loyalty/card-design/published"), &s.admin_token()).await;
    let (st, _) = raw(&app, hero(1)).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "unpublished: gone at once");
}

/// Every place a card is drawn gets the same design: the join page, the
/// member's card page and the teller's lookup carry it (and nothing for the
/// classic look), with the unit the card counts.
#[sqlx::test]
async fn the_public_pages_and_the_teller_get_the_shops_design(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    stamps(&pool, s.org, 8).await;
    let m = member(&pool, s.org, "201000000116", "Ghada").await;
    let token: String = sqlx::query_scalar("SELECT member_token FROM loyalty_memberships WHERE id = $1")
        .bind(m)
        .fetch_one(&pool)
        .await
        .unwrap();
    let join = || test::TestRequest::get().uri(&format!("/public/loyalty/join-info?branch_id={}", s.branch));
    let card = || test::TestRequest::get().uri(&format!("/public/loyalty/card/{token}"));
    let (_, j) = call(&app, join(), "").await;
    assert!(j.get("card").is_none(), "classic: nothing extra, {j}");
    let (_, c) = call(&app, card(), "").await;
    assert!(c.get("card").is_none() && c.get("tracks").is_none(), "{c}");

    // A published design reaches all three at once (the design cache is
    // invalidated by the publish).
    let good = design(json!([{"kind": {"type": "stamps", "track": 1}, "cell": {"col": 0, "row": 0, "w": 12, "h": 3}}]));
    call(&app, test::TestRequest::put().uri("/loyalty/card-design").set_json(json!({"design": good})), &s.admin_token()).await;
    call(&app, test::TestRequest::post().uri("/loyalty/card-design/publish"), &s.admin_token()).await;
    let (_, j) = call(&app, join(), "").await;
    assert_eq!(j["card"]["revision"], 1, "{j}");
    assert_eq!(j["card"]["design"]["elements"][0]["kind"]["type"], "stamps");
    assert_eq!(j["card"]["track_one_size"], 8);
    assert_eq!(j["card"]["stamp_unit"], "item", "a POS shop stamping per item");
    let (_, c) = call(&app, card(), "").await;
    assert_eq!(c["card"]["revision"], 1);
    let (_, scan) = call(
        &app,
        test::TestRequest::post().uri("/loyalty/lookup").set_json(json!({"branch_id": s.branch, "customer_id": m})),
        &s.teller_token(),
    )
    .await;
    assert_eq!(scan["card"]["revision"], 1);
    // Tracks: the member's card page lists them; the tracks carry no items.
    put_tracks(&app, &s, three_tracks(&s)).await;
    let (_, c) = call(&app, card(), "").await;
    assert_eq!(c["tracks"].as_array().unwrap().len(), 3, "{c}");
    assert_eq!(c["card"]["tracks"][1]["items"], json!([]));
}
