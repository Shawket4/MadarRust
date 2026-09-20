//! Dashboard-integration follow-ups: force-close reconciliation, till
//! deductions, coded payment-method refusals, tenant grants, branch settings.
use actix_web::{App, http::StatusCode, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::{JwtSecret, create_token};
use madar_rust::models::UserRole;

const SECRET: &str = "test_secret";
const ORG: &str = "10000000-0000-4000-8000-000000000001";
const BRANCH_A: &str = "10000000-0000-4000-8000-0000000000a1";
const ADMIN: &str = "10000000-0000-4000-8000-00000000ad01";
const TELLER_A: &str = "10000000-0000-4000-8000-00000000ee0a";
const ITEM: &str = "10000000-0000-4000-8000-0000000e0001";

fn uid(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap()
}

fn bearer(user: &str, role: UserRole) -> (&'static str, String) {
    let tok = create_token(
        &JwtSecret(SECRET.into()),
        uid(user),
        Some(uid(ORG)),
        role,
        None,
        24,
    )
    .unwrap();
    ("Authorization", format!("Bearer {tok}"))
}

async fn seeded(pool: &PgPool) {
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    let seed = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/legacy_till_golden/seed.sql"
    ))
    .unwrap();
    sqlx::raw_sql(&seed).execute(pool).await.expect("seed.sql");
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(JwtSecret(SECRET.into())))
                .app_data(web::Data::new(madar_rust::realtime::hub::BranchEventHub::new()))
                .configure(madar_rust::tills::legacy_routes::configure)
                .configure(madar_rust::tills::routes::configure)
                .configure(madar_rust::orders::routes::configure)
                .configure(madar_rust::branches::routes::configure)
                .configure(madar_rust::payment_methods::routes::configure)
                .configure(madar_rust::sync::routes::configure)
                .configure(|c| madar_rust::reports::routes::configure(c, web::Data::new($pool.clone()))),
        )
        .await
    };
}

/// Open a till for teller A at branch A (new route) and ring one cash and one card sale.
async fn till_with_sales<S, B>(app: &S) -> Uuid
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    let till = Uuid::new_v4();
    let r = test::call_service(
        app,
        test::TestRequest::post()
            .uri(&format!("/tills/branches/{BRANCH_A}/open"))
            .insert_header(bearer(TELLER_A, UserRole::Teller))
            .set_json(json!({ "id": till, "opening_cash": 1000 }))
            .to_request(),
    )
    .await;
    assert!(r.status().is_success(), "open: {}", r.status());
    for (method, tendered) in [("cash", Some(5000)), ("card", None)] {
        let mut body = json!({
            "branch_id": BRANCH_A, "till_id": till, "payment_method": method,
            "idempotency_key": Uuid::new_v4(),
            "items": [{ "menu_item_id": ITEM, "quantity": 1, "unit_price": 5000, "addons": [], "optional_field_ids": [] }],
            "subtotal": 5000, "tax_amount": 0, "total_amount": 5000
        });
        if let Some(t) = tendered {
            body["amount_tendered"] = json!(t);
            body["change_given"] = json!(0);
        }
        let r = test::call_service(
            app,
            test::TestRequest::post()
                .uri("/orders")
                .insert_header(bearer(TELLER_A, UserRole::Teller))
                .set_json(body)
                .to_request(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::CREATED);
    }
    till
}

// ── (c) force-close stores nothing as counted ───────────────────────────────

