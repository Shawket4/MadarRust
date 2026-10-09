//! The shop's links page: `GET/PUT /orgs/{id}/links-page` and the public
//! `GET /public/orgs/links`.
//!
//! What these pin, in order of how badly it would go wrong:
//!   * a module shows only when the switch that ALREADY governs it is on
//!     (ordering channels, online booking, the org's loyalty programme, any
//!     active branch for the menu) — the page can hide, never invent;
//!   * the buttons open the same addresses the QR codes do;
//!   * what the editor saves comes back on the public page, and the social
//!     links it saves land in `organizations.social_links`, not a copy;
//!   * the closed vocabulary and https rule hold on the way in;
//!   * a switched-off shop is a 404 identical to no shop.

use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::JwtSecret;
use madar_rust::models::UserRole;
use madar_rust::orgs::routes;

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn token(uid: Uuid, org: Uuid, role: UserRole) -> String {
    madar_rust::auth::jwt::create_token(&secret(), uid, Some(org), role, None, 24).unwrap()
}

/// The same environment in every test of this binary (tests may share a
/// process): shop subdomains ON, generic hosts configured.
fn env() {
    unsafe {
        std::env::set_var("PUBLIC_SHOP_SUBDOMAINS", "1");
        std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://order.madar-pos.cloud");
        std::env::set_var("PUBLIC_LOYALTY_BASE_URL", "https://loyalty.madar-pos.cloud");
        std::env::set_var(
            "PUBLIC_RESERVATIONS_BASE_URL",
            "https://reservations.madar-pos.cloud",
        );
    }
}

async fn shop(pool: &PgPool, slug: &str, branded: bool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, is_active, custom_branding) \
         VALUES ($1, $2, $3, true, $4)",
    )
    .bind(id)
    .bind(format!("Shop {slug}"))
    .bind(slug)
    .bind(branded)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn branch(pool: &PgPool, org: Uuid, name: &str, code: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO branches (id, org_id, name, code, address, phone, latitude, longitude) \
         VALUES ($1, $2, $3, $4, '14 Road 9, Maadi', '+20 100 555 0192', 29.96, 31.25)",
    )
    .bind(id)
    .bind(org)
    .bind(name)
    .bind(code)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn user(pool: &PgPool, org: Uuid, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, $3, $4, 'h', $5::user_role)",
    )
    .bind(id)
    .bind(org)
    .bind(format!("{role}-{id}"))
    .bind(format!("{id}@t.com"))
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn admin_token(pool: &PgPool, org: Uuid) -> String {
    token(user(pool, org, "org_admin").await, org, UserRole::OrgAdmin)
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .configure(routes::configure),
        )
        .await
    };
}

async fn public_page<S>(app: &S, query: &str) -> (u16, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let resp = test::call_service(
        app,
        test::TestRequest::get()
            .uri(&format!("/public/orgs/links?{query}"))
            .to_request(),
    )
    .await;
    let status = resp.status().as_u16();
    let body = test::read_body(resp).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn kinds(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["kind"].as_str().unwrap().to_string())
        .collect()
}

/// A shop that has never opened the editor still has a page, and it shows
/// only what is actually switched on: with one branch and nothing else set
/// up, that is the read-only menu.
#[sqlx::test]
async fn a_shop_with_no_settings_shows_what_is_switched_on(pool: PgPool) {
    env();
    let org = shop(&pool, "drops", true).await;
    branch(&pool, org, "Maadi", "MA").await;
    let app = app!(pool);

    let (status, page) = public_page(&app, "slug=drops").await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(
        kinds(&page),
        vec!["menu"],
        "ordering, rewards and booking are all off"
    );
    assert_eq!(
        page["items"][0]["href"],
        "https://drops.madar-pos.cloud/order/menu"
    );
    assert_eq!(page["items"][0]["path"], "/order/menu");
    assert_eq!(page["brand"]["name"], "Shop drops");
    assert_eq!(page["branches"].as_array().unwrap().len(), 1);
    assert!(
        page["branches"][0]["directions_url"]
            .as_str()
            .unwrap()
            .contains("29.96,31.25"),
        "no Maps link saved: Directions searches the coordinates"
    );
}

