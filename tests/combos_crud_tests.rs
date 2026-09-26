//! Combos, deals and the combo settings: the dashboard CRUD (COMBOS_CONTRACT.md
//! §2.2, §2.3 as adjusted by the owner's answers in §11).
//!
//! Economics of the fixture's "Lunch deal" (P 15000), hand-computed from
//! tests/common/combos.rs (Beans cost 100 per g):
//!   Main  = Burger 12000 one_size, cost 10 g × 100 = 1000
//!   Side  = Fries   4000 one_size, cost  5 g × 100 =  500
//!   Drink = Latte (item choice, included Regular 5000, cost 1000) or any
//!           Drinks item at its cheapest size: Latte Regular 5000 (1000),
//!           Cola 3000 (cost 1 g = 100). Default = the slot default, Latte.
//!   list_default = 12000 + 4000 + 5000 = 21000; saving_default = 6000
//!   list_min     = 12000 + 4000 + 3000 (Cola) = 19000
//!   list_max     = 12000 + 4000 + 5000 = 21000
//!   cost_default = 1000 + 500 + 1000 = 2500
//!   cost_max     = each slot's dearest cost: 1000 + 500 + 1000 = 2500
//!   margin = (15000 − 2500) / 15000 = 0.83333… → "0.8333" (both)

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
                .configure(madar_rust::menu::routes::configure)
                .configure(madar_rust::combos::routes::configure),
        )
        .await
    };
}

