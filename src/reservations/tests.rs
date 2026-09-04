use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::realtime::hub::BranchEventHub;
use crate::reservations::floor::{FloorSection, FloorTable};

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn admin_token(user_id: Uuid, org_id: Uuid) -> String {
    crate::auth::jwt::create_token(
        &get_secret(),
        user_id,
        Some(org_id),
        UserRole::OrgAdmin,
        None,
        24,
    )
    .unwrap()
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Test Org', $2)")
        .bind(org_id)
        .bind(format!("org-{org_id}"))
        .execute(pool)
        .await
        .unwrap();
    org_id
}

async fn seed_branch(pool: &PgPool, org_id: Uuid) -> Uuid {
    let branch_id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Branch')")
        .bind(branch_id)
        .bind(org_id)
        .execute(pool)
        .await
        .unwrap();
    branch_id
}

async fn seed_admin(pool: &PgPool, org_id: Uuid) -> Uuid {
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, 'Admin', $3, 'hash', 'org_admin'::user_role)",
    )
    .bind(user_id)
    .bind(org_id)
    .bind(format!("admin-{user_id}@test.com"))
    .execute(pool)
    .await
    .unwrap();
    user_id
}

async fn grant(pool: &PgPool, role: &str, resource: &str, action: &str) {
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) \
         VALUES ($1::user_role, $2::permission_resource, $3::permission_action, true) \
         ON CONFLICT DO NOTHING",
    )
    .bind(role)
    .bind(resource)
    .bind(action)
    .execute(pool)
    .await
    .unwrap();
}

async fn grant_all(pool: &PgPool) {
    for (res, act) in [
        ("floor_plan", "create"),
        ("floor_plan", "read"),
        ("floor_plan", "update"),
        ("floor_plan", "delete"),
        ("reservations", "create"),
        ("reservations", "read"),
        ("reservations", "update"),
        ("open_tickets", "update"),
    ] {
        grant(pool, "org_admin", res, act).await;
    }
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(get_secret()))
                .app_data(web::Data::new(BranchEventHub::new()))
                .configure(crate::reservations::routes::configure)
                .configure(crate::tickets::routes::configure),
        )
        .await
    };
}

