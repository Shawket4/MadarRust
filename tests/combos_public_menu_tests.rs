//! The public menus carry combos and deals (COMBOS_CONTRACT.md §2.5, owner
//! answers §11): the online storefront (`/public/branches/{id}/menu`, channel
//! `online`) and the QR table menu (`/public/tables/{id}/menu`, channel `qr`).
//!
//! The fixture's "Lunch deal" (P 15000) expands its Drink slot: the Latte
//! item choice (included Regular 5000; Large 6000 → extra 1000), then the
//! Drinks category choice adds Cola (one size, 3000). The Latte appears once.

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
                .app_data(web::Data::new(
                    madar_rust::realtime::hub::BranchEventHub::new(),
                ))
                .configure(madar_rust::delivery::routes::configure)
                .configure(madar_rust::tickets::routes::configure),
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
) -> (u16, Value) {
    let resp = test::call_service(app, test::TestRequest::get().uri(uri).to_request()).await;
    let status = resp.status().as_u16();
    let body = test::read_body(resp).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

async fn storefront(pool: &PgPool, s: &Shop) -> Uuid {
    sqlx::query(
        "INSERT INTO branch_delivery_settings (branch_id, pickup_enabled) VALUES ($1, true)",
    )
    .bind(s.branch)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO branch_tables (org_id, branch_id, label) VALUES ($1, $2, 'T1') RETURNING id",
    )
    .bind(s.org)
    .bind(s.branch)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn find<'a>(menu: &'a Value, id: Uuid) -> Option<&'a Value> {
    menu["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == json!(id))
}

#[sqlx::test]
async fn the_online_menu_carries_the_combo_with_its_choices_expanded(pool: PgPool) {
    let s = shop(&pool).await;
    storefront(&pool, &s).await;
    sqlx::query("UPDATE menu_items SET meal_combo_id = $2, meal_slot_id = $3 WHERE id = $1")
        .bind(s.latte)
        .bind(s.combo)
        .bind(s.slot_drink)
        .execute(&pool)
        .await
        .unwrap();
    let app = app!(pool);
    let uri = format!(
        "/public/branches/{}/menu?channel=pickup&preview=true",
        s.branch
    );
    let (st, m) = get(&app, &uri).await;
    assert_eq!(st, 200, "{m}");
    let c = find(&m, s.combo).expect("the combo is on the menu");
    assert_eq!(c["kind"], "combo");
    assert_eq!(c["price"], 15000);
    assert_eq!(c["meal"], Value::Null);
    let combo = &c["combo"];
    assert_eq!(combo["is_fixed"], false);
    let slots = combo["slots"].as_array().unwrap();
    assert_eq!(slots.len(), 3);
    assert_eq!(slots[0]["id"], json!(s.slot_main));
    assert_eq!(slots[0]["choices"][0]["menu_item_id"], json!(s.burger));
    assert_eq!(slots[0]["choices"][0]["base_price"], 12000);
    let drink = &slots[2];
    assert_eq!(drink["default_item_id"], json!(s.latte));
    let choices = drink["choices"].as_array().unwrap();
    assert_eq!(choices.len(), 2, "{drink}");
    assert_eq!(choices[0]["menu_item_id"], json!(s.latte));
    assert_eq!(choices[0]["name"], "Latte");
    assert_eq!(choices[0]["base_price"], 5000);
    assert_eq!(choices[0]["included_size_label"], "Regular");
    assert_eq!(
        choices[0]["sizes"],
        json!([{"label": "Large", "price": 6000, "extra": 1000},
               {"label": "Regular", "price": 5000, "extra": 0}])
    );
    assert_eq!(choices[1]["menu_item_id"], json!(s.cola));
    assert_eq!(choices[1]["base_price"], 3000);
    assert_eq!(choices[1]["surcharge"], 0);

    let latte = find(&m, s.latte).unwrap();
    assert_eq!(latte["kind"], "item");
    assert_eq!(
        latte["meal"],
        json!({"combo_id": s.combo, "slot_id": s.slot_drink})
    );
    assert_eq!(find(&m, s.burger).unwrap()["meal"], Value::Null);
    assert_eq!(m["deals"], json!([]));
}