#[sqlx::test]
async fn force_close_reconciliation_is_not_counted(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let till = till_with_sales(&app).await;

    let r = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/tills/{till}/force-close"))
            .insert_header(bearer(ADMIN, UserRole::OrgAdmin))
            .set_json(json!({ "reason": "teller went home" }))
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let body: Value = test::read_body_json(r).await;
    assert_eq!(body["status"], "force_closed");
    assert_eq!(body["reconciliation_status"], "unreviewed");
    assert!(body["closing_cash_declared"].is_null());

    let lines: Vec<(String, bool, String, Option<i32>, i32)> = sqlx::query_as(
        "SELECT method, is_cash, status, declared_amount, system_total FROM till_reconciliations WHERE till_id = $1 ORDER BY is_cash DESC, method",
    )
    .bind(till)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(lines.len(), 2, "{lines:?}");
    let cash = &lines[0];
    assert!(cash.1);
    assert_eq!(cash.2, "unreviewed", "the cash row was never counted");
    assert_eq!(cash.3, None, "no declared amount on a force-close");
    assert_eq!(cash.4, 6000, "the system total is still snapshotted");
    assert_eq!(
        (lines[1].0.as_str(), lines[1].2.as_str(), lines[1].3),
        ("card", "unreviewed", None)
    );

    // A repeated force-close does not rewrite the lines.
    let r = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/tills/{till}/force-close"))
            .insert_header(bearer(ADMIN, UserRole::OrgAdmin))
            .set_json(json!({}))
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM till_reconciliations WHERE till_id = $1")
        .bind(till)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 2);
}

#[sqlx::test]
async fn legacy_force_close_shape_unchanged(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let till = till_with_sales(&app).await;
    let r = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/{till}/force-close"))
            .insert_header(bearer(ADMIN, UserRole::OrgAdmin))
            .set_json(json!({ "reason": "old dashboard" }))
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let body: Value = test::read_body_json(r).await;
    // The legacy Shift keeps its keys and values (the golden checks the full shape).
    assert_eq!(body["status"], "force_closed");
    assert_eq!(body["id"], json!(till));
    assert!(body.get("till_id").is_some());
    assert!(body["closing_cash_declared"].is_null());
    let (status, declared): (String, Option<i32>) = sqlx::query_as(
        "SELECT status, declared_amount FROM till_reconciliations WHERE till_id = $1 AND is_cash",
    )
    .bind(till)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((status.as_str(), declared), ("unreviewed", None));
}

#[core::prelude::v1::test]
fn force_close_lines_clear_every_count() {
    use madar_rust::tills::reconcile::*;
    let totals = vec![
        MethodTotal {
            method: "cash".into(),
            payment_method_id: None,
            is_cash: true,
            system_total: 900,
            order_count: 2,
        },
        MethodTotal {
            method: "card".into(),
            payment_method_id: None,
            is_cash: false,
            system_total: 400,
            order_count: 1,
        },
    ];
    let lines = force_close_lines(plan_lines(&totals, 900, 900, None, &[], true).unwrap());
    assert!(
        lines.iter().all(|l| l.status == STATUS_UNREVIEWED
            && l.declared_amount.is_none()
            && l.note.is_none())
    );
    assert_eq!(lines[0].system_total, 900);
}

// ── (g) till deductions come from the ledger ────────────────────────────────

#[sqlx::test]
async fn till_deductions_list_the_ledger_rows_of_the_tills_orders(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let till = till_with_sales(&app).await;
    let orders: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM orders WHERE till_id = $1 ORDER BY order_number")
            .bind(till)
            .fetch_all(&pool)
            .await
            .unwrap();
    let beans: Uuid = sqlx::query_scalar(
        "INSERT INTO org_ingredients (org_id, name, unit, category_id) \
         VALUES ($1, 'Beans', 'g'::inventory_unit, ingredient_category_id($1, 'coffee_bean')) RETURNING id",
    )
    .bind(uid(ORG))
    .fetch_one(&pool)
    .await
    .unwrap();
    // A sale on the first order, its void restock, and branch waste (no till).
    for (qty, kind, src_type, src) in [
        (-18.0, "sale", Some("order"), Some(orders[0])),
        (18.0, "void_restock", Some("order"), Some(orders[0])),
        (-5.0, "waste", Some("waste"), None),
    ] {
        sqlx::query(
            "INSERT INTO inventory_movements (branch_id, org_ingredient_id, type, quantity, balance_after, source_type, source_id) \
             VALUES ($1, $2, $3::inventory_movement_type, $4::float8::numeric, 0, $5, $6)",
        )
        .bind(uid(BRANCH_A))
        .bind(beans)
        .bind(kind)
        .bind(qty)
        .bind(src_type)
        .bind(src)
        .execute(&pool)
        .await
        .unwrap();
    }

    for path in [
        format!("/reports/tills/{till}/deductions"),
        format!("/reports/shifts/{till}/deductions"),
    ] {
        let r = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&path)
                .insert_header(bearer(ADMIN, UserRole::OrgAdmin))
                .to_request(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK, "{path}");
        let rows: Vec<Value> = test::read_body_json(r).await;
        assert_eq!(rows.len(), 2, "{path}: {rows:?}");
        assert_eq!(rows[0]["source"], "sale");
        assert_eq!(rows[0]["quantity_deducted"], json!(18.0));
        assert_eq!(rows[0]["order_id"], json!(orders[0]));
        assert_eq!(rows[0]["inventory_item_id"], json!(beans));
        assert_eq!(rows[0]["item_name"], "Beans");
        assert_eq!(rows[0]["unit"], "g");
        assert!(rows[0]["order_item_id"].is_null());
        assert_eq!(rows[1]["source"], "void_restock");
        assert_eq!(rows[1]["quantity_deducted"], json!(-18.0));
    }
}

