//! The Bundles report (C6, COMBOS_CONTRACT.md §2.6) and the item reports'
//! `line_kind` rule: a combo's parts count as their items everywhere, the
//! header only in the Bundles report.
//!
//! The sales are seeded in SQL in exactly the shape the order path stores
//! (header + parts, deal lines), so the figures below are hand-computed and
//! independent of the pricing code.
//!
//! Sales (all today at the shop's Main branch, tax 0):
//! A. 2 × Lunch deal (P 15000), the worked example × 2:
//!    header qty 2, 0/0, combo_unit_price 15000;
//!    Burger  qty 2, unit 12000, share 17142,              line 17142, cost 2000
//!    Fries   qty 2, unit  4000, share  5716,              line  5716, cost 1000
//!    Latte L qty 2, unit  6000, share  7142 + surch 2000, line  9142, cost 2800
//!      + Oat milk add-on 1500 × 2 = 3000
//!    order total 17142 + 5716 + 9142 + 3000 = 35000 (= 2 × 17500).
//!    Then ONE of the two combos is refunded: 17500 in refund lines
//!    header 1 × 0, Burger 1 × 8571, Fries 1 × 2858, Latte 1 × (4571 + 1500).
//! B. 1 Burger 12000 alone (cost 1000).
//! C. A deal "Any 2 bites for 90": Croissant × 2 (5500 each, deal_minor 2000,
//!    line 9000, cost 400) + a Cookie 4000 (not in the deal, cost 100).
//! D. A VOIDED sale of 1 Lunch deal: excluded everywhere.
//!
//! Bundles, combo row (refunds netted: each part keeps (q − r)/q):
//!   sold    = 2 − 1                                     = 1
//!   orders  = 1 (A)
//!   revenue = (17142 + 5716 + 9142 + 3000) / 2          = 17500
//!   list    = (2×12000 + 2×4000 + 2×6000 + 3000) / 2    = 23500
//!   saving  = 23500 − 17500                             = 6000
//!   cost    = (2000 + 1000 + 2800) / 2                  = 2900
//!   margin  = (17500 − 2900) / 17500 = 0.834285…        → "0.8343"
//! Bundles, deal row (the consumed units only):
//!   sold 1 (times), orders 1, revenue 2 × 5500 − 2000 = 9000,
//!   list 11000, saving 2000, cost 400, margin 8600/9000 = 0.9555… → "0.9556"
//! Totals: sold 2, revenue 26500, list 34500, saving 8000, cost 3300.
//! Mix: Main Burger one_size 1 (0), Side Fries one_size 1 (0),
//!      Drink Latte Large 1 (surcharges 2000 × 1/2 = 1000).
//!
//! Item sales keep their existing basis (orders refunded IN FULL drop out;
//! a partial refund is not netted per line): Burger = 2 (part, A) + 1 (B) = 3,
//! revenue 17142 + 12000 = 29142. The "Lunch deal" header never appears.
//! Units on the sales report: A parts 6, B 1, C 3 → 10 (headers not units).

mod common;

use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use common::combos::{Shop, secret, shop, token};
use madar_rust::models::UserRole;

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .configure(|cfg| {
                    madar_rust::reports::routes::configure(cfg, web::Data::new($pool.clone()))
                })
                .configure(madar_rust::insights::routes::configure),
        )
        .await
    };
}

async fn get(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    uri: &str,
    token: &str,
) -> (u16, Value) {
    let req = test::TestRequest::get()
        .uri(uri)
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(app, req).await;
    let status = resp.status().as_u16();
    let body = test::read_body(resp).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

async fn order(pool: &PgPool, s: &Shop, n: i32, status: &str, total: i32) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO orders (branch_id, teller_id, till_id, idempotency_key, subtotal, discount_amount,
             tax_amount, total_amount, status, order_number, payment_method, order_ref)
         VALUES ($1, $2, $3, gen_random_uuid(), $4, 0, 0, $4, $5::order_status, $6, 'cash', gen_random_uuid()::text)
         RETURNING id",
    )
    .bind(s.branch)
    .bind(s.admin)
    .bind(s.till)
    .bind(total)
    .bind(status)
    .bind(n)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[allow(clippy::too_many_arguments)]
