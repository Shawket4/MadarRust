//! OPT-A read fast paths (`madar_rust::fastpath`): every cached answer is the
//! database's answer, and a write is visible on the very next read — the
//! writer's own (same instance, no wait) and another instance's (once its
//! notification is in, which `fastpath::barrier` waits for).
mod common;

use std::io::Read;

use actix_web::http::{StatusCode, header};
use actix_web::{App, middleware::from_fn, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use common::combos::{Shop, secret, shop};
use madar_rust::fastpath;

macro_rules! app {
    ($pool:expr, $cache:expr) => {{
        unsafe {
            std::env::set_var("MADAR_DISABLE_RATE_LIMIT", "1");
            std::env::set_var("MADAR_DISABLE_AUTO_TRANSLATION", "1");
        }
        let read_pool = web::Data::new($pool.clone());
        test::init_service(
            App::new()
                // As in main: the fence inside the tenant scope.
                .wrap(from_fn(fastpath::fence))
                .wrap(from_fn(madar_rust::db::tenant_scope))
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new($cache))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(
                    madar_rust::auth::org_status::OrgStatusCache::new(),
                ))
                .app_data(web::Data::new(
                    madar_rust::realtime::hub::BranchEventHub::new(),
                ))
                .app_data(madar_rust::qr_card::routes::make_provider())
                .app_data(web::Data::new(
                    madar_rust::demo::config::DemoConfig::from_env(),
                ))
                .app_data(web::Data::new(madar_rust::ai::AiState::from_env()))
                .configure(|cfg| madar_rust::app_routes::configure_api(cfg, read_pool.clone())),
        )
        .await
    }};
}

async fn setup(pool: &PgPool) -> Shop {
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    madar_rust::authz::sync_catalogue(pool).await.unwrap();
    shop(pool).await
}

/// Start the LISTEN task and wait for its session.
async fn listen(pool: &PgPool) {
    madar_rust::sync::pull::listener::spawn(
        pool.clone(),
        madar_rust::realtime::hub::BranchEventHub::new(),
    );
    for _ in 0..200 {
        if fastpath::live() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("the fast-path listener never started");
}

fn cache() -> madar_rust::menu::cache::MenuCache {
    let c = madar_rust::menu::cache::MenuCache::with_ttl(std::time::Duration::from_secs(3600));
    c.assume_prod_shape();
    c
}

async fn head(pool: &PgPool, branch: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COALESCE(max(seq), 0) FROM sync_changes WHERE branch_id = $1")
        .bind(branch)
        .fetch_one(pool)
        .await
        .unwrap()
}

struct Res {
    status: StatusCode,
    headers: header::HeaderMap,
    bytes: web::Bytes,
}

impl Res {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
    }
    fn header(&self, h: header::HeaderName) -> Option<String> {
        self.headers
            .get(h)
            .map(|v| v.to_str().unwrap().to_string())
    }
}

async fn send<S, B>(app: &S, req: test::TestRequest, bearer: &str) -> Res
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    let res = test::call_service(
        app,
        req.insert_header(("Authorization", format!("Bearer {bearer}")))
            .to_request(),
    )
    .await;
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = test::read_body(res).await;
    Res {
        status,
        headers,
        bytes,
    }
}

fn pull_req(since: i64, body: Value) -> test::TestRequest {
    test::TestRequest::post()
        .uri(&format!("/sync/pull?since={since}"))
        .set_json(body)
}

fn without_time(mut v: Value) -> Value {
    v.as_object_mut().unwrap().remove("server_time");
    v
}

