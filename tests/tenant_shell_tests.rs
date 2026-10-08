//! The tenant shell: `GET /public/tenant-shell`, the HTML nginx serves for a
//! page on a shop's own host (`rue.madar-pos.cloud`).
//!
//! What these pin, in order of how badly it would go wrong:
//!   * the apps keep loading: the template's scripts survive, and anything the
//!     shell cannot do (no template, no database) is a 5xx that nginx answers
//!     with the static file;
//!   * an unknown shop, a switched-off shop and a path no app routes are real
//!     404s, never the app with a 200;
//!   * the page names the shop: title, description, canonical, share tags,
//!     JSON-LD with its menu and actions, and a noscript copy, all escaped;
//!   * a person's own pages (a card, an order, a booking) stay out of search;
//!   * a brand or links edit is on the page at once.

use std::path::PathBuf;

use actix_web::{App, test, web};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

mod common;

use common::shops::{branch, menu_item, pickup, shop};
use madar_rust::tenant_shell::{self, Page, ShellConfig, classify, slug_of_host};

/// The same environment in every test of this binary: shop subdomains ON.
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

/// An app entry as the dashboard builds it: the generic head and noscript
/// between their markers.
fn entry(bundle: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\" dir=\"ltr\">\n<head>\n<meta charset=\"UTF-8\" />\n\
         <!-- madar:head -->\n<title>Madar POS — {bundle}</title>\n\
         <meta name=\"description\" content=\"Generic {bundle}.\" />\n<!-- /madar:head -->\n\
         <script type=\"module\" crossorigin src=\"/assets/{bundle}-abc.js\"></script>\n</head>\n\
         <body>\n<!-- madar:noscript -->\n<noscript><p>Generic {bundle}.</p></noscript>\n\
         <!-- /madar:noscript -->\n<div id=\"root\"></div>\n</body>\n</html>\n"
    )
}

/// A shell directory holding the named bundles' entries (`loyalty`, `order`,
/// `book`), the way the container mounts them.
fn shells(bundles: &[&str]) -> ShellConfig {
    let dir = std::env::temp_dir().join(format!("tenant-shell-{}", Uuid::new_v4()));
    for b in bundles {
        let file = match *b {
            "loyalty" => "loyalty.html",
            "order" => "order.html",
            "book" => "reservations.html",
            other => panic!("no bundle {other}"),
        };
        std::fs::create_dir_all(dir.join(b)).unwrap();
        std::fs::write(dir.join(b).join(file), entry(b)).unwrap();
    }
    ShellConfig { dir }
}

macro_rules! app {
    ($pool:expr, $shells:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new($shells.clone()))
                .configure(tenant_shell::configure),
        )
        .await
    };
}

/// One page, asked for the way nginx asks: host and path as headers.
async fn page<S>(app: &S, host: &str, path: &str) -> (u16, String)
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
            .uri("/public/tenant-shell")
            .insert_header(("X-Shell-Host", host))
            .insert_header(("X-Shell-Path", path))
            .to_request(),
    )
    .await;
    let status = resp.status().as_u16();
    if status == 200 {
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/html; charset=utf-8"
        );
    }
    let body = test::read_body(resp).await;
    (status, String::from_utf8(body.to_vec()).unwrap())
}

/// The page's JSON-LD block, parsed.
fn ld(html: &str) -> Value {
    let open = "<script type=\"application/ld+json\">";
    let a = html.find(open).expect("a JSON-LD block") + open.len();
    let b = a + html[a..].find("</script>").unwrap();
    serde_json::from_str(&html[a..b]).unwrap()
}

