//! The shared tenant pool (`db.rs`): one pool of `madar_app` connections for
//! every org, stamped with the caller's org each time one is handed out.
//!
//! What must hold, because this is the line between one shop's data and
//! another's:
//! - a connection last used by org A, handed to org B, sees only B's rows;
//! - a connection handed out with no org in scope sees nothing (RLS errors);
//! - work spawned through `db::spawn` keeps its request's org;
//! - a second org inside one request gets its own pool and never re-stamps the
//!   first;
//! - an actix request runs on the shared pool when `tenant_scope` wraps it.

use madar_rust::db::{self, Db};
use sqlx::PgPool;
use uuid::Uuid;

async fn seed_org(pool: &PgPool, label: &str) -> Uuid {
    let org = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, $2, $3)")
        .bind(org)
        .bind(format!("Org {label}"))
        .bind(format!("org-{}", org.simple()))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(Uuid::new_v4())
        .bind(org)
        .bind(format!("Cat {label}"))
        .execute(pool)
        .await
        .unwrap();
    org
}

async fn visible(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT name FROM categories ORDER BY name")
        .fetch_all(pool)
        .await
}

#[sqlx::test]
async fn a_reused_connection_takes_the_new_orgs_stamp(base: PgPool) {
    let a = seed_org(&base, "A").await;
    let b = seed_org(&base, "B").await;

    // Alternate orgs many times; whichever idle connection is popped, it must
    // carry the org of the task that asked for it.
    let mut shared = None;
    let mut backends = std::collections::HashSet::new();
    for i in 0..20 {
        let (org, want) = if i % 2 == 0 {
            (a, "Cat A")
        } else {
            (b, "Cat B")
        };
        // Let the previous connection finish returning to the pool, so this
        // checkout pops it (the case under test) rather than opening a new one.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let (seen, pid) = db::scoped(None, async {
            let pool = Db::for_org(&base, org).await.into_inner();
            shared = Some(pool.clone());
            let mut conn = pool.acquire().await.unwrap();
            let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *conn)
                .await
                .unwrap();
            let seen: Vec<String> = sqlx::query_scalar("SELECT name FROM categories ORDER BY name")
                .fetch_all(&mut *conn)
                .await
                .unwrap();
            (seen, pid)
        })
        .await;
        backends.insert(pid);
        assert_eq!(seen, vec![want.to_string()], "iteration {i}");
    }
    assert!(
        backends.len() < 10,
        "the test must exercise reuse across orgs; {} backends served 20 checkouts",
        backends.len()
    );
    shared.unwrap().close().await;
}

#[sqlx::test]
async fn a_connection_handed_out_with_no_org_sees_nothing(base: PgPool) {
    let a = seed_org(&base, "A").await;
    let pool = db::scoped(None, async { Db::for_org(&base, a).await.into_inner() }).await;

    // Same pool, no tenant scope: stamped '' and the policies' uuid cast fails.
    let res = visible(&pool).await;
    assert!(res.is_err(), "unscoped use must fail closed, got {res:?}");

    // And inside a scope that never claimed an org.
    let res = db::scoped(None, visible(&pool)).await;
    assert!(res.is_err(), "an empty scope must fail closed, got {res:?}");
    pool.close().await;
}

#[sqlx::test]
async fn spawned_work_keeps_its_requests_org(base: PgPool) {
    let a = seed_org(&base, "A").await;
    let _b = seed_org(&base, "B").await;

    let seen = db::scoped(None, async {
        let pool = Db::for_org(&base, a).await.into_inner();
        db::spawn(async move { visible(&pool).await.unwrap() })
            .await
            .unwrap()
    })
    .await;
    assert_eq!(seen, vec!["Cat A".to_string()]);

    // A bare tokio::spawn loses the scope and fails closed rather than leaking.
    let res = db::scoped(None, async {
        let pool = Db::for_org(&base, a).await.into_inner();
        tokio::spawn(async move { visible(&pool).await })
            .await
            .unwrap()
    })
    .await;
    assert!(res.is_err(), "unscoped spawn must fail closed, got {res:?}");
}

#[sqlx::test]
async fn a_second_org_in_one_request_gets_its_own_pool(base: PgPool) {
    let a = seed_org(&base, "A").await;
    let b = seed_org(&base, "B").await;

    db::scoped(None, async {
        let pa = Db::for_org(&base, a).await.into_inner();
        let pb = Db::for_org(&base, b).await.into_inner();
        assert_eq!(db::current_org(), Some(a), "the first org keeps the scope");
        assert_eq!(visible(&pb).await.unwrap(), vec!["Cat B".to_string()]);
        assert_eq!(visible(&pa).await.unwrap(), vec!["Cat A".to_string()]);
        pb.close().await;
        pa.close().await;
    })
    .await;
}

#[sqlx::test]
async fn concurrent_requests_for_different_orgs_never_cross(base: PgPool) {
    let mut orgs = Vec::new();
    for i in 0..8 {
        orgs.push((seed_org(&base, &i.to_string()).await, format!("Cat {i}")));
    }
    let mut tasks = Vec::new();
    for round in 0..10 {
        for (org, want) in orgs.clone() {
            let base = base.clone();
            tasks.push(tokio::spawn(db::scoped(None, async move {
                let pool = Db::for_org(&base, org).await.into_inner();
                let mut tx = pool.begin().await.unwrap();
                let seen: Vec<String> = sqlx::query_scalar("SELECT name FROM categories")
                    .fetch_all(&mut *tx)
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
                assert_eq!(seen, vec![want], "round {round}");
            })));
        }
    }
    for t in tasks {
        t.await.unwrap();
    }
}

mod http {
    use super::*;
    use actix_web::{App, HttpResponse, test, web};

    async fn names(pool: Db) -> HttpResponse {
        HttpResponse::Ok().json(visible(pool.get_ref()).await.unwrap())
    }

    #[sqlx::test]
    async fn requests_alternating_orgs_each_see_their_own(base: PgPool) {
        let a = seed_org(&base, "A").await;
        let b = seed_org(&base, "B").await;
        let app = test::init_service(
            App::new()
                .wrap(actix_web::middleware::from_fn(db::tenant_scope))
                .app_data(web::Data::new(base.clone()))
                .route(
                    "/names/{org}",
                    web::get().to(
                        |path: web::Path<Uuid>, base: web::Data<PgPool>| async move {
                            // What the extractor does with verified claims.
                            names(Db::for_org(base.get_ref(), path.into_inner()).await).await
                        },
                    ),
                ),
        )
        .await;
        for i in 0..20 {
            let (org, want) = if i % 2 == 0 {
                (a, "Cat A")
            } else {
                (b, "Cat B")
            };
            let req = test::TestRequest::get()
                .uri(&format!("/names/{org}"))
                .to_request();
            let got: Vec<String> = test::call_and_read_body_json(&app, req).await;
            assert_eq!(got, vec![want.to_string()], "request {i}");
        }
    }
}