/// An idle pull is answered from memory with the database's body, and the
/// next poll after a write — this instance's own (no wait) or another's (its
/// notification in) — carries the change.
#[sqlx::test]
async fn an_idle_pull_comes_from_memory_and_the_next_poll_after_a_write_is_fresh(pool: PgPool) {
    let s = setup(&pool).await;
    listen(&pool).await;
    let app = app!(pool, cache());
    let tok = s.teller_token();
    let since = head(&pool, s.branch).await;
    let body = json!({ "branch_id": s.branch });

    let first = send(&app, pull_req(since, body.clone()), &tok).await;
    assert_eq!(first.status, StatusCode::OK);
    let hits = fastpath::stats().idle_hits;
    let second = send(&app, pull_req(since, body.clone()), &tok).await;
    assert_eq!(fastpath::stats().idle_hits, hits + 1, "served from memory");
    assert_eq!(
        without_time(second.json()),
        without_time(first.json()),
        "the cached answer is the database's"
    );
    // ...and both are what `pull_core` (the database path) answers.
    let db = madar_rust::sync::pull::pull_core(
        &pool,
        s.org,
        &serde_json::from_value(body.clone()).unwrap(),
        Some(since),
    )
    .await
    .unwrap();
    assert_eq!(
        without_time(second.json()),
        without_time(serde_json::to_value(&db).unwrap())
    );
    assert!(
        second.header(header::ETAG).is_none(),
        "a v1 till that sends no If-None-Match gets no new header"
    );

    // This instance's own write (the dashboard adds a category): no wait.
    let made = send(
        &app,
        test::TestRequest::post()
            .uri("/categories")
            .set_json(json!({ "org_id": s.org, "name": "Fresh" })),
        &s.admin_token(),
    )
    .await;
    assert!(made.status.is_success(), "{:?}", made.json());
    let after = send(&app, pull_req(since, body.clone()), &tok).await;
    let changes = after.json()["changes"].clone();
    assert!(
        changes
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["type"] == "category" && c["data"]["name"] == "Fresh"),
        "the write is on the next poll: {changes}"
    );

    // Another instance's write (straight to the database).
    let since = after.json()["next"].as_i64().unwrap();
    send(&app, pull_req(since, body.clone()), &tok).await; // idle → cached
    sqlx::query("UPDATE categories SET name = 'Elsewhere' WHERE org_id = $1 AND name = 'Fresh'")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    fastpath::barrier(&pool).await;
    let after = send(&app, pull_req(since, body), &tok).await;
    assert!(
        after.json()["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["data"]["name"] == "Elsewhere"),
        "another instance's write is on the next poll once notified"
    );
}

/// The opt-in ETag: a till that names a protocol (or sends If-None-Match)
/// gets an ETag on an idle answer and a 304 while nothing changed; a write
/// makes the next poll a 200 with the change.
#[sqlx::test]
async fn an_idle_pull_with_its_etag_is_a_304_until_something_changes(pool: PgPool) {
    let s = setup(&pool).await;
    listen(&pool).await;
    let app = app!(pool, cache());
    let tok = s.teller_token();
    let since = head(&pool, s.branch).await;
    let body = json!({ "branch_id": s.branch, "protocol": 2 });

    let first = send(&app, pull_req(since, body.clone()), &tok).await;
    let etag = first.header(header::ETAG).expect("an idle v2 answer has an ETag");
    assert_eq!(first.json()["protocol"], 2);
    let again = send(
        &app,
        pull_req(since, body.clone()).insert_header((header::IF_NONE_MATCH, etag.clone())),
        &tok,
    )
    .await;
    assert_eq!(again.status, StatusCode::NOT_MODIFIED);
    assert!(again.bytes.is_empty());

    let made = send(
        &app,
        test::TestRequest::post()
            .uri("/categories")
            .set_json(json!({ "org_id": s.org, "name": "New" })),
        &s.admin_token(),
    )
    .await;
    assert!(made.status.is_success());
    let after = send(
        &app,
        pull_req(since, body).insert_header((header::IF_NONE_MATCH, etag)),
        &tok,
    )
    .await;
    assert_eq!(after.status, StatusCode::OK);
    assert!(!after.json()["changes"].as_array().unwrap().is_empty());
    assert!(
        after.header(header::ETAG).is_none(),
        "only an idle answer carries an ETag"
    );
}