#[actix_web::test]
async fn classify_mirrors_the_app_routers() {
    let org = Uuid::new_v4();
    let br = Uuid::new_v4();
    let cases: Vec<(String, Option<Page>)> = vec![
        ("/".into(), Some(Page::Links)),
        ("/?utm_source=ig".into(), Some(Page::Links)),
        ("/rewards".into(), Some(Page::Rewards)),
        ("/rewards/".into(), Some(Page::Rewards)),
        ("/join/MA".into(), Some(Page::Join)),
        (format!("/join/org/{org}"), Some(Page::Join)),
        ("/card/AbC-12_x".into(), Some(Page::Card)),
        ("/order/".into(), Some(Page::Order)),
        ("/order/?branch=x&table=y".into(), Some(Page::Order)),
        ("/order/menu".into(), Some(Page::Menu)),
        ("/order/track/9f2c".into(), Some(Page::Track)),
        ("/order/now/tok_1".into(), Some(Page::OrderAgain)),
        (format!("/order/order/{org}"), Some(Page::Order)),
        (format!("/order/{org}"), Some(Page::Order)),
        (format!("/order/{org}/{br}"), Some(Page::Order)),
        ("/book/".into(), Some(Page::Book)),
        ("/book/manage/tok".into(), Some(Page::Booking)),
        (format!("/book/{org}"), Some(Page::Book)),
        (format!("/book/{org}/{br}"), Some(Page::Book)),
        // Not routed by any app: a real 404.
        ("/wp-admin".into(), None),
        ("/llms.txt".into(), None),
        ("/openapi.json".into(), None),
        ("/order/not-a-uuid".into(), None),
        ("/order/menu/extra".into(), None),
        ("/book/manage/".into(), None),
        ("/card/".into(), None),
        ("/card/a.b".into(), None),
        ("//rewards".into(), None),
        ("rewards".into(), None),
    ];
    for (path, want) in cases {
        assert_eq!(classify(&path), want, "{path}");
    }
}

#[actix_web::test]
async fn only_one_label_under_the_shop_domain_is_a_shop() {
    assert_eq!(
        slug_of_host("drops.madar-pos.cloud").as_deref(),
        Some("drops")
    );
    assert_eq!(
        slug_of_host("Drops.Madar-POS.cloud:443").as_deref(),
        Some("drops")
    );
    assert_eq!(slug_of_host("madar-pos.cloud"), None);
    assert_eq!(slug_of_host("a.b.madar-pos.cloud"), None);
    assert_eq!(slug_of_host("drops.example.com"), None);
    assert_eq!(slug_of_host("dr_ops.madar-pos.cloud"), None);
    assert_eq!(slug_of_host(""), None);
}

/// An entry built before the markers existed still gets the shop's head (its
/// own title removed, so there is one) and noscript.
#[actix_web::test]
async fn an_entry_without_markers_still_gets_the_shop() {
    let old = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Madar — Rewards</title>\
               <script src=\"/assets/a.js\"></script></head><body class=\"x\"><div id=\"root\"></div></body></html>";
    let out = tenant_shell::inject(old, "<title>Shop</title>\n", "<noscript>Shop</noscript>");
    assert_eq!(out.matches("<title>").count(), 1, "{out}");
    assert!(out.contains("<title>Shop</title>\n</head>"), "{out}");
    assert!(
        out.contains("<body class=\"x\">\n<noscript>Shop</noscript><div id=\"root\">"),
        "{out}"
    );
    assert!(out.contains("<script src=\"/assets/a.js\"></script>"));
}

