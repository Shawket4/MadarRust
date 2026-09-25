//! A combo sold offline and replayed (COMBOS_CONTRACT.md §2.7, §3.2): the
//! till's figures are recorded as charged, the server's quote is the
//! expectation, a difference is flagged, and nothing is ever refused.
//!
//! Figures (the shop of `common::combos`, tax 0):
//! - As the server prices it: 8571 / 2858 / 3571 + 1000, oat 1500 = 17500.
//! - A till still on P = 14000 charged 8000 / 2667 / 3333 + 1000, oat 1500
//!   = 16500 (the shares it sent sum to its own P).
//! - Unpicked: the slot defaults, Burger, Fries, Latte at its included
//!   Regular: weights 12000/4000/5000 → 8571 / 2858 / 3571 = 15000.
//! - A Croissant (5500) in the Drink slot, which does not admit it: priced
//!   under the relaxed rule, weights 12000/4000/5500, W = 21500: uptos
//!   round(15000·12000/21500) = round(8372.09) = 8372,
//!   round(15000·16000/21500) = round(11162.79) = 11163, 15000
//!   → 8372 / 2791 / 3837.

mod common;

use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use common::combos::{Shop, secret, shop};
use madar_rust::realtime::hub::BranchEventHub;

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(BranchEventHub::new()))
                .configure(madar_rust::orders::routes::configure)
                .configure(madar_rust::sync::routes::configure),
        )
        .await
    };
}

fn replay_body(s: &Shop, items: Value) -> Value {
    json!({
        "op": "create_order",
        "teller_id": s.admin,
        "request": {
            "branch_id": s.branch,
            "shift_id": s.till,
            "payment_method": "cash",
            "idempotency_key": Uuid::new_v4(),
            "items": items,
        }
    })
}

async fn replay<S, B>(app: &S, s: &Shop, body: Value) -> Uuid
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    let key = body["request"]["idempotency_key"].clone();
    let req = test::TestRequest::post()
        .uri("/sync/replay")
        .insert_header(("Authorization", format!("Bearer {}", s.admin_token())))
        .set_json(body)
        .to_request();
    let resp = test::call_service(app, req).await;
    let status = resp.status();
    let bytes = test::read_body(resp).await;
    assert!(
        status.is_success(),
        "{status}: {}",
        String::from_utf8_lossy(&bytes)
    );
    Uuid::parse_str(key.as_str().unwrap()).unwrap()
}

struct Stored {
    id: Uuid,
    subtotal: i32,
    flagged: bool,
    expected_total: Option<i32>,
    /// (line_kind, menu_item_id, line_total, share, surcharge, flagged)
    lines: Vec<(String, Uuid, i32, i32, i32, bool)>,
    addons: i64,
    flags: Vec<String>,
}