/// A cursor the sweeper purged past must resync on the next poll even though
/// "nothing changed" was cached for it.
#[sqlx::test]
async fn a_raised_purge_watermark_ends_the_cached_idle_answer(pool: PgPool) {
    let s = setup(&pool).await;
    listen(&pool).await;
    let app = app!(pool, cache());
    let tok = s.teller_token();
    let since = head(&pool, s.branch).await;
    let body = json!({ "branch_id": s.branch });
    send(&app, pull_req(since, body.clone()), &tok).await;
    let hits = fastpath::stats().idle_hits;
    send(&app, pull_req(since, body.clone()), &tok).await;
    assert_eq!(fastpath::stats().idle_hits, hits + 1);

    // What `raise_watermark` does, from another instance.
    sqlx::query(
        "INSERT INTO sync_feed_watermarks (branch_id, purged_through_seq, updated_at) VALUES ($1, $2, now())
         ON CONFLICT (branch_id) DO UPDATE SET purged_through_seq = EXCLUDED.purged_through_seq",
    )
    .bind(s.branch)
    .bind(since + 1)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("SELECT pg_notify('sync_feed_purged', $1)")
        .bind(s.branch.to_string())
        .execute(&pool)
        .await
        .unwrap();
    fastpath::barrier(&pool).await;
    let after = send(&app, pull_req(since, body), &tok).await;
    assert_eq!(after.json()["resync_required"], true);
}

/// The cached idle answer lives no longer than its clock: the earliest moment
/// a clock-gated row leaves the live set (`sync_idle_valid_until`).
#[sqlx::test]
async fn the_idle_deadline_is_the_first_clock_gated_exit(pool: PgPool) {
    let s = setup(&pool).await;
    let none: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT sync_idle_valid_until($1)")
            .bind(s.branch)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(none.is_none(), "nothing on the clock");
    sqlx::query(
        "INSERT INTO delivery_orders (org_id, branch_id, channel, customer_name, customer_phone, cart,
                                      tax_amount, tax_rate_applied, tax_inclusive, status, updated_at)
         VALUES ($1, $2, 'pickup', 'Old', '0102', '[]', 0, 0, false, 'cancelled', now() - interval '47 hours')",
    )
    .bind(s.org)
    .bind(s.branch)
    .execute(&pool)
    .await
    .unwrap();
    let until: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT sync_idle_valid_until($1)")
            .bind(s.branch)
            .fetch_one(&pool)
            .await
            .unwrap();
    let left = until - chrono::Utc::now();
    assert!(
        left > chrono::Duration::minutes(55) && left <= chrono::Duration::hours(1),
        "a delivery finished 47 h ago leaves in an hour: {left}"
    );
}

/// The deadline covers every live gate that reads the clock. A new one must
/// be added to `sync_idle_valid_until` (and the sweeper's TIME_BASED list).
#[sqlx::test]
async fn only_the_three_known_live_gates_read_the_clock(pool: PgPool) {
    let clocked: Vec<String> = sqlx::query_scalar(
        "SELECT proname::text FROM pg_proc
          WHERE proname LIKE 'sync\\_live\\_%' AND prosrc ~* '(now\\(|current_(date|timestamp)|clock_timestamp|localtime)'
          ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        clocked,
        [
            "sync_live_booking",
            "sync_live_delivery",
            "sync_live_kitchen_ticket"
        ]
    );
}

/// `/catalog/sync?since=<current>` is answered from memory; a menu write —
/// here or elsewhere — makes the next call `changed: true`.
#[sqlx::test]
async fn catalog_sync_at_the_current_revision_comes_from_memory_until_the_menu_changes(pool: PgPool) {
    let s = setup(&pool).await;
    listen(&pool).await;
    let app = app!(pool, cache());
    let tok = s.teller_token();
    let get = |since: i64| {
        test::TestRequest::get().uri(&format!(
            "/catalog/sync?branch_id={}&since={since}",
            s.branch
        ))
    };
    let full = send(
        &app,
        test::TestRequest::get().uri(&format!("/catalog/sync?branch_id={}", s.branch)),
        &tok,
    )
    .await;
    assert_eq!(full.status, StatusCode::OK, "{:?}", full.json());
    let rev = full.json()["catalog_revision"].as_i64().unwrap();
    let first = send(&app, get(rev), &tok).await;
    assert_eq!(first.json()["changed"], false);
    let hits = fastpath::stats().catalog_hits;
    let second = send(&app, get(rev), &tok).await;
    assert_eq!(fastpath::stats().catalog_hits, hits + 1, "served from memory");
    assert_eq!(second.bytes, first.bytes, "the same body");

    // This instance's write.
    let made = send(
        &app,
        test::TestRequest::post()
            .uri("/categories")
            .set_json(json!({ "org_id": s.org, "name": "Specials" })),
        &s.admin_token(),
    )
    .await;
    assert!(made.status.is_success());
    let after = send(&app, get(rev), &tok).await;
    assert_eq!(after.json()["changed"], true, "the next call sees the write");
    let rev = after.json()["catalog_revision"].as_i64().unwrap();

    // Another instance's write.
    assert_eq!(send(&app, get(rev), &tok).await.json()["changed"], false);
    sqlx::query("UPDATE menu_items SET name = 'Flat white' WHERE id = $1")
        .bind(s.latte)
        .execute(&pool)
        .await
        .unwrap();
    fastpath::barrier(&pool).await;
    assert_eq!(send(&app, get(rev), &tok).await.json()["changed"], true);
}

