//! The public menus (`/public/menu-display`, the dine-in table menu
//! `/public/tables/{id}/menu`) are served from the menu cache, and an edit is
//! on the very next request — whatever wrote it. Every write below is plain
//! SQL, as another instance's handler (or any write path) would leave the
//! database: the cache's key is read from the database on every request, so
//! nothing here relies on this process having seen the write.
//!
//! Also pinned: the cached body is the uncached body byte for byte, it carries
//! an ETag and `Cache-Control: public, no-cache`, a matching `If-None-Match`
//! is a 304, and the menu without prices still has no price key when it comes
//! from the cache.

mod common;

use std::time::Duration;

use actix_web::{App, test, web};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use common::combos::{Shop, secret, shop};
use madar_rust::menu::cache::{MenuCache, public_fingerprint};

fn env() {
    unsafe {
        std::env::set_var("MADAR_DISABLE_RATE_LIMIT", "1");
    }
}

fn cache() -> web::Data<MenuCache> {
    let c = MenuCache::with_ttl(Duration::from_secs(3600));
    c.assume_prod_shape();
    web::Data::new(c)
}

macro_rules! app {
    ($pool:expr, $cache:expr) => {{
        env();
        let mut a = App::new()
            .app_data(web::Data::new($pool.clone()))
            .app_data(web::Data::new(secret()))
            .app_data(web::Data::new(
                madar_rust::realtime::hub::BranchEventHub::new(),
            ));
        if let Some(c) = $cache {
            a = a.app_data(c);
        }
        test::init_service(
            a.configure(madar_rust::orgs::routes::configure)
                .configure(madar_rust::tickets::routes::configure),
        )
        .await
    }};
}

struct Got {
    status: u16,
    etag: Option<String>,
    cache_control: Option<String>,
    body: Vec<u8>,
}

impl Got {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

async fn get<S>(app: &S, uri: &str, inm: Option<&str>) -> Got
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let mut req = test::TestRequest::get().uri(uri);
    if let Some(t) = inm {
        req = req.insert_header(("If-None-Match", t));
    }
    let resp = test::call_service(app, req.to_request()).await;
    let h = |n: &str| {
        resp.headers()
            .get(n)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let (etag, cache_control) = (h("etag"), h("cache-control"));
    let status = resp.status().as_u16();
    Got {
        status,
        etag,
        cache_control,
        body: test::read_body(resp).await.to_vec(),
    }
}

async fn exec(pool: &PgPool, sql: &str, ids: &[Uuid]) {
    let mut q = sqlx::query(sql);
    for id in ids {
        q = q.bind(*id);
    }
    q.execute(pool).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
}

async fn table(pool: &PgPool, s: &Shop) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO branch_tables (org_id, branch_id, label) VALUES ($1, $2, 'T1') RETURNING id",
    )
    .bind(s.org)
    .bind(s.branch)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn page_on(pool: &PgPool, s: &Shop) {
    exec(
        pool,
        "INSERT INTO org_links_pages (org_id, menu_display_enabled, menu_display_title_en) \
         VALUES ($1, true, 'Our menu')",
        &[s.org],
    )
    .await;
}

fn names(items: &Value) -> Vec<String> {
    items
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap().to_string())
        .collect()
}

