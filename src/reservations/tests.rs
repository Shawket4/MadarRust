use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::realtime::hub::BranchEventHub;
use crate::reservations::bookings::BookingView;
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
                // The deprecated booking flow is unmounted by default; the
                // tests keep exercising its handlers directly.
                .configure(crate::reservations::routes::configure_bookings)
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

#[sqlx::test]
async fn seat_booking_opens_ticket_then_move_table(pool: PgPool) {
    let app = app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let admin = seed_admin(&pool, org_id).await;
    grant_all(&pool).await;
    let token = admin_token(admin, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    // Two tables.
    let mk_table =
        |label: &str| serde_json::json!({ "branch_id": branch_id, "label": label, "seats": 4 });
    let t1: FloorTable = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/floor/tables")
                .insert_header(auth.clone())
                .set_json(&mk_table("T1"))
                .to_request(),
        )
        .await,
    )
    .await;
    let t2: FloorTable = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/floor/tables")
                .insert_header(auth.clone())
                .set_json(&mk_table("T2"))
                .to_request(),
        )
        .await,
    )
    .await;

    // Create a walk-in booking.
    let booking: BookingView = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/reservations")
                .insert_header(auth.clone())
                .set_json(&serde_json::json!({
                    "branch_id": branch_id, "kind": "walk_in",
                    "customer_name": "Sam", "customer_phone": "01001234567", "party_size": 2
                }))
                .to_request(),
        )
        .await,
    )
    .await;
    assert_eq!(booking.status, "confirmed");

    // Seat it on T1 → status seated, T1 occupied, a ticket opens.
    let seated: BookingView = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri(&format!("/reservations/{}/assign", booking.id))
                .insert_header(auth.clone())
                .set_json(&serde_json::json!({ "table_ids": [t1.id] }))
                .to_request(),
        )
        .await,
    )
    .await;
    assert_eq!(seated.status, "seated");
    assert_eq!(seated.table_ids, vec![t1.id]);

    let t1_status: String = sqlx::query_scalar("SELECT status FROM branch_tables WHERE id = $1")
        .bind(t1.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(t1_status, "seated");
    let ticket_id: Uuid =
        sqlx::query_scalar("SELECT id FROM open_tickets WHERE booking_id = $1 AND status = 'open'")
            .bind(booking.id)
            .fetch_one(&pool)
            .await
            .unwrap();

    // Move the ticket to T2 → T1 freed, T2 seated, booking assignment follows.
    let req = test::TestRequest::patch()
        .uri(&format!("/open-tickets/{ticket_id}/table"))
        .insert_header(auth.clone())
        .set_json(&serde_json::json!({ "table_id": t2.id }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "move table: {:?}",
        resp.status()
    );

    let t1_status: String = sqlx::query_scalar("SELECT status FROM branch_tables WHERE id = $1")
        .bind(t1.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let t2_status: String = sqlx::query_scalar("SELECT status FROM branch_tables WHERE id = $1")
        .bind(t2.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(t1_status, "free");
    assert_eq!(t2_status, "seated");
    let assigned: Vec<Uuid> =
        sqlx::query_scalar("SELECT table_id FROM booking_tables WHERE booking_id = $1")
            .bind(booking.id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(assigned, vec![t2.id]);
}

#[sqlx::test]
async fn no_show_marks_table_free(pool: PgPool) {
    let app = app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let admin = seed_admin(&pool, org_id).await;
    grant_all(&pool).await;
    let token = admin_token(admin, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    let t1: FloorTable = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/floor/tables")
                .insert_header(auth.clone())
                .set_json(&serde_json::json!({ "branch_id": branch_id, "label": "T1" }))
                .to_request(),
        )
        .await,
    )
    .await;
    let booking: BookingView = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/reservations")
                .insert_header(auth.clone())
                .set_json(&serde_json::json!({
                    "branch_id": branch_id, "customer_name": "Late", "customer_phone": "01007654321"
                }))
                .to_request(),
        )
        .await,
    )
    .await;
    // Seat then no-show → table goes back to free.
    test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/reservations/{}/assign", booking.id))
            .insert_header(auth.clone())
            .set_json(&serde_json::json!({ "table_ids": [t1.id] }))
            .to_request(),
    )
    .await;
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/reservations/{}", booking.id))
            .insert_header(auth.clone())
            .set_json(&serde_json::json!({ "status": "no_show" }))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());
    let status: String = sqlx::query_scalar("SELECT status FROM branch_tables WHERE id = $1")
        .bind(t1.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "free");
}

