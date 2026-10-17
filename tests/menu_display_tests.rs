//! The menu without prices: `GET/PUT /orgs/{id}/links-page/menu-display`,
//! `GET /public/menu-display` and its code (`/orgs/{id}/links-qr?target=`).
//!
//! What these pin, in order of how badly it would go wrong:
//!   * no price, cost, surcharge, discount or deal key anywhere in the public
//!     answer — checked recursively over the whole JSON, on a menu that HAS
//!     sizes, add-ons, optionals and a combo with priced choices;
//!   * off is a 404, the same as no shop;
//!   * an item switched off at the branch, or inactive, is not on it;
//!   * a Free shop can switch it on and open it, and still cannot touch the
//!     paid links page;
//!   * turning it on changes neither links-page GET by a single byte.

mod common;

use std::sync::Arc;

use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use common::combos::{Shop, secret, shop};
use madar_rust::qr_card::shlink::ShortLinkProvider;
use madar_rust::qr_card::shlink::fake::FakeShortLinkProvider;

fn env() {
    unsafe {
        std::env::set_var("MADAR_DISABLE_RATE_LIMIT", "1");
        std::env::set_var("PUBLIC_SHOP_SUBDOMAINS", "1");
        std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://order.madar-pos.cloud");
        std::env::set_var("PUBLIC_LOYALTY_BASE_URL", "https://loyalty.madar-pos.cloud");
        std::env::set_var(
            "PUBLIC_RESERVATIONS_BASE_URL",
            "https://reservations.madar-pos.cloud",
        );
    }
}

macro_rules! app {
    ($pool:expr) => {{
        env();
        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(fake))
                .configure(madar_rust::orgs::routes::configure),
        )
        .await
    }};
}

async fn call<S>(app: &S, req: test::TestRequest) -> (u16, Vec<u8>)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let resp = test::call_service(app, req.to_request()).await;
    let status = resp.status().as_u16();
    (status, test::read_body(resp).await.to_vec())
}

fn json_of(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or(Value::Null)
}

fn get(uri: &str, token: Option<&str>) -> test::TestRequest {
    let r = test::TestRequest::get().uri(uri);
    match token {
        Some(t) => r.insert_header(("Authorization", format!("Bearer {t}"))),
        None => r,
    }
}

fn put(uri: &str, token: &str, body: Value) -> test::TestRequest {
    test::TestRequest::put()
        .uri(uri)
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(body)
}

async fn set_plan(pool: &PgPool, org: Uuid, plan: &str) {
    sqlx::query("UPDATE organizations SET plan = $2 WHERE id = $1")
        .bind(org)
        .bind(plan)
        .execute(pool)
        .await
        .unwrap();
}

async fn switch_on<S>(app: &S, s: &Shop, body: Value) -> Value
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let (status, b) = call(
        app,
        put(
            &format!("/orgs/{}/links-page/menu-display", s.org),
            &s.admin_token(),
            body,
        ),
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&b));
    json_of(&b)
}

/// Every key, at every depth, that could carry a figure.
fn money_keys(v: &Value, path: &str, out: &mut Vec<String>) {
    const WORDS: [&str; 10] = [
        "price",
        "cost",
        "surcharge",
        "extra",
        "discount",
        "deal",
        "fee",
        "total",
        "amount",
        "value",
    ];
    match v {
        Value::Object(m) => {
            for (k, child) in m {
                let lower = k.to_ascii_lowercase();
                // `pricing` / `option_pricing` hold no "price": check the stem.
                if WORDS.iter().any(|w| lower.contains(w)) || lower.contains("pricing") {
                    out.push(format!("{path}.{k}"));
                }
                money_keys(child, &format!("{path}.{k}"), out);
            }
        }
        Value::Array(a) => {
            for (i, child) in a.iter().enumerate() {
                money_keys(child, &format!("{path}[{i}]"), out);
            }
        }
        _ => {}
    }
}

fn item_names(page: &Value) -> Vec<String> {
    page["menu"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap().to_string())
        .collect()
}