/// Each module follows the switch that already exists for it.
#[sqlx::test]
async fn modules_follow_the_switches_that_already_exist(pool: PgPool) {
    env();
    let org = shop(&pool, "drops", true).await;
    let maadi = branch(&pool, org, "Maadi", "MA").await;
    let zamalek = branch(&pool, org, "Zamalek", "ZA").await;
    sqlx::query(
        "INSERT INTO branch_delivery_settings (branch_id, pickup_enabled) VALUES ($1, true)",
    )
    .bind(maadi)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO branch_booking_settings (branch_id, org_id, enabled) VALUES ($1, $2, true)",
    )
    .bind(zamalek)
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO loyalty_settings (org_id, enabled, mode) VALUES ($1, true, 'visits')")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    let app = app!(pool);

    let (_, page) = public_page(&app, "slug=drops").await;
    assert_eq!(kinds(&page), vec!["order", "menu", "rewards", "book"]);
    let items = page["items"].as_array().unwrap();
    assert_eq!(items[0]["href"], "https://drops.madar-pos.cloud/order/");
    assert_eq!(items[0]["branch_names"], json!(["Maadi"]));
    assert_eq!(items[0]["channels"], json!(["pickup"]));
    // Rewards moved off the root, which is this page now.
    assert_eq!(items[2]["href"], "https://drops.madar-pos.cloud/rewards");
    assert_eq!(items[3]["href"], "https://drops.madar-pos.cloud/book/");
    assert_eq!(items[3]["branch_names"], json!(["Zamalek"]));
    assert_eq!(page["loyalty_mode"], "visits");
}