async fn line(
    pool: &PgPool,
    order: Uuid,
    item: Uuid,
    name: &str,
    kind: &str,
    header: Option<Uuid>,
    slot: Option<(Uuid, &str)>,
    size: Option<&str>,
    qty: i32,
    unit: i32,
    share: i32,
    surcharge: i32,
    total: i32,
    cost: i64,
    deal_minor: i32,
    combo_unit_price: Option<i32>,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO order_items (order_id, menu_item_id, item_name, line_kind, combo_line_id,
             combo_slot_id, combo_slot_name, size_label, quantity, unit_price, combo_share,
             combo_surcharge, line_total, line_cost, unit_cost, cost_missing, deal_minor, combo_unit_price)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                 CASE WHEN $9 > 0 THEN $14 / $9 END, false, $15, $16)
         RETURNING id",
    )
    .bind(order)
    .bind(item)
    .bind(name)
    .bind(kind)
    .bind(header)
    .bind(slot.map(|s| s.0))
    .bind(slot.map(|s| s.1))
    .bind(size)
    .bind(qty)
    .bind(unit)
    .bind(share)
    .bind(surcharge)
    .bind(total)
    .bind(cost)
    .bind(deal_minor)
    .bind(combo_unit_price)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn addon(pool: &PgPool, s: &Shop, line: Uuid, qty: i32) {
    sqlx::query(
        "INSERT INTO order_item_addons (order_item_id, addon_item_id, addon_name, unit_price, quantity, line_total)
         VALUES ($1, $2, 'Oat milk', 1500, $3, 1500 * $3)",
    )
    .bind(line)
    .bind(s.oat)
    .bind(qty)
    .execute(pool)
    .await
    .unwrap();
}

/// A header + its three parts for `n` lunch combos; returns (header, burger, fries, latte).
async fn lunch(pool: &PgPool, s: &Shop, o: Uuid, n: i32) -> (Uuid, Uuid, Uuid, Uuid) {
    let h = line(
        pool,
        o,
        s.combo,
        "Lunch deal",
        "combo",
        None,
        None,
        None,
        n,
        0,
        0,
        0,
        0,
        0,
        0,
        Some(15000),
    )
    .await;
    let b = line(
        pool,
        o,
        s.burger,
        "Burger",
        "combo_part",
        Some(h),
        Some((s.slot_main, "Main")),
        Some("one_size"),
        n,
        12000,
        8571 * n,
        0,
        8571 * n,
        1000 * i64::from(n),
        0,
        None,
    )
    .await;
    let f = line(
        pool,
        o,
        s.fries,
        "Fries",
        "combo_part",
        Some(h),
        Some((s.slot_side, "Side")),
        Some("one_size"),
        n,
        4000,
        2858 * n,
        0,
        2858 * n,
        500 * i64::from(n),
        0,
        None,
    )
    .await;
    let l = line(
        pool,
        o,
        s.latte,
        "Latte",
        "combo_part",
        Some(h),
        Some((s.slot_drink, "Drink")),
        Some("Large"),
        n,
        6000,
        3571 * n,
        1000 * n,
        4571 * n,
        1400 * i64::from(n),
        0,
        None,
    )
    .await;
    addon(pool, s, l, n).await;
    (h, b, f, l)
}

struct Seeded {
    s: Shop,
    today: String,
}

