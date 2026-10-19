//! A kitchen screen with no accounts (owner, 2026-10-11): activated once with
//! a code from the branch plan, it signs itself in with its device credential
//! (`POST /auth/device-session`) for ever after, and a fired round reaches it
//! by push when its stream is down (`push::kitchen`).

use std::time::Duration;

use actix_http::Request;
use actix_web::dev::{Service, ServiceResponse};
use actix_web::{App, http::StatusCode, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::{JwtSecret, create_token};
use madar_rust::kitchen::KitchenTicketView;
use madar_rust::models::UserRole;
use madar_rust::push::fake;
use madar_rust::realtime::hub::BranchEventHub;

const WAIT: Duration = Duration::from_secs(10);

fn secret() -> JwtSecret {
    JwtSecret("secret".into())
}

struct World {
    org: Uuid,
    branch: Uuid,
    other_branch: Uuid,
    hub: BranchEventHub,
}

async fn world(pool: &PgPool) -> World {
    fake::install();
    madar_rust::push::pos::set_grace(Duration::from_millis(50));
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    let org = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Cafe', $2)")
        .bind(org)
        .bind(format!("org-{org}"))
        .execute(pool)
        .await
        .unwrap();
    let mut ids = Vec::new();
    for name in ["Arkan", "Zamalek"] {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1,$2,$3)")
            .bind(id)
            .bind(org)
            .bind(name)
            .execute(pool)
            .await
            .unwrap();
        ids.push(id);
    }
    World {
        org,
        branch: ids[0],
        other_branch: ids[1],
        hub: BranchEventHub::new(),
    }
}

macro_rules! app {
    ($pool:expr, $w:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new($w.hub.clone()))
                .configure(madar_rust::auth::routes::configure)
                .configure(madar_rust::users::routes::configure)
                .configure(madar_rust::kitchen::routes::configure)
                .configure(madar_rust::push::routes::configure)
                .configure(madar_rust::devices::routes::configure)
                .configure(madar_rust::realtime::routes::configure),
        )
        .await
    };
}

async fn send<S>(app: &S, req: test::TestRequest) -> (StatusCode, Value)
where
    S: Service<Request, Response = ServiceResponse, Error = actix_web::Error>,
{
    let resp = test::call_service(app, req.to_request()).await;
    let status = resp.status();
    let bytes = test::read_body(resp).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A code made for `branch` (as the branch plan makes one), entered on a
/// fresh install: the install id and the credential activation returns.
async fn activate<S>(app: &S, pool: &PgPool, w: &World, kind: &str) -> (Uuid, String)
where
    S: Service<Request, Response = ServiceResponse, Error = actix_web::Error>,
{
    let code = format!("{:08}", Uuid::new_v4().as_u128() % 100_000_000);
    sqlx::query(
        "INSERT INTO device_activation_codes (org_id, branch_id, code, label, kind) VALUES ($1,$2,$3,'Grill screen',$4)",
    )
    .bind(w.org)
    .bind(w.branch)
    .bind(&code)
    .bind(kind)
    .execute(pool)
    .await
    .unwrap();
    let device = Uuid::new_v4();
    let (st, b) = send(
        app,
        test::TestRequest::post()
            .uri("/auth/activate-device")
            .set_json(json!({ "code": code, "device_id": device, "platform": "ios" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    (device, b["device_token"].as_str().unwrap().to_string())
}

fn session_req(device: Uuid, credential: &str) -> test::TestRequest {
    test::TestRequest::post()
        .uri("/auth/device-session")
        .insert_header(("X-Madar-Device", device.to_string()))
        .insert_header(("X-Madar-Device-Token", credential.to_string()))
}

#[sqlx::test]
async fn a_kitchen_screen_signs_itself_in_with_its_code_and_nothing_else(pool: PgPool) {
    let w = world(&pool).await;
    let app = app!(pool, w);
    let (device, credential) = activate(&app, &pool, &w, "kds").await;

    let (st, first) = send(&app, session_req(device, &credential)).await;
    assert_eq!(st, StatusCode::OK, "{first}");
    assert_eq!(first["user"]["role"], "kitchen");
    assert_eq!(first["user"]["name"], "Grill screen");
    assert_eq!(first["user"]["branch_id"], w.branch.to_string());
    let token = first["token"].as_str().unwrap().to_string();

    // Its session works the kitchen: the board reads at its branch.
    let (st, b) = send(
        &app,
        test::TestRequest::get()
            .uri(&format!("/kitchen/orders?branch_id={}", w.branch))
            .insert_header(("Authorization", format!("Bearer {token}"))),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "the screen reads its board: {b}");

    // Signing in again (the session ran out) is the same screen, not a new one.
    let (_, again) = send(&app, session_req(device, &credential)).await;
    assert_eq!(again["user"]["id"], first["user"]["id"]);
    let principals: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE device_id = $1")
        .bind(device)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(principals, 1);

    // Not a person: no staff list shows it.
    let owner = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES ($1,$2,'Owner','o@t.com','h','org_admin')",
    )
    .bind(owner)
    .bind(w.org)
    .execute(&pool)
    .await
    .unwrap();
    let owner_token =
        create_token(&secret(), owner, Some(w.org), UserRole::OrgAdmin, None, 24).unwrap();
    let (st, users) = send(
        &app,
        test::TestRequest::get()
            .uri(&format!("/users?org_id={}", w.org))
            .insert_header(("Authorization", format!("Bearer {owner_token}"))),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{users}");
    let listed = users.to_string();
    assert!(
        !listed.contains(first["user"]["id"].as_str().unwrap()),
        "a kitchen screen is not staff: {listed}"
    );
}

#[sqlx::test]
async fn only_a_live_kitchen_screen_with_its_own_credential_gets_a_session(pool: PgPool) {
    let w = world(&pool).await;
    let app = app!(pool, w);
    let (device, credential) = activate(&app, &pool, &w, "kds").await;

    let (st, _) = send(&app, session_req(device, "not-the-credential")).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "a wrong credential");
    let (st, _) = send(&app, session_req(Uuid::new_v4(), &credential)).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "someone else's install");
    let (st, _) = send(&app, test::TestRequest::post().uri("/auth/device-session")).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "no device at all");

    let (till, till_credential) = activate(&app, &pool, &w, "pos").await;
    let (st, _) = send(&app, session_req(till, &till_credential)).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "a till still needs a person");

    sqlx::query("UPDATE devices SET retired_at = now() WHERE id = $1")
        .bind(device)
        .execute(&pool)
        .await
        .unwrap();
    let (st, _) = send(&app, session_req(device, &credential)).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "a retired screen");
}