async fn call(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let req = match method {
        "GET" => test::TestRequest::get(),
        "POST" => test::TestRequest::post(),
        "PUT" => test::TestRequest::put(),
        "DELETE" => test::TestRequest::delete(),
        _ => unreachable!(),
    }
    .uri(uri)
    .insert_header(("Authorization", format!("Bearer {token}")));
    let req = match body {
        Some(b) => req.set_json(b),
        None => req,
    }
    .to_request();
    let resp = test::call_service(app, req).await;
    let status = resp.status().as_u16();
    let body = test::read_body(resp).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// A new combo: Burger (fixed) + any Drinks item, a Large surcharge of 800 on
/// the Latte... here via the category choice, weekday lunch window.
fn new_combo(s: &Shop, price: i32) -> Value {
    json!({
        "name": "Burger & drink",
        "name_translations": {"ar": "برجر ومشروب"},
        "category_id": s.mains,
        "price": price,
        "windows": [{"branch_id": null, "weekdays": 62, "starts_at": "12:00", "ends_at": "16:00"}],
        "slots": [
            {"name": "Main", "sort": 0, "min": 1, "max": 1, "default_item_id": s.burger,
             "choices": [{"menu_item_id": s.burger}]},
            {"name": "Drink", "name_translations": {"ar": "مشروب"}, "sort": 1, "min": 1, "max": 1,
             "default_item_id": s.latte, "default_size_label": "Regular",
             "choices": [{"category_id": s.drinks, "included_size_label": null,
                          "size_surcharges": [{"size_label": "Large", "surcharge": 800}]}]}
        ]
    })
}

#[sqlx::test]
async fn the_economics_of_the_lunch_deal_are_hand_computed(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let (st, c) = call(
        &app,
        "GET",
        &format!("/combos/{}", s.combo),
        &s.admin_token(),
        None,
    )
    .await;
    assert_eq!(st, 200, "{c}");
    let e = &c["economics"];
    assert_eq!(e["price"], 15000);
    assert_eq!(e["list_default"], 21000);
    assert_eq!(e["list_min"], 19000);
    assert_eq!(e["list_max"], 21000);
    assert_eq!(e["cost_default"], 2500);
    assert_eq!(e["cost_max"], 2500);
    assert_eq!(e["margin_default"], "0.8333");
    assert_eq!(e["margin_worst"], "0.8333");
    assert_eq!(e["saving_default"], 6000);
    assert_eq!(e["min_margin"], Value::Null);
    assert_eq!(e["warnings"], json!([]));
    assert_eq!(c["kind"], "combo");
    assert_eq!(c["price"], 15000);
    assert_eq!(c["is_fixed"], false);
    assert_eq!(c["available_now"], true);
    assert_eq!(c["slots"].as_array().unwrap().len(), 3);
    assert_eq!(c["slots"][2]["choices"][1]["category_id"], json!(s.drinks));
    assert_eq!(c["slots"][2]["choices"][1]["name"], "Drinks");

    // A minimum margin above it warns (never blocks).
    let (st, _) = call(
        &app,
        "PUT",
        "/settings/combos",
        &s.admin_token(),
        Some(json!({"min_margin": "0.9"})),
    )
    .await;
    assert_eq!(st, 200);
    let (_, c) = call(
        &app,
        "GET",
        &format!("/combos/{}", s.combo),
        &s.admin_token(),
        None,
    )
    .await;
    assert_eq!(c["economics"]["min_margin"], "0.9000");
    assert_eq!(
        c["economics"]["warnings"],
        json!([{"code": "MARGIN_BELOW_MIN", "vars": {"margin": "0.8333", "min": "0.9000"}}])
    );

    // The same figures from a draft, nothing saved.
    let (st, e) = call(
        &app,
        "POST",
        "/combos/economics",
        &s.admin_token(),
        Some(json!({
            "name": "Draft", "price": 15000,
            "slots": c["slots"].as_array().unwrap().iter().map(|sl| json!({
                "name": sl["name"], "min": sl["min"], "max": sl["max"], "sort": sl["sort"],
                "default_item_id": sl["default_item_id"],
                "choices": sl["choices"].as_array().unwrap().iter().map(|ch| json!({
                    "menu_item_id": ch["menu_item_id"], "category_id": ch["category_id"],
                    "included_size_label": ch["included_size_label"], "surcharge": ch["surcharge"]
                })).collect::<Vec<_>>()
            })).collect::<Vec<_>>()
        })),
    )
    .await;
    assert_eq!(st, 200, "{e}");
    assert_eq!(e["list_default"], 21000);
    assert_eq!(e["cost_default"], 2500);
    assert_eq!(e["margin_default"], "0.8333");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM menu_items WHERE name = 'Draft'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[sqlx::test]
async fn create_then_read_back_and_the_list(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let (st, c) = call(
        &app,
        "POST",
        "/combos",
        &s.admin_token(),
        Some(new_combo(&s, 14000)),
    )
    .await;
    assert_eq!(st, 201, "{c}");
    let id: Uuid = serde_json::from_value(c["id"].clone()).unwrap();
    assert_eq!(c["kind"], "combo");
    assert_eq!(c["price"], 14000);
    assert_eq!(c["name_translations"]["ar"], "برجر ومشروب");
    assert_eq!(c["windows"][0]["weekdays"], 62);
    assert_eq!(c["windows"][0]["starts_at"], "12:00");
    assert_eq!(
        c["slots"][1]["choices"][0]["size_surcharges"],
        json!([{"size_label": "Large", "surcharge": 800}])
    );
    assert_eq!(c["slots"][1]["default_size_label"], "Regular");
    // stored as a menu item whose one_size carries P
    let (kind, p): (String, i32) = sqlx::query_as(
        "SELECT mi.kind, z.price FROM menu_items mi JOIN menu_item_sizes z ON z.menu_item_id = mi.id AND z.label = 'one_size' WHERE mi.id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((kind.as_str(), p), ("combo", 14000));
    // list_default = Burger 12000 + the Drink default Latte Regular 5000 = 17000
    assert_eq!(c["economics"]["list_default"], 17000);
    assert_eq!(c["economics"]["saving_default"], 3000);

    let (st, g) = call(
        &app,
        "GET",
        &format!("/combos/{id}"),
        &s.admin_token(),
        None,
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(g["slots"], c["slots"]);

    let (st, l) = call(&app, "GET", "/combos?q=burger", &s.admin_token(), None).await;
    assert_eq!(st, 200, "{l}");
    assert_eq!(l["total"], 1);
    assert_eq!(l["data"][0]["id"], json!(id));
    assert_eq!(l["data"][0]["slot_count"], 2);
    assert_eq!(l["data"][0]["window_count"], 1);
    let (_, l) = call(&app, "GET", "/combos", &s.admin_token(), None).await;
    assert_eq!(l["total"], 2);
    let lunch = l["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!(s.combo))
        .unwrap();
    assert_eq!(lunch["margin_default"], "0.8333");
    assert_eq!(lunch["warning_count"], 0);
    assert_eq!(lunch["is_fixed"], false);
    assert_eq!(lunch["available_now"], true);
    let (_, l) = call(
        &app,
        "GET",
        "/combos?is_active=false",
        &s.admin_token(),
        None,
    )
    .await;
    assert_eq!(l["total"], 0);
}

#[sqlx::test]
async fn a_combo_with_warnings_still_saves(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    // P above the à la carte value: NO_SAVING, still 201.
    let (st, c) = call(
        &app,
        "POST",
        "/combos",
        &s.admin_token(),
        Some(new_combo(&s, 30000)),
    )
    .await;
    assert_eq!(st, 201, "{c}");
    let codes: Vec<&str> = c["economics"]["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"NO_SAVING"), "{codes:?}");
}

#[sqlx::test]
async fn put_diffs_slots_in_place(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    // fries' "make it a meal" points at the Side slot, which the edit deletes
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/menu-items/{}/meal", s.fries),
        &s.admin_token(),
        Some(json!({"combo_id": s.combo, "slot_id": s.slot_side})),
    )
    .await;
    assert_eq!(st, 204);
    let (_, c) = call(
        &app,
        "GET",
        &format!("/combos/{}", s.combo),
        &s.admin_token(),
        None,
    )
    .await;
    let main_choice = c["slots"][0]["choices"][0]["id"].clone();
    let body = json!({
        "name": "Lunch deal", "category_id": s.mains, "price": 16000,
        "slots": [
            {"id": s.slot_main, "name": "Main course", "sort": 0, "min": 1, "max": 1, "default_item_id": s.burger,
             "choices": [{"id": main_choice, "menu_item_id": s.burger, "surcharge": 100}]},
            {"id": s.slot_drink, "name": "Drink", "sort": 1, "min": 1, "max": 1, "default_item_id": s.latte,
             "choices": [{"menu_item_id": s.latte, "included_size_label": "Regular"}]},
            {"name": "Dessert", "sort": 2, "min": 0, "max": 1, "choices": [{"menu_item_id": s.cookie}]}
        ]
    });
    let (st, c) = call(
        &app,
        "PUT",
        &format!("/combos/{}", s.combo),
        &s.admin_token(),
        Some(body),
    )
    .await;
    assert_eq!(st, 200, "{c}");
    assert_eq!(c["price"], 16000);
    assert_eq!(c["slots"][0]["id"], json!(s.slot_main));
    assert_eq!(c["slots"][0]["name"], "Main course");
    assert_eq!(c["slots"][0]["choices"][0]["id"], main_choice);
    assert_eq!(c["slots"][0]["choices"][0]["surcharge"], 100);
    assert_eq!(c["slots"][1]["id"], json!(s.slot_drink));
    assert_eq!(c["slots"][1]["choices"].as_array().unwrap().len(), 1);
    assert_eq!(c["slots"][2]["name"], "Dessert");
    let side: i64 = sqlx::query_scalar("SELECT count(*) FROM combo_slots WHERE id = $1")
        .bind(s.slot_side)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(side, 0);
    let meal: Option<Uuid> =
        sqlx::query_scalar("SELECT meal_combo_id FROM menu_items WHERE id = $1")
            .bind(s.fries)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(meal, None);
    let p: i32 = sqlx::query_scalar(
        "SELECT price FROM menu_item_sizes WHERE menu_item_id = $1 AND label = 'one_size'",
    )
    .bind(s.combo)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(p, 16000);
    // PUT on a plain item is a 404
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/combos/{}", s.burger),
        &s.admin_token(),
        Some(new_combo(&s, 1)),
    )
    .await;
    assert_eq!(st, 404);
}

#[sqlx::test]
async fn invalid_combos_are_refused_with_codes(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let tok = s.admin_token();
    let mut b = new_combo(&s, 10000);
    b["slots"] = json!([]);
    let (st, e) = call(&app, "POST", "/combos", &tok, Some(b)).await;
    assert_eq!(
        (st, e["code"].as_str()),
        (400, Some("COMBO_SLOTS_REQUIRED"))
    );

    let mut b = new_combo(&s, 10000);
    b["slots"][1]["min"] = json!(2);
    let (st, e) = call(&app, "POST", "/combos", &tok, Some(b)).await;
    assert_eq!((st, e["code"].as_str()), (400, Some("COMBO_SLOT_INVALID")));
    assert_eq!(e["vars"], json!({"slot_index": 1, "field": "min"}));

    let mut b = new_combo(&s, 10000);
    b["slots"][0]["choices"] = json!([]);
    let (_, e) = call(&app, "POST", "/combos", &tok, Some(b)).await;
    assert_eq!(e["vars"], json!({"slot_index": 0, "field": "choices"}));

    let mut b = new_combo(&s, 10000);
    b["slots"][0]["default_item_id"] = json!(s.fries);
    let (_, e) = call(&app, "POST", "/combos", &tok, Some(b)).await;
    assert_eq!(
        e["vars"],
        json!({"slot_index": 0, "field": "default_item_id"})
    );

    let mut b = new_combo(&s, 10000);
    b["slots"][0]["choices"] = json!([{"menu_item_id": s.combo}]);
    b["slots"][0]["default_item_id"] = Value::Null;
    let (st, e) = call(&app, "POST", "/combos", &tok, Some(b)).await;
    assert_eq!((st, e["code"].as_str()), (400, Some("COMBO_NESTED")));

    let mut b = new_combo(&s, 10000);
    b["slots"][0]["choices"] = json!([{"menu_item_id": Uuid::new_v4()}]);
    b["slots"][0]["default_item_id"] = Value::Null;
    let (st, e) = call(&app, "POST", "/combos", &tok, Some(b)).await;
    assert_eq!(
        (st, e["code"].as_str()),
        (400, Some("COMBO_CHOICE_NOT_ALLOWED"))
    );

    let mut b = new_combo(&s, 10000);
    b["windows"][0]["starts_at"] = json!("25:00");
    let (st, e) = call(&app, "POST", "/combos", &tok, Some(b)).await;
    assert_eq!((st, e["code"].as_str()), (400, Some("COMBO_SLOT_INVALID")));
    assert_eq!(e["vars"], json!({"window_index": 0, "field": "starts_at"}));

    let mut b = new_combo(&s, 10000);
    b["price"] = json!(-1);
    let (st, _) = call(&app, "POST", "/combos", &tok, Some(b)).await;
    assert_eq!(st, 400);

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM menu_items WHERE kind = 'combo'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1, "nothing half-saved");
}

#[sqlx::test]
async fn make_it_a_meal_links_and_unlinks(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let tok = s.admin_token();
    // Cola is admitted by the Drink slot's category choice
    let (st, e) = call(
        &app,
        "PUT",
        &format!("/menu-items/{}/meal", s.cola),
        &tok,
        Some(json!({"combo_id": s.combo, "slot_id": s.slot_drink})),
    )
    .await;
    assert_eq!(st, 204, "{e}");
    let (c, sl): (Option<Uuid>, Option<Uuid>) =
        sqlx::query_as("SELECT meal_combo_id, meal_slot_id FROM menu_items WHERE id = $1")
            .bind(s.cola)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((c, sl), (Some(s.combo), Some(s.slot_drink)));
    // the item reads the link back, so the studio shows what is saved (live T3:
    // GET /menu-items/{id} always said `meal: null`, the studio read "Not offered")
    let (st, item) = call(&app, "GET", &format!("/menu-items/{}", s.cola), &tok, None).await;
    assert_eq!(st, 200, "{item}");
    assert_eq!(
        item["meal"],
        json!({"combo_id": s.combo, "slot_id": s.slot_drink}),
        "{item}"
    );
    // the burger has no place in the Drink slot
    let (st, e) = call(
        &app,
        "PUT",
        &format!("/menu-items/{}/meal", s.burger),
        &tok,
        Some(json!({"combo_id": s.combo, "slot_id": s.slot_drink})),
    )
    .await;
    assert_eq!((st, e["code"].as_str()), (400, Some("MEAL_TARGET_INVALID")));
    // one of the two alone is invalid
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/menu-items/{}/meal", s.burger),
        &tok,
        Some(json!({"combo_id": s.combo, "slot_id": null})),
    )
    .await;
    assert_eq!(st, 400);
    // a null body unlinks
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/menu-items/{}/meal", s.cola),
        &tok,
        Some(Value::Null),
    )
    .await;
    assert_eq!(st, 204);
    let c: Option<Uuid> = sqlx::query_scalar("SELECT meal_combo_id FROM menu_items WHERE id = $1")
        .bind(s.cola)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(c, None);
    let (_, item) = call(&app, "GET", &format!("/menu-items/{}", s.cola), &tok, None).await;
    assert_eq!(item["meal"], Value::Null, "{item}");
    // both null unlinks too
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/menu-items/{}/meal", s.cola),
        &tok,
        Some(json!({"combo_id": null, "slot_id": null})),
    )
    .await;
    assert_eq!(st, 204);
}