async fn seed(pool: &PgPool) -> Seeded {
    let s = shop(pool).await;
    // A: 2 × Lunch deal, then one of them refunded.
    let a = order(pool, &s, 1, "completed", 35000).await;
    let (h, b, f, l) = lunch(pool, &s, a, 2).await;
    let refund: Uuid = sqlx::query_scalar(
        "INSERT INTO order_refunds (org_id, branch_id, order_id, till_id, amount, method, is_cash, reason, issued_by)
         VALUES ($1, $2, $3, $4, 17500, 'cash', true, 'customer_request', $5) RETURNING id",
    )
    .bind(s.org)
    .bind(s.branch)
    .bind(a)
    .bind(s.till)
    .bind(s.admin)
    .fetch_one(pool)
    .await
    .unwrap();
    for (item, amount) in [(h, 0), (b, 8571), (f, 2858), (l, 4571 + 1500)] {
        sqlx::query(
            "INSERT INTO order_refund_lines (org_id, refund_id, order_item_id, quantity, amount) VALUES ($1, $2, $3, 1, $4)",
        )
        .bind(s.org)
        .bind(refund)
        .bind(item)
        .bind(amount)
        .execute(pool)
        .await
        .unwrap();
    }
    // B: a Burger alone.
    let o = order(pool, &s, 2, "completed", 12000).await;
    line(
        pool,
        o,
        s.burger,
        "Burger",
        "item",
        None,
        None,
        Some("one_size"),
        1,
        12000,
        0,
        0,
        12000,
        1000,
        0,
        None,
    )
    .await;
    // C: two croissants in a deal, and a cookie.
    let o = order(pool, &s, 3, "completed", 13000).await;
    let cr = line(
        pool,
        o,
        s.croissant,
        "Croissant",
        "item",
        None,
        None,
        Some("one_size"),
        2,
        5500,
        0,
        0,
        9000,
        400,
        2000,
        None,
    )
    .await;
    line(
        pool,
        o,
        s.cookie,
        "Cookie",
        "item",
        None,
        None,
        Some("one_size"),
        1,
        4000,
        0,
        0,
        4000,
        100,
        0,
        None,
    )
    .await;
    let rule: Uuid = sqlx::query_scalar(
        "INSERT INTO deal_rules (org_id, name, kind, qty, price) VALUES ($1, 'Any 2 bites for 90', 'n_for_price', 2, 9000) RETURNING id",
    )
    .bind(s.org)
    .fetch_one(pool)
    .await
    .unwrap();
    let od: Uuid = sqlx::query_scalar(
        "INSERT INTO order_deals (org_id, order_id, deal_rule_id, deal_name, times, discount, discount_server)
         VALUES ($1, $2, $3, 'Any 2 bites for 90', 1, 2000, 2000) RETURNING id",
    )
    .bind(s.org)
    .bind(o)
    .bind(rule)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO order_deal_lines (order_deal_id, order_item_id, org_id, units, discount) VALUES ($1, $2, $3, 2, 2000)",
    )
    .bind(od)
    .bind(cr)
    .bind(s.org)
    .execute(pool)
    .await
    .unwrap();
    // D: a voided lunch combo.
    let o = order(pool, &s, 4, "voided", 17500).await;
    lunch(pool, &s, o, 1).await;

    let today: String = sqlx::query_scalar(
        "SELECT to_char((now() AT TIME ZONE 'Africa/Cairo')::date, 'YYYY-MM-DD')",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    Seeded { s, today }
}

fn row<'a>(report: &'a Value, kind: &str) -> &'a Value {
    report["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["kind"] == kind)
        .unwrap_or_else(|| panic!("no {kind} row in {report}"))
}

#[sqlx::test]

