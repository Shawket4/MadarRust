//! The shared tenant pool never lets one shop's connection serve another
//! shop's rows (pre-deploy gate, 2026-10-17).
//!
//! The pool's connections are `madar_app` sessions whose `app.org_id` is a
//! SESSION setting, so a connection returned to the pool still carries the
//! last shop's org. What makes that safe is `db::shared_pool`'s
//! `before_acquire`: every hand-out re-stamps the connection with the
//! acquiring task's org, or with '' when the task has none, and '' fails every
//! flat RLS policy's uuid cast (an error, never rows).
//!
//! These tests interleave two shops on the same pool across every way a
//! connection is acquired (the `Db` extractor behind `tenant_scope`, `Db::for_org`,
//! `PublicDb::scoped`, `db::spawn`) and check that each sees exactly its own
//! rows, that the SAME backend served both shops (so the re-stamp is what is
//! being tested), and that a used connection acquired with no org reads
//! nothing.

use std::collections::{HashMap, HashSet};

use actix_web::{App, HttpMessage, HttpResponse, test, web};
use madar_rust::auth::jwt::Claims;
use madar_rust::db::{self, Db, PublicDb};
use madar_rust::models::UserRole;
use sqlx::PgPool;
use uuid::Uuid;

struct Shop {
    org: Uuid,
    orders: i64,
}

async fn shop(pool: &PgPool, n_orders: i64) -> Shop {
    let (org, branch, till, teller) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations (id, name, slug) VALUES ('{org}', 'Iso', 'iso-{org}');
         INSERT INTO users (id, name, role, org_id, pin_hash) VALUES ('{teller}', 'T', 'teller', '{org}', 'x');
         INSERT INTO branches (id, org_id, name, code) VALUES ('{branch}', '{org}', 'Main', 'MAIN');
         INSERT INTO tills (id, branch_id, teller_id) VALUES ('{till}', '{branch}', '{teller}');
         INSERT INTO orders (branch_id, till_id, teller_id, order_number, payment_method, order_ref,
                             subtotal, tax_amount, discount_amount, total_amount, status, service_mode)
              SELECT '{branch}', '{till}', '{teller}', g, 'cash', 'ISO-' || g || '-{org}',
                     100, 0, 0, 100, 'completed', 'takeaway'
                FROM generate_series(1, {n_orders}) g;"
    ))
    .execute(pool)
    .await
    .unwrap();
    Shop { org, orders: n_orders }
}

/// What a tenant session sees of `orders`, and which backend served it.
#[derive(Debug)]
struct Seen {
    total: i64,
    foreign: i64,
    stamped: String,
    pid: i32,
}

async fn seen(pool: &PgPool, me: Uuid) -> Seen {
    let (total, foreign, stamped, pid): (i64, i64, String, i32) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE org_id <> $1),
                current_setting('app.org_id', true), pg_backend_pid()
           FROM orders",
    )
    .bind(me)
    .fetch_one(pool)
    .await
    .unwrap();
    Seen { total, foreign, stamped, pid }
}

fn check(s: &Seen, shop: &Shop, path: &str) {
    assert_eq!(s.foreign, 0, "{path}: another shop's rows leaked: {s:?}");
    assert_eq!(s.total, shop.orders, "{path}: not exactly the shop's own rows: {s:?}");
    assert_eq!(s.stamped, shop.org.to_string(), "{path}: wrong org stamped: {s:?}");
}

/// The Db extractor path, end to end: `tenant_scope` outermost, the verified
/// claims in the request (as the JWT middleware leaves them), a handler that
/// takes `Db`.
async fn by_extractor(pool: &PgPool, shops: &[&Shop]) -> Vec<(usize, Seen)> {
    async fn handler(req: actix_web::HttpRequest, db: Db) -> HttpResponse {
        let me = req.extensions().get::<Claims>().unwrap().org_id().unwrap();
        let s = seen(db.get_ref(), me).await;
        HttpResponse::Ok().json(serde_json::json!([s.total, s.foreign, s.stamped, s.pid]))
    }
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .route("/seen", web::get().to(handler))
            .wrap_fn(|req, srv| {
                let org = req.headers().get("x-org").unwrap().to_str().unwrap().to_string();
                req.extensions_mut().insert(Claims {
                    sub: Uuid::new_v4().to_string(),
                    org_id: Some(org),
                    role: UserRole::OrgAdmin,
                    branch_id: None,
                    exp: usize::MAX,
                    iat: 0,
                });
                actix_web::dev::Service::call(srv, req)
            })
            .wrap(actix_web::middleware::from_fn(db::tenant_scope)),
    )
    .await;
    let mut out = Vec::new();
    for round in 0..6 {
        for (i, s) in shops.iter().enumerate() {
            let req = test::TestRequest::get()
                .uri("/seen")
                .insert_header(("x-org", s.org.to_string()))
                .to_request();
            let v: serde_json::Value = test::call_and_read_body_json(&app, req).await;
            let seen = Seen {
                total: v[0].as_i64().unwrap(),
                foreign: v[1].as_i64().unwrap(),
                stamped: v[2].as_str().unwrap().to_string(),
                pid: v[3].as_i64().unwrap() as i32,
            };
            let _ = round;
            out.push((i, seen));
        }
    }
    out
}

