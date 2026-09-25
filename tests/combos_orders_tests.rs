//! Selling a combo at the till, live (COMBOS_CONTRACT.md §3, §4).
//!
//! The worked example `combo/lunch_large_latte` (§4), P = 15000:
//!   Burger (base 12000), Fries (4000), Latte picked Large (included Regular
//!   5000, Large 6000, no owner surcharge → size_extra 1000) + oat milk 1500.
//!   Weights 12000/4000/5000, W = 21000; cumulative uptos
//!   round(15000·12000/21000) = 8571, round(15000·16000/21000) = 11429, 15000
//!   → shares 8571 / 2858 / 3571. Part line totals 8571, 2858, 4571; oat 1500.
//!   unit_total 17500. At n = 2 every figure doubles: 17142 / 5716 / 9142 / 3000.
//! Cola through the Drink slot's category choice (Cola 3000, its only size):
//!   weights 12000/4000/3000, W = 19000: uptos round(15000·12000/19000) =
//!   round(9473.68) = 9474, round(15000·16000/19000) = round(12631.58) = 12632,
//!   15000 → shares 9474 / 3158 / 2368.

mod common;

use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use common::combos::{Shop, secret, shop};

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .configure(madar_rust::orders::routes::configure),
        )
        .await
    };
}

async fn post_order<S, B>(app: &S, token: &str, body: Value) -> (u16, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    let req = test::TestRequest::post()
        .uri("/orders")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(body)
        .to_request();
    let resp = test::call_service(app, req).await;
    let status = resp.status().as_u16();
    let bytes = test::read_body(resp).await;
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

fn lines(v: &Value) -> Vec<Value> {
    v["items"].as_array().cloned().unwrap_or_default()
}

fn part<'a>(items: &'a [Value], item: Uuid) -> &'a Value {
    items
        .iter()
        .find(|l| l["line_kind"] == "combo_part" && l["menu_item_id"] == item.to_string())
        .unwrap_or_else(|| panic!("no part for {item}: {items:#?}"))
}