#[sqlx::test]
async fn floor_section_and_table_crud(pool: PgPool) {
    let app = app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let admin = seed_admin(&pool, org_id).await;
    grant_all(&pool).await;
    let token = admin_token(admin, org_id);

    // Create a section.
    let req = test::TestRequest::post()
        .uri("/floor/sections")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(&serde_json::json!({ "branch_id": branch_id, "name": "Patio" }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "create section: {:?}",
        resp.status()
    );
    let section: FloorSection = test::read_body_json(resp).await;
    assert_eq!(section.name, "Patio");

    // Create a table in it with geometry + seats.
    let req = test::TestRequest::post()
        .uri("/floor/tables")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(&serde_json::json!({
            "branch_id": branch_id, "label": "T1", "section_id": section.id,
            "seats": 4, "shape": "circle", "pos_x": 100.0, "pos_y": 50.0
        }))
        .to_request();
    let table: FloorTable = test::read_body_json(test::call_service(&app, req).await).await;
    assert_eq!(table.seats, 4);
    assert_eq!(table.shape, "circle");
    assert_eq!(table.status, "free");

    // List shows it.
    let req = test::TestRequest::get()
        .uri(&format!("/floor/tables?branch_id={branch_id}"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let tables: Vec<FloorTable> = test::read_body_json(test::call_service(&app, req).await).await;
    assert_eq!(tables.len(), 1);
}

/// The host board's default view — `GET /reservations?branch_id=…` with no
/// status and no date — plus each filter combination. Every branch of the
/// query builder must bind exactly the parameters its SQL references.

/// The date filter must resolve in the BRANCH's effective timezone, the way
/// every other date bucket in the codebase does — not in whatever zone the
/// Postgres server happens to be configured with. Casting a `timestamptz`
/// straight to `::date` silently uses the session zone, so a late-night
/// booking lands on the wrong calendar day (and the day it lands on changes
/// between dev, CI and prod depending on the server's `TimeZone` setting).

/// Times that reach a CUSTOMER (the WhatsApp departure nudge) must be the
/// branch's local wall clock, not the UTC instant. Formatting `reserved_for`
/// directly told a customer with an 8pm Cairo booking to arrive at 17:00.

// ── Layout autosave: optimistic concurrency ─────────────────────────────────

/// Helper: seed an org/branch/admin and one table, returning (token, branch, table).
async fn seed_one_table(pool: &PgPool, label: &str) -> (String, Uuid, FloorTable) {
    let org_id = seed_org(pool).await;
    let branch_id = seed_branch(pool, org_id).await;
    let user_id = seed_admin(pool, org_id).await;
    grant_all(pool).await;
    let token = admin_token(user_id, org_id);
    let app = app!(pool.clone());
    let req = test::TestRequest::post()
        .uri("/floor/tables")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(&serde_json::json!({
            "branch_id": branch_id, "label": label,
            "seats": 2, "shape": "rect", "pos_x": 0.0, "pos_y": 0.0
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    assert!(
        status.is_success(),
        "create table {status}: {}",
        String::from_utf8_lossy(&body)
    );
    let table: FloorTable = serde_json::from_slice(&body).unwrap();
    (token, branch_id, table)
}

async fn save_layout_req(
    pool: &PgPool,
    token: &str,
    branch_id: Uuid,
    body: serde_json::Value,
) -> actix_web::dev::ServiceResponse {
    let app = app!(pool.clone());
    let _ = branch_id;
    let req = test::TestRequest::put()
        .uri("/floor/layout")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(&body)
        .to_request();
    test::call_service(&app, req).await
}

/// The guard has to survive the round trip through JSON, or it silently never
/// matches and every autosave becomes a conflict. Postgres keeps `timestamptz`
/// to microseconds; this pins that the serialized form does too.
#[sqlx::test]
async fn a_matching_guard_saves_and_the_timestamp_survives_json(pool: PgPool) {
    let (token, branch_id, table) = seed_one_table(&pool, "T1").await;

    let resp = save_layout_req(
        &pool,
        &token,
        branch_id,
        serde_json::json!({
            "branch_id": branch_id,
            "tables": [{
                "id": table.id, "section_id": null,
                "pos_x": 40.0, "pos_y": 60.0, "width": 80.0, "height": 80.0,
                "rotation": 0.0,
                "expected_updated_at": table.updated_at,
            }],
        }),
    )
    .await;
    assert!(resp.status().is_success(), "status {:?}", resp.status());

    let rows: Vec<FloorTable> = test::read_body_json(resp).await;
    let saved = rows.iter().find(|r| r.id == table.id).unwrap();
    assert_eq!(saved.pos_x, 40.0);
    assert_eq!(saved.pos_y, 60.0);
    // The write moved the row on, so the old token must no longer match.
    assert!(saved.updated_at > table.updated_at);
}

/// The reason the guard exists: with autosave, two managers arranging the same
/// room overwrite each other every few hundred milliseconds, invisibly.
#[sqlx::test]
async fn a_stale_guard_is_refused_and_names_the_table(pool: PgPool) {
    let (token, branch_id, table) = seed_one_table(&pool, "Window 3").await;

    // Manager A saves.
    let first = save_layout_req(
        &pool,
        &token,
        branch_id,
        serde_json::json!({
            "branch_id": branch_id,
            "tables": [{
                "id": table.id, "section_id": null,
                "pos_x": 10.0, "pos_y": 10.0, "width": 80.0, "height": 80.0,
                "rotation": 0.0, "expected_updated_at": table.updated_at,
            }],
        }),
    )
    .await;
    assert!(first.status().is_success());

    // Manager B still holds the ORIGINAL timestamp and saves on top.
    let second = save_layout_req(
        &pool,
        &token,
        branch_id,
        serde_json::json!({
            "branch_id": branch_id,
            "tables": [{
                "id": table.id, "section_id": null,
                "pos_x": 999.0, "pos_y": 999.0, "width": 80.0, "height": 80.0,
                "rotation": 0.0, "expected_updated_at": table.updated_at,
            }],
        }),
    )
    .await;
    assert_eq!(
        second.status().as_u16(),
        409,
        "a lost race must be a conflict"
    );

    let body: serde_json::Value = test::read_body_json(second).await;
    let text = body.to_string();
    // Naming the table is the point: "someone changed the layout" is not
    // something a manager can act on.
    assert!(
        text.contains("Window 3"),
        "error must name the table: {text}"
    );

    // And B's write must NOT have landed.
    let app = app!(pool.clone());
    let req = test::TestRequest::get()
        .uri(&format!("/floor/tables?branch_id={branch_id}"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let rows: Vec<FloorTable> = test::read_body_json(test::call_service(&app, req).await).await;
    let now = rows.iter().find(|r| r.id == table.id).unwrap();
    assert_eq!(now.pos_x, 10.0, "the loser's write must not have landed");
}

/// One stale table must not let the rest of the batch through -- a half-applied
/// layout is an arrangement neither person made.
#[sqlx::test]
async fn a_conflict_rolls_back_the_whole_batch(pool: PgPool) {
    let (token, branch_id, t1) = seed_one_table(&pool, "A1").await;
    let app = app!(pool.clone());
    let req = test::TestRequest::post()
        .uri("/floor/tables")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(&serde_json::json!({
            "branch_id": branch_id, "label": "A2",
            "seats": 2, "shape": "rect", "pos_x": 0.0, "pos_y": 0.0
        }))
        .to_request();
    let t2: FloorTable = test::read_body_json(test::call_service(&app, req).await).await;

    // Move t1 so its token goes stale, leaving t2's token still valid.
    let bump = save_layout_req(
        &pool,
        &token,
        branch_id,
        serde_json::json!({
            "branch_id": branch_id,
            "tables": [{
                "id": t1.id, "section_id": null, "pos_x": 5.0, "pos_y": 5.0,
                "width": 80.0, "height": 80.0, "rotation": 0.0,
                "expected_updated_at": t1.updated_at,
            }],
        }),
    )
    .await;
    assert!(bump.status().is_success());

    // Now save BOTH with t1's stale token: t2 is fresh and would otherwise land.
    let resp = save_layout_req(
        &pool,
        &token,
        branch_id,
        serde_json::json!({
            "branch_id": branch_id,
            "tables": [
                { "id": t1.id, "section_id": null, "pos_x": 111.0, "pos_y": 111.0,
                  "width": 80.0, "height": 80.0, "rotation": 0.0,
                  "expected_updated_at": t1.updated_at },
                { "id": t2.id, "section_id": null, "pos_x": 222.0, "pos_y": 222.0,
                  "width": 80.0, "height": 80.0, "rotation": 0.0,
                  "expected_updated_at": t2.updated_at },
            ],
        }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 409);

    let req = test::TestRequest::get()
        .uri(&format!("/floor/tables?branch_id={branch_id}"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let rows: Vec<FloorTable> = test::read_body_json(test::call_service(&app, req).await).await;
    let after2 = rows.iter().find(|r| r.id == t2.id).unwrap();
    assert_eq!(after2.pos_x, 0.0, "t2 must be rolled back with the batch");
}

/// Existing clients -- and the POS -- send no guard, and must keep working.
#[sqlx::test]
async fn omitting_the_guard_keeps_the_previous_behaviour(pool: PgPool) {
    let (token, branch_id, table) = seed_one_table(&pool, "T9").await;
    // Move it once so any token the caller might have held is already stale.
    let _ = save_layout_req(
        &pool,
        &token,
        branch_id,
        serde_json::json!({
            "branch_id": branch_id,
            "tables": [{ "id": table.id, "section_id": null, "pos_x": 7.0, "pos_y": 7.0,
                         "width": 80.0, "height": 80.0, "rotation": 0.0 }],
        }),
    )
    .await;

    let resp = save_layout_req(
        &pool,
        &token,
        branch_id,
        serde_json::json!({
            "branch_id": branch_id,
            "tables": [{ "id": table.id, "section_id": null, "pos_x": 33.0, "pos_y": 44.0,
                         "width": 80.0, "height": 80.0, "rotation": 0.0 }],
        }),
    )
    .await;
    assert!(resp.status().is_success(), "no guard means no conflict");
    let rows: Vec<FloorTable> = test::read_body_json(resp).await;
    assert_eq!(rows.iter().find(|r| r.id == table.id).unwrap().pos_x, 33.0);
}
