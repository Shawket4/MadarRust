//! The branch plan (kitchen target spec BB-*, CH-*): read whole, saved whole.

use actix_web::{App, http::StatusCode, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::{JwtSecret, create_token};
use madar_rust::branch_plan::{
    BranchPlan, PlanDevice, PlanPrinter, PlanSection, check_plan, routing_mode_for,
};
use madar_rust::models::UserRole;

fn secret() -> JwtSecret {
    JwtSecret("secret".into())
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1,'Org',$2)")
        .bind(id)
        .bind(format!("org-{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_branch(pool: &PgPool, org: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1,$2,'Branch')")
        .bind(id)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_user(pool: &PgPool, org: Uuid, role: UserRole, role_name: &str) -> String {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, name, role, org_id, email, password_hash)
         VALUES ($1, 'U', $2::user_role, $3, $4, 'x')",
    )
    .bind(id)
    .bind(role_name)
    .bind(org)
    .bind(format!("{id}@t.com"))
    .execute(pool)
    .await
    .unwrap();
    create_token(&secret(), id, Some(org), role, None, 24).unwrap()
}

async fn seed_category(pool: &PgPool, org: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO categories (org_id, name) VALUES ($1,$2) RETURNING id")
        .bind(org)
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(
                    madar_rust::realtime::hub::BranchEventHub::new(),
                ))
                .configure(madar_rust::branch_plan::routes::configure)
                .configure(madar_rust::devices::routes::configure)
                .configure(madar_rust::auth::routes::configure),
        )
        .await
    };
}