/// A permission check is answered from memory, and a revocation is honoured on
/// the very next request: deactivating the account here (HTTP, no wait), a
/// session cutoff elsewhere (SQL, notified), a role grant removed elsewhere.
#[sqlx::test]
async fn permission_checks_come_from_memory_and_a_revocation_is_honoured_next_request(pool: PgPool) {
    let s = setup(&pool).await;
    listen(&pool).await;
    let app = app!(pool, cache());
    let tok = s.teller_token();
    let list = || test::TestRequest::get().uri(&format!("/discounts?org_id={}", s.org));
    assert_eq!(send(&app, list(), &tok).await.status, StatusCode::OK);
    let before = fastpath::stats();
    assert_eq!(send(&app, list(), &tok).await.status, StatusCode::OK);
    let after = fastpath::stats();
    assert!(
        after.account_hits > before.account_hits && after.epoch_hits > before.epoch_hits,
        "the second check is from memory"
    );

    // Session cutoff from elsewhere (a password change on another instance).
    sqlx::query("UPDATE users SET sessions_valid_after = now() + interval '1 minute' WHERE id = $1")
        .bind(s.teller)
        .execute(&pool)
        .await
        .unwrap();
    fastpath::barrier(&pool).await;
    assert_eq!(
        send(&app, list(), &tok).await.status,
        StatusCode::UNAUTHORIZED,
        "the cut-off token is refused on the next request"
    );
    sqlx::query("UPDATE users SET sessions_valid_after = NULL WHERE id = $1")
        .bind(s.teller)
        .execute(&pool)
        .await
        .unwrap();
    fastpath::barrier(&pool).await;
    assert_eq!(send(&app, list(), &tok).await.status, StatusCode::OK);
    assert_eq!(send(&app, list(), &tok).await.status, StatusCode::OK);

    // The teller's role taken away elsewhere (the org's epoch moves).
    sqlx::query("UPDATE role_assignments SET revoked_at = now() WHERE user_id = $1")
        .bind(s.teller)
        .execute(&pool)
        .await
        .unwrap();
    fastpath::barrier(&pool).await;
    let denied = send(&app, list(), &tok).await.status;
    assert_eq!(denied, StatusCode::FORBIDDEN, "the revoked role is gone at once");

    // Deactivated here, by the admin over HTTP: no wait at all.
    let mgr = s.manager_token();
    assert_eq!(send(&app, list(), &mgr).await.status, StatusCode::OK);
    assert_eq!(send(&app, list(), &mgr).await.status, StatusCode::OK);
    let res = send(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/users/{}", s.manager))
            .set_json(json!({ "is_active": false })),
        &s.admin_token(),
    )
    .await;
    assert!(res.status.is_success(), "{:?}", res.json());
    assert_eq!(
        send(&app, list(), &mgr).await.status,
        StatusCode::FORBIDDEN,
        "a deactivated account is refused on its next request"
    );
}