// ── (d) coded payment-method refusal ────────────────────────────────────────

#[sqlx::test]
async fn empty_restricted_allow_list_has_a_structured_code(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let r = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!(
                "/payment-methods/availability/branches/{BRANCH_A}"
            ))
            .insert_header(bearer(ADMIN, UserRole::OrgAdmin))
            .set_json(json!({ "restricted": true, "payment_method_ids": [] }))
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let body: Value = test::read_body_json(r).await;
    assert_eq!(body["code"], "EMPTY_ALLOW_LIST");
    assert_eq!(
        body["error"],
        "EMPTY_ALLOW_LIST: a restricted list needs at least one payment method"
    );
}

#[core::prelude::v1::test]
fn reconciliation_refusals_carry_codes() {
    use madar_rust::tills::reconcile::*;
    let totals = vec![
        MethodTotal {
            method: "cash".into(),
            payment_method_id: None,
            is_cash: true,
            system_total: 0,
            order_count: 0,
        },
        MethodTotal {
            method: "card".into(),
            payment_method_id: None,
            is_cash: false,
            system_total: 900,
            order_count: 1,
        },
    ];
    let input = |amount, note: Option<&str>| ReconciliationInput {
        method: "card".into(),
        status: "disagreed".into(),
        declared_amount: amount,
        note: note.map(Into::into),
    };
    let e = plan_lines(&totals, 0, 0, None, &[input(None, Some("x"))], false).unwrap_err();
    assert!(
        matches!(&e, madar_rust::errors::AppError::Coded { status: 400, code, .. } if *code == CODE_AMOUNT_REQUIRED)
    );
    assert!(
        e.to_string()
            .starts_with("RECONCILIATION_AMOUNT_REQUIRED: ")
    );
    let e = plan_lines(&totals, 0, 0, None, &[input(Some(800), None)], false).unwrap_err();
    assert!(
        matches!(&e, madar_rust::errors::AppError::Coded { status: 400, code, .. } if *code == CODE_NOTE_REQUIRED)
    );
}

// ── (e) tenant grants ───────────────────────────────────────────────────────