#[sqlx::test]
async fn the_combo_settings_round_trip(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let tok = s.admin_token();
    let (st, g) = call(&app, "GET", "/settings/combos", &tok, None).await;
    assert_eq!(st, 200, "{g}");
    assert_eq!(
        g,
        json!({"min_margin": null,
        "channels": {"pos": true, "qr": true, "online": true, "delivery": true},
        "branch_overrides": []})
    );
    let (st, g) = call(&app, "PUT", "/settings/combos", &tok,
        Some(json!({"min_margin": "0.55", "channels": {"pos": true, "qr": false, "online": true, "delivery": true}}))).await;
    assert_eq!(st, 200, "{g}");
    assert_eq!(g["min_margin"], "0.5500");
    assert_eq!(g["channels"]["qr"], false);
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/settings/combos/branches/{}", s.branch),
        &tok,
        Some(json!({"online": false, "qr": true})),
    )
    .await;
    assert_eq!(st, 204);
    let (_, g) = call(&app, "GET", "/settings/combos", &tok, None).await;
    assert_eq!(
        g["branch_overrides"],
        json!([{"branch_id": s.branch,
        "sell": {"pos": null, "qr": true, "online": false, "delivery": null},
        "effective": {"pos": true, "qr": true, "online": false, "delivery": true}}])
    );
    // min_margin omitted clears it; channels omitted keeps them
    let (_, g) = call(&app, "PUT", "/settings/combos", &tok, Some(json!({}))).await;
    assert_eq!(g["min_margin"], Value::Null);
    assert_eq!(g["channels"]["qr"], false);
    // all-null override = inherit
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/settings/combos/branches/{}", s.branch),
        &tok,
        Some(json!({})),
    )
    .await;
    assert_eq!(st, 204);
    let (_, g) = call(&app, "GET", "/settings/combos", &tok, None).await;
    assert_eq!(g["branch_overrides"], json!([]));
    let (_, _) = call(
        &app,
        "PUT",
        &format!("/settings/combos/branches/{}", s.branch2),
        &tok,
        Some(json!({"pos": false})),
    )
    .await;
    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/settings/combos/branches/{}", s.branch2),
        &tok,
        None,
    )
    .await;
    assert_eq!(st, 204);
    let (_, g) = call(&app, "GET", "/settings/combos", &tok, None).await;
    assert_eq!(g["branch_overrides"], json!([]));
    // a margin outside 0..1 is refused
    let (st, _) = call(
        &app,
        "PUT",
        "/settings/combos",
        &tok,
        Some(json!({"min_margin": "1.5"})),
    )
    .await;
    assert_eq!(st, 400);
    // a branch of another org is a 404
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/settings/combos/branches/{}", Uuid::new_v4()),
        &tok,
        Some(json!({"pos": false})),
    )
    .await;
    assert_eq!(st, 404);
}