async fn flags(pool: &PgPool, order: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT capability FROM authz_replay_flags WHERE subject_id = $1::uuid ORDER BY capability",
    )
    .bind(order)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn a_live_lunch_combo_is_the_worked_example(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let (st, v) = post_order(
        &app,
        &s.admin_token(),
        s.order_body(json!([s.lunch_line(1)])),
    )
    .await;
    assert_eq!(st, 201, "{v:#}");
    let items = lines(&v);
    assert_eq!(items.len(), 4, "{items:#?}");
    // Header first, then the parts in slot order (Main, Side, Drink).
    let h = &items[0];
    assert_eq!(h["line_kind"], "combo");
    assert_eq!(h["menu_item_id"], s.combo.to_string());
    assert_eq!(
        (h["unit_price"].as_i64(), h["line_total"].as_i64()),
        (Some(0), Some(0))
    );
    assert_eq!(h["combo_unit_price"], 15000);
    assert_eq!(h["quantity"], 1);
    assert_eq!(h["line_cost"], 0);
    assert_eq!(h["cost_missing"], false);
    let order: Vec<String> = items[1..]
        .iter()
        .map(|l| l["menu_item_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        order,
        vec![
            s.burger.to_string(),
            s.fries.to_string(),
            s.latte.to_string()
        ]
    );
    let hid = h["id"].clone();
    for (item, unit, share, sur, total, slot) in [
        (s.burger, 12000, 8571, 0, 8571, "Main"),
        (s.fries, 4000, 2858, 0, 2858, "Side"),
        (s.latte, 6000, 3571, 1000, 4571, "Drink"),
    ] {
        let p = part(&items, item);
        assert_eq!(p["combo_line_id"], hid);
        assert_eq!(p["unit_price"], unit, "{p:#}");
        assert_eq!(p["combo_share"], share, "{p:#}");
        assert_eq!(p["combo_surcharge"], sur, "{p:#}");
        assert_eq!(p["line_total"], total, "{p:#}");
        assert_eq!(p["combo_slot_name"], slot);
        assert_eq!(p["deal_minor"], 0);
        assert!(p["combo_unit_price"].is_null());
        // A part's size is always explicit.
        assert!(p["size_label"].is_string(), "{p:#}");
    }
    let latte = part(&items, s.latte);
    assert_eq!(latte["size_label"], "Large");
    let addons = latte["addons"].as_array().unwrap();
    assert_eq!(addons.len(), 1);
    assert_eq!(addons[0]["unit_price"], 1500);
    assert_eq!(addons[0]["line_total"], 1500);
    assert_eq!(v["subtotal"], 17500);
    assert_eq!(v["total_amount"], 17500);
    assert_eq!(v["price_flagged"], false);
    assert_eq!(v["deals"], json!([]));
    // The old-till stubs stay on every line.
    for l in &items {
        assert!(l["bundle_id"].is_null());
        assert_eq!(l["bundle_components"], json!([]));
    }
}

#[sqlx::test]
async fn two_lunch_combos_double_every_figure_and_the_money_adds_up(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let (st, v) = post_order(
        &app,
        &s.admin_token(),
        s.order_body(json!([s.lunch_line(2)])),
    )
    .await;
    assert_eq!(st, 201, "{v:#}");
    let items = lines(&v);
    assert_eq!(items[0]["quantity"], 2);
    assert_eq!(items[0]["combo_unit_price"], 15000);
    for (item, qty, total) in [(s.burger, 2, 17142), (s.fries, 2, 5716), (s.latte, 2, 9142)] {
        let p = part(&items, item);
        assert_eq!(p["quantity"], qty);
        assert_eq!(p["line_total"], total, "{p:#}");
    }
    let latte = part(&items, s.latte);
    assert_eq!(latte["combo_surcharge"], 2000);
    assert_eq!(latte["addons"][0]["line_total"], 3000);
    assert_eq!(v["subtotal"], 35000);

    // The money identities (§1.2), line by line.
    let mut sum = 0;
    for l in &items {
        let lt = l["line_total"].as_i64().unwrap();
        match l["line_kind"].as_str().unwrap() {
            "combo" => assert_eq!((l["unit_price"].as_i64(), lt), (Some(0), 0)),
            "combo_part" => assert_eq!(
                lt,
                l["combo_share"].as_i64().unwrap() + l["combo_surcharge"].as_i64().unwrap()
            ),
            k => panic!("unexpected {k}"),
        }
        sum += lt;
        for a in l["addons"].as_array().unwrap() {
            assert_eq!(
                a["line_total"].as_i64().unwrap(),
                a["unit_price"].as_i64().unwrap()
                    * a["quantity"].as_i64().unwrap()
                    * l["quantity"].as_i64().unwrap()
            );
            sum += a["line_total"].as_i64().unwrap();
        }
    }
    assert_eq!(sum, v["subtotal"].as_i64().unwrap());
    // Σ shares == n × P.
    let shares: i64 = items
        .iter()
        .map(|l| l["combo_share"].as_i64().unwrap())
        .sum();
    assert_eq!(shares, 30000);
}

#[sqlx::test]
async fn each_part_deducts_and_costs_exactly_what_the_item_alone_would(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let (st, combo) = post_order(
        &app,
        &s.admin_token(),
        s.order_body(json!([s.lunch_line(1)])),
    )
    .await;
    assert_eq!(st, 201, "{combo:#}");
    let (st, alone) = post_order(
        &app,
        &s.admin_token(),
        s.order_body(json!([
            {"menu_item_id": s.burger, "quantity": 1},
            {"menu_item_id": s.fries, "quantity": 1},
            {"menu_item_id": s.latte, "quantity": 1, "size_label": "Large",
             "addons": [{"addon_item_id": s.oat, "quantity": 1}]},
        ])),
    )
    .await;
    assert_eq!(st, 201, "{alone:#}");
    let (ci, ai) = (lines(&combo), lines(&alone));
    for item in [s.burger, s.fries, s.latte] {
        let p = part(&ci, item);
        let a = ai
            .iter()
            .find(|l| l["menu_item_id"] == item.to_string())
            .unwrap();
        assert_eq!(p["deductions_snapshot"], a["deductions_snapshot"], "{item}");
        assert_eq!(p["line_cost"], a["line_cost"], "{item}");
        assert_eq!(p["unit_cost"], a["unit_cost"], "{item}");
        assert_eq!(p["cost_missing"], a["cost_missing"], "{item}");
    }
    // Burger 10 g, Fries 5 g, Latte Large 14 g of Beans at 100 per g.
    assert_eq!(part(&ci, s.burger)["line_cost"], 1000);
    assert_eq!(part(&ci, s.fries)["line_cost"], 500);
    assert_eq!(ci[0]["deductions_snapshot"], json!([]));
    // Stock: two identical sales, 29 g each.
    let moved: i64 = sqlx::query_scalar(
        "SELECT (-SUM(quantity))::bigint FROM inventory_movements WHERE source_id = $1::uuid",
    )
    .bind(combo["id"].as_str().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(moved, 29);
}

#[sqlx::test]
async fn a_category_choice_is_split_by_its_own_price(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let line = json!({"menu_item_id": s.combo, "quantity": 1, "combo": {"picks": [
        {"slot_id": s.slot_main, "menu_item_id": s.burger},
        {"slot_id": s.slot_side, "menu_item_id": s.fries},
        {"slot_id": s.slot_drink, "menu_item_id": s.cola},
    ]}});
    let (st, v) = post_order(&app, &s.admin_token(), s.order_body(json!([line]))).await;
    assert_eq!(st, 201, "{v:#}");
    let items = lines(&v);
    assert_eq!(part(&items, s.burger)["line_total"], 9474);
    assert_eq!(part(&items, s.fries)["line_total"], 3158);
    assert_eq!(part(&items, s.cola)["line_total"], 2368);
    assert_eq!(v["subtotal"], 15000);
}

#[sqlx::test]
async fn bad_picks_are_coded_refusals_live(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let tok = s.admin_token();

    let no_picks = json!({"menu_item_id": s.combo, "quantity": 1});
    let (st, v) = post_order(&app, &tok, s.order_body(json!([no_picks]))).await;
    assert_eq!(
        (st, v["code"].as_str()),
        (400, Some("COMBO_PICKS_REQUIRED")),
        "{v:#}"
    );

    let missing_side = json!({"menu_item_id": s.combo, "quantity": 1, "combo": {"picks": [
        {"slot_id": s.slot_main, "menu_item_id": s.burger},
        {"slot_id": s.slot_drink, "menu_item_id": s.latte},
    ]}});
    let (st, v) = post_order(&app, &tok, s.order_body(json!([missing_side]))).await;
    assert_eq!(
        (st, v["code"].as_str()),
        (400, Some("COMBO_SLOT_TOO_FEW")),
        "{v:#}"
    );
    assert_eq!(v["vars"]["slot_id"], s.slot_side.to_string());
    assert_eq!(v["vars"]["min"], 1);
    assert_eq!(v["vars"]["got"], 0);
    assert_eq!(v["vars"]["slot"], "Side");
    assert_eq!(v["error"], "Choose at least 1 for Side.");

    let two_mains = json!({"menu_item_id": s.combo, "quantity": 1, "combo": {"picks": [
        {"slot_id": s.slot_main, "menu_item_id": s.burger, "quantity": 2},
        {"slot_id": s.slot_side, "menu_item_id": s.fries},
        {"slot_id": s.slot_drink, "menu_item_id": s.latte},
    ]}});
    let (st, v) = post_order(&app, &tok, s.order_body(json!([two_mains]))).await;
    assert_eq!(
        (st, v["code"].as_str()),
        (400, Some("COMBO_SLOT_TOO_MANY")),
        "{v:#}"
    );

    let burger_as_drink = json!({"menu_item_id": s.combo, "quantity": 1, "combo": {"picks": [
        {"slot_id": s.slot_main, "menu_item_id": s.burger},
        {"slot_id": s.slot_side, "menu_item_id": s.fries},
        {"slot_id": s.slot_drink, "menu_item_id": s.croissant},
    ]}});
    let (st, v) = post_order(&app, &tok, s.order_body(json!([burger_as_drink]))).await;
    assert_eq!(
        (st, v["code"].as_str()),
        (400, Some("COMBO_CHOICE_NOT_ALLOWED")),
        "{v:#}"
    );
    assert_eq!(v["vars"]["menu_item_id"], s.croissant.to_string());

    let nested = json!({"menu_item_id": s.combo, "quantity": 1, "combo": {"picks": [
        {"slot_id": s.slot_main, "menu_item_id": s.combo},
        {"slot_id": s.slot_side, "menu_item_id": s.fries},
        {"slot_id": s.slot_drink, "menu_item_id": s.latte},
    ]}});
    let (st, v) = post_order(&app, &tok, s.order_body(json!([nested]))).await;
    assert_eq!(
        (st, v["code"].as_str()),
        (400, Some("COMBO_NESTED")),
        "{v:#}"
    );

    // Nothing was written.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM orders WHERE till_id = $1")
        .bind(s.till)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[sqlx::test]
async fn no_staff_drink_and_no_reward_inside_a_combo(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let mut line = s.lunch_line(1);
    line["staff_drink"] = json!({"id": Uuid::new_v4(), "note": "for Ali"});
    let (st, v) = post_order(&app, &s.admin_token(), s.order_body(json!([line]))).await;
    assert_eq!(
        (st, v["code"].as_str()),
        (400, Some("STAFF_DRINK_IN_COMBO")),
        "{v:#}"
    );

    let mut body = s.order_body(json!([s.lunch_line(1)]));
    body["loyalty_customer_id"] = json!(Uuid::new_v4());
    body["loyalty_redemptions"] = json!([{"item_index": 0, "units": 1}]);
    let (st, v) = post_order(&app, &s.admin_token(), body).await;
    assert_eq!(
        (st, v["code"].as_str()),
        (409, Some("REWARD_IN_COMBO")),
        "{v:#}"
    );
    assert_eq!(v["error"], "Rewards can't be used inside a combo.");
}

#[sqlx::test]
async fn an_unavailable_combo_is_flagged_not_refused_at_the_till(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    // The POS channel is off for combos at the org.
    sqlx::query("INSERT INTO combo_channel_settings (org_id, sell_pos) VALUES ($1, false)")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let (st, v) = post_order(
        &app,
        &s.admin_token(),
        s.order_body(json!([s.lunch_line(1)])),
    )
    .await;
    assert_eq!(st, 201, "{v:#}");
    let items = lines(&v);
    assert!(
        items
            .iter()
            .all(|l| l["price_flagged"] == true || l.get("price_flagged").is_none())
    );
    let flagged: bool = sqlx::query_scalar(
        "SELECT bool_and(price_flagged) FROM order_items WHERE order_id = $1::uuid",
    )
    .bind(v["id"].as_str().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(flagged);
    assert_eq!(v["subtotal"], 17500);
    assert_eq!(
        flags(&pool, v["id"].as_str().unwrap()).await,
        vec!["menu.combos:unavailable".to_string()]
    );
}

#[sqlx::test]
async fn a_combo_and_plain_lines_share_one_order(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let (st, v) = post_order(
        &app,
        &s.admin_token(),
        s.order_body(json!([
            {"menu_item_id": s.cookie, "quantity": 2},
            s.lunch_line(1),
            {"menu_item_id": s.cola, "quantity": 1},
        ])),
    )
    .await;
    assert_eq!(st, 201, "{v:#}");
    assert_eq!(v["subtotal"], 8000 + 17500 + 3000);
    let items = lines(&v);
    assert_eq!(items.len(), 6);
    // Header immediately followed by its three parts.
    let h = items
        .iter()
        .position(|l| l["line_kind"] == "combo")
        .unwrap();
    for k in 1..=3 {
        assert_eq!(items[h + k]["combo_line_id"], items[h]["id"]);
    }
    // GET returns the same lines, each header still followed by its parts.
    let req = test::TestRequest::get()
        .uri(&format!("/orders/{}", v["id"].as_str().unwrap()))
        .insert_header(("Authorization", format!("Bearer {}", s.admin_token())))
        .to_request();
    let got: Value = test::call_and_read_body_json(&app, req).await;
    let ids = |v: &Value| -> Vec<String> {
        let mut x: Vec<String> = lines(v).iter().map(|l| l["id"].to_string()).collect();
        x.sort();
        x
    };
    assert_eq!(ids(&got), ids(&v));
    let gi = lines(&got);
    let h = gi.iter().position(|l| l["line_kind"] == "combo").unwrap();
    for k in 1..=3 {
        assert_eq!(gi[h + k]["combo_line_id"], gi[h]["id"]);
    }
    let _ = Shop::admin_token;
}

#[sqlx::test]
async fn each_part_earns_its_stamp_and_the_header_earns_none(pool: PgPool) {
    let s = shop(&pool).await;
    madar_rust::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();
    // A stamp card that stamps every item (C7: as if each were bought alone).
    sqlx::query(
        "INSERT INTO loyalty_settings \
            (org_id, branch_id, enabled, mode, earn_piastres_per_point, default_reward_cost, \
             require_otp, stamp_per_line_item) \
         VALUES ($1, NULL, true, 'visits', 1000, 10, false, true)",
    )
    .bind(s.org)
    .execute(&pool)
    .await
    .unwrap();
    let member = common::members::seed_loyalty_member(
        &pool,
        s.org,
        "201000000077",
        "Ali",
        "Mcombotoken000000001",
    )
    .await;
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .configure(madar_rust::orders::routes::configure)
            .configure(madar_rust::loyalty::routes::configure),
    )
    .await;
    let (st, v) = post_order(
        &app,
        &s.admin_token(),
        s.order_body(json!([s.lunch_line(1)])),
    )
    .await;
    assert_eq!(st, 201, "{v:#}");
    let req = test::TestRequest::post()
        .uri("/loyalty/award")
        .insert_header(("Authorization", format!("Bearer {}", s.admin_token())))
        .set_json(
            json!({"branch_id": s.branch, "order_id": v["id"], "token": "Mcombotoken000000001"}),
        )
        .to_request();
    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let body: Value = test::read_body_json(resp).await;
    assert!(status.is_success(), "{status} {body:#}");
    let visits: i32 =
        sqlx::query_scalar("SELECT visits_balance FROM loyalty_customers WHERE id = $1")
            .bind(member)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        visits, 3,
        "Burger, Fries, Latte; never the Lunch deal itself"
    );
}