/// A shop that never switched it on has no page — the same 404 as a shop that
/// does not exist — and switching it off again takes it away.
#[sqlx::test]
async fn off_is_a_404(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);

    let (status, _) = call(
        &app,
        get(&format!("/public/menu-display?org_id={}", s.org), None),
    )
    .await;
    assert_eq!(status, 404, "never switched on");
    let (status, _) = call(
        &app,
        get(
            &format!("/public/menu-display?org_id={}", Uuid::new_v4()),
            None,
        ),
    )
    .await;
    assert_eq!(status, 404, "no such shop");

    switch_on(&app, &s, json!({"enabled": true})).await;
    let (status, _) = call(
        &app,
        get(&format!("/public/menu-display?org_id={}", s.org), None),
    )
    .await;
    assert_eq!(status, 200);

    switch_on(&app, &s, json!({"enabled": false})).await;
    let (status, _) = call(
        &app,
        get(&format!("/public/menu-display?org_id={}", s.org), None),
    )
    .await;
    assert_eq!(status, 404, "switched off again");

    // A shop that is switched off altogether, with the page on.
    switch_on(&app, &s, json!({"enabled": true})).await;
    sqlx::query("UPDATE organizations SET is_active = false WHERE id = $1")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let (status, _) = call(
        &app,
        get(&format!("/public/menu-display?org_id={}", s.org), None),
    )
    .await;
    assert_eq!(status, 404, "an inactive shop");
}