#[sqlx::test]
async fn public_booking_requires_verified_phone(pool: PgPool) {
    let app = app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    // Branch must be accepting waitlist for a no-time booking.
    sqlx::query(
        "INSERT INTO branch_reservation_settings (branch_id, accepting_waitlist) VALUES ($1, true)",
    )
    .bind(branch_id)
    .execute(&pool)
    .await
    .unwrap();

    let phone = crate::delivery::normalize_phone("01001234567").unwrap();

    // Without a valid device token → 401.
    let req = test::TestRequest::post()
        .uri("/public/reservations")
        .set_json(&serde_json::json!({
            "branch_id": branch_id, "customer_name": "Guest",
            "customer_phone": "01001234567", "device_token": "bogus", "party_size": 2
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 401, "bogus token should 401");

    // With a real device token for the phone → created.
    let token = crate::delivery::whatsapp::issue_device_token("secret", &phone).unwrap();
    let req = test::TestRequest::post()
        .uri("/public/reservations")
        .set_json(&serde_json::json!({
            "branch_id": branch_id, "customer_name": "Guest",
            "customer_phone": "01001234567", "device_token": token, "party_size": 2
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "verified booking: {:?}",
        resp.status()
    );
}

/// The host board's default view — `GET /reservations?branch_id=…` with no
/// status and no date — plus each filter combination. Every branch of the
/// query builder must bind exactly the parameters its SQL references.
#[sqlx::test]
async fn list_bookings_every_filter_combination(pool: PgPool) {
    let app = app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let admin = seed_admin(&pool, org_id).await;
    grant_all(&pool).await;
    let token = admin_token(admin, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    for q in [
        String::new(),
        "&status=confirmed".to_string(),
        "&date=2026-08-03".to_string(),
        "&status=confirmed&date=2026-08-03".to_string(),
    ] {
        let req = test::TestRequest::get()
            .uri(&format!("/reservations?branch_id={branch_id}{q}"))
            .insert_header(auth.clone())
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(
            resp.status(),
            200,
            "GET /reservations?branch_id=…{q} must succeed"
        );
    }
}

/// The date filter must resolve in the BRANCH's effective timezone, the way
/// every other date bucket in the codebase does — not in whatever zone the
/// Postgres server happens to be configured with. Casting a `timestamptz`
/// straight to `::date` silently uses the session zone, so a late-night
/// booking lands on the wrong calendar day (and the day it lands on changes
/// between dev, CI and prod depending on the server's `TimeZone` setting).
#[sqlx::test]
async fn booking_date_filter_uses_branch_timezone(pool: PgPool) {
    let app = app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let admin = seed_admin(&pool, org_id).await;
    grant_all(&pool).await;
    let token = admin_token(admin, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    // A zone far from both UTC and the server default, so the assertion can't
    // pass by accident on a differently-configured Postgres.
    // 2026-08-02T12:00Z is Aug 3 in Kiritimati (UTC+14) but Aug 2 in Cairo/UTC.
    sqlx::query("UPDATE branches SET timezone = 'Pacific/Kiritimati'::timezone_name WHERE id = $1")
        .bind(branch_id)
        .execute(&pool)
        .await
        .unwrap();

    let created: BookingView = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/reservations")
                .insert_header(auth.clone())
                .set_json(&serde_json::json!({
                    "branch_id": branch_id,
                    "customer_name": "Late Night",
                    "customer_phone": "01001112222",
                    "party_size": 2,
                    "reserved_for": "2026-08-02T12:00:00Z"
                }))
                .to_request(),
        )
        .await,
    )
    .await;

    let list = |date: &str| {
        test::TestRequest::get()
            .uri(&format!("/reservations?branch_id={branch_id}&date={date}"))
            .insert_header(auth.clone())
            .to_request()
    };

    let on_local_day: Vec<BookingView> =
        test::read_body_json(test::call_service(&app, list("2026-08-03")).await).await;
    assert!(
        on_local_day.iter().any(|b| b.id == created.id),
        "the booking must appear on its BRANCH-LOCAL day (Aug 3 in Kiritimati)"
    );

    let on_utc_day: Vec<BookingView> =
        test::read_body_json(test::call_service(&app, list("2026-08-02")).await).await;
    assert!(
        !on_utc_day.iter().any(|b| b.id == created.id),
        "and must NOT appear on the UTC/session-zone day (Aug 2)"
    );
}

/// Times that reach a CUSTOMER (the WhatsApp departure nudge) must be the
/// branch's local wall clock, not the UTC instant. Formatting `reserved_for`
/// directly told a customer with an 8pm Cairo booking to arrive at 17:00.
#[sqlx::test]
async fn nudge_time_is_branch_local_not_utc(pool: PgPool) {
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;

    // Cairo is UTC+3 in August (DST): 17:00Z is a 20:00 booking.
    let at = chrono::DateTime::parse_from_rfc3339("2026-08-02T17:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);

    let cairo = crate::reservations::bookings::local_hhmm(&pool, branch_id, at)
        .await
        .unwrap();
    assert_eq!(
        cairo, "20:00",
        "a Cairo branch must read its own wall clock"
    );

    sqlx::query("UPDATE branches SET timezone = 'Pacific/Kiritimati'::timezone_name WHERE id = $1")
        .bind(branch_id)
        .execute(&pool)
        .await
        .unwrap();
    let kiritimati = crate::reservations::bookings::local_hhmm(&pool, branch_id, at)
        .await
        .unwrap();
    assert_eq!(
        kiritimati, "07:00",
        "the zone is dynamic per branch, not hardcoded to Cairo"
    );
}

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
