//! The unified stream's contract: auth, topic × permission filtering, resume
//! via `Last-Event-ID`, and the `resync` frame when a gap cannot be replayed.

use std::pin::Pin;
use std::time::Duration;

use actix_web::body::MessageBody;
use actix_web::http::StatusCode;
use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use super::event::{BranchEvent, Topic};
use super::hub::BranchEventHub;
use crate::auth::jwt::{JwtSecret, create_token};
use crate::models::UserRole;

fn secret() -> JwtSecret {
    JwtSecret("secret".into())
}
fn token(uid: Uuid, org: Uuid, role: UserRole, branch: Option<Uuid>) -> String {
    create_token(&secret(), uid, Some(org), role, branch, 24).unwrap()
}
async fn seed_org(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(id)
        .bind(format!("org-{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn seed_branch(pool: &PgPool, org: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Branch')")
        .bind(id)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn seed_user(pool: &PgPool, org: Uuid, role: &str) -> Uuid {
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
async fn perms(pool: &PgPool) {
    crate::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
}

macro_rules! app {
    ($pool:expr, $hub:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new($hub.clone()))
                .configure(crate::realtime::routes::configure),
        )
        .await
    };
}

fn get(uri: &str, token: &str) -> test::TestRequest {
    test::TestRequest::get()
        .uri(uri)
        .insert_header(("Authorization", format!("Bearer {token}")))
}

/// Read whatever the stream emits within `window` (the body is infinite, so
/// this is the only way to look at it). Keep-alive pings may be mixed in.
async fn read_for(mut body: actix_web::body::BoxBody, window: Duration) -> String {
    let mut out = String::new();
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        let next = tokio::time::timeout(
            left,
            futures::future::poll_fn(|cx| Pin::new(&mut body).as_mut().poll_next(cx)),
        )
        .await;
        match next {
            Ok(Some(Ok(bytes))) => out.push_str(&String::from_utf8_lossy(&bytes)),
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => break,
        }
    }
    out
}

#[sqlx::test]
async fn stream_requires_auth(pool: PgPool) {
    let hub = BranchEventHub::new();
    let app = app!(pool, hub);
    let branch = Uuid::new_v4();
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/realtime/stream?branch_id={branch}"))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn stream_forbids_when_no_topic_is_readable(pool: PgPool) {
    // perms NOT seeded → nothing readable → 403 (the client treats it as terminal).
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let hub = BranchEventHub::new();
    let app = app!(pool, hub);
    let t = token(teller, org, UserRole::Teller, Some(branch));
    let resp = test::call_service(
        &app,
        get(&format!("/realtime/stream?branch_id={branch}"), &t).to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn stream_opens_with_sse_headers_for_a_permitted_role(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let hub = BranchEventHub::new();
    let app = app!(pool, hub);
    let t = token(teller, org, UserRole::Teller, Some(branch));
    let resp = test::call_service(
        &app,
        get(&format!("/realtime/stream?branch_id={branch}"), &t).to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    // Must opt out of compression so the Compress middleware can't buffer frames.
    assert_eq!(resp.headers().get("content-encoding").unwrap(), "identity");
    assert_eq!(resp.headers().get("x-accel-buffering").unwrap(), "no");
}

#[sqlx::test]
async fn topics_are_filtered_by_permission_not_by_request(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let kitchen = seed_user(&pool, org, "kitchen").await;
    let teller = seed_user(&pool, org, "teller").await;
    let hub = BranchEventHub::new();
    let app = app!(pool, hub);

    // A kitchen device asking ONLY for bookings gets nothing readable → 403.
    let k = token(kitchen, org, UserRole::Kitchen, Some(branch));
    let resp = test::call_service(
        &app,
        get(
            &format!("/realtime/stream?branch_id={branch}&topics=bookings"),
            &k,
        )
        .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "kitchen has no bookings:read"
    );

    // A kitchen device asking for kitchen+bookings opens, but only kitchen events flow.
    let resp = test::call_service(
        &app,
        get(
            &format!("/realtime/stream?branch_id={branch}&topics=kitchen,bookings"),
            &k,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    hub.publish(
        branch,
        BranchEvent::new(
            Topic::Bookings,
            "booking.created",
            &serde_json::json!({"id": 1}),
        ),
    );
    hub.publish(
        branch,
        BranchEvent::new(
            Topic::Kitchen,
            "kitchen.fired",
            &serde_json::json!({"id": 2}),
        ),
    );
    let body = read_for(resp.into_body(), Duration::from_millis(400)).await;
    assert!(
        body.contains("event: kitchen.fired"),
        "kitchen event delivered: {body}"
    );
    assert!(
        !body.contains("booking.created"),
        "bookings topic filtered out: {body}"
    );

    // A teller sees bookings (tellers hold bookings:read by default).
    let t = token(teller, org, UserRole::Teller, Some(branch));
    let resp = test::call_service(
        &app,
        get(
            &format!("/realtime/stream?branch_id={branch}&topics=bookings"),
            &t,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    hub.publish(
        branch,
        BranchEvent::new(
            Topic::Bookings,
            "booking.created",
            &serde_json::json!({"id": 3}),
        ),
    );
    let body = read_for(resp.into_body(), Duration::from_millis(400)).await;
    assert!(
        body.contains("event: booking.created"),
        "teller gets bookings: {body}"
    );
    assert!(body.contains("id: "), "frames carry ids for resume: {body}");
}

#[sqlx::test]
async fn resume_replays_the_gap_or_asks_for_a_resync(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let hub = BranchEventHub::new();
    let app = app!(pool, hub);
    let t = token(teller, org, UserRole::Teller, Some(branch));

    // Prime the bus (a first subscriber creates it) and publish three events.
    let _first = hub.subscribe(branch);
    for n in 1..=3 {
        hub.publish(
            branch,
            BranchEvent::new(Topic::Tickets, "ticket.fired", &serde_json::json!({"n": n})),
        );
    }

    // Resume after id 1 → ids 2 and 3 replayed, no resync.
    let resp = test::call_service(
        &app,
        get(
            &format!("/realtime/stream?branch_id={branch}&topics=tickets"),
            &t,
        )
        .insert_header(("Last-Event-ID", "1"))
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_for(resp.into_body(), Duration::from_millis(300)).await;
    assert!(
        body.contains("id: 2\n") && body.contains("id: 3\n"),
        "replayed 2,3: {body}"
    );
    assert!(!body.contains("id: 1\n"), "already seen: {body}");
    assert!(
        !body.contains("event: resync"),
        "complete replay needs no resync: {body}"
    );

    // A cursor from a previous process lifetime → resync frame first.
    let resp = test::call_service(
        &app,
        get(
            &format!("/realtime/stream?branch_id={branch}&topics=tickets"),
            &t,
        )
        .insert_header(("Last-Event-ID", "999"))
        .to_request(),
    )
    .await;
    let body = read_for(resp.into_body(), Duration::from_millis(300)).await;
    assert!(
        body.starts_with("event: resync"),
        "stale cursor opens with resync: {body}"
    );
}
