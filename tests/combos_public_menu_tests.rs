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

fn find(menu: &Value, id: Uuid) -> Option<&Value> {
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

/// Arabic slot names on both public menus: the storefront and the QR table
/// menu carry each slot's `name_translations` (the page picks the Arabic in
/// Arabic); a slot with none carries `{}`.
#[sqlx::test]
async fn both_public_menus_carry_each_slot_s_arabic_name(pool: PgPool) {
    let s = shop(&pool).await;
    let table = storefront(&pool, &s).await;
    for (slot, ar) in [(s.slot_main, "الطبق الرئيسي"), (s.slot_drink, "مشروب")] {
        sqlx::query("UPDATE combo_slots SET name_translations = $2 WHERE id = $1")
            .bind(slot)
            .bind(json!({"ar": ar}))
            .execute(&pool)
            .await
            .unwrap();
    }
    let app = app!(pool);
    for uri in [
        format!(
            "/public/branches/{}/menu?channel=pickup&preview=true",
            s.branch
        ),
        format!("/public/tables/{table}/menu"),
    ] {
        let (st, m) = get(&app, &uri).await;
        assert_eq!(st, 200, "{uri}: {m}");
        let slots = find(&m, s.combo).expect("the combo is on the menu")["combo"]["slots"]
            .as_array()
            .unwrap()
            .clone();
        let names: Vec<(Value, Value)> = slots
            .iter()
            .map(|sl| (sl["name"].clone(), sl["name_translations"].clone()))
            .collect();
        assert_eq!(
            names,
            vec![
                (json!("Main"), json!({"ar": "الطبق الرئيسي"})),
                (json!("Side"), json!({})),
                (json!("Drink"), json!({"ar": "مشروب"})),
            ],
            "{uri}"
        );
    }
}

/// Unavailable choices are shown greyed, not hidden (owner, 2026-09-27): a
/// choice switched off at the branch stays in its slot with `available:
/// false` and no sizes, on both public menus, and is never the default (the
/// slot's default is cleared when it is the unavailable one). An item choice
/// whose item was deactivated shows the same way; a deleted one does not.
#[sqlx::test]
async fn an_unavailable_choice_is_shown_greyed_and_never_the_default(pool: PgPool) {
    let s = shop(&pool).await;
    let table = storefront(&pool, &s).await;
    // Cola (reached through the Drinks category) is off at this branch, and
    // so is the Latte, the Drink slot's own default.
    for item in [s.cola, s.latte] {
        sqlx::query(
            "INSERT INTO branch_menu_overrides (branch_id, menu_item_id, is_available) VALUES ($1, $2, false)",
        )
        .bind(s.branch)
        .bind(item)
        .execute(&pool)
        .await
        .unwrap();
    }
    // A second Drinks item keeps the slot sellable.
    let tea = common::combos::item(&pool, s.org, s.drinks, "Tea", 2000).await;
    let app = app!(pool);
    for uri in [
        format!(
            "/public/branches/{}/menu?channel=pickup&preview=true",
            s.branch
        ),
        format!("/public/tables/{table}/menu"),
    ] {
        let (st, m) = get(&app, &uri).await;
        assert_eq!(st, 200, "{uri}: {m}");
        let drink =
            &find(&m, s.combo).expect("still on sale: Tea is available")["combo"]["slots"][2];
        let choices: Vec<(Value, Value, usize)> = drink["choices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                (
                    c["menu_item_id"].clone(),
                    c["available"].clone(),
                    c["sizes"].as_array().map_or(0, Vec::len),
                )
            })
            .collect();
        // The Latte item choice first, then the category in name order.
        assert_eq!(
            choices,
            vec![
                (json!(s.latte), json!(false), 0),
                (json!(s.cola), json!(false), 0),
                (json!(tea), json!(true), 1),
            ],
            "{uri}: {drink:#}"
        );
        assert_eq!(drink["choices"][0]["name"], "Latte", "{uri}");
        assert_eq!(drink["default_item_id"], Value::Null, "{uri}");
        assert_eq!(drink["default_size_label"], Value::Null, "{uri}");
        // The other slots are untouched: available, their defaults kept.
        let main = &find(&m, s.combo).unwrap()["combo"]["slots"][0];
        assert_eq!(main["choices"][0]["available"], true, "{uri}");
        assert_eq!(main["default_item_id"], json!(s.burger), "{uri}");
    }

    // Deactivated (not deleted), the Latte item choice still shows greyed;
    // deleted, it is gone.
    sqlx::query("DELETE FROM branch_menu_overrides WHERE menu_item_id = $1")
        .bind(s.latte)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE menu_items SET is_active = false WHERE id = $1")
        .bind(s.latte)
        .execute(&pool)
        .await
        .unwrap();
    let online = format!(
        "/public/branches/{}/menu?channel=pickup&preview=true",
        s.branch
    );
    let (_, m) = get(&app, &online).await;
    let drink = &find(&m, s.combo).unwrap()["combo"]["slots"][2];
    assert_eq!(drink["choices"][0]["menu_item_id"], json!(s.latte));
    assert_eq!(drink["choices"][0]["available"], false);
    sqlx::query("UPDATE menu_items SET deleted_at = now() WHERE id = $1")
        .bind(s.latte)
        .execute(&pool)
        .await
        .unwrap();
    let (_, m) = get(&app, &online).await;
    let drink = &find(&m, s.combo).unwrap()["combo"]["slots"][2];
    assert!(
        drink["choices"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["menu_item_id"] != json!(s.latte)),
        "{drink:#}"
    );
}

/// A category choice listed before an item's own choice doesn't lend the item
/// its surcharge: the item's own choice prices it, as the order is charged.
#[sqlx::test]
async fn an_item_s_own_choice_wins_over_its_category_s_wherever_listed(pool: PgPool) {
    let s = shop(&pool).await;
    storefront(&pool, &s).await;
    sqlx::query(
        "UPDATE combo_slot_choices SET sort = -1, surcharge = 700 WHERE slot_id = $1 AND category_id = $2",
    )
    .bind(s.slot_drink)
    .bind(s.drinks)
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
    let combo = &find(&m, s.combo).expect("the combo is on the menu")["combo"];
    let drink = combo["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|sl| sl["id"] == json!(s.slot_drink))
        .unwrap();
    let choice_of = |id: Uuid| {
        drink["choices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["menu_item_id"] == json!(id))
            .cloned()
            .unwrap()
    };
    let latte = choice_of(s.latte);
    assert_eq!(latte["surcharge"], 0, "{drink}");
    assert_eq!(latte["included_size_label"], "Regular");
    assert_eq!(choice_of(s.cola)["surcharge"], 700, "{drink}");
}