#[sqlx::test]
async fn madar_app_can_use_every_table(pool: PgPool) {
    let missing: Vec<String> = sqlx::query_scalar(
        "SELECT n.nspname || '.' || c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
          WHERE n.nspname = 'public' AND c.relkind IN ('r', 'p', 'v') \
            AND NOT (has_table_privilege('madar_app', c.oid, 'SELECT') AND has_table_privilege('madar_app', c.oid, 'INSERT')) \
          ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(missing.is_empty(), "madar_app lacks grants on {missing:?}");
    let archive: bool = sqlx::query_scalar(
        "SELECT has_schema_privilege('madar_app', 'archive', 'USAGE') AND has_table_privilege('madar_app', 'archive.till_entities', 'SELECT')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(archive);
}

/// The legacy entity id comes from the archive through the TENANT pool, and
/// only for the caller's org.
#[sqlx::test]
async fn legacy_entity_id_reads_the_archive_as_the_tenant(pool: PgPool) {
    seeded(&pool).await;
    let archived = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO archive.till_entities (id, org_id, branch_id, name, is_default, is_active, created_at, updated_at) \
         VALUES ($1, $2, $3, 'Till 1', true, true, now(), now())",
    )
    .bind(archived)
    .bind(uid(ORG))
    .bind(uid(BRANCH_A))
    .execute(&pool)
    .await
    .unwrap();
    let tenant = madar_rust::db::tenant_pool(&pool, uid(ORG)).await;
    assert_eq!(
        madar_rust::tills::legacy::legacy_till_entity_id(&tenant, uid(BRANCH_A)).await,
        archived
    );
    let other = madar_rust::db::tenant_pool(&pool, Uuid::new_v4()).await;
    assert_eq!(
        madar_rust::tills::legacy::legacy_till_entity_id(&other, uid(BRANCH_A)).await,
        madar_rust::tills::legacy::synthesized_till_id(uid(BRANCH_A)),
        "another org never sees the row"
    );
}

// ── (a) branch settings ─────────────────────────────────────────────────────

#[sqlx::test]
async fn branch_old_bill_hours_and_standard_float_read_and_write(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let admin = bearer(ADMIN, UserRole::OrgAdmin);
    let get = |h: (&'static str, String)| {
        test::TestRequest::get()
            .uri(&format!("/branches/{BRANCH_A}"))
            .insert_header(h)
            .to_request()
    };

    let r = test::call_service(&app, get(admin.clone())).await;
    assert_eq!(r.status(), StatusCode::OK);
    let b: Value = test::read_body_json(r).await;
    assert_eq!(b["old_bill_hours"], 3);
    assert!(b["standard_float"].is_null());

    // PATCH sets both.
    let r = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/branches/{BRANCH_A}"))
            .insert_header(admin.clone())
            .set_json(json!({ "old_bill_hours": 6, "standard_float": 50000 }))
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let b: Value = test::read_body_json(r).await;
    assert_eq!(
        (b["old_bill_hours"].clone(), b["standard_float"].clone()),
        (json!(6), json!(50000))
    );

    // PUT leaves absent fields alone; explicit null clears the float.
    let r = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!("/branches/{BRANCH_A}"))
            .insert_header(admin.clone())
            .set_json(json!({ "name": "Golden A2" }))
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let b: Value = test::read_body_json(r).await;
    assert_eq!(
        (b["old_bill_hours"].clone(), b["standard_float"].clone()),
        (json!(6), json!(50000))
    );
    let r = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!("/branches/{BRANCH_A}"))
            .insert_header(admin.clone())
            .set_json(json!({ "standard_float": null }))
            .to_request(),
    )
    .await;
    let b: Value = test::read_body_json(r).await;
    assert!(b["standard_float"].is_null());
    assert_eq!(b["old_bill_hours"], 6);

    // Validation: out of range → 400, nothing written.
    for bad in [
        json!({ "old_bill_hours": 0 }),
        json!({ "old_bill_hours": 169 }),
        json!({ "standard_float": -1 }),
    ] {
        let r = test::call_service(
            &app,
            test::TestRequest::patch()
                .uri(&format!("/branches/{BRANCH_A}"))
                .insert_header(admin.clone())
                .set_json(bad.clone())
                .to_request(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
    let (hours, float): (i16, Option<i32>) =
        sqlx::query_as("SELECT old_bill_hours, standard_float FROM branches WHERE id = $1")
            .bind(uid(BRANCH_A))
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((hours, float), (6, None));

    // The till report's standard float reads the branch column.
    sqlx::query("UPDATE branches SET standard_float = 40000 WHERE id = $1")
        .bind(uid(BRANCH_A))
        .execute(&pool)
        .await
        .unwrap();
    let r = test::call_service(&app, get(admin.clone())).await;
    let b: Value = test::read_body_json(r).await;
    assert_eq!(b["standard_float"], 40000);
}

// ── (h) order responses carry the device numbering ──────────────────────────

#[sqlx::test]
async fn order_responses_carry_device_numbering(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let till = till_with_sales(&app).await; // two server-numbered sales
    let device = Uuid::new_v4();
    let teller = bearer(TELLER_A, UserRole::Teller);
    let r = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/orders")
            .insert_header(teller.clone())
            .set_json(json!({
                "branch_id": BRANCH_A, "till_id": till, "payment_method": "cash", "idempotency_key": Uuid::new_v4(),
                "device_id": device, "device_code": "36B", "order_number": 12, "verification": "lan",
                "order_ref": "GLDA-260913-36B-0012",
                "items": [{ "menu_item_id": ITEM, "quantity": 1, "unit_price": 5000, "addons": [], "optional_field_ids": [] }],
                "subtotal": 5000, "tax_amount": 0, "total_amount": 5000, "amount_tendered": 5000, "change_given": 0
            }))
            .to_request(),
    )
    .await;
    let status = r.status();
    let created: Value = test::read_body_json(r).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let expect = |o: &Value, what: &str| {
        assert_eq!(o["device_id"], json!(device), "{what}");
        assert_eq!(o["device_code"], "36B", "{what}");
        assert_eq!(o["display_number"], "36B-12", "{what}");
        assert_eq!(
            o["verification"], "server",
            "{what}: a live sale is server-verified"
        );
        assert_eq!(o["order_number"], 12, "{what}");
    };
    expect(&created, "create");
    let id = created["id"].as_str().unwrap().to_string();

    let r = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/orders/{id}"))
            .insert_header(teller.clone())
            .to_request(),
    )
    .await;
    expect(&test::read_body_json::<Value, _>(r).await, "get");

    let r = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/orders?branch_id={BRANCH_A}&till_id={till}"))
            .insert_header(teller.clone())
            .to_request(),
    )
    .await;
    let list: Value = test::read_body_json(r).await;
    let rows = list["data"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    expect(rows.iter().find(|o| o["id"] == json!(id)).unwrap(), "list");
    // Server-numbered sales: no device, the bare number, verification as stored.
    let plain = rows.iter().find(|o| o["order_number"] == 1).unwrap();
    assert!(plain["device_id"].is_null() && plain["device_code"].is_null());
    assert_eq!(plain["display_number"], "1");
    assert_eq!(plain["verification"], "server");
}