/// `/menu-items?full=true`: an ETag on every cached body, a 304 for a caller
/// that holds it, the stored codings decode to the same bytes, and a write
/// makes the held ETag stale at once.
#[sqlx::test]
async fn menu_items_carries_an_etag_and_stored_codings_and_a_write_ends_the_304(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool, cache());
    let tok = s.teller_token();
    let uri = format!("/menu-items?org_id={}&branch_id={}&full=true", s.org, s.branch);
    let plain = send(&app, test::TestRequest::get().uri(&uri), &tok).await;
    assert_eq!(plain.status, StatusCode::OK);
    let etag = plain.header(header::ETAG).expect("an ETag");
    assert!(plain.header(header::CONTENT_ENCODING).is_none());

    for (coding, decode) in [
        ("br", (|b: &[u8]| {
            let mut out = Vec::new();
            brotli::Decompressor::new(b, 4096).read_to_end(&mut out).unwrap();
            out
        }) as fn(&[u8]) -> Vec<u8>),
        ("gzip", |b: &[u8]| {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(b).read_to_end(&mut out).unwrap();
            out
        }),
        ("zstd", |b: &[u8]| zstd::decode_all(b).unwrap()),
    ] {
        let res = send(
            &app,
            test::TestRequest::get()
                .uri(&uri)
                .insert_header((header::ACCEPT_ENCODING, coding)),
            &tok,
        )
        .await;
        assert_eq!(res.header(header::CONTENT_ENCODING).as_deref(), Some(coding));
        assert_eq!(decode(&res.bytes), plain.bytes.to_vec(), "{coding} decodes to the body");
        assert_eq!(res.header(header::ETAG).as_deref(), Some(etag.as_str()));
    }

    let held = send(
        &app,
        test::TestRequest::get()
            .uri(&uri)
            .insert_header((header::IF_NONE_MATCH, etag.clone())),
        &tok,
    )
    .await;
    assert_eq!(held.status, StatusCode::NOT_MODIFIED);
    assert!(held.bytes.is_empty());

    sqlx::query("UPDATE menu_items SET name = 'Cortado' WHERE id = $1")
        .bind(s.latte)
        .execute(&pool)
        .await
        .unwrap();
    let fresh = send(
        &app,
        test::TestRequest::get()
            .uri(&uri)
            .insert_header((header::IF_NONE_MATCH, etag.clone())),
        &tok,
    )
    .await;
    assert_eq!(fresh.status, StatusCode::OK, "the held body is stale now");
    assert_ne!(fresh.header(header::ETAG), Some(etag));
    assert!(String::from_utf8_lossy(&fresh.bytes).contains("Cortado"));
}

/// With no listener nothing is served from memory: every read is the
/// database's, exactly as before this module existed.
#[sqlx::test]
async fn without_a_listener_nothing_is_cached(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool, cache());
    let tok = s.teller_token();
    let since = head(&pool, s.branch).await;
    let body = json!({ "branch_id": s.branch });
    for _ in 0..3 {
        send(&app, pull_req(since, body.clone()), &tok).await;
    }
    assert!(!fastpath::live());
    assert_eq!(fastpath::stats(), fastpath::Stats::default());
}

/// An old till's `/menu-items?full=true` through the cache (stored coding,
/// ETag) decodes to exactly the bytes the uncached path serves.
#[sqlx::test]
async fn the_cached_menu_body_decodes_to_the_uncached_bytes(pool: PgPool) {
    let s = setup(&pool).await;
    // SAFETY: nextest runs each test in its own process.
    unsafe { std::env::set_var("MENU_CACHE_TTL_SECS", "0") };
    let uncached = app!(pool, madar_rust::menu::cache::MenuCache::from_env());
    let cached = app!(pool, cache());
    let tok = s.teller_token();
    for uri in [
        format!("/menu-items?org_id={}&full=true", s.org),
        format!("/menu-items?org_id={}&branch_id={}&full=true", s.org, s.branch),
        format!("/menu-items?org_id={}", s.org),
    ] {
        let want = send(&uncached, test::TestRequest::get().uri(&uri), &tok).await;
        assert!(want.header(header::ETAG).is_none(), "no cache, no ETag");
        for _ in 0..2 {
            let got = send(
                &cached,
                test::TestRequest::get()
                    .uri(&uri)
                    .insert_header((header::ACCEPT_ENCODING, "gzip, br")),
                &tok,
            )
            .await;
            assert_eq!(got.header(header::CONTENT_ENCODING).as_deref(), Some("br"));
            let mut out = Vec::new();
            brotli::Decompressor::new(&got.bytes[..], 4096)
                .read_to_end(&mut out)
                .unwrap();
            assert_eq!(out, want.bytes.to_vec(), "{uri}");
        }
    }
}