fn two_bites(s: &Shop) -> Value {
    json!({"name": "Any 2 bites for 90", "name_translations": {"ar": "أي قطعتين بـ ٩٠"},
           "kind": "n_for_price", "qty": 2, "price": 9000,
           "pool": [{"category_id": s.bakery}],
           "windows": [{"weekdays": 127, "starts_at": "08:00", "ends_at": "11:00"}]})
}

#[sqlx::test]
async fn deals_crud(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let tok = s.admin_token();
    let (st, d) = call(&app, "POST", "/deals", &tok, Some(two_bites(&s))).await;
    assert_eq!(st, 201, "{d}");
    let id = d["id"].as_str().unwrap().to_string();
    assert_eq!(d["kind"], "n_for_price");
    assert_eq!(d["price"], 9000);
    assert_eq!(
        d["pool"],
        json!([{"menu_item_id": null, "category_id": s.bakery, "size_label": null}])
    );
    assert_eq!(d["reward_pool"], json!([]));
    assert_eq!(d["windows"][0]["starts_at"], "08:00");
    assert_eq!(d["is_active"], true);

    let b2g1 = json!({"name": "Buy 2 lattes get a cookie", "kind": "buy_get", "qty": 2, "get_qty": 1,
        "get_percent": 100, "max_per_order": 1, "pool": [{"menu_item_id": s.latte, "size_label": "Large"}],
        "reward_pool": [{"menu_item_id": s.cookie}]});
    let (st, d2) = call(&app, "PUT", &format!("/deals/{id}"), &tok, Some(b2g1)).await;
    assert_eq!(st, 200, "{d2}");
    assert_eq!(d2["id"], json!(id));
    assert_eq!(d2["kind"], "buy_get");
    assert_eq!(d2["price"], Value::Null);
    assert_eq!(d2["reward_pool"][0]["menu_item_id"], json!(s.cookie));
    assert_eq!(d2["pool"][0]["size_label"], "Large");
    assert_eq!(d2["windows"], json!([]));

    let (st, _) = call(
        &app,
        "PUT",
        &format!("/deals/{id}/branches/{}", s.branch),
        &tok,
        Some(json!({"is_active": false})),
    )
    .await;
    assert_eq!(st, 204);
    let (st, l) = call(&app, "GET", "/deals", &tok, None).await;
    assert_eq!(st, 200);
    assert_eq!(l.as_array().unwrap().len(), 1);
    assert_eq!(
        l[0]["branch_overrides"],
        json!([{"branch_id": s.branch, "is_active": false}])
    );
    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/deals/{id}/branches/{}", s.branch),
        &tok,
        None,
    )
    .await;
    assert_eq!(st, 204);
    let (_, l) = call(&app, "GET", "/deals", &tok, None).await;
    assert_eq!(l[0]["branch_overrides"], json!([]));

    let (_, l) = call(&app, "GET", "/deals?is_active=false", &tok, None).await;
    assert_eq!(l, json!([]));
    let (st, _) = call(&app, "DELETE", &format!("/deals/{id}"), &tok, None).await;
    assert_eq!(st, 204);
    let (_, l) = call(&app, "GET", "/deals", &tok, None).await;
    assert_eq!(l, json!([]));
    let del: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT deleted_at FROM deal_rules WHERE id = $1::uuid")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(del.is_some(), "soft delete");
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/deals/{id}"),
        &tok,
        Some(two_bites(&s)),
    )
    .await;
    assert_eq!(st, 404);
}