async fn call(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    req: test::TestRequest,
    bearer: Option<&str>,
) -> (StatusCode, Value) {
    let req = match bearer {
        Some(b) => req.insert_header(("Authorization", format!("Bearer {b}"))),
        None => req,
    };
    let resp = test::call_service(app, req.to_request()).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

async fn setup(pool: &PgPool) -> (Uuid, Uuid, String) {
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    let org = seed_org(pool).await;
    let branch = seed_branch(pool, org).await;
    let owner = seed_user(pool, org, UserRole::OrgAdmin, "org_admin").await;
    (org, branch, owner)
}

// ── Pieces ──────────────────────────────────────────────────

fn device(kind: &str, name: &str) -> PlanDevice {
    PlanDevice {
        id: Uuid::new_v4(),
        kind: kind.into(),
        name: name.into(),
        receipt_printer_id: None,
        device_id: None,
        x: 0.0,
        y: 0.0,
    }
}

fn printer(role: &str, connection: &str) -> PlanPrinter {
    PlanPrinter {
        id: Uuid::new_v4(),
        role: role.into(),
        name: format!("{role} printer"),
        connection: connection.into(),
        brand: None,
        ip: (connection == "network").then(|| "192.168.1.40".to_string()),
        port: (connection == "network").then_some(9100),
        paper_mm: 80,
        host_device_id: None,
        x: 0.0,
        y: 0.0,
    }
}

fn section(name: &str, is_default: bool) -> PlanSection {
    PlanSection {
        id: Uuid::new_v4(),
        name: name.into(),
        is_default,
        category_ids: vec![],
        screen_ids: vec![],
        printer_ids: vec![],
        x: 0.0,
        y: 0.0,
    }
}

/// Case 3 of the spec: a till with a receipt printer, one kitchen screen that
/// shows every category.
fn till_and_screen(categories: Vec<Uuid>) -> BranchPlan {
    let receipt = printer("receipt", "usb");
    let mut till = device("pos", "Till 1");
    till.receipt_printer_id = Some(receipt.id);
    let receipt = PlanPrinter {
        host_device_id: Some(till.id),
        ..receipt
    };
    let screen = device("kitchen", "Kitchen screen");
    let mut kitchen = section("Kitchen", true);
    kitchen.category_ids = categories;
    kitchen.screen_ids = vec![screen.id];
    BranchPlan {
        devices: vec![till, screen],
        printers: vec![receipt],
        sections: vec![kitchen],
        till_prints_kitchen: false,
    }
}

// ── Pure rules ──────────────────────────────────────────────

#[::core::prelude::v1::test]
fn a_sound_plan_has_no_problems() {
    assert!(check_plan(&till_and_screen(vec![])).is_empty());
    assert!(check_plan(&BranchPlan::default()).is_empty());
}

#[::core::prelude::v1::test]
fn a_section_with_nowhere_to_send_orders_is_refused() {
    let mut plan = till_and_screen(vec![]);
    plan.sections[0].screen_ids.clear();
    let problems = check_plan(&plan);
    assert!(
        problems.iter().any(|p| p.contains("go nowhere")),
        "{problems:?}"
    );
}

#[::core::prelude::v1::test]
fn a_plugged_in_printer_needs_its_device_and_a_network_one_its_address() {
    let mut plan = till_and_screen(vec![]);
    plan.printers[0].host_device_id = None;
    assert!(check_plan(&plan).iter().any(|p| p.contains("plugged in")));

    let mut plan = till_and_screen(vec![]);
    let mut kp = printer("kitchen", "network");
    kp.ip = Some("not-an-ip".into());
    plan.sections[0].printer_ids.push(kp.id);
    plan.printers.push(kp);
    assert!(check_plan(&plan).iter().any(|p| p.contains("valid IP")));
}

#[::core::prelude::v1::test]
fn links_must_point_at_the_right_kind_of_piece() {
    let mut plan = till_and_screen(vec![]);
    // A section "showing" on the till is nonsense.
    let till = plan.devices[0].id;
    plan.sections[0].screen_ids.push(till);
    assert!(
        check_plan(&plan)
            .iter()
            .any(|p| p.contains("not a kitchen screen"))
    );
}

#[::core::prelude::v1::test]
fn one_default_section_and_one_section_per_category() {
    let cat = Uuid::new_v4();
    let mut plan = till_and_screen(vec![cat]);
    let mut second = section("Kitchen", true);
    second.screen_ids = plan.sections[0].screen_ids.clone();
    second.category_ids = vec![cat];
    plan.sections.push(second);
    let problems = check_plan(&plan);
    assert!(
        problems.iter().any(|p| p.contains("Exactly one section")),
        "{problems:?}"
    );
    assert!(
        problems.iter().any(|p| p.contains("two sections")),
        "{problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains("Two sections are called")),
        "{problems:?}"
    );
}

#[::core::prelude::v1::test]
fn the_routing_mode_follows_the_plan() {
    let mut plan = BranchPlan::default();
    assert_eq!(routing_mode_for(&plan), "off");
    plan.till_prints_kitchen = true;
    assert_eq!(routing_mode_for(&plan), "till");

    let plan = till_and_screen(vec![]);
    assert_eq!(routing_mode_for(&plan), "kds");
    let plan = BranchPlan {
        till_prints_kitchen: true,
        ..till_and_screen(vec![])
    };
    assert_eq!(routing_mode_for(&plan), "both");

    // Case 2: a kitchen printer and no screen — chits are printed from the till.
    let mut plan = till_and_screen(vec![]);
    let kp = printer("kitchen", "network");
    plan.sections[0].screen_ids.clear();
    plan.sections[0].printer_ids = vec![kp.id];
    plan.printers.push(kp);
    assert_eq!(routing_mode_for(&plan), "till");
}

// ── Reading a branch never saved ────────────────────────────

#[sqlx::test]
async fn a_branch_never_saved_opens_on_what_it_already_has(pool: PgPool) {
    let (org, branch, owner) = setup(&pool).await;
    let app = app!(pool);
    let cat = seed_category(&pool, org, "Burgers").await;
    let grill: Uuid = sqlx::query_scalar(
        "INSERT INTO kitchen_stations (org_id, branch_id, name, is_default, printer_brand, printer_ip, printer_port)
         VALUES ($1, $2, 'Grill', true, 'epson', '192.168.1.50', 9100) RETURNING id",
    )
    .bind(org)
    .bind(branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO category_station_routes (branch_id, category_id, station_id) VALUES ($1,$2,$3)")
        .bind(branch)
        .bind(cat)
        .bind(grill)
        .execute(&pool)
        .await
        .unwrap();
    let kds = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO devices (id, org_id, branch_id, code, kind) VALUES ($1,$2,$3,'K1','kds')",
    )
    .bind(kds)
    .bind(org)
    .bind(branch)
    .execute(&pool)
    .await
    .unwrap();

    let (st, view) = call(
        &app,
        test::TestRequest::get().uri(&format!("/branch-plan?branch_id={branch}")),
        Some(&owner),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{view}");
    assert_eq!(view["saved"], false);
    assert_eq!(view["version"], 0);
    let sections = view["plan"]["sections"].as_array().unwrap();
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0]["id"], grill.to_string());
    assert_eq!(sections[0]["category_ids"][0], cat.to_string());
    // The station's own printer became a kitchen printer piece, linked.
    let printers = view["plan"]["printers"].as_array().unwrap();
    assert_eq!(printers.len(), 1);
    assert_eq!(printers[0]["role"], "kitchen");
    assert_eq!(printers[0]["ip"], "192.168.1.50");
    assert_eq!(sections[0]["printer_ids"][0], printers[0]["id"]);
    // The registered KDS became a kitchen screen slot it already fills.
    let devices = view["plan"]["devices"].as_array().unwrap();
    assert_eq!(devices[0]["kind"], "kitchen");
    assert_eq!(devices[0]["device_id"], kds.to_string());
    // Stable: the same pieces on a second read, so an unsaved draft keeps its ids.
    let (_, again) = call(
        &app,
        test::TestRequest::get().uri(&format!("/branch-plan?branch_id={branch}")),
        Some(&owner),
    )
    .await;
    assert_eq!(again["plan"], view["plan"]);
}