/// The shop's root page: everything a crawler needs, escaped, and the app
/// still loads.
#[sqlx::test]
async fn the_links_page_names_the_shop(pool: PgPool) {
    env();
    let org = shop(&pool, "drops", "Drops \"&\" <Café>").await;
    let maadi = branch(&pool, org, "Maadi").await;
    pickup(&pool, maadi).await;
    menu_item(&pool, org, "Coffee", "Flat white", 8500).await;
    let shells = shells(&["loyalty", "order", "book"]);
    let app = app!(pool, shells);

    let (status, html) = page(&app, "drops.madar-pos.cloud", "/").await;
    assert_eq!(status, 200, "{html}");

    // The app still loads, and the generic head and noscript are gone.
    assert!(
        html.contains(
            "<script type=\"module\" crossorigin src=\"/assets/loyalty-abc.js\"></script>"
        )
    );
    assert!(html.contains("<div id=\"root\"></div>"));
    assert!(!html.contains("Generic loyalty"), "{html}");
    assert_eq!(html.matches("<title>").count(), 1);

    let name = "Drops &quot;&amp;&quot; &lt;Café&gt;";
    assert!(
        html.contains(&format!("<title>{name}: order online, menu</title>")),
        "{html}"
    );
    assert!(html.contains(&format!(
        "<meta name=\"description\" content=\"Order online and see the menu at {name}.\">"
    )));
    assert!(html.contains("<link rel=\"canonical\" href=\"https://drops.madar-pos.cloud/\">"));
    assert!(html.contains("<meta property=\"og:url\" content=\"https://drops.madar-pos.cloud/\">"));
    assert!(html.contains(&format!(
        "<meta property=\"og:site_name\" content=\"{name}\">"
    )));
    assert!(!html.contains("noindex"));
    // No raw markup from the name anywhere outside the JSON-LD.
    assert!(!html.contains("<Café>"), "{html}");

    let ld = ld(&html);
    assert_eq!(ld["@type"], "Restaurant");
    assert_eq!(ld["name"], "Drops \"&\" <Café>");
    assert_eq!(ld["url"], "https://drops.madar-pos.cloud/");
    assert_eq!(ld["telephone"], "+20 100 555 0192");
    assert_eq!(ld["address"]["streetAddress"], "14 Road 9, Maadi");
    let section = &ld["hasMenu"]["hasMenuSection"][0];
    assert_eq!(section["name"], "Coffee");
    assert_eq!(section["hasMenuItem"][0]["name"], "Flat white");
    assert_eq!(section["hasMenuItem"][0]["offers"][0]["price"], "85.00");
    assert_eq!(
        section["hasMenuItem"][0]["offers"][0]["priceCurrency"],
        "EGP"
    );
    assert_eq!(ld["potentialAction"][0]["@type"], "OrderAction");
    assert_eq!(
        ld["potentialAction"][0]["target"]["urlTemplate"],
        "https://drops.madar-pos.cloud/order/"
    );
    assert_eq!(
        ld["potentialAction"].as_array().unwrap().len(),
        1,
        "booking is off: no ReserveAction"
    );

    // The noscript copy: links, menu, branches, and the way back to Madar.
    let ns = &html[html.find("<noscript>").unwrap()..html.find("</noscript>").unwrap()];
    assert!(ns.contains(&format!("<h1>{name}</h1>")), "{ns}");
    assert!(
        ns.contains("<a href=\"https://drops.madar-pos.cloud/order/\">Order online</a>"),
        "{ns}"
    );
    assert!(ns.contains("<li>Flat white — 85.00 EGP</li>"), "{ns}");
    assert!(
        ns.contains("Maadi: 14 Road 9, Maadi · <a href=\"tel:+201005550192\">+20 100 555 0192</a>"),
        "{ns}"
    );
    assert!(ns.contains("<a href=\"https://get.madar-pos.cloud/\">Powered by Madar POS</a>"));
}

/// The menu page is served from the ordering bundle and canonical at its own
/// address; a page that belongs to one person is kept out of search.
#[sqlx::test]
async fn each_page_has_its_own_entry_and_address(pool: PgPool) {
    env();
    let org = shop(&pool, "drops", "Drops").await;
    branch(&pool, org, "Maadi").await;
    menu_item(&pool, org, "Coffee", "Flat white", 8500).await;
    let shells = shells(&["loyalty", "order", "book"]);
    let app = app!(pool, shells);

    let (status, html) = page(&app, "drops.madar-pos.cloud", "/order/menu?branch=nope").await;
    assert_eq!(status, 200, "{html}");
    assert!(
        html.contains("/assets/order-abc.js"),
        "the ordering bundle's entry"
    );
    assert!(html.contains("<title>Drops — Menu</title>"));
    assert!(
        html.contains("<link rel=\"canonical\" href=\"https://drops.madar-pos.cloud/order/menu\">")
    );
    assert!(
        html.contains("<li>Flat white — 85.00 EGP</li>"),
        "an unknown ?branch= falls back to the shop's own"
    );

    let (status, html) = page(&app, "drops.madar-pos.cloud", "/book/").await;
    assert_eq!(status, 200);
    assert!(html.contains("/assets/book-abc.js"));
    assert!(html.contains("<title>Drops — Book a table</title>"));
    assert!(
        !html.contains("Flat white"),
        "the booking page carries no menu"
    );

    for path in [
        "/card/AbC123",
        "/order/track/9f2c",
        "/book/manage/tok",
        "/join/MA",
    ] {
        let (status, html) = page(&app, "drops.madar-pos.cloud", path).await;
        assert_eq!(status, 200, "{path}");
        assert!(
            html.contains("<meta name=\"robots\" content=\"noindex\">"),
            "{path}"
        );
        assert!(!html.contains("rel=\"canonical\""), "{path}");
    }
}