#[sqlx::test]
async fn invalid_deals_name_the_field(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let tok = s.admin_token();
    let cases: Vec<(Value, &str)> = vec![
        (json!({"qty": 1}), "qty"),
        (json!({"price": null}), "price"),
        (json!({"pool": []}), "pool"),
        (json!({"kind": "half_off"}), "kind"),
        (json!({"get_qty": 1}), "get_qty"),
        (
            json!({"pool": [{"menu_item_id": s.croissant, "category_id": s.bakery}]}),
            "pool",
        ),
        (json!({"pool": [{"menu_item_id": s.combo}]}), "pool"),
        (json!({"name": " "}), "name"),
        (json!({"max_per_order": 0}), "max_per_order"),
    ];
    for (patch, field) in cases {
        let mut b = two_bites(&s);
        for (k, v) in patch.as_object().unwrap() {
            b[k] = v.clone();
        }
        let (st, e) = call(&app, "POST", "/deals", &tok, Some(b)).await;
        assert_eq!(
            (st, e["code"].as_str()),
            (400, Some("DEAL_INVALID")),
            "{field}: {e}"
        );
        assert_eq!(e["vars"]["field"], field, "{e}");
    }
    let mut b = two_bites(&s);
    b["kind"] = json!("buy_get");
    b["price"] = Value::Null;
    b["get_qty"] = json!(1);
    let (_, e) = call(&app, "POST", "/deals", &tok, Some(b)).await;
    assert_eq!(e["vars"]["field"], "get_percent");
}