/// The menu, with everything the configurator needs — sizes, add-ons, the
/// combo's slots — and not one key that could hold a figure.
#[sqlx::test]
async fn on_is_the_menu_with_no_price_anywhere(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    switch_on(
        &app,
        &s,
        json!({"enabled": true, "title_en": "Our menu", "title_ar": "قائمتنا"}),
    )
    .await;

    let (status, body) = call(
        &app,
        get(&format!("/public/menu-display?org_id={}", s.org), None),
    )
    .await;
    assert_eq!(status, 200);
    let page = json_of(&body);

    let mut found = Vec::new();
    money_keys(&page, "$", &mut found);
    assert!(found.is_empty(), "money-shaped keys on the wire: {found:?}");

    // And it is the real menu.
    assert_eq!(page["title_en"], "Our menu");
    assert_eq!(page["title_ar"], "قائمتنا");
    assert_eq!(page["brand"]["name"], "Combo Org");
    assert_eq!(
        page["branch"]["id"],
        s.branch.to_string(),
        "the first branch by default"
    );
    let names = item_names(&page);
    for n in [
        "Burger",
        "Fries",
        "Latte",
        "Cola",
        "Croissant",
        "Cookie",
        "Lunch deal",
    ] {
        assert!(names.contains(&n.to_string()), "{n} in {names:?}");
    }
    let latte = page["menu"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["name"] == "Latte")
        .unwrap();
    let sizes: Vec<&str> = latte["sizes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|z| z["label"].as_str().unwrap())
        .collect();
    assert_eq!(sizes, vec!["Large", "Regular"]);
    assert_eq!(
        latte["sizes"][0].as_object().unwrap().len(),
        1,
        "a size is its label"
    );
    let addons = page["menu"]["addons"].as_array().unwrap();
    assert!(
        addons.iter().any(|a| a["name"] == "Oat milk"),
        "the add-on catalogue, by name: {addons:?}"
    );
    let combo = page["menu"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["kind"] == "combo")
        .expect("the combo is on the menu");
    let slots = combo["combo"]["slots"].as_array().unwrap();
    assert_eq!(slots.len(), 3);
    assert!(
        slots
            .iter()
            .all(|sl| !sl["choices"].as_array().unwrap().is_empty())
    );

    // The raw text too: no currency anywhere.
    let text = String::from_utf8(body).unwrap();
    assert!(!text.contains("EGP"), "{text}");
}

/// An item switched off at the page's branch, or inactive, is not on it; the
/// branch the shop picks is the one whose availability counts.
#[sqlx::test]
async fn unavailable_and_inactive_items_are_hidden(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    switch_on(&app, &s, json!({"enabled": true})).await;

    // Cola: off at the first branch (its sizeless sentinel size).
    sqlx::query(
        "INSERT INTO menu_price_overrides (scope, branch_id, target_type, target_id, is_available) \
         VALUES ('branch', $1, 'menu_item_size', (md5($2::text || ':one_size'))::uuid, false)",
    )
    .bind(s.branch)
    .bind(s.cola)
    .execute(&pool)
    .await
    .unwrap();
    // Cookie: inactive everywhere.
    sqlx::query("UPDATE menu_items SET is_active = false WHERE id = $1")
        .bind(s.cookie)
        .execute(&pool)
        .await
        .unwrap();

    let (_, body) = call(
        &app,
        get(&format!("/public/menu-display?org_id={}", s.org), None),
    )
    .await;
    let names = item_names(&json_of(&body));
    assert!(!names.contains(&"Cola".to_string()), "{names:?}");
    assert!(!names.contains(&"Cookie".to_string()), "{names:?}");
    assert!(names.contains(&"Burger".to_string()));

    // At the second branch Cola is on.
    switch_on(&app, &s, json!({"enabled": true, "branch_id": s.branch2})).await;
    let (_, body) = call(
        &app,
        get(&format!("/public/menu-display?org_id={}", s.org), None),
    )
    .await;
    let page = json_of(&body);
    assert_eq!(page["branch"]["id"], s.branch2.to_string());
    let names = item_names(&page);
    assert!(names.contains(&"Cola".to_string()), "{names:?}");
    assert!(
        !names.contains(&"Cookie".to_string()),
        "inactive is inactive everywhere"
    );
}

/// What the editor saves comes back, and what it may not save is refused.
#[sqlx::test]
async fn settings_round_trip(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let uri = format!("/orgs/{}/links-page/menu-display", s.org);

    let (status, body) = call(&app, get(&uri, Some(&s.admin_token()))).await;
    assert_eq!(status, 200);
    let v = json_of(&body);
    assert_eq!(v["enabled"], false, "off until switched on");
    assert_eq!(v["branch_id"], Value::Null);
    assert_eq!(v["effective_branch_id"], s.branch.to_string());
    assert_eq!(v["branches"].as_array().unwrap().len(), 2);
    assert_eq!(
        v["public_url"],
        format!("https://loyalty.madar-pos.cloud/menu/{}", s.org),
        "no own host: the generic host, the shop named in the path"
    );

    let saved = switch_on(
        &app,
        &s,
        json!({"enabled": true, "title_en": "  Menu  ", "title_ar": "", "branch_id": s.branch2}),
    )
    .await;
    assert_eq!(saved["enabled"], true);
    assert_eq!(saved["title_en"], "Menu", "trimmed");
    assert_eq!(saved["title_ar"], Value::Null, "blank is none");
    assert_eq!(saved["branch_id"], s.branch2.to_string());
    assert_eq!(saved["effective_branch_id"], s.branch2.to_string());

    let (_, body) = call(&app, get(&uri, Some(&s.admin_token()))).await;
    assert_eq!(json_of(&body), saved, "GET reads back what PUT saved");

    // A deleted branch falls back to the first one.
    sqlx::query("UPDATE branches SET deleted_at = now() WHERE id = $1")
        .bind(s.branch2)
        .execute(&pool)
        .await
        .unwrap();
    let (_, body) = call(&app, get(&uri, Some(&s.admin_token()))).await;
    assert_eq!(json_of(&body)["effective_branch_id"], s.branch.to_string());

    // Refused: someone else's branch, a title too long.
    let other = shop(&pool).await;
    let (status, _) = call(
        &app,
        put(
            &uri,
            &s.admin_token(),
            json!({"enabled": true, "branch_id": other.branch}),
        ),
    )
    .await;
    assert_eq!(status, 400);
    let (status, _) = call(
        &app,
        put(
            &uri,
            &s.admin_token(),
            json!({"enabled": true, "title_en": "x".repeat(61)}),
        ),
    )
    .await;
    assert_eq!(status, 400);

    // Another shop's token.
    let (status, _) = call(&app, get(&uri, Some(&other.admin_token()))).await;
    assert_eq!(status, 403);

    // A shop on its own host gets `/menu` there.
    sqlx::query("UPDATE organizations SET custom_branding = true, slug = 'drops' WHERE id = $1")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let (_, body) = call(&app, get(&uri, Some(&s.admin_token()))).await;
    assert_eq!(
        json_of(&body)["public_url"],
        "https://drops.madar-pos.cloud/menu"
    );
    let (status, _) = call(&app, get("/public/menu-display?slug=drops", None)).await;
    assert_eq!(status, 200, "by slug, as the shop's own host asks");
}

/// A Free shop switches it on, its customers open it, and it gets its code —
/// while the paid links page stays shut to it, writes included.
#[sqlx::test]
async fn a_free_shop_has_it(pool: PgPool) {
    let s = shop(&pool).await;
    set_plan(&pool, s.org, "free").await;
    let app = app!(pool);

    let saved = switch_on(&app, &s, json!({"enabled": true, "title_en": "Menu"})).await;
    assert_eq!(saved["enabled"], true);
    let (status, body) = call(
        &app,
        get(&format!("/public/menu-display?org_id={}", s.org), None),
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert!(!item_names(&json_of(&body)).is_empty());

    let (status, body) = call(
        &app,
        get(
            &format!("/orgs/{}/links-qr?target=menu_display&card=false", s.org),
            Some(&s.admin_token()),
        ),
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let qr = json_of(&body);
    assert_eq!(qr["kind"], "org_menu_display");
    assert_eq!(
        qr["long_url"],
        format!("https://loyalty.madar-pos.cloud/menu/{}", s.org)
    );
    assert!(
        qr["qr_data_url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png")
    );

    // The links page itself is still the paid feature: neither read, nor
    // written, nor its code.
    let links = format!("/orgs/{}/links-page", s.org);
    let (status, _) = call(&app, get(&links, Some(&s.admin_token()))).await;
    assert_eq!(status, 403);
    let (status, _) = call(
        &app,
        put(
            &links,
            &s.admin_token(),
            json!({"items": [], "tagline_en": "Paid only", "show_cover": false}),
        ),
    )
    .await;
    assert_eq!(status, 403);
    let tagline: Option<String> =
        sqlx::query_scalar("SELECT tagline_en FROM org_links_pages WHERE org_id = $1")
            .bind(s.org)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(tagline, None, "the refused PUT wrote nothing");
    let (status, _) = call(
        &app,
        get(
            &format!("/orgs/{}/links-qr?card=false", s.org),
            Some(&s.admin_token()),
        ),
    )
    .await;
    assert_eq!(status, 403, "the links page's own code stays paid");
}

/// Turning the page on (or saving it at all) changes neither links-page
/// answer by a single byte: it is not one of the page's buttons.
#[sqlx::test]
async fn the_links_page_is_byte_identical(pool: PgPool) {
    let s = shop(&pool).await;
    sqlx::query("UPDATE organizations SET custom_branding = true, slug = 'drops' WHERE id = $1")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let app = app!(pool);
    let editor = format!("/orgs/{}/links-page", s.org);

    let (status, public_before) = call(&app, get("/public/orgs/links?slug=drops", None)).await;
    assert_eq!(status, 200);
    let (status, editor_before) = call(&app, get(&editor, Some(&s.admin_token()))).await;
    assert_eq!(status, 200);

    switch_on(
        &app,
        &s,
        json!({"enabled": true, "title_en": "Menu", "branch_id": s.branch2}),
    )
    .await;

    let (_, public_after) = call(&app, get("/public/orgs/links?slug=drops", None)).await;
    let (_, editor_after) = call(&app, get(&editor, Some(&s.admin_token()))).await;
    assert_eq!(
        String::from_utf8_lossy(&public_after),
        String::from_utf8_lossy(&public_before)
    );
    assert_eq!(
        String::from_utf8_lossy(&editor_after),
        String::from_utf8_lossy(&editor_before)
    );

    // And a links-page save leaves the menu settings alone.
    let (status, _) = call(
        &app,
        put(
            &editor,
            &s.admin_token(),
            json!({"items": [], "tagline_en": "Hi"}),
        ),
    )
    .await;
    assert_eq!(status, 200);
    let (_, body) = call(
        &app,
        get(
            &format!("/orgs/{}/links-page/menu-display", s.org),
            Some(&s.admin_token()),
        ),
    )
    .await;
    let v = json_of(&body);
    assert_eq!(v["enabled"], true);
    assert_eq!(v["title_en"], "Menu");
    assert_eq!(v["branch_id"], s.branch2.to_string());
}

/// A paid shop's links-page code is unchanged by the new target.
#[sqlx::test]
async fn the_links_code_is_unchanged(pool: PgPool) {
    let s = shop(&pool).await;
    sqlx::query("UPDATE organizations SET custom_branding = true, slug = 'drops' WHERE id = $1")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let app = app!(pool);
    let (status, body) = call(
        &app,
        get(
            &format!("/orgs/{}/links-qr?card=false", s.org),
            Some(&s.admin_token()),
        ),
    )
    .await;
    assert_eq!(status, 200);
    let qr = json_of(&body);
    assert_eq!(qr["kind"], "org_links");
    assert_eq!(qr["long_url"], "https://drops.madar-pos.cloud/");

    let (status, _) = call(
        &app,
        get(
            &format!("/orgs/{}/links-qr?target=nope", s.org),
            Some(&s.admin_token()),
        ),
    )
    .await;
    assert_eq!(status, 400);
}
