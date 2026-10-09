//! The POS catalogue feed and the old-till shield (COMBOS_CONTRACT.md §2.4).
//!
//! A POS before 0.9.0 builds its menu from `GET /menu-items?full=true` and
//! `/catalog/sync`, so both omit `kind=combo` rows unless the caller can sell
//! them: a browser, the dashboard, a KDS, or `pos/≥0.9.0`. A native client
//! that names no version is treated as old.

mod common;

use actix_web::{App, test, web};
use serde_json::Value;
use sqlx::PgPool;

use common::combos::{Shop, secret, shop};

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .configure(madar_rust::menu::routes::configure),
        )
        .await
    };
}

async fn get<S, B>(app: &S, s: &Shop, uri: &str, client: Option<(&str, &str)>) -> Value
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    let mut req = test::TestRequest::get()
        .uri(uri)
        .insert_header(("Authorization", format!("Bearer {}", s.admin_token())));
    if let Some(h) = client {
        req = req.insert_header(h);
    }
    let resp = test::call_service(app, req.to_request()).await;
    assert!(resp.status().is_success(), "{uri}: {}", resp.status());
    test::read_body_json(resp).await
}

fn rows(v: &Value) -> Vec<Value> {
    if let Some(a) = v.as_array() {
        return a.clone();
    }
    for key in ["data", "items"] {
        if let Some(a) = v[key].as_array() {
            return a.clone();
        }
    }
    panic!("no rows in {v:#}")
}

fn combo_row(v: &Value, s: &Shop) -> Option<Value> {
    rows(v).into_iter().find(|r| r["id"] == s.combo.to_string())
}

#[sqlx::test]
async fn old_tills_never_see_a_combo_and_new_ones_do(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let full = format!(
        "/menu-items?org_id={}&full=true&branch_id={}",
        s.org, s.branch
    );
    let sync = format!("/catalog/sync?branch_id={}", s.branch);
    for uri in [&full, &sync] {
        // No client named, and an old POS: shielded.
        assert!(
            combo_row(&get(&app, &s, uri, None).await, &s).is_none(),
            "{uri} bare"
        );
        let old = get(
            &app,
            &s,
            uri,
            Some(("X-Madar-Client", "pos/0.8.1 (android)")),
        )
        .await;
        assert!(combo_row(&old, &s).is_none(), "{uri} 0.8.1");
        // The plain items are still there.
        assert!(rows(&old).iter().any(|r| r["id"] == s.burger.to_string()));

        // POS 0.9.0 and a browser see it.
        let new = get(
            &app,
            &s,
            uri,
            Some(("X-Madar-Client", "pos/0.9.0 (android)")),
        )
        .await;
        let c = combo_row(&new, &s).unwrap_or_else(|| panic!("{uri} 0.9.0: no combo"));
        assert_eq!(c["kind"], "combo");
        let browser = get(&app, &s, uri, Some(("User-Agent", "Mozilla/5.0"))).await;
        assert!(combo_row(&browser, &s).is_some(), "{uri} browser");
    }
}

#[sqlx::test]
async fn a_full_row_carries_the_combo_and_the_meal_link(pool: PgPool) {
    let s = shop(&pool).await;
    sqlx::query("UPDATE menu_items SET meal_combo_id = $2, meal_slot_id = $3 WHERE id = $1")
        .bind(s.latte)
        .bind(s.combo)
        .bind(s.slot_drink)
        .execute(&pool)
        .await
        .unwrap();
    let app = app!(pool);
    let v = get(
        &app,
        &s,
        &format!(
            "/menu-items?org_id={}&full=true&branch_id={}",
            s.org, s.branch
        ),
        Some(("X-Madar-Client", "pos/0.9.0 (android)")),
    )
    .await;
    let c = combo_row(&v, &s).unwrap();
    let combo = &c["combo"];
    assert_eq!(combo["is_fixed"], false);
    assert_eq!(combo["sell"]["pos"], true);
    let slots = combo["slots"].as_array().unwrap();
    assert_eq!(slots.len(), 3);
    assert_eq!(slots[2]["name"], "Drink");
    // The category choice stays a category: the till expands it itself.
    assert!(
        slots[2]["choices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|ch| ch["category_id"] == s.drinks.to_string())
    );
    let latte = rows(&v)
        .into_iter()
        .find(|r| r["id"] == s.latte.to_string())
        .unwrap();
    assert_eq!(latte["kind"], "item");
    assert_eq!(latte["meal"]["combo_id"], s.combo.to_string());
    assert_eq!(latte["meal"]["slot_id"], s.slot_drink.to_string());
}

/// Arabic slot names reach the till on every feed it builds its menu from:
/// `GET /menu-items?full=true`, `/catalog/sync` and the `/sync/pull`
/// `menu_item` row. A slot with none carries `{}` (the till falls back to
/// `name`).
#[sqlx::test]
async fn every_feed_carries_each_slot_s_arabic_name(pool: PgPool) {
    let s = shop(&pool).await;
    sqlx::query("UPDATE combo_slots SET name_translations = $2 WHERE id = $1")
        .bind(s.slot_drink)
        .bind(serde_json::json!({"ar": "مشروب"}))
        .execute(&pool)
        .await
        .unwrap();
    let app = app!(pool);
    let check = |row: &Value, from: &str| {
        let slots = row["combo"]["slots"]
            .as_array()
            .unwrap_or_else(|| panic!("{from}: {row:#}"));
        let by = |id: uuid::Uuid| {
            slots
                .iter()
                .find(|sl| sl["id"] == id.to_string())
                .unwrap_or_else(|| panic!("{from}: no slot {id}"))
        };
        assert_eq!(by(s.slot_drink)["name"], "Drink", "{from}");
        assert_eq!(
            by(s.slot_drink)["name_translations"]["ar"],
            "مشروب",
            "{from}"
        );
        assert_eq!(
            by(s.slot_main)["name_translations"],
            serde_json::json!({}),
            "{from}"
        );
    };
    for uri in [
        format!(
            "/menu-items?org_id={}&full=true&branch_id={}",
            s.org, s.branch
        ),
        format!("/catalog/sync?branch_id={}", s.branch),
    ] {
        let v = get(
            &app,
            &s,
            &uri,
            Some(("X-Madar-Client", "pos/0.9.0 (android)")),
        )
        .await;
        check(
            &combo_row(&v, &s).unwrap_or_else(|| panic!("{uri}: no combo")),
            &uri,
        );
    }

    let pull = madar_rust::sync::pull::pull_core(
        &pool,
        s.org,
        &madar_rust::sync::pull::PullRequest {
            branch_id: s.branch,
            device_id: None,
            types: Some(vec!["menu_item".into()]),
            limit: None,
            ledger_page_size: None,
            snapshot_cursor: None,
        },
        None,
    )
    .await
    .unwrap();
    let row = pull.data["menu_item"]
        .iter()
        .find(|r| r["id"] == s.combo.to_string())
        .expect("the combo's menu_item row");
    check(row, "/sync/pull");
}