#[sqlx::test]
async fn a_channel_switched_off_hides_combos_and_deals(pool: PgPool) {
    let s = shop(&pool).await;
    let table = storefront(&pool, &s).await;
    let deal: Uuid = sqlx::query_scalar(
        "INSERT INTO deal_rules (org_id, name, kind, qty, price) VALUES ($1, 'Any 2 bites', 'n_for_price', 2, 9000) RETURNING id",
    )
    .bind(s.org)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO deal_rule_items (org_id, deal_rule_id, category_id) VALUES ($1, $2, $3)",
    )
    .bind(s.org)
    .bind(deal)
    .bind(s.bakery)
    .execute(&pool)
    .await
    .unwrap();
    let app = app!(pool);
    let online = format!(
        "/public/branches/{}/menu?channel=pickup&preview=true",
        s.branch
    );
    let qr = format!("/public/tables/{table}/menu");

    // Everything on: both menus show the combo and the deal.
    for uri in [&online, &qr] {
        let (st, m) = get(&app, uri).await;
        assert_eq!(st, 200, "{uri}: {m}");
        assert!(find(&m, s.combo).is_some(), "{uri}");
        assert_eq!(m["deals"][0]["id"], json!(deal), "{uri}");
        assert_eq!(m["deals"][0]["pool"][0]["category_id"], json!(s.bakery));
    }

    // Online off at this branch: gone online, still on the QR menu.
    sqlx::query("INSERT INTO combo_channel_branch_overrides (branch_id, org_id, sell_online) VALUES ($1, $2, false)")
        .bind(s.branch)
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let (_, m) = get(&app, &online).await;
    assert!(find(&m, s.combo).is_none());
    assert!(find(&m, s.burger).is_some(), "items stay");
    assert_eq!(m["deals"], json!([]));
    let (_, m) = get(&app, &qr).await;
    assert!(find(&m, s.combo).is_some());

    // QR off org-wide: gone on the table menu too.
    sqlx::query("INSERT INTO combo_channel_settings (org_id, sell_qr) VALUES ($1, false)")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let (_, m) = get(&app, &qr).await;
    assert!(find(&m, s.combo).is_none());
    assert_eq!(m["deals"], json!([]));

    // A deal switched off at the branch leaves the menus.
    sqlx::query("DELETE FROM combo_channel_settings WHERE org_id = $1")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO deal_rule_branch_overrides (deal_rule_id, branch_id, org_id, is_active) VALUES ($1, $2, $3, false)")
        .bind(deal)
        .bind(s.branch)
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let (_, m) = get(&app, &qr).await;
    assert!(find(&m, s.combo).is_some());
    assert_eq!(m["deals"], json!([]));
}

#[sqlx::test]
async fn a_combo_outside_its_window_or_with_an_empty_slot_is_hidden(pool: PgPool) {
    let s = shop(&pool).await;
    let table = storefront(&pool, &s).await;
    let app = app!(pool);
    let qr = format!("/public/tables/{table}/menu");

    // The only Main (Burger) switched off at the branch: the slot is empty.
    sqlx::query(
        "INSERT INTO branch_menu_overrides (branch_id, menu_item_id, is_available) VALUES ($2, $1, false)",
    )
    .bind(s.burger)
    .bind(s.branch)
    .execute(&pool)
    .await
    .unwrap();
    let (st, m) = get(&app, &qr).await;
    assert_eq!(st, 200, "{m}");
    assert!(find(&m, s.burger).is_none(), "the burger is off here");
    assert!(find(&m, s.combo).is_none(), "so is the combo");
    sqlx::query("DELETE FROM branch_menu_overrides")
        .execute(&pool)
        .await
        .unwrap();

    // A window on a single weekday that isn't today, all day.
    let (_, m) = get(&app, &qr).await;
    assert!(find(&m, s.combo).is_some());
    let dow: i32 =
        sqlx::query_scalar("SELECT extract(dow FROM now() AT TIME ZONE 'Africa/Cairo')::int")
            .fetch_one(&pool)
            .await
            .unwrap();
    let other = 1i16 << ((dow + 3) % 7);
    sqlx::query("INSERT INTO sale_windows (org_id, combo_item_id, weekdays) VALUES ($1, $2, $3)")
        .bind(s.org)
        .bind(s.combo)
        .bind(other)
        .execute(&pool)
        .await
        .unwrap();
    let (_, m) = get(&app, &qr).await;
    assert!(find(&m, s.combo).is_none());
}
