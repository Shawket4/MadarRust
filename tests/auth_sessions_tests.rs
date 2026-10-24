//! Server-side sessions (`auth::sessions`, `/admin/sessions`, the shared
//! cookie's `/auth/session`): every sign-in is a revocable row, a revoked one
//! refuses its token with a coded 401, and a token from before sessions is
//! adopted rather than signed out.

use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::{Claims, JwtSecret, verify_token};
use madar_rust::auth::{admin_sessions, routes, sessions};
use madar_rust::models::UserRole;

const SECRET: &str = "secret";
const ORIGIN: &str = "http://localhost:5173";

fn secret() -> JwtSecret {
    JwtSecret(SECRET.to_string())
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .configure(routes::configure)
                .configure(admin_sessions::configure),
        )
        .await
    };
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(id)
        .bind(format!("sessions-{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_branch(pool: &PgPool, org: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(org)
        .bind(format!("Branch {id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

/// A password user. A low bcrypt cost keeps the suite fast.
async fn seed_user(pool: &PgPool, org: Option<Uuid>, role: &str, email: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(&format!(
        "INSERT INTO users (id, org_id, name, role, email, password_hash)
         VALUES ($1, $2, $3, '{role}'::user_role, $4, $5)"
    ))
    .bind(id)
    .bind(org)
    .bind(format!("User {email}"))
    .bind(email)
    .bind(bcrypt::hash("password123", 4).unwrap())
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn seed_teller(pool: &PgPool, org: Uuid, branch: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, role, pin_hash)
         VALUES ($1, $2, 'Teller One', 'teller'::user_role, $3)",
    )
    .bind(id)
    .bind(org)
    .bind(bcrypt::hash("1234", 4).unwrap())
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(id)
        .bind(branch)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn call<S>(app: &S, req: test::TestRequest) -> (u16, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let resp = test::call_service(app, req.to_request()).await;
    let status = resp.status().as_u16();
    let bytes = test::read_body(resp).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn login<S>(app: &S, email: &str) -> String
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let (st, body) = call(
        app,
        test::TestRequest::post()
            .uri("/auth/login")
            .set_json(json!({ "email": email, "password": "password123" })),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    body["token"].as_str().unwrap().to_string()
}

fn get(uri: &str, token: &str) -> test::TestRequest {
    test::TestRequest::get()
        .uri(uri)
        .insert_header(("Authorization", format!("Bearer {token}")))
}

fn post(uri: &str, token: &str, body: Value) -> test::TestRequest {
    test::TestRequest::post()
        .uri(uri)
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(body)
}

fn claims(token: &str) -> Claims {
    verify_token(&secret(), token).unwrap()
}

/// A token as minted before sessions existed: no `sid`, issued `ago` seconds
/// in the past.
fn legacy_token(user: Uuid, org: Option<Uuid>, role: UserRole, ago: i64) -> String {
    let now = chrono::Utc::now().timestamp();
    let c = Claims {
        sub: user.to_string(),
        org_id: org.map(|o| o.to_string()),
        role,
        branch_id: None,
        iat: (now - ago) as usize,
        exp: (now - ago + 24 * 3600) as usize,
        sid: None,
    };
    jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &c,
        &jsonwebtoken::EncodingKey::from_secret(SECRET.as_bytes()),
    )
    .unwrap()
}

async fn count(pool: &PgPool, sql: &str, id: Uuid) -> i64 {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn a_password_sign_in_is_a_48h_session_and_revoking_it_refuses_the_token(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    seed_user(&pool, None, "super_admin", "root@madar.test").await;
    let owner = seed_user(&pool, Some(org), "org_admin", "owner@shop.test").await;
    let root = login(&app, "root@madar.test").await;
    let token = login(&app, "owner@shop.test").await;

    let c = claims(&token);
    let sid = c.session_id().expect("a new token names its session");
    let hours = (c.exp - c.iat) as i64 / 3600;
    assert_eq!(hours, sessions::DASHBOARD_HOURS);
    let kind: String = sqlx::query_scalar("SELECT kind FROM auth_sessions WHERE id = $1")
        .bind(sid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kind, "password");

    assert_eq!(call(&app, get("/auth/me", &token)).await.0, 200);

    // A second sign-in of the same person is a second session.
    let other = login(&app, "owner@shop.test").await;

    let (st, body) = call(
        &app,
        post(
            &format!("/admin/sessions/{sid}/revoke"),
            &root,
            json!({ "reason": "lost laptop" }),
        ),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["revoked"], 1);

    let (st, body) = call(&app, get("/auth/me", &token)).await;
    assert_eq!(st, 401);
    assert_eq!(body["code"], "SESSION_REVOKED");
    // The POS needs the JSON error body: a bare 401 reads as a captive portal.
    assert!(body["error"].is_string(), "{body}");

    assert_eq!(
        call(&app, get("/auth/me", &other)).await.0,
        200,
        "only that one"
    );

    let (_, row) = call(&app, get(&format!("/admin/sessions/{sid}"), &root)).await;
    assert_eq!(row["state"], "revoked");
    assert_eq!(row["revoke_reason"], "lost laptop");
    assert_eq!(row["revoked_by_name"], "User root@madar.test");
    assert_eq!(row["user_id"], owner.to_string());
}

#[sqlx::test(migrations = "./migrations")]
async fn a_pin_sign_in_is_a_24h_till_session(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    seed_teller(&pool, org, branch).await;
    let (st, body) = call(
        &app,
        test::TestRequest::post()
            .uri("/auth/login")
            .insert_header(("X-Madar-Device-Id", "tablet-7"))
            .insert_header(("X-Madar-Client", "pos/0.11.0 (android)"))
            .set_json(json!({ "name": "Teller One", "pin": "1234", "branch_id": branch })),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    let c = claims(body["token"].as_str().unwrap());
    assert_eq!((c.exp - c.iat) as i64 / 3600, sessions::TILL_HOURS);
    let row: (String, Option<Uuid>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT kind, branch_id, device_id, client FROM auth_sessions WHERE id = $1",
    )
    .bind(c.session_id().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, "pin");
    assert_eq!(row.1, Some(branch));
    assert_eq!(row.2.as_deref(), Some("tablet-7"));
    assert_eq!(row.3.as_deref(), Some("pos/0.11.0 (android)"));
}

#[sqlx::test(migrations = "./migrations")]
async fn a_token_from_before_sessions_keeps_working_and_is_adopted_once(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    seed_user(&pool, None, "super_admin", "root@madar.test").await;
    let owner = seed_user(&pool, Some(org), "org_admin", "owner@shop.test").await;
    let root = login(&app, "root@madar.test").await;
    let old = legacy_token(owner, Some(org), UserRole::OrgAdmin, 3600);

    // Two first requests at once: still one row.
    let (a, b) = futures::join!(
        call(&app, get("/auth/me", &old)),
        call(&app, get("/auth/me", &old))
    );
    assert_eq!((a.0, b.0), (200, 200), "nobody is signed out by the deploy");
    let sql = "SELECT count(*) FROM auth_sessions WHERE user_id = $1 AND kind = 'legacy'";
    assert_eq!(count(&pool, sql, owner).await, 1);
    assert_eq!(call(&app, get("/auth/me", &old)).await.0, 200);
    assert_eq!(count(&pool, sql, owner).await, 1);

    // Its row carries the token's own expiry, and it can be revoked.
    let (_, page) = call(
        &app,
        get(&format!("/admin/sessions?user_id={owner}"), &root),
    )
    .await;
    let row = &page["sessions"][0];
    assert_eq!(row["kind"], "legacy");
    let sid = row["id"].as_str().unwrap();
    call(
        &app,
        post(
            &format!("/admin/sessions/{sid}/revoke"),
            &root,
            json!({ "reason": "test" }),
        ),
    )
    .await;
    let (st, body) = call(&app, get("/auth/me", &old)).await;
    assert_eq!((st, body["code"].as_str()), (401, Some("SESSION_REVOKED")));
}

#[sqlx::test(migrations = "./migrations")]
async fn signing_a_person_out_everywhere_also_ends_tokens_never_seen(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    seed_user(&pool, None, "super_admin", "root@madar.test").await;
    let owner = seed_user(&pool, Some(org), "org_admin", "owner@shop.test").await;
    let root = login(&app, "root@madar.test").await;
    let fresh = login(&app, "owner@shop.test").await;
    let unseen = legacy_token(owner, Some(org), UserRole::OrgAdmin, 60);

    let (st, body) = call(
        &app,
        post(
            &format!("/admin/users/{owner}/revoke-sessions"),
            &root,
            json!({ "reason": "left the company" }),
        ),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["revoked"], 1);
    for t in [&fresh, &unseen] {
        let (st, body) = call(&app, get("/auth/me", t)).await;
        assert_eq!(st, 401, "{body}");
    }
    let sql = "SELECT count(*) FROM auth_sessions WHERE user_id = $1 AND kind = 'legacy'";
    assert_eq!(
        count(&pool, sql, owner).await,
        0,
        "never revived by adoption"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn an_org_sign_out_of_its_tills_leaves_the_owner_signed_in(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    seed_teller(&pool, org, branch).await;
    seed_user(&pool, None, "super_admin", "root@madar.test").await;
    seed_user(&pool, Some(org), "org_admin", "owner@shop.test").await;
    let root = login(&app, "root@madar.test").await;
    let owner = login(&app, "owner@shop.test").await;
    let (_, body) = call(
        &app,
        test::TestRequest::post()
            .uri("/auth/login")
            .set_json(json!({ "name": "Teller One", "pin": "1234", "branch_id": branch })),
    )
    .await;
    let till = body["token"].as_str().unwrap().to_string();

    let (st, body) = call(
        &app,
        post(
            &format!("/admin/orgs/{org}/revoke-sessions"),
            &root,
            json!({ "reason": "stolen tablet", "kinds": ["pin", "device"] }),
        ),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["revoked"], 1);
    assert_eq!(call(&app, get("/auth/me", &till)).await.0, 401);
    assert_eq!(call(&app, get("/auth/me", &owner)).await.0, 200);

    let (st, _) = call(
        &app,
        post(
            &format!("/admin/orgs/{org}/revoke-sessions"),
            &root,
            json!({ "reason": "x", "kinds": ["nonsense"] }),
        ),
    )
    .await;
    assert_eq!(st, 400);
}

#[sqlx::test(migrations = "./migrations")]
async fn only_a_super_admin_manages_sessions(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let owner_id = seed_user(&pool, Some(org), "org_admin", "owner@shop.test").await;
    let owner = login(&app, "owner@shop.test").await;
    let sid = claims(&owner).session_id().unwrap();
    for req in [
        get("/admin/sessions", &owner),
        get(&format!("/admin/sessions/{sid}"), &owner),
        post(
            &format!("/admin/sessions/{sid}/revoke"),
            &owner,
            json!({ "reason": "x" }),
        ),
        post(
            &format!("/admin/users/{owner_id}/revoke-sessions"),
            &owner,
            json!({ "reason": "x" }),
        ),
        post(
            &format!("/admin/orgs/{org}/revoke-sessions"),
            &owner,
            json!({ "reason": "x" }),
        ),
    ] {
        assert_eq!(call(&app, req).await.0, 403);
    }
    assert_eq!(call(&app, get("/auth/me", &owner)).await.0, 200);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_list_filters_and_pages(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    seed_user(&pool, None, "super_admin", "root@madar.test").await;
    seed_user(&pool, Some(org), "org_admin", "owner@shop.test").await;
    let root = login(&app, "root@madar.test").await;
    for _ in 0..3 {
        login(&app, "owner@shop.test").await;
    }
    let (st, page) = call(&app, get("/admin/sessions?q=owner@shop&limit=2", &root)).await;
    assert_eq!(st, 200, "{page}");
    assert_eq!(page["sessions"].as_array().unwrap().len(), 2);
    let next = &page["next"];
    let (_, rest) = call(
        &app,
        get(
            &format!(
                "/admin/sessions?q=owner@shop&limit=2&before={}&before_id={}",
                urlencoding(next["before"].as_str().unwrap()),
                next["before_id"].as_str().unwrap()
            ),
            &root,
        ),
    )
    .await;
    assert_eq!(rest["sessions"].as_array().unwrap().len(), 1);
    assert!(rest["next"].is_null());

    let (_, by_org) = call(&app, get(&format!("/admin/sessions?org_id={org}"), &root)).await;
    assert_eq!(by_org["sessions"].as_array().unwrap().len(), 3);
    let (_, revoked) = call(&app, get("/admin/sessions?state=revoked", &root)).await;
    assert!(revoked["sessions"].as_array().unwrap().is_empty());
    assert_eq!(
        call(&app, get("/admin/sessions?state=bogus", &root))
            .await
            .0,
        400
    );
}

fn urlencoding(s: &str) -> String {
    s.replace('+', "%2B").replace(':', "%3A")
}

#[sqlx::test(migrations = "./migrations")]
async fn last_seen_is_written_at_most_every_five_minutes(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    seed_user(&pool, Some(org), "org_admin", "owner@shop.test").await;
    let token = login(&app, "owner@shop.test").await;
    let sid = claims(&token).session_id().unwrap();
    let seen = |ago: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(&format!(
                "UPDATE auth_sessions SET last_seen_at = now() - interval '{ago}' WHERE id = $1"
            ))
            .bind(sid)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    let age = || {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, f64>(
                "SELECT extract(epoch FROM now() - last_seen_at)::float8
                   FROM auth_sessions WHERE id = $1",
            )
            .bind(sid)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    seen("1 minute").await;
    call(&app, get("/auth/me", &token)).await;
    assert!(age().await >= 59.0, "fresh enough: not rewritten");
    seen("10 minutes").await;
    call(&app, get("/auth/me", &token)).await;
    assert!(age().await < 5.0, "stale: rewritten");
}

#[sqlx::test(migrations = "./migrations")]
async fn the_shared_cookie_ends_with_its_session(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    seed_user(&pool, Some(org), "org_admin", "owner@shop.test").await;
    let token = login(&app, "owner@shop.test").await;

    // Share it, as the dashboard does after signing in.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/auth/session")
            .insert_header(("Origin", ORIGIN))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 204);
    let cookie = resp
        .response()
        .cookies()
        .find(|c| c.name() == madar_rust::auth::session_cookie::SESSION_COOKIE)
        .expect("the session cookie")
        .into_owned();
    // No Path: the browser scopes it to where it was set, `/api/auth` behind
    // the Rewards host's proxy.
    assert_eq!(cookie.path(), None);

    // The other app adopts it.
    let adopt = || {
        test::TestRequest::get()
            .uri("/auth/session")
            .insert_header(("Origin", ORIGIN))
            .cookie(cookie.clone())
    };
    let (st, body) = call(&app, adopt()).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["token"], token);

    // Rewards by Madar calls it same-origin, from a host in no CORS list.
    let (st, _) = call(
        &app,
        test::TestRequest::get()
            .uri("/auth/session")
            .insert_header(("Origin", "https://rewards.madar-pos.cloud"))
            .cookie(cookie.clone()),
    )
    .await;
    assert_eq!(st, 200);

    // Its adopt is a same-origin GET, which carries no Origin: the browser's
    // Sec-Fetch-Site and the host the proxy forwards stand in for it.
    let (st, _) = call(
        &app,
        test::TestRequest::get()
            .uri("/auth/session")
            .insert_header(("Host", "rewards.madar-pos.cloud"))
            .insert_header(("X-Forwarded-Proto", "https"))
            .insert_header(("Sec-Fetch-Site", "same-origin"))
            .cookie(cookie.clone()),
    )
    .await;
    assert_eq!(st, 200);
    // The same from a shop's own hostname, or a cross-site fetch, is refused.
    for (host, site) in [
        ("rue.madar-pos.cloud", "same-origin"),
        ("rewards.madar-pos.cloud", "cross-site"),
    ] {
        let (st, _) = call(
            &app,
            test::TestRequest::get()
                .uri("/auth/session")
                .insert_header(("Host", host))
                .insert_header(("X-Forwarded-Proto", "https"))
                .insert_header(("Sec-Fetch-Site", site))
                .cookie(cookie.clone()),
        )
        .await;
        assert_eq!(st, 403, "{host} {site}");
    }

    // A shop's own hostname is same-site but not allowed.
    let (st, _) = call(
        &app,
        test::TestRequest::get()
            .uri("/auth/session")
            .insert_header(("Origin", "https://rue.madar-pos.cloud"))
            .cookie(cookie.clone()),
    )
    .await;
    assert_eq!(st, 403);

    // Signing out ends the session itself, so the copy the other app holds
    // stops working, and the cookie is never handed back again.
    let (st, _) = call(
        &app,
        test::TestRequest::delete()
            .uri("/auth/session")
            .insert_header(("Origin", ORIGIN))
            .cookie(cookie.clone()),
    )
    .await;
    assert_eq!(st, 204);
    let (st, body) = call(&app, get("/auth/me", &token)).await;
    assert_eq!((st, body["code"].as_str()), (401, Some("SESSION_REVOKED")));
    let (st, body) = call(&app, adopt()).await;
    assert_eq!((st, body["code"].as_str()), (401, Some("SESSION_NONE")));
}

#[sqlx::test(migrations = "./migrations")]
async fn a_pin_session_is_never_shared(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    seed_teller(&pool, org, branch).await;
    let (_, body) = call(
        &app,
        test::TestRequest::post()
            .uri("/auth/login")
            .set_json(json!({ "name": "Teller One", "pin": "1234", "branch_id": branch })),
    )
    .await;
    let (st, _) = call(
        &app,
        test::TestRequest::post()
            .uri("/auth/session")
            .insert_header(("Origin", ORIGIN))
            .insert_header((
                "Authorization",
                format!("Bearer {}", body["token"].as_str().unwrap()),
            )),
    )
    .await;
    assert_eq!(st, 403);
}