#[sqlx::test]
async fn a_teller_cannot_edit_combos_deals_or_settings(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let tok = s.teller_token();
    let deal: Uuid = sqlx::query_scalar(
        "INSERT INTO deal_rules (org_id, name, kind, qty, price) VALUES ($1, 'd', 'n_for_price', 2, 9000) RETURNING id",
    )
    .bind(s.org).fetch_one(&pool).await.unwrap();
    let meal = json!({"combo_id": s.combo, "slot_id": s.slot_drink});
    let cases: Vec<(&str, String, Option<Value>)> = vec![
        ("POST", "/combos".into(), Some(new_combo(&s, 100))),
        (
            "PUT",
            format!("/combos/{}", s.combo),
            Some(new_combo(&s, 100)),
        ),
        ("PUT", format!("/menu-items/{}/meal", s.latte), Some(meal)),
        ("PUT", "/settings/combos".into(), Some(json!({}))),
        (
            "PUT",
            format!("/settings/combos/branches/{}", s.branch),
            Some(json!({"pos": false})),
        ),
        (
            "DELETE",
            format!("/settings/combos/branches/{}", s.branch),
            None,
        ),
        ("POST", "/deals".into(), Some(two_bites(&s))),
        ("PUT", format!("/deals/{deal}"), Some(two_bites(&s))),
        ("DELETE", format!("/deals/{deal}"), None),
        (
            "PUT",
            format!("/deals/{deal}/branches/{}", s.branch),
            Some(json!({"is_active": false})),
        ),
        (
            "DELETE",
            format!("/deals/{deal}/branches/{}", s.branch),
            None,
        ),
    ];
    for (m, uri, body) in cases {
        let (st, e) = call(&app, m, &uri, &tok, body).await;
        assert_eq!(st, 403, "{m} {uri}: {e}");
    }
    // the branch manager: menu.combos.edit is the owner's by default
    let (st, _) = call(
        &app,
        "POST",
        "/combos",
        &s.manager_token(),
        Some(new_combo(&s, 100)),
    )
    .await;
    assert_eq!(st, 403);
    // reading is menu.items.read
    let (st, _) = call(&app, "GET", "/combos", &tok, None).await;
    assert_eq!(st, 200);
    let (st, _) = call(&app, "GET", "/deals", &tok, None).await;
    assert_eq!(st, 200);
}
