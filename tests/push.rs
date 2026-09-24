//! `/push/token`: register, rebind to a new user, sign-out, and cleanup on
//! Dawam's phone revocation. The Dawam app registers for its EMPLOYEE through
//! `/staff/me/push-token`; `/push/token` is a Madar user's, and refuses both a
//! staff token and the `dawam` app.

use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::JwtSecret;
use madar_rust::models::UserRole;

mod common;

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn token_for(user: Uuid, org: Uuid) -> String {
    madar_rust::auth::jwt::create_token(&secret(), user, Some(org), UserRole::Teller, None, 24)
        .unwrap()
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .configure(madar_rust::push::routes::configure),
        )
        .await
    };
}

macro_rules! call {
    ($app:expr, $method:ident, $uri:expr, $token:expr, $body:expr) => {{
        let req = test::TestRequest::$method()
            .uri($uri)
            .insert_header(("Authorization", format!("Bearer {}", $token)))
            .set_json(&$body)
            .to_request();
        test::call_service(&$app, req).await
    }};
}

async fn org_and_user(pool: &PgPool) -> (Uuid, Uuid) {
    let org = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Cafe', $2)")
        .bind(org)
        .bind(format!("org-{org}"))
        .execute(pool)
        .await
        .unwrap();
    let u = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, 'A', $3, 'hash', 'teller')",
    )
    .bind(u)
    .bind(org)
    .bind(format!("{u}@test.com"))
    .execute(pool)
    .await
    .unwrap();
    (org, u)
}

