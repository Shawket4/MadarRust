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

    // Move the ticket to T2 → T1 dirty, T2 seated, booking assignment follows.
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
    assert_eq!(t1_status, "dirty");
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