/// Off the branding tier there is no own host: the buttons open the generic
/// hosts, built exactly as the QR codes build them, and the colours are
/// Madar's.
#[sqlx::test]
async fn an_unbranded_shop_links_to_the_generic_hosts(pool: PgPool) {
    env();
    let org = shop(&pool, "plain", false).await;
    branch(&pool, org, "Main", "MN").await;
    let app = app!(pool);

    let (status, page) = public_page(&app, &format!("org_id={org}")).await;
    assert_eq!(status, 200);
    assert_eq!(
        page["items"][0]["href"],
        format!("https://order.madar-pos.cloud/order/{org}?preview=1")
    );
    assert_eq!(page["brand"]["custom_branding"], false);
    assert_eq!(page["brand"]["background_color"], "#0D6273");

    // And the page itself has no address to hand out: it lives at the root of
    // a shop's own host, and this shop has none.
    let tok = admin_token(&pool, org).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/orgs/{org}/links-page"))
            .insert_header(("Authorization", format!("Bearer {tok}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let got: Value = test::read_body_json(resp).await;
    assert!(got["public_url"].is_null(), "{got}");
}

/// What the editor saves is what the public page shows — and the socials go
/// to the organisation's own column, where the wallet pass reads them too.
#[sqlx::test]
async fn saving_the_editor_shapes_the_public_page(pool: PgPool) {
    env();
    let org = shop(&pool, "drops", true).await;
    let maadi = branch(&pool, org, "Maadi", "MA").await;
    let zamalek = branch(&pool, org, "Zamalek", "ZA").await;
    sqlx::query("INSERT INTO loyalty_settings (org_id, enabled, mode) VALUES ($1, true, 'points')")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    let tok = admin_token(&pool, org).await;
    let app = app!(pool);

    let body = json!({
        "items": [
            {"kind": "rewards", "visible": true},
            {"kind": "custom", "visible": true, "title_en": "Beans to brew at home",
             "title_ar": "بُن للتحضير في البيت", "url": "https://dropscoffee.shop"},
            {"kind": "custom", "visible": false, "title_en": "Ramadan catering",
             "url": "https://forms.gle/drops"},
            {"kind": "menu", "visible": false},
            {"kind": "order", "visible": true},
            {"kind": "book", "visible": true}
        ],
        "tagline_en": "  Specialty coffee in Maadi. ",
        "tagline_ar": "قهوة مختصة",
        "show_cover": true,
        "show_branches": true,
        "branches": [
            {"branch_id": maadi, "visible": true, "maps_url": "https://maps.app.goo.gl/q8Rd2k"},
            {"branch_id": zamalek, "visible": false}
        ],
        "social_links": {"instagram": "https://instagram.com/drops", "tiktok": ""}
    });
    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!("/orgs/{org}/links-page"))
            .insert_header(("Authorization", format!("Bearer {tok}")))
            .set_json(&body)
            .to_request(),
    )
    .await;
    let status = resp.status().as_u16();
    let saved: Value = test::read_body_json(resp).await;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(saved["tagline_en"], "Specialty coffee in Maadi.");
    assert_eq!(saved["public_url"], "https://drops.madar-pos.cloud/");
    assert!(
        saved["items"][1]["id"].is_string(),
        "a custom link is given an id"
    );

    // Stored on the org, not copied.
    let social: Value = sqlx::query_scalar("SELECT social_links FROM organizations WHERE id = $1")
        .bind(org)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(social, json!({"instagram": "https://instagram.com/drops"}));

    let (_, page) = public_page(&app, "slug=drops").await;
    // Hidden custom link and hidden menu are gone; ordering and booking are
    // off at every branch, so they are not shown whatever the list says.
    assert_eq!(kinds(&page), vec!["rewards", "custom"]);
    assert_eq!(page["items"][1]["title_ar"], "بُن للتحضير في البيت");
    assert_eq!(page["items"][1]["href"], "https://dropscoffee.shop");
    assert_eq!(page["tagline_ar"], "قهوة مختصة");
    assert_eq!(page["socials"][0]["key"], "instagram");
    let branches = page["branches"].as_array().unwrap();
    assert_eq!(branches.len(), 1, "Zamalek is hidden");
    assert_eq!(
        branches[0]["directions_url"],
        "https://maps.app.goo.gl/q8Rd2k"
    );

    // And the editor reads it back.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/orgs/{org}/links-page"))
            .insert_header(("Authorization", format!("Bearer {tok}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let got: Value = test::read_body_json(resp).await;
    assert_eq!(got["items"].as_array().unwrap().len(), 6);
    let zam = got["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == "Zamalek")
        .unwrap();
    assert_eq!(zam["visible"], false);
    let rewards = got["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["kind"] == "rewards")
        .unwrap();
    assert_eq!(rewards["available"], true);
    assert_eq!(rewards["path"], "/rewards");
}

/// The page is a public, printed surface: the closed vocabulary and the https
/// rule hold on the way in.
#[sqlx::test]
async fn what_the_editor_may_save(pool: PgPool) {
    env();
    let org = shop(&pool, "drops", true).await;
    let other = shop(&pool, "other", true).await;
    let foreign_branch = branch(&pool, other, "Elsewhere", "EL").await;
    let tok = admin_token(&pool, org).await;
    let app = app!(pool);

    for (why, body) in [
        (
            "plaintext link",
            json!({"items": [{"kind": "custom", "title_en": "X", "url": "http://x.example"}],
                   "show_cover": true, "show_branches": true}),
        ),
        (
            "script link",
            json!({"items": [{"kind": "custom", "title_en": "X", "url": "javascript:alert(1)"}],
                   "show_cover": true, "show_branches": true}),
        ),
        (
            "untitled link",
            json!({"items": [{"kind": "custom", "title_en": " ", "url": "https://x.example"}],
                   "show_cover": true, "show_branches": true}),
        ),
        (
            "unknown kind",
            json!({"items": [{"kind": "myspace"}], "show_cover": true, "show_branches": true}),
        ),
        (
            "another shop's branch",
            json!({"items": [], "show_cover": true, "show_branches": true,
                   "branches": [{"branch_id": foreign_branch, "visible": false}]}),
        ),
        (
            "unknown social platform",
            json!({"items": [], "show_cover": true, "show_branches": true,
                   "social_links": {"myspace": "https://myspace.com/x"}}),
        ),
    ] {
        let resp = test::call_service(
            &app,
            test::TestRequest::put()
                .uri(&format!("/orgs/{org}/links-page"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .set_json(&body)
                .to_request(),
        )
        .await;
        assert_eq!(resp.status().as_u16(), 400, "{why} must be refused");
    }
}

/// Editing is the org settings capability; another shop cannot even read it.
#[sqlx::test]
async fn who_may_edit(pool: PgPool) {
    env();
    let org = shop(&pool, "drops", true).await;
    let other = shop(&pool, "other", true).await;
    let teller = token(user(&pool, org, "teller").await, org, UserRole::Teller);
    let stranger = admin_token(&pool, other).await;
    let app = app!(pool);

    let put = |tok: String| {
        test::TestRequest::put()
            .uri(&format!("/orgs/{org}/links-page"))
            .insert_header(("Authorization", format!("Bearer {tok}")))
            .set_json(json!({"items": [], "show_cover": true, "show_branches": true}))
            .to_request()
    };
    let resp = test::call_service(&app, put(teller)).await;
    assert_eq!(
        resp.status().as_u16(),
        403,
        "a teller does not edit the shop's page"
    );
    let resp = test::call_service(&app, put(stranger.clone())).await;
    assert_eq!(resp.status().as_u16(), 403);
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/orgs/{org}/links-page"))
            .insert_header(("Authorization", format!("Bearer {stranger}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 403, "another shop cannot read it");
}

/// A switched-off shop and a shop that never existed answer identically, as
/// `/public/orgs/brand` does — a wildcard host answers every name anyone types.
#[sqlx::test]
async fn a_switched_off_shop_has_no_page(pool: PgPool) {
    env();
    let org = shop(&pool, "closed", true).await;
    sqlx::query("UPDATE organizations SET is_active = false WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    let app = app!(pool);

    let (a, body_a) = public_page(&app, "slug=closed").await;
    let (b, body_b) = public_page(&app, "slug=never-existed-xyz").await;
    assert_eq!((a, b), (404, 404));
    assert_eq!(body_a, body_b);
    let (c, _) = public_page(&app, &format!("org_id={org}")).await;
    assert_eq!(c, 404, "by id too");
}