/// Every key, at every depth, that could carry a figure (menu_display_tests).
fn money_keys(v: &Value, out: &mut Vec<String>) {
    const WORDS: [&str; 10] = [
        "price", "cost", "surcharge", "extra", "discount", "deal", "fee", "total", "amount",
        "value",
    ];
    match v {
        Value::Object(m) => {
            for (k, child) in m {
                let lower = k.to_ascii_lowercase();
                if WORDS.iter().any(|w| lower.contains(w)) || lower.contains("pricing") {
                    out.push(k.clone());
                }
                money_keys(child, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|c| money_keys(c, out)),
        _ => {}
    }
}

#[sqlx::test]
async fn an_edit_is_on_the_next_table_menu_request(pool: PgPool) {
    let s = shop(&pool).await;
    let t = table(&pool, &s).await;
    let app = app!(pool, Some(cache()));
    let uri = format!("/public/tables/{t}/menu");

    let first = get(&app, &uri, None).await;
    assert_eq!(first.status, 200);
    assert_eq!(first.cache_control.as_deref(), Some("public, no-cache"));
    let etag = first.etag.clone().expect("an ETag");
    let again = get(&app, &uri, None).await;
    assert_eq!(again.body, first.body, "a hit is the same body");
    assert_eq!(again.etag.as_deref(), Some(etag.as_str()));
    let held = get(&app, &uri, Some(&etag)).await;
    assert_eq!(held.status, 304);
    assert!(held.body.is_empty());

    // The catalogue: an item's name.
    exec(&pool, "UPDATE menu_items SET name = 'Flat white' WHERE id = $1", &[s.latte]).await;
    let after = get(&app, &uri, Some(&etag)).await;
    assert_eq!(after.status, 200, "the old ETag no longer matches");
    assert!(names(&after.json()["items"]).contains(&"Flat white".to_string()));
    assert_ne!(after.etag.as_deref(), Some(etag.as_str()));

    // A size price.
    exec(&pool, "UPDATE menu_item_sizes SET price = 6100 WHERE menu_item_id = $1 AND label = 'Large'", &[s.latte]).await;
    let body = get(&app, &uri, None).await.json();
    let latte = body["items"].as_array().unwrap().iter().find(|i| i["id"] == s.latte.to_string()).unwrap().clone();
    assert!(latte["sizes"].to_string().contains("6100"), "{latte}");

    // A branch override: the item switched off at this branch.
    exec(
        &pool,
        "INSERT INTO menu_price_overrides (scope, branch_id, target_type, target_id, is_available) \
         SELECT 'branch', $1, 'menu_item_size', z.id, false FROM menu_item_sizes z WHERE z.menu_item_id = $2",
        &[s.branch, s.cola],
    )
    .await;
    assert!(!names(&get(&app, &uri, None).await.json()["items"]).contains(&"Cola".to_string()));

    // The branch's timezone or name is not on the table menu, but the table
    // and plan checks still run per request: a deactivated table is gone.
    exec(&pool, "UPDATE branch_tables SET is_active = false WHERE id = $1", &[t]).await;
    assert_eq!(get(&app, &uri, None).await.status, 404);
}

#[sqlx::test]
async fn an_edit_is_on_the_next_menu_display_request(pool: PgPool) {
    let s = shop(&pool).await;
    page_on(&pool, &s).await;
    let app = app!(pool, Some(cache()));
    let uri = format!("/public/menu-display?org_id={}", s.org);

    let first = get(&app, &uri, None).await;
    assert_eq!(first.status, 200, "{}", String::from_utf8_lossy(&first.body));
    assert_eq!(first.cache_control.as_deref(), Some("public, no-cache"));
    assert_eq!(get(&app, &uri, None).await.body, first.body);
    assert_eq!(get(&app, &uri, first.etag.as_deref()).await.status, 304);

    // The page's own settings.
    exec(&pool, "UPDATE org_links_pages SET menu_display_title_en = 'Today' WHERE org_id = $1", &[s.org]).await;
    assert_eq!(get(&app, &uri, None).await.json()["title_en"], "Today");
    // The shop's name (branding) and the branch's name.
    exec(&pool, "UPDATE organizations SET name = 'Renamed' WHERE id = $1", &[s.org]).await;
    assert_eq!(get(&app, &uri, None).await.json()["brand"]["name"], "Renamed");
    exec(&pool, "UPDATE branches SET name = 'Downtown' WHERE id = $1", &[s.branch]).await;
    assert_eq!(get(&app, &uri, None).await.json()["branch"]["name"], "Downtown");
    // The catalogue.
    exec(&pool, "UPDATE menu_items SET name = 'Flat white' WHERE id = $1", &[s.latte]).await;
    let page = get(&app, &uri, None).await.json();
    assert!(names(&page["menu"]["items"]).contains(&"Flat white".to_string()));
    // Still no price key anywhere, from the cache.
    let hit = get(&app, &uri, None).await.json();
    let mut keys = Vec::new();
    money_keys(&hit, &mut keys);
    assert!(keys.is_empty(), "price keys on the cached menu: {keys:?}");
    // Off is a 404 on the next request, on is back.
    exec(&pool, "UPDATE org_links_pages SET menu_display_enabled = false WHERE org_id = $1", &[s.org]).await;
    assert_eq!(get(&app, &uri, None).await.status, 404);
    exec(&pool, "UPDATE org_links_pages SET menu_display_enabled = true WHERE org_id = $1", &[s.org]).await;
    assert_eq!(get(&app, &uri, None).await.status, 200);
}

/// With and without the cache, first hit and later hits: the same bytes.
#[sqlx::test]
async fn the_cached_body_is_the_uncached_body(pool: PgPool) {
    let s = shop(&pool).await;
    page_on(&pool, &s).await;
    let t = table(&pool, &s).await;
    let cached = app!(pool, Some(cache()));
    let plain = app!(pool, None::<web::Data<MenuCache>>);
    for uri in [
        format!("/public/tables/{t}/menu"),
        format!("/public/menu-display?org_id={}", s.org),
    ] {
        let want = get(&plain, &uri, None).await;
        assert_eq!(want.status, 200);
        assert!(want.etag.is_some(), "the uncached answer carries an ETag too");
        for _ in 0..2 {
            let got = get(&cached, &uri, None).await;
            assert_eq!(got.body, want.body, "{uri}");
            assert_eq!(got.etag, want.etag, "{uri}");
        }
    }
}

/// The clock is in the key only for an org with sale windows, to the minute,
/// or finer when a window boundary has seconds.
#[sqlx::test]
async fn the_clock_is_in_the_key_only_with_sale_windows(pool: PgPool) {
    let s = shop(&pool).await;
    let fp = || public_fingerprint(&pool, s.org);
    // `shop` may seed windows of its own; start from none.
    exec(&pool, "DELETE FROM sale_windows WHERE org_id = $1", &[s.org]).await;
    let a = fp().await.unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(a, fp().await.unwrap(), "no windows: no clock");
    exec(
        &pool,
        "INSERT INTO sale_windows (org_id, combo_item_id, starts_at, ends_at) \
         VALUES ($1, $2, '08:00:30', '11:00')",
        &[s.org, s.combo],
    )
    .await;
    let b = fp().await.unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_ne!(b, fp().await.unwrap(), "a window with seconds: the clock to the microsecond");
    exec(&pool, "UPDATE sale_windows SET starts_at = '08:00' WHERE org_id = $1", &[s.org]).await;
    let c = fp().await.unwrap();
    assert!(c.len() > a.len(), "a window on whole minutes: the minute is in the key");
}