fn round(branch: Uuid) -> KitchenTicketView {
    serde_json::from_value(json!({
        "id": Uuid::new_v4(), "branch_id": branch, "source_type": "open_ticket",
        "source_id": Uuid::new_v4(), "table_label": "T4", "kitchen_ref": "K-12",
        "round_number": 1, "status": "firing", "created_at": "2026-10-11T12:00:00Z",
        "items": [],
    }))
    .unwrap()
}

#[sqlx::test]
async fn a_fired_round_pushes_the_branchs_screens_that_are_not_watching(pool: PgPool) {
    let w = world(&pool).await;
    let app = app!(pool, w);

    // Two screens at Arkan: one with its stream up, one without.
    let mut screens = Vec::new();
    for _ in 0..2 {
        let (device, credential) = activate(&app, &pool, &w, "kds").await;
        let (_, s) = send(&app, session_req(device, &credential)).await;
        let token = s["token"].as_str().unwrap().to_string();
        let fcm = format!("fcm-{}", Uuid::new_v4());
        let (st, b) = send(
            &app,
            test::TestRequest::put()
                .uri("/push/token")
                .insert_header(("Authorization", format!("Bearer {token}")))
                .insert_header(("X-Madar-Device", device.to_string()))
                .set_json(json!({ "app": "kds", "token": fcm, "locale": "en", "platform": "ios" })),
        )
        .await;
        assert_eq!(st, StatusCode::NO_CONTENT, "{b}");
        screens.push((device, token, fcm));
    }
    let (watching_device, watching_token, watching_fcm) = &screens[0];
    let (_, _, idle_fcm) = &screens[1];
    let _stream = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!(
                "/realtime/stream?branch_id={}&topics=kitchen",
                w.branch
            ))
            .insert_header(("Authorization", format!("Bearer {watching_token}")))
            .insert_header(("X-Madar-Device", watching_device.to_string()))
            .to_request(),
    )
    .await;
    assert_eq!(
        _stream.status(),
        StatusCode::OK,
        "the screen's live stream opens"
    );

    let fired = round(w.branch);
    madar_rust::push::kitchen::fired(&pool, &w.hub, &fired);
    assert!(fake::wait_done(&madar_rust::push::kitchen::tag(fired.id), WAIT).await);

    assert!(
        fake::sent_to(watching_fcm).is_empty(),
        "the stream already told it"
    );
    let sent = fake::sent_to(idle_fcm);
    assert_eq!(sent.len(), 1, "the screen with no stream is pushed once");
    let m = &sent[0]["message"];
    assert_eq!(m["notification"]["title"], "New kitchen order");
    assert_eq!(m["notification"]["body"], "K-12 · T4");
    assert_eq!(m["data"]["kind"], "kitchen_fired");
    assert_eq!(m["data"]["ticket_id"], fired.id.to_string());

    // A round at another branch rings nobody here.
    let elsewhere = round(w.other_branch);
    madar_rust::push::kitchen::fired(&pool, &w.hub, &elsewhere);
    assert!(fake::wait_done(&madar_rust::push::kitchen::tag(elsewhere.id), WAIT).await);
    assert_eq!(fake::sent_to(idle_fcm).len(), 1);
}