async fn live_row(pool: &PgPool, token: &str) -> Option<(Uuid, String, String)> {
    sqlx::query_as(
        "SELECT COALESCE(user_id, employee_id), app, locale FROM push_devices \
          WHERE token = $1 AND revoked_at IS NULL",
    )
    .bind(token)
    .fetch_optional(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn registering_a_token_creates_a_live_device(pool: PgPool) {
    let (org, u) = org_and_user(&pool).await;
    let app = app!(pool);
    let resp = call!(
        &app,
        put,
        "/push/token",
        token_for(u, org),
        serde_json::json!({"app": "manager", "token": "tok-1", "locale": "en", "platform": "ios"})
    );
    assert_eq!(resp.status(), 204);
    let row = live_row(&pool, "tok-1").await.unwrap();
    assert_eq!(row, (u, "manager".to_string(), "en".to_string()));
}

#[sqlx::test]
async fn the_same_token_rebinds_to_whoever_registers_it_next(pool: PgPool) {
    let (org, u1) = org_and_user(&pool).await;
    let (_, u2) = org_and_user(&pool).await;
    let app = app!(pool);
    call!(
        &app,
        put,
        "/push/token",
        token_for(u1, org),
        serde_json::json!({"app": "manager", "token": "shared", "locale": "ar"})
    );
    call!(
        &app,
        put,
        "/push/token",
        token_for(u2, org),
        serde_json::json!({"app": "manager", "token": "shared", "locale": "ar"})
    );
    let row = live_row(&pool, "shared").await.unwrap();
    assert_eq!(
        row.0, u2,
        "the token now belongs to whoever registered it last"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM push_devices WHERE token = 'shared'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "no duplicate row left behind");
}

#[sqlx::test]
async fn signing_out_revokes_only_that_device(pool: PgPool) {
    let (org, u) = org_and_user(&pool).await;
    let app = app!(pool);
    call!(
        &app,
        put,
        "/push/token",
        token_for(u, org),
        serde_json::json!({"app": "manager", "token": "tok-a"})
    );
    call!(
        &app,
        put,
        "/push/token",
        token_for(u, org),
        serde_json::json!({"app": "dashboard", "token": "tok-b"})
    );
    let resp = call!(
        &app,
        delete,
        "/push/token",
        token_for(u, org),
        serde_json::json!({"app": "manager", "token": "tok-a"})
    );
    assert_eq!(resp.status(), 204);
    assert!(live_row(&pool, "tok-a").await.is_none());
    assert!(live_row(&pool, "tok-b").await.is_some());
}

#[sqlx::test]
async fn revoking_a_dawam_phone_also_revokes_its_push_device(pool: PgPool) {
    let (org, _u) = org_and_user(&pool).await;
    let e = common::employees::employee(&pool, org, "B", None, None, false, &[], 0).await;
    sqlx::query(
        "INSERT INTO push_devices (org_id, employee_id, app, token, locale) \
         VALUES ($1, $2, 'dawam', 'phone-tok', 'ar')",
    )
    .bind(org)
    .bind(e)
    .execute(&pool)
    .await
    .unwrap();
    madar_rust::staff::dawam::revoke_devices(&pool, e)
        .await
        .unwrap();
    assert!(live_row(&pool, "phone-tok").await.is_none());
}

#[sqlx::test]
async fn the_dawam_app_and_a_staff_token_cannot_use_push_token(pool: PgPool) {
    let (org, u) = org_and_user(&pool).await;
    let app = app!(pool);
    // A user session can't claim the staff app's pushes: those belong to the
    // employee, registered through /staff/me/push-token.
    let resp = call!(
        &app,
        put,
        "/push/token",
        token_for(u, org),
        serde_json::json!({"app": "dawam", "token": "tok-d"})
    );
    assert_eq!(resp.status(), 400);
    // And a staff token is not a Madar session anywhere outside /staff.
    common::employees::set_modules(&pool, org, &["pos", "dawam"]).await;
    let e = common::employees::employee(&pool, org, "B", None, Some("+201012345670"), true, &[], 0)
        .await;
    let s = common::employees::session(&pool, e).await;
    let resp = call!(
        &app,
        put,
        "/push/token",
        s.token,
        serde_json::json!({"app": "manager", "token": "tok-e"})
    );
    assert_eq!(resp.status(), 401);
    assert!(live_row(&pool, "tok-e").await.is_none());
}

/// Every `"staff.n_…"` key literal under `src/`, plus `staff.n_flag_{kind}`
/// for every kind the database allows on `attendance_flags`.
async fn every_staff_key(pool: &PgPool) -> std::collections::BTreeSet<String> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") && !p.ends_with("push/words.rs") {
                // Not the words table itself: its keys are what is checked.
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    // A whole literal only: `"staff.n_flag_{kind}"` (a format string) is
    // covered by the flag kinds below.
    let lit = regex::Regex::new(r#""(staff\.n_[a-z_]+)""#).unwrap();
    let mut keys = std::collections::BTreeSet::new();
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        for c in lit.captures_iter(&text) {
            keys.insert(c[1].to_string());
        }
    }
    let def: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
          WHERE conname = 'attendance_flags_kind_chk'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let kinds: Vec<String> = regex::Regex::new(r"'([a-z_]+)'::text")
        .unwrap()
        .captures_iter(&def)
        .map(|c| c[1].to_string())
        .collect();
    assert!(kinds.len() >= 6, "the flag kinds were read: {def}");
    for k in kinds {
        keys.insert(format!("staff.n_flag_{k}"));
    }
    keys
}

/// E2E PN-1 (APP-6): a notification the server writes is also PUSHED, which
/// needs its words in both languages on the server (`push::words`, from
/// madar-core's i18n). A key without words rendered to nothing and its push
/// was silently skipped — seven keys were. A new key without words fails here.
#[sqlx::test]
async fn every_staff_notification_key_has_push_words(pool: PgPool) {
    let keys = every_staff_key(&pool).await;
    assert!(keys.len() > 40, "{keys:?}");
    let missing: Vec<String> = keys
        .iter()
        .filter(|k| {
            [false, true]
                .iter()
                .any(|ar| madar_rust::push::render(k, &serde_json::json!({}), *ar).is_none())
        })
        .cloned()
        .collect();
    assert!(
        missing.is_empty(),
        "keys with no push words — add them to madar-core's i18n.rs and run \
         scripts/sync_push_words.py: {missing:?}"
    );
}