async fn stored(pool: &PgPool, key: Uuid) -> Stored {
    let (id, subtotal, flagged, expected_total): (Uuid, i32, bool, Option<i32>) = sqlx::query_as(
        "SELECT id, subtotal, price_flagged, price_expected_total FROM orders WHERE idempotency_key = $1",
    )
    .bind(key)
    .fetch_one(pool)
    .await
    .unwrap();
    let lines = sqlx::query_as(
        "SELECT line_kind, menu_item_id, line_total, combo_share, combo_surcharge, price_flagged \
           FROM order_items WHERE order_id = $1 \
          ORDER BY COALESCE(combo_line_id, id), combo_line_id IS NOT NULL, id",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap();
    let addons: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(a.line_total), 0)::bigint FROM order_item_addons a \
           JOIN order_items i ON i.id = a.order_item_id WHERE i.order_id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap();
    let flags = sqlx::query_scalar(
        "SELECT capability FROM authz_replay_flags WHERE subject_id = $1 ORDER BY capability",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap();
    Stored {
        id,
        subtotal,
        flagged,
        expected_total,
        lines,
        addons,
        flags,
    }
}

fn totals(st: &Stored) -> Vec<(String, i32)> {
    st.lines.iter().map(|l| (l.0.clone(), l.2)).collect()
}

#[sqlx::test]
async fn a_replay_at_the_server_s_prices_is_clean(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let line = json!({"menu_item_id": s.combo, "quantity": 1, "unit_price": 15000, "combo": {"picks": [
        {"slot_id": s.slot_main, "menu_item_id": s.burger, "size_label": "one_size", "share": 8571, "surcharge": 0},
        {"slot_id": s.slot_side, "menu_item_id": s.fries, "size_label": "one_size", "share": 2858, "surcharge": 0},
        {"slot_id": s.slot_drink, "menu_item_id": s.latte, "size_label": "Large", "share": 3571, "surcharge": 1000,
         "addons": [{"addon_item_id": s.oat, "quantity": 1, "unit_price": 1500}]},
    ]}});
    let key = replay(&app, &s, replay_body(&s, json!([line]))).await;
    let st = stored(&pool, key).await;
    assert_eq!(st.subtotal, 17500);
    assert!(!st.flagged);
    assert!(st.lines.iter().all(|l| !l.5));
    assert_eq!(
        totals(&st),
        vec![
            ("combo".into(), 0),
            ("combo_part".into(), 8571),
            ("combo_part".into(), 2858),
            ("combo_part".into(), 4571)
        ]
    );
    assert!(st.flags.is_empty(), "{:?}", st.flags);
}

#[sqlx::test]
async fn a_replay_at_an_old_price_is_kept_as_charged_and_flagged(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let line = json!({"menu_item_id": s.combo, "quantity": 1, "unit_price": 14000, "combo": {"picks": [
        {"slot_id": s.slot_main, "menu_item_id": s.burger, "share": 8000, "surcharge": 0},
        {"slot_id": s.slot_side, "menu_item_id": s.fries, "share": 2667, "surcharge": 0},
        {"slot_id": s.slot_drink, "menu_item_id": s.latte, "size_label": "Large", "share": 3333, "surcharge": 1000,
         "addons": [{"addon_item_id": s.oat, "quantity": 1, "unit_price": 1500}]},
    ]}});
    let key = replay(&app, &s, replay_body(&s, json!([line]))).await;
    let st = stored(&pool, key).await;
    assert_eq!(st.subtotal, 16500, "what the customer paid");
    assert_eq!(st.addons, 1500);
    assert_eq!(
        totals(&st),
        vec![
            ("combo".into(), 0),
            ("combo_part".into(), 8000),
            ("combo_part".into(), 2667),
            ("combo_part".into(), 4333)
        ]
    );
    assert!(st.flagged);
    assert!(
        st.lines.iter().all(|l| l.5),
        "header and every part flagged"
    );
    assert_eq!(st.expected_total, Some(17500), "the server's figure");
    assert_eq!(st.flags, vec!["menu.combos:price_mismatch".to_string()]);
    let p: Option<i32> = sqlx::query_scalar(
        "SELECT combo_unit_price FROM order_items WHERE order_id = $1 AND line_kind = 'combo'",
    )
    .bind(st.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(p, Some(14000));
}

#[sqlx::test]
async fn an_unpicked_replay_takes_the_slot_defaults(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let line = json!({"menu_item_id": s.combo, "quantity": 1, "unit_price": 15000});
    let key = replay(&app, &s, replay_body(&s, json!([line]))).await;
    let st = stored(&pool, key).await;
    assert_eq!(
        st.lines.iter().map(|l| (l.1, l.2)).collect::<Vec<_>>()[1..],
        [(s.burger, 8571), (s.fries, 2858), (s.latte, 3571)]
    );
    assert_eq!(st.subtotal, 15000);
    assert!(st.flagged);
    assert_eq!(st.flags, vec!["menu.combos:unpicked".to_string()]);
}

#[sqlx::test]
async fn invalid_picks_are_priced_under_a_relaxed_rule_and_flagged(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let line = json!({"menu_item_id": s.combo, "quantity": 1, "unit_price": 15000, "combo": {"picks": [
        {"slot_id": s.slot_main, "menu_item_id": s.burger},
        {"slot_id": s.slot_side, "menu_item_id": s.fries},
        {"slot_id": s.slot_drink, "menu_item_id": s.croissant},
    ]}});
    let key = replay(&app, &s, replay_body(&s, json!([line]))).await;
    let st = stored(&pool, key).await;
    assert_eq!(
        st.lines.iter().map(|l| (l.1, l.2)).collect::<Vec<_>>()[1..],
        [(s.burger, 8372), (s.fries, 2791), (s.croissant, 3837)]
    );
    assert!(st.flagged);
    assert_eq!(st.flags, vec!["menu.combos:picks_invalid".to_string()]);
}

#[sqlx::test]
async fn a_replayed_staff_drink_or_reward_in_a_combo_is_dropped_and_flagged(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let mut line = s.lunch_line(1);
    line["unit_price"] = json!(15000);
    line["staff_drink"] = json!({"id": Uuid::new_v4(), "note": "for Ali"});
    let mut body = replay_body(&s, json!([line]));
    body["request"]["loyalty_redemptions"] = json!([{"item_index": 0, "units": 1}]);
    let key = replay(&app, &s, body).await;
    let st = stored(&pool, key).await;
    assert_eq!(st.subtotal, 17500, "rung as paid");
    assert_eq!(
        st.flags,
        vec![
            "loyalty.redeem:in_combo".to_string(),
            "orders.staff_drink.record:item_not_eligible".to_string()
        ]
    );
    let comped: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(staff_comp_minor), 0)::bigint + COALESCE(SUM(reward_covered), 0)::bigint \
           FROM order_items WHERE order_id = $1",
    )
    .bind(st.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(comped, 0);
}

#[sqlx::test]
async fn an_unavailable_combo_replays_flagged(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    // A window that is never open now: one weekday only, not today's, … the
    // simplest never-open window is a date range in the past.
    sqlx::query(
        "INSERT INTO sale_windows (org_id, combo_item_id, valid_from, valid_to) \
         VALUES ($1, $2, '2020-01-01', '2020-01-02')",
    )
    .bind(s.org)
    .bind(s.combo)
    .execute(&pool)
    .await
    .unwrap();
    let mut line = s.lunch_line(1);
    line["unit_price"] = json!(15000);
    let key = replay(&app, &s, replay_body(&s, json!([line]))).await;
    let st = stored(&pool, key).await;
    assert_eq!(st.subtotal, 17500);
    assert!(st.flagged);
    assert_eq!(st.flags, vec!["menu.combos:unavailable".to_string()]);
}