// ── Saving ──────────────────────────────────────────────────

#[sqlx::test]
async fn saving_writes_the_whole_plan_and_the_columns_the_pos_reads(pool: PgPool) {
    let (org, branch, owner) = setup(&pool).await;
    let app = app!(pool);
    let burgers = seed_category(&pool, org, "Burgers").await;
    let drinks = seed_category(&pool, org, "Drinks").await;

    // Case 4: two sections; drinks print on a network kitchen printer.
    let mut plan = till_and_screen(vec![burgers]);
    let kp = printer("kitchen", "network");
    let mut bar = section("Bar", false);
    bar.category_ids = vec![drinks];
    bar.printer_ids = vec![kp.id];
    plan.printers.push(kp.clone());
    plan.sections.push(bar.clone());

    let (st, saved) = call(
        &app,
        test::TestRequest::put()
            .uri("/branch-plan")
            .set_json(json!({ "branch_id": branch, "expected_version": 0, "plan": plan })),
        Some(&owner),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{saved}");
    assert_eq!(saved["version"], 1);
    assert_eq!(saved["saved"], true);
    assert_eq!(saved["routing_mode"], "kds");

    // Read back: the same plan, piece for piece.
    let back: BranchPlan = serde_json::from_value(saved["plan"].clone()).unwrap();
    assert_eq!(back, plan);

    // Categories are the kitchen's routing table.
    let routed: Uuid = sqlx::query_scalar(
        "SELECT station_id FROM category_station_routes WHERE branch_id = $1 AND category_id = $2",
    )
    .bind(branch)
    .bind(drinks)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(routed, bar.id);
    // The POS still prints a section's chits from the station row.
    let ip: Option<String> =
        sqlx::query_scalar("SELECT printer_ip FROM kitchen_stations WHERE id = $1")
            .bind(bar.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(ip.as_deref(), Some("192.168.1.40"));
    let versions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM branch_plan_versions WHERE branch_id = $1")
            .bind(branch)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(versions, 1);

    // CH-1: changed later — the bar's printer is replaced by a screen, and a
    // name swap between the two sections never trips the unique index.
    let mut next = back.clone();
    let screen2 = device("kitchen", "Bar screen");
    next.devices.push(screen2.clone());
    next.printers.retain(|p| p.id != kp.id);
    next.sections[1].printer_ids = vec![];
    next.sections[1].screen_ids = vec![screen2.id];
    next.sections[0].name = "Bar".into();
    next.sections[1].name = "Kitchen".into();
    let (st, saved) = call(
        &app,
        test::TestRequest::put()
            .uri("/branch-plan")
            .set_json(json!({ "branch_id": branch, "expected_version": 1, "plan": next })),
        Some(&owner),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{saved}");
    assert_eq!(saved["version"], 2);
    let ip: Option<String> =
        sqlx::query_scalar("SELECT printer_ip FROM kitchen_stations WHERE id = $1")
            .bind(bar.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(ip, None, "no printer left on that section");

    let (st, versions) = call(
        &app,
        test::TestRequest::get().uri(&format!("/branch-plan/versions?branch_id={branch}")),
        Some(&owner),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(versions.as_array().unwrap().len(), 2);
    assert_eq!(versions[0]["version"], 2);
}

#[sqlx::test]
async fn a_save_over_someone_elses_is_refused(pool: PgPool) {
    let (_, branch, owner) = setup(&pool).await;
    let app = app!(pool);
    let plan = till_and_screen(vec![]);
    let put = |v: i32| {
        test::TestRequest::put()
            .uri("/branch-plan")
            .set_json(json!({ "branch_id": branch, "expected_version": v, "plan": plan }))
    };
    let (st, _) = call(&app, put(0), Some(&owner)).await;
    assert_eq!(st, StatusCode::OK);
    let (st, body) = call(&app, put(0), Some(&owner)).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(body["code"], "PLAN_CHANGED");
}

#[sqlx::test]
async fn a_plan_that_would_lose_orders_is_refused_and_nothing_is_written(pool: PgPool) {
    let (_, branch, owner) = setup(&pool).await;
    let app = app!(pool);
    let mut plan = till_and_screen(vec![]);
    plan.sections[0].screen_ids.clear();
    let (st, _) = call(
        &app,
        test::TestRequest::put()
            .uri("/branch-plan")
            .set_json(json!({ "branch_id": branch, "expected_version": 0, "plan": plan })),
        Some(&owner),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let slots: i64 =
        sqlx::query_scalar("SELECT count(*) FROM branch_device_slots WHERE branch_id = $1")
            .bind(branch)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(slots, 0);
}

#[sqlx::test]
async fn a_section_with_items_cooking_cannot_be_removed(pool: PgPool) {
    let (org, branch, owner) = setup(&pool).await;
    let app = app!(pool);
    let plan = till_and_screen(vec![]);
    let (st, _) = call(
        &app,
        test::TestRequest::put()
            .uri("/branch-plan")
            .set_json(json!({ "branch_id": branch, "expected_version": 0, "plan": plan })),
        Some(&owner),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // A waiter's bill has a line on the kitchen section's board.
    let waiter: Uuid = sqlx::query_scalar(
        "INSERT INTO users (name, role, org_id, email, password_hash)
         VALUES ('W', 'waiter', $1, $2, 'x') RETURNING id",
    )
    .bind(org)
    .bind(format!("{}@t.com", Uuid::new_v4()))
    .fetch_one(&pool)
    .await
    .unwrap();
    let bill: Uuid = sqlx::query_scalar(
        "INSERT INTO open_tickets (org_id, branch_id, opened_by) VALUES ($1,$2,$3) RETURNING id",
    )
    .bind(org)
    .bind(branch)
    .bind(waiter)
    .fetch_one(&pool)
    .await
    .unwrap();
    let round: Uuid = sqlx::query_scalar(
        "INSERT INTO open_ticket_rounds (open_ticket_id, round_number, fired_by) VALUES ($1, 1, $2) RETURNING id",
    )
    .bind(bill)
    .bind(waiter)
    .fetch_one(&pool)
    .await
    .unwrap();
    let ticket: Uuid = sqlx::query_scalar(
        "INSERT INTO kitchen_tickets (org_id, branch_id, open_ticket_id, round_id) VALUES ($1,$2,$3,$4) RETURNING id",
    )
    .bind(org)
    .bind(branch)
    .bind(bill)
    .bind(round)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO kitchen_ticket_items (kitchen_ticket_id, station_id, line) VALUES ($1,$2,'{}')")
        .bind(ticket)
        .bind(plan.sections[0].id)
        .execute(&pool)
        .await
        .unwrap();

    let (_, view) = call(
        &app,
        test::TestRequest::get().uri(&format!("/branch-plan?branch_id={branch}")),
        Some(&owner),
    )
    .await;
    assert_eq!(view["open_items"][0]["count"], 1);

    // Back to a till only (case 1): the kitchen section would go.
    let mut till_only = plan.clone();
    till_only.sections.clear();
    till_only.devices.retain(|d| d.kind == "pos");
    till_only.till_prints_kitchen = true;
    let put = |p: &BranchPlan| {
        test::TestRequest::put()
            .uri("/branch-plan")
            .set_json(json!({ "branch_id": branch, "expected_version": 1, "plan": p }))
    };
    let (st, body) = call(&app, put(&till_only), Some(&owner)).await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "SECTION_HAS_OPEN_TICKETS");

    // Once it is bumped, the section can go; it is soft-deleted, not erased.
    sqlx::query("UPDATE kitchen_ticket_items SET bumped_at = now() WHERE kitchen_ticket_id = $1")
        .bind(ticket)
        .execute(&pool)
        .await
        .unwrap();
    let (st, saved) = call(&app, put(&till_only), Some(&owner)).await;
    assert_eq!(st, StatusCode::OK, "{saved}");
    assert_eq!(saved["routing_mode"], "till");
    let deleted: bool =
        sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM kitchen_stations WHERE id = $1")
            .bind(plan.sections[0].id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(deleted);
}

#[sqlx::test]
async fn only_someone_who_sets_up_the_kitchen_may_read_or_save(pool: PgPool) {
    let (org, branch, _) = setup(&pool).await;
    let app = app!(pool);
    let teller = seed_user(&pool, org, UserRole::Teller, "teller").await;
    let (st, _) = call(
        &app,
        test::TestRequest::get().uri(&format!("/branch-plan?branch_id={branch}")),
        Some(&teller),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _) = call(
        &app,
        test::TestRequest::put().uri("/branch-plan").set_json(
            json!({ "branch_id": branch, "expected_version": 0, "plan": till_and_screen(vec![]) }),
        ),
        Some(&teller),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

// ── Slots and activation codes (BB-8) ───────────────────────

#[sqlx::test]
async fn a_code_made_for_a_slot_puts_the_device_in_it(pool: PgPool) {
    let (_, branch, owner) = setup(&pool).await;
    let app = app!(pool);
    let plan = till_and_screen(vec![]);
    let screen = plan.devices[1].clone();
    let (st, _) = call(
        &app,
        test::TestRequest::put()
            .uri("/branch-plan")
            .set_json(json!({ "branch_id": branch, "expected_version": 0, "plan": plan })),
        Some(&owner),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let (st, code) = call(
        &app,
        test::TestRequest::post()
            .uri("/devices/activation-codes")
            .set_json(json!({ "branch_id": branch, "slot_id": screen.id })),
        Some(&owner),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{code}");
    assert_eq!(code["kind"], "kds", "a kitchen screen runs the KDS app");
    assert_eq!(code["label"], "Kitchen screen");
    assert_eq!(code["slot_id"], screen.id.to_string());

    let tablet = Uuid::new_v4();
    let (st, act) = call(
        &app,
        test::TestRequest::post()
            .uri("/auth/activate-device")
            .set_json(json!({ "code": code["code"], "device_id": tablet })),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{act}");
    assert_eq!(act["slot_id"], screen.id.to_string());

    let (_, view) = call(
        &app,
        test::TestRequest::get().uri(&format!("/branch-plan?branch_id={branch}")),
        Some(&owner),
    )
    .await;
    let slot = view["plan"]["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == screen.id.to_string())
        .unwrap()
        .clone();
    assert_eq!(slot["device_id"], tablet.to_string());

    // A slot of another branch is refused.
    let (st, _) = call(
        &app,
        test::TestRequest::post()
            .uri("/devices/activation-codes")
            .set_json(json!({ "branch_id": branch, "slot_id": Uuid::new_v4() })),
        Some(&owner),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}