async fn the_bundles_report_is_hand_computed(pool: PgPool) {
    let Seeded { s, today } = seed(&pool).await;
    let app = app!(pool);
    let (st, r) = get(
        &app,
        &format!(
            "/reports/bundles?from={today}&to={today}&branch_id={}",
            s.branch
        ),
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, 200, "{r}");
    assert_eq!(r["from"], json!(today));
    assert_eq!(r["rows"].as_array().unwrap().len(), 2, "{r}");

    let c = row(&r, "combo");
    assert_eq!(c["id"], json!(s.combo));
    assert_eq!(c["name"], "Lunch deal");
    assert_eq!(c["sold"], 1);
    assert_eq!(c["orders"], 1);
    assert_eq!(c["revenue"], 17500);
    assert_eq!(c["list_value"], 23500);
    assert_eq!(c["saving"], 6000);
    assert_eq!(c["cost"], 2900);
    assert_eq!(c["cost_missing"], false);
    assert_eq!(c["margin"], "0.8343");

    let d = row(&r, "deal");
    assert_eq!(d["name"], "Any 2 bites for 90");
    assert_eq!(d["sold"], 1);
    assert_eq!(d["orders"], 1);
    assert_eq!(d["revenue"], 9000);
    assert_eq!(d["list_value"], 11000);
    assert_eq!(d["saving"], 2000);
    assert_eq!(d["cost"], 400);
    assert_eq!(d["margin"], "0.9556");

    assert_eq!(
        r["totals"],
        json!({"sold": 2, "revenue": 26500, "list_value": 34500, "saving": 8000, "cost": 3300})
    );

    // Every branch of the org (no branch_id) is the same here.
    let (st, all) = get(
        &app,
        &format!("/reports/bundles?from={today}&to={today}"),
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, 200, "{all}");
    assert_eq!(all["totals"], r["totals"]);

    // The kind filter.
    for kind in ["combo", "deal"] {
        let (st, k) = get(
            &app,
            &format!("/reports/bundles?from={today}&to={today}&kind={kind}"),
            &s.admin_token(),
        )
        .await;
        assert_eq!(st, 200, "{k}");
        let rows = k["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{k}");
        assert_eq!(rows[0]["kind"], kind);
        assert_eq!(k["totals"]["sold"], 1);
    }

    // Another day: nothing.
    let (st, empty) = get(
        &app,
        "/reports/bundles?from=2020-01-01&to=2020-01-02",
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(empty["rows"], json!([]));
    assert_eq!(
        empty["totals"],
        json!({"sold": 0, "revenue": 0, "list_value": 0, "saving": 0, "cost": 0})
    );
}

#[sqlx::test]

async fn the_mix_counts_each_slot_s_picks_net_of_refunds(pool: PgPool) {
    let Seeded { s, today } = seed(&pool).await;
    let app = app!(pool);
    let (st, m) = get(
        &app,
        &format!(
            "/reports/bundles/combos/{}/mix?from={today}&to={today}&branch_id={}",
            s.combo, s.branch
        ),
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, 200, "{m}");
    assert_eq!(m["combo_id"], json!(s.combo));
    let slots = m["slots"].as_array().unwrap();
    let names: Vec<&str> = slots.iter().map(|x| x["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Main", "Side", "Drink"]);
    let pick = |i: usize| &slots[i]["picks"][0];
    assert_eq!(slots[0]["slot_id"], json!(s.slot_main));
    assert_eq!(pick(0)["menu_item_id"], json!(s.burger));
    assert_eq!(pick(0)["name"], "Burger");
    assert_eq!(pick(0)["count"], 1);
    assert_eq!(pick(0)["surcharge_total"], 0);
    assert_eq!(pick(1)["menu_item_id"], json!(s.fries));
    assert_eq!(pick(1)["count"], 1);
    assert_eq!(pick(2)["menu_item_id"], json!(s.latte));
    assert_eq!(pick(2)["size_label"], "Large");
    assert_eq!(pick(2)["count"], 1);
    assert_eq!(pick(2)["surcharge_total"], 1000);
    for x in slots {
        assert_eq!(x["picks"].as_array().unwrap().len(), 1, "{x}");
    }
}

#[sqlx::test]

async fn a_teller_cannot_read_the_bundles_report(pool: PgPool) {
    let Seeded { s, today } = seed(&pool).await;
    let app = app!(pool);
    for uri in [
        format!(
            "/reports/bundles?from={today}&to={today}&branch_id={}",
            s.branch
        ),
        format!(
            "/reports/bundles/combos/{}/mix?from={today}&to={today}&branch_id={}",
            s.combo, s.branch
        ),
    ] {
        let (st, body) = get(&app, &uri, &s.teller_token()).await;
        assert_eq!(st, 403, "{uri}: {body}");
    }
    // A backwards window and an unknown kind are the caller's mistake.
    let (st, _) = get(
        &app,
        &format!("/reports/bundles?from={today}&to=2020-01-01"),
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, 400);
    let (st, _) = get(
        &app,
        &format!("/reports/bundles?from={today}&to={today}&kind=bundle"),
        &s.admin_token(),
    )
    .await;
    assert_eq!(st, 400);
    let _ = token(s.admin, s.org, UserRole::OrgAdmin);
}

#[sqlx::test]

async fn item_reports_count_parts_as_their_items_and_never_the_header(pool: PgPool) {
    let Seeded { s, today } = seed(&pool).await;
    let app = app!(pool);
    let t = s.admin_token();

    // branch_sales: top items, by category, units.
    let (st, r) = get(&app, &format!("/reports/branches/{}/sales", s.branch), &t).await;
    assert_eq!(st, 200, "{r}");
    let top = r["top_items"].as_array().unwrap();
    assert!(
        top.iter().all(|i| i["menu_item_id"] != json!(s.combo)),
        "the header is not an item: {top:?}"
    );
    let burger = top
        .iter()
        .find(|i| i["menu_item_id"] == json!(s.burger))
        .unwrap();
    assert_eq!(burger["quantity_sold"], 3);
    assert_eq!(burger["revenue"], 29142);
    for cat in r["by_category"].as_array().unwrap() {
        for i in cat["items"].as_array().unwrap() {
            assert_ne!(i["menu_item_id"], json!(s.combo), "{cat}");
        }
    }
    assert_eq!(r["total_line_items"], 10, "{r}");

    // items-combined.
    let (st, rows) = get(
        &app,
        &format!("/reports/branches/{}/items-combined", s.branch),
        &t,
    )
    .await;
    assert_eq!(st, 200, "{rows}");
    let rows = rows.as_array().unwrap();
    assert!(
        rows.iter().all(|i| i["item_id"] != json!(s.combo)),
        "{rows:?}"
    );
    let b = rows
        .iter()
        .find(|i| i["item_id"] == json!(s.burger))
        .unwrap();
    assert_eq!(b["total_qty"], 3);

    // POS metrics' top items.
    let (st, m) = get(
        &app,
        &format!(
            "/reports/branches/{}/pos-metrics?from={today}&to={today}",
            s.branch
        ),
        &t,
    )
    .await;
    assert_eq!(st, 200, "{m}");
    let top = m["top_items"].as_array().unwrap();
    assert!(
        top.iter().all(|i| i["item_id"] != json!(s.combo)),
        "{top:?}"
    );
    let b = top
        .iter()
        .find(|i| i["item_id"] == json!(s.burger))
        .unwrap();
    assert_eq!(b["quantity"], 3);

    // The insights margin ledger: the combo is not a SKU with zero sales.
    let (st, l) = get(
        &app,
        &format!("/insights/branches/{}/menu-margin", s.branch),
        &t,
    )
    .await;
    assert_eq!(st, 200, "{l}");
    let rows = l["rows"].as_array().unwrap();
    assert!(
        rows.iter().all(|i| i["menu_item_id"] != json!(s.combo)),
        "{rows:?}"
    );
    let b = rows
        .iter()
        .find(|i| i["menu_item_id"] == json!(s.burger))
        .unwrap();
    assert_eq!(b["quantity_sold"], 3);
}