/// The 404s: no such shop (or switched off, the same answer), not a shop host
/// at all, and a path none of the apps routes.
#[sqlx::test]
async fn unknown_shops_and_paths_are_real_404s(pool: PgPool) {
    env();
    let org = shop(&pool, "drops", "Drops").await;
    shop(&pool, "closed", "Closed").await;
    sqlx::query("UPDATE organizations SET is_active = false WHERE slug = 'closed'")
        .execute(&pool)
        .await
        .unwrap();
    branch(&pool, org, "Maadi").await;
    let shells = shells(&["loyalty", "order", "book"]);
    let app = app!(pool, shells);

    for host in [
        "nobody.madar-pos.cloud",
        "closed.madar-pos.cloud",
        "example.com",
        "",
    ] {
        let (status, html) = page(&app, host, "/").await;
        assert_eq!(status, 404, "{host}");
        assert!(
            html.contains("There's no shop at this address."),
            "{host}: {html}"
        );
        assert!(html.contains("<a href=\"https://get.madar-pos.cloud/\">Madar POS</a>"));
        assert!(html.contains("noindex"));
    }

    for path in [
        "/wp-admin",
        "/llms.txt",
        "/order/not-a-uuid",
        "/rewards/extra",
    ] {
        let (status, html) = page(&app, "drops.madar-pos.cloud", path).await;
        assert_eq!(status, 404, "{path}");
        assert!(html.contains("This page isn't here."), "{path}");
        assert!(
            html.contains("<a href=\"https://drops.madar-pos.cloud/\">Back to Drops</a>"),
            "{html}"
        );
    }
}

/// When the entry is not there the answer is a 5xx, which nginx turns into
/// the static file: the shell can only ever add to the page.
#[sqlx::test]
async fn a_missing_entry_hands_the_page_back_to_nginx(pool: PgPool) {
    env();
    let org = shop(&pool, "drops", "Drops").await;
    branch(&pool, org, "Maadi").await;
    let shells = shells(&["loyalty"]);
    let app = app!(pool, shells);

    assert_eq!(page(&app, "drops.madar-pos.cloud", "/").await.0, 200);
    assert_eq!(page(&app, "drops.madar-pos.cloud", "/order/").await.0, 503);
    assert_eq!(page(&app, "drops.madar-pos.cloud", "/book/").await.0, 503);
}

/// A rendered page is reused, and a brand or links edit drops it at once.
#[sqlx::test]
async fn an_edit_is_on_the_page_at_once(pool: PgPool) {
    env();
    let org = shop(&pool, "drops", "Drops").await;
    branch(&pool, org, "Maadi").await;
    let shells = shells(&["loyalty"]);
    let app = app!(pool, shells);

    let (_, html) = page(&app, "drops.madar-pos.cloud", "/").await;
    assert!(html.contains("<title>Drops: menu</title>"), "{html}");

    sqlx::query("UPDATE organizations SET name = 'Drops Coffee' WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO org_links_pages (org_id, tagline_en) VALUES ($1, 'Specialty coffee in Maadi.')",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    let (_, html) = page(&app, "drops.madar-pos.cloud", "/").await;
    assert!(
        html.contains("<title>Drops: menu</title>"),
        "served from the cache"
    );

    tenant_shell::invalidate(org);
    let (_, html) = page(&app, "drops.madar-pos.cloud", "/").await;
    assert!(html.contains("<h1>Drops Coffee</h1>"), "{html}");
    assert!(
        html.contains("<meta name=\"description\" content=\"Specialty coffee in Maadi.\">"),
        "the tagline is the description"
    );
}

/// HEAD is answered like GET (link checkers use it), without a body.
#[sqlx::test]
async fn head_is_answered(pool: PgPool) {
    env();
    let org = shop(&pool, "drops", "Drops").await;
    branch(&pool, org, "Maadi").await;
    let shells = shells(&["loyalty"]);
    let app = app!(pool, shells);

    let resp = test::call_service(
        &app,
        test::TestRequest::default()
            .method(actix_web::http::Method::HEAD)
            .uri("/public/tenant-shell?host=drops.madar-pos.cloud&path=/")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
}

/// The shell reads the template from disk again after a deploy replaces it.
#[sqlx::test]
async fn a_new_entry_is_picked_up(pool: PgPool) {
    env();
    let org = shop(&pool, "drops", "Drops").await;
    branch(&pool, org, "Maadi").await;
    let shells = shells(&["loyalty"]);
    let app = app!(pool, shells);

    let (_, html) = page(&app, "drops.madar-pos.cloud", "/").await;
    assert!(html.contains("/assets/loyalty-abc.js"));

    let file: PathBuf = shells.dir.join("loyalty/loyalty.html");
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(
        &file,
        entry("loyalty").replace("loyalty-abc.js", "loyalty-def.js"),
    )
    .unwrap();
    let (_, html) = page(&app, "drops.madar-pos.cloud", "/").await;
    assert!(html.contains("/assets/loyalty-def.js"), "{html}");
}