#[sqlx::test]
async fn two_shops_interleaved_on_the_shared_pool_never_see_each_other(pool: PgPool) {
    let a = shop(&pool, 3).await;
    let b = shop(&pool, 5).await;
    let shops = [&a, &b];
    let mut pids: HashMap<i32, HashSet<usize>> = HashMap::new();
    let mut note = |i: usize, s: &Seen| {
        pids.entry(s.pid).or_default().insert(i);
    };

    for round in 0..6 {
        for (i, s) in shops.iter().enumerate() {
            // Db::for_org inside a request-like scope (what the extractor does).
            let got = db::scoped(None, async {
                let d = Db::for_org(&pool, s.org).await;
                seen(d.get_ref(), s.org).await
            })
            .await;
            check(&got, s, &format!("Db::for_org round {round}"));
            note(i, &got);

            // A public route: resolver -> scoped(org).
            let got = db::scoped(None, async {
                let p = PublicDb::new(&pool);
                let d = p.scoped(s.org).await;
                seen(d.get_ref(), s.org).await
            })
            .await;
            check(&got, s, &format!("PublicDb::scoped round {round}"));
            note(i, &got);

            // Work spawned from a request carries the request's org.
            let (pool2, org) = (pool.clone(), s.org);
            let got = db::scoped(None, async move {
                let d = Db::for_org(&pool2, org).await;
                db::spawn(async move { seen(d.get_ref(), org).await }).await.unwrap()
            })
            .await;
            check(&got, s, &format!("db::spawn round {round}"));
            note(i, &got);
        }
    }
    for (i, got) in by_extractor(&pool, &shops).await {
        check(&got, shops[i], "Db extractor");
        note(i, &got);
    }

    // The point of the test: one backend served BOTH shops, so the re-stamp on
    // acquire is what kept them apart (not two private connections).
    assert!(
        pids.values().any(|who| who.len() == 2),
        "no backend was shared by the two shops: {pids:?}"
    );
    // And these were the shared pool's madar_app sessions, under RLS.
    let role: String = db::scoped(None, async {
        let d = Db::for_org(&pool, a.org).await;
        sqlx::query_scalar("SELECT current_user::text").fetch_one(d.get_ref()).await.unwrap()
    })
    .await;
    assert_eq!(role, "madar_app");
}

/// A used connection acquired with no org in scope (a forgotten scope, a bare
/// `tokio::spawn`) reads nothing: it is re-stamped '' and the policy refuses.
#[sqlx::test]
async fn a_used_connection_acquired_without_an_org_reads_nothing(pool: PgPool) {
    let a = shop(&pool, 4).await;
    // Use the shared pool for shop A, and keep a handle on that pool.
    let shared = db::scoped(None, async {
        let d = Db::for_org(&pool, a.org).await;
        let got = seen(d.get_ref(), a.org).await;
        check(&got, &a, "warm-up");
        d.into_inner()
    })
    .await;

    // (1) a bare tokio::spawn: no tenant scope at all.
    let p = shared.clone();
    let bare = tokio::spawn(async move {
        let stamped: String = sqlx::query_scalar("SELECT current_setting('app.org_id', true)")
            .fetch_one(&p)
            .await
            .unwrap();
        let rows = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM orders").fetch_one(&p).await;
        (stamped, rows.ok())
    })
    .await
    .unwrap();
    assert_eq!(bare.0, "", "a connection handed out unscoped still carried an org");
    assert!(
        matches!(bare.1, None | Some(0)),
        "an unscoped session read rows: {:?}",
        bare.1
    );

    // (2) inside a request scope that never claimed an org.
    let p = shared.clone();
    let scoped = db::scoped(None, async move {
        let stamped: String = sqlx::query_scalar("SELECT current_setting('app.org_id', true)")
            .fetch_one(&p)
            .await
            .unwrap();
        let rows = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM orders").fetch_one(&p).await;
        (stamped, rows.ok())
    })
    .await;
    assert_eq!(scoped.0, "");
    assert!(matches!(scoped.1, None | Some(0)), "an unclaimed scope read rows: {:?}", scoped.1);

    // (3) the same holds for every tenant table (RLS on, an org_id column),
    // not just orders: each either refuses or returns nothing. (Org-less
    // global catalogs such as `capabilities` / `role_permissions` have an
    // explicit read-all policy and no tenant rows; they are not in this set.)
    let p = shared.clone();
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
          WHERE n.nspname = 'public' AND c.relkind = 'r' AND c.relrowsecurity
            AND EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = c.oid
                         AND a.attname = 'org_id' AND NOT a.attisdropped)",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(tables.len() > 30, "{tables:?}");
    let leaks = tokio::spawn(async move {
        let mut leaks = Vec::new();
        for t in tables {
            let r = sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {t}"))
                .fetch_one(&p)
                .await;
            if let Ok(n) = r
                && n > 0
            {
                leaks.push((t, n));
            }
        }
        leaks
    })
    .await
    .unwrap();
    assert!(leaks.is_empty(), "tables readable with no org stamped: {leaks:?}");
}