#[sqlx::test]
async fn retiring_a_screen_in_the_dashboard_ends_its_kitchen_access_at_once(pool: PgPool) {
    let w = world(&pool).await;
    let app = app!(pool, w);
    let (device, credential) = activate(&app, &pool, &w, "kds").await;
    let (_, s) = send(&app, session_req(device, &credential)).await;
    let token = s["token"].as_str().unwrap().to_string();
    let board = || {
        test::TestRequest::get()
            .uri(&format!("/kitchen/orders?branch_id={}", w.branch))
            .insert_header(("Authorization", format!("Bearer {token}")))
    };
    assert_eq!(send(&app, board()).await.0, StatusCode::OK);

    let owner = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES ($1,$2,'Owner','o@t.com','h','org_admin')",
    )
    .bind(owner)
    .bind(w.org)
    .execute(&pool)
    .await
    .unwrap();
    let owner_token =
        create_token(&secret(), owner, Some(w.org), UserRole::OrgAdmin, None, 24).unwrap();
    let (st, b) = send(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/devices/{device}"))
            .insert_header(("Authorization", format!("Bearer {owner_token}")))
            .set_json(json!({ "retired": true })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");

    // The session it already holds reads nothing more...
    assert_eq!(
        send(&app, board()).await.0,
        StatusCode::FORBIDDEN,
        "retired: no kitchen role"
    );
    // ...and it can never mint another one.
    assert_eq!(
        send(&app, session_req(device, &credential)).await.0,
        StatusCode::UNAUTHORIZED
    );
}

/// A kitchen screen's own session drains its queued bumps through
/// `/sync/replay` (the KDS core is outbox-first): the bump must land.
#[sqlx::test]
async fn a_kitchen_screens_queued_bump_lands(pool: PgPool) {
    let w = world(&pool).await;
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .app_data(web::Data::new(w.hub.clone()))
            .configure(madar_rust::auth::routes::configure)
            .configure(madar_rust::devices::routes::configure)
            .configure(madar_rust::kitchen::routes::configure)
            .configure(madar_rust::sync::routes::configure),
    )
    .await;
    let (device, credential) = activate(&app, &pool, &w, "kds").await;
    let (st, s) = send(&app, session_req(device, &credential)).await;
    assert_eq!(st, StatusCode::OK, "{s}");
    let token = s["token"].as_str().unwrap().to_string();
    let me: Uuid = s["user"]["id"].as_str().unwrap().parse().unwrap();

    let ot: Uuid = sqlx::query_scalar(
        "INSERT INTO open_tickets (org_id, branch_id, opened_by) VALUES ($1,$2,$3) RETURNING id",
    )
    .bind(w.org)
    .bind(w.branch)
    .bind(me)
    .fetch_one(&pool)
    .await
    .unwrap();
    let round: Uuid = sqlx::query_scalar(
        "INSERT INTO open_ticket_rounds (open_ticket_id, round_number, fired_by) VALUES ($1, 1, $2) RETURNING id",
    )
    .bind(ot)
    .bind(me)
    .fetch_one(&pool)
    .await
    .unwrap();
    let kt: Uuid = sqlx::query_scalar(
        "INSERT INTO kitchen_tickets (org_id, branch_id, open_ticket_id, round_id) VALUES ($1,$2,$3,$4) RETURNING id",
    )
    .bind(w.org)
    .bind(w.branch)
    .bind(ot)
    .bind(round)
    .fetch_one(&pool)
    .await
    .unwrap();
    let item: Uuid = sqlx::query_scalar(
        "INSERT INTO kitchen_ticket_items (kitchen_ticket_id, line) VALUES ($1,'{\"name\":\"Latte\"}') RETURNING id",
    )
    .bind(kt)
    .fetch_one(&pool)
    .await
    .unwrap();

    let (st, b) = send(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .insert_header(("X-Madar-Device", device.to_string()))
            .set_json(json!({ "op": "bump_kitchen_item", "teller_id": me, "item_id": item })),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "the screen's queued bump: {b}");
    let bumped: bool =
        sqlx::query_scalar("SELECT bumped_at IS NOT NULL FROM kitchen_ticket_items WHERE id = $1")
            .bind(item)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(bumped);
}