/// A replayed sale from a device that never registered lands (and registers
/// the device) instead of failing on the devices FK and dead-lettering.
#[sqlx::test]
async fn replayed_sale_registers_an_unknown_device(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let till = till_with_sales(&app).await;
    let device = Uuid::new_v4();
    let r = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(bearer(TELLER_A, UserRole::Teller))
            .set_json(json!({ "op": "create_order", "teller_id": TELLER_A, "device_id": device, "device_code": "7QX",
                "request": {
                    "branch_id": BRANCH_A, "till_id": till, "payment_method": "cash", "idempotency_key": Uuid::new_v4(),
                    "order_number": 3, "verification": "unverified", "order_ref": "GLDA-260913-7QX-0003",
                    "items": [{ "menu_item_id": ITEM, "quantity": 1, "unit_price": 5000, "addons": [], "optional_field_ids": [] }],
                    "subtotal": 5000, "tax_amount": 0, "total_amount": 5000, "amount_tendered": 5000, "change_given": 0 } }))
            .to_request(),
    )
    .await;
    let status = r.status();
    let body: Value = test::read_body_json(r).await;
    assert!(status.is_success(), "{status}: {body}");
    assert_eq!(body["display_number"], "7QX-3");
    assert_eq!(body["verification"], "unverified");
    let code: String = sqlx::query_scalar("SELECT code FROM devices WHERE id = $1")
        .bind(device)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(code, "7QX");
}
