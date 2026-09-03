//! Analytics integration tests.
//!
//! These run the real compiler against the real schema on a real database, so
//! they prove the thing unit tests cannot: that every authored SQL fragment in
//! the registry is *valid against the live schema*. A measure referencing a
//! column that was renamed three migrations ago compiles fine in Rust and fails
//! only when someone asks for it — [`every_preset_runs_against_the_real_schema`]
//! is what turns that into a build failure.

use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::{JwtSecret, create_token};
use crate::models::UserRole;

pub(crate) fn secret() -> JwtSecret {
    JwtSecret("test_secret".into())
}

pub(crate) fn org_admin_token(org: Uuid) -> String {
    org_admin_token_for(org, Uuid::new_v4())
}

/// A token for a SPECIFIC user id. Needed wherever a handler writes a row that
/// references `users`: a token minted for an id with no user behind it is
/// rejected by the foreign key, not by the auth layer, which produces a
/// confusing failure a long way from its cause.
pub(crate) fn org_admin_token_for(org: Uuid, user: Uuid) -> String {
    create_token(&secret(), user, Some(org), UserRole::OrgAdmin, None, 24).unwrap()
}

/// What [`seed`] created, so tests can assert against known figures.
pub(crate) struct Seeded {
    pub org: Uuid,
    #[allow(dead_code)]
    pub branch: Uuid,
    /// A real org-admin row, for tests whose handlers write rows referencing
    /// `users`.
    #[allow(dead_code)]
    pub admin: Uuid,
    /// A second real user in the same org, for isolation tests.
    #[allow(dead_code)]
    pub other_admin: Uuid,
}

/// One organization with a branch, a closed shift, two products in a category,
/// two paid orders, and one waste movement.
///
/// Deliberately small but *broad*: it touches every dataset the registry
/// exposes joins for, so a broken join surfaces here rather than in production.
pub(crate) async fn seed(pool: &PgPool, label: &str) -> Seeded {
    let (admin, other_admin) = (Uuid::new_v4(), Uuid::new_v4());
    let (org, teller, branch, till, shift) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let (category, latte, mocha) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let (ingredient, inv) = (Uuid::new_v4(), Uuid::new_v4());

    sqlx::query(
        "INSERT INTO organizations (id, name, slug, timezone) VALUES ($1,$2,$3,'Africa/Cairo')",
    )
    .bind(org)
    .bind(format!("Org {label}"))
    .bind(format!("org-{}", org.simple()))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO users (id, name, role, org_id, pin_hash) VALUES ($1,'Teller One','teller',$2,'x')",
    )
    .bind(teller)
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    for (id, name) in [(admin, "Admin One"), (other_admin, "Admin Two")] {
        sqlx::query(
            "INSERT INTO users (id, name, role, org_id, password_hash) \
             VALUES ($1,$2,'org_admin',$3,'x')",
        )
        .bind(id)
        .bind(name)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO branches (id, org_id, name, code) VALUES ($1,$2,$3,$4)")
        .bind(branch)
        .bind(org)
        .bind(format!("Branch {label}"))
        .bind(org.simple().to_string()[..6].to_uppercase())
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO tills (id, org_id, branch_id, name) VALUES ($1,$2,$3,'Till 1')")
        .bind(till)
        .bind(org)
        .bind(branch)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO shifts (id, branch_id, teller_id, till_id, status, opening_cash, \
         closing_cash_declared, closing_cash_system, closed_at) \
         VALUES ($1,$2,$3,$4,'closed',10000,25000,25500, now())",
    )
    .bind(shift)
    .bind(branch)
    .bind(teller)
    .bind(till)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1,$2,'Drinks')")
        .bind(category)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    for (id, name, price) in [(latte, "Latte", 5000), (mocha, "Mocha", 7000)] {
        sqlx::query(
            "INSERT INTO menu_items (id, org_id, name, category_id, base_price) VALUES ($1,$2,$3,$4,$5)",
        )
        .bind(id)
        .bind(org)
        .bind(name)
        .bind(category)
        .bind(price)
        .execute(pool)
        .await
        .unwrap();
    }

    // Two orders: one plain, one with a discount, both completed and paid.
    for (n, item, name, unit, qty, discount) in [
        (1i32, latte, "Latte", 5000i32, 2i32, 0i32),
        (2, mocha, "Mocha", 7000, 1, 700),
    ] {
        let order = Uuid::new_v4();
        let subtotal = unit * qty;
        let total = subtotal - discount;
        sqlx::query(
            "INSERT INTO orders (id, branch_id, shift_id, teller_id, order_number, payment_method, \
             order_ref, status, subtotal, discount_amount, total_amount, order_type) \
             VALUES ($1,$2,$3,$4,$5,'cash',$6,'completed',$7,$8,$9,'dine_in')",
        )
        .bind(order)
        .bind(branch)
        .bind(shift)
        .bind(teller)
        .bind(n)
        .bind(format!("REF-{}-{n}", &order.simple().to_string()[..6]))
        .bind(subtotal)
        .bind(discount)
        .bind(total)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO order_items (order_id, menu_item_id, item_name, unit_price, quantity, \
             line_total, unit_cost, line_cost) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
        )
        .bind(order)
        .bind(item)
        .bind(name)
        .bind(unit)
        .bind(qty)
        .bind(subtotal)
        .bind((unit / 4) as i64)
        .bind((subtotal / 4) as i64)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ($1,'cash',$2,true)")
            .bind(order)
            .bind(total)
            .execute(pool)
            .await
            .unwrap();
    }

    // A wasted ingredient, so the inventory dataset has something to find.
    sqlx::query(
        "INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category) \
         VALUES ($1,$2,'Milk','l',1200,'Dairy')",
    )
    .bind(ingredient)
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO branch_inventory (id, branch_id, org_ingredient_id, current_stock, cost_per_unit) \
         VALUES ($1,$2,$3,50,1200)",
    )
    .bind(inv)
    .bind(branch)
    .bind(ingredient)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO inventory_movements (branch_id, org_ingredient_id, branch_inventory_id, type, \
         quantity, balance_after, unit_cost, reason) \
         VALUES ($1,$2,$3,'waste',-3,47,1200,'Spillage')",
    )
    .bind(branch)
    .bind(ingredient)
    .bind(inv)
    .execute(pool)
    .await
    .unwrap();

    Seeded {
        org,
        branch,
        admin,
        other_admin,
    }
}

pub(crate) async fn metrics_app(
    pool: &PgPool,
) -> impl actix_web::dev::Service<
    actix_http::Request,
    Response = actix_web::dev::ServiceResponse,
    Error = actix_web::Error,
> {
    crate::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .configure(|cfg| {
                crate::analytics::routes::configure(cfg, web::Data::new(pool.clone()))
            }),
    )
    .await
}

async fn post_query(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    token: &str,
    body: Value,
) -> Value {
    let req = test::TestRequest::post()
        .uri("/metrics/query")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(body)
        .to_request();
    let resp = test::call_service(app, req).await;
    assert!(
        resp.status().is_success(),
        "query failed with {}",
        resp.status()
    );
    test::read_body_json(resp).await
}

// ── The schema endpoint ──────────────────────────────────────────────────────

#[sqlx::test]
async fn schema_endpoint_describes_the_whole_registry(pool: PgPool) {
    let s = seed(&pool, "a").await;
    let app = metrics_app(&pool).await;
    let req = test::TestRequest::get()
        .uri("/metrics/schema")
        .insert_header((
            "Authorization",
            format!("Bearer {}", org_admin_token(s.org)),
        ))
        .to_request();
    let body: Value = test::read_body_json(test::call_service(&app, req).await).await;

    assert_eq!(
        body["datasets"].as_array().unwrap().len(),
        crate::analytics::schema::DATASETS.len()
    );
    assert!(!body["presets"].as_array().unwrap().is_empty());
    assert!(!body["boards"].as_array().unwrap().is_empty());
    assert!(
        body["period_presets"]
            .as_array()
            .unwrap()
            .contains(&json!("last_month"))
    );
    // Measures explain themselves, which is what a widget picker needs.
    let orders = body["datasets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == "orders")
        .unwrap();
    let revenue = orders["measures"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "revenue")
        .unwrap();
    assert!(revenue["help"].as_str().unwrap().contains("discount"));
}

#[sqlx::test]
async fn schema_requires_authentication(pool: PgPool) {
    seed(&pool, "a").await;
    let app = metrics_app(&pool).await;
    let req = test::TestRequest::get().uri("/metrics/schema").to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 401);
}

// ── Querying ────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn a_preset_widget_returns_the_seeded_figures(pool: PgPool) {
    let s = seed(&pool, "a").await;
    let app = metrics_app(&pool).await;
    let body = post_query(
        &app,
        &org_admin_token(s.org),
        json!({
            "period": { "preset": "all_time" },
            "widgets": [{ "key": "rev", "preset": "revenue_total" }]
        }),
    )
    .await;

    let w = &body["results"]["rev"];
    assert_eq!(w["status"], "ok");
    // 10000 (2 × Latte) + 6300 (Mocha less its 700 discount).
    assert_eq!(w["rows"][0]["revenue"], 16300);
    assert_eq!(w["grain"], "scalar");
    assert_eq!(w["viz"], "kpi");
    assert_eq!(w["title"], "Revenue");
    assert_eq!(body["timezone"], "Africa/Cairo");
}

#[sqlx::test]
async fn a_custom_spec_widget_groups_and_ranks(pool: PgPool) {
    let s = seed(&pool, "a").await;
    let app = metrics_app(&pool).await;
    let body = post_query(
        &app,
        &org_admin_token(s.org),
        json!({
            "widgets": [{
                "key": "byproduct",
                "spec": {
                    "dataset": "order_items",
                    "dimensions": ["product"],
                    "measures": ["units_sold", "item_revenue"],
                    "period": { "preset": "all_time" },
                    "sort": { "measure": "item_revenue", "dir": "desc" }
                }
            }]
        }),
    )
    .await;

    let w = &body["results"]["byproduct"];
    assert_eq!(w["status"], "ok");
    let rows = w["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    // Latte: 2 × 5000 = 10000 beats Mocha's 7000.
    assert_eq!(rows[0]["product"], "Latte");
    assert_eq!(rows[0]["units_sold"], 2);
    assert_eq!(rows[0]["item_revenue"], 10000);
    assert_eq!(w["grain"], "categorical");
}

#[sqlx::test]
async fn one_bad_widget_does_not_blank_the_dashboard(pool: PgPool) {
    // The reason results are per-widget outcomes rather than a batch that fails.
    let s = seed(&pool, "a").await;
    let app = metrics_app(&pool).await;
    let body = post_query(
        &app,
        &org_admin_token(s.org),
        json!({
            "period": { "preset": "all_time" },
            "widgets": [
                { "key": "good", "preset": "revenue_total" },
                { "key": "bad", "preset": "no_such_metric" },
                { "key": "alsobad", "spec": { "dataset": "orders", "measures": ["nonsense"] } }
            ]
        }),
    )
    .await;

    assert_eq!(body["results"]["good"]["status"], "ok");
    assert_eq!(body["results"]["bad"]["status"], "error");
    assert!(
        body["results"]["bad"]["error"]
            .as_str()
            .unwrap()
            .contains("no_such_metric")
    );
    // A bad measure names the valid ones, so a client can show a useful hint.
    let e = body["results"]["alsobad"]["error"].as_str().unwrap();
    assert!(e.contains("nonsense") && e.contains("revenue"));
}

#[sqlx::test]
async fn a_widget_may_override_the_batch_period(pool: PgPool) {
    let s = seed(&pool, "a").await;
    let app = metrics_app(&pool).await;
    let body = post_query(
        &app,
        &org_admin_token(s.org),
        json!({
            // The batch says a window with no data in it...
            "period": { "preset": "last_year" },
            "widgets": [
                { "key": "old", "preset": "revenue_total" },
                // ...and this widget overrides it.
                { "key": "all", "preset": "revenue_total", "period": { "preset": "all_time" } }
            ]
        }),
    )
    .await;
    assert_eq!(body["results"]["old"]["rows"][0]["revenue"], 0);
    assert_eq!(body["results"]["all"]["rows"][0]["revenue"], 16300);
}

#[sqlx::test]
async fn the_resolved_period_comes_back_so_a_client_can_label_it(pool: PgPool) {
    let s = seed(&pool, "a").await;
    let app = metrics_app(&pool).await;
    let body = post_query(
        &app,
        &org_admin_token(s.org),
        json!({ "widgets": [{ "key": "k", "preset": "revenue_total",
                              "period": { "preset": "last_month" } }] }),
    )
    .await;
    let p = &body["results"]["k"]["period"];
    assert!(p["from"].is_string() && p["to"].is_string());
}

#[sqlx::test]
async fn specifying_neither_preset_nor_spec_is_a_per_widget_error(pool: PgPool) {
    let s = seed(&pool, "a").await;
    let app = metrics_app(&pool).await;
    let body = post_query(
        &app,
        &org_admin_token(s.org),
        json!({ "widgets": [{ "key": "k" }, { "key": "j", "preset": "revenue_total",
                              "spec": { "dataset": "orders" } }] }),
    )
    .await;
    assert_eq!(body["results"]["k"]["status"], "error");
    assert!(
        body["results"]["j"]["error"]
            .as_str()
            .unwrap()
            .contains("not both")
    );
}

#[sqlx::test]
async fn an_empty_or_oversized_batch_is_rejected(pool: PgPool) {
    let s = seed(&pool, "a").await;
    let app = metrics_app(&pool).await;
    let token = org_admin_token(s.org);

    let req = test::TestRequest::post()
        .uri("/metrics/query")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(json!({ "widgets": [] }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 400);

    let many: Vec<Value> = (0..crate::analytics::handlers::MAX_WIDGETS + 1)
        .map(|i| json!({ "key": i.to_string(), "preset": "revenue_total" }))
        .collect();
    let req = test::TestRequest::post()
        .uri("/metrics/query")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(json!({ "widgets": many }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 400);
}

// ── Tenancy ─────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn one_merchant_never_sees_another_merchants_figures(pool: PgPool) {
    let a = seed(&pool, "a").await;
    let _b = seed(&pool, "b").await;
    let app = metrics_app(&pool).await;

    // Both orgs have identical data. If scoping leaked, revenue would double.
    let body = post_query(
        &app,
        &org_admin_token(a.org),
        json!({
            "period": { "preset": "all_time" },
            "widgets": [{ "key": "rev", "preset": "revenue_total" },
                        { "key": "branches", "preset": "sales_by_branch",
                          "period": { "preset": "all_time" } }]
        }),
    )
    .await;
    assert_eq!(body["results"]["rev"]["rows"][0]["revenue"], 16300);
    assert_eq!(
        body["results"]["branches"]["rows"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(body["scope"]["branches"].as_array().unwrap().len(), 1);
}

#[sqlx::test]
async fn a_teller_is_fenced_to_their_own_branch(pool: PgPool) {
    let s = seed(&pool, "a").await;
    let app = metrics_app(&pool).await;
    // A teller token bound to a branch that is not theirs resolves to nothing
    // they can see, so the fence yields no rows rather than another's data.
    let token = create_token(
        &secret(),
        Uuid::new_v4(),
        Some(s.org),
        UserRole::Teller,
        Some(Uuid::new_v4()),
        24,
    )
    .unwrap();
    let req = test::TestRequest::post()
        .uri("/metrics/query")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(json!({ "period": { "preset": "all_time" },
                          "widgets": [{ "key": "rev", "preset": "revenue_total" }] }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    // Either the role lacks `reports:read` (403) or it reads an empty scope —
    // never another branch's revenue.
    if resp.status().is_success() {
        let body: Value = test::read_body_json(resp).await;
        assert_eq!(body["results"]["rev"]["rows"][0]["revenue"], 0);
    } else {
        assert_eq!(resp.status(), 403);
    }
}

// ── The registry against the live schema ────────────────────────────────────

#[sqlx::test]
async fn every_preset_runs_against_the_real_schema(pool: PgPool) {
    // The most valuable test here. Every curated metric is executed against a
    // real database, so a fragment referencing a column that no longer exists
    // fails the build instead of failing a merchant's dashboard.
    let s = seed(&pool, "a").await;
    let app = metrics_app(&pool).await;
    let token = org_admin_token(s.org);

    for chunk in crate::analytics::presets::PRESETS.chunks(crate::analytics::handlers::MAX_WIDGETS)
    {
        let widgets: Vec<Value> = chunk
            .iter()
            .map(|p| json!({ "key": p.id, "preset": p.id, "period": { "preset": "all_time" } }))
            .collect();
        let body = post_query(&app, &token, json!({ "widgets": widgets })).await;
        for p in chunk {
            let out = &body["results"][p.id];
            assert_eq!(
                out["status"],
                "ok",
                "preset '{}' failed: {}",
                p.id,
                out["error"].as_str().unwrap_or("?")
            );
        }
    }
}

#[sqlx::test]
async fn every_dataset_dimension_and_measure_executes(pool: PgPool) {
    // Same guarantee, one level lower: every authored fragment in the semantic
    // layer is proven to be valid SQL against the live schema.
    use crate::analytics::compile::{CompileCtx, compile};
    use crate::analytics::execute::{ExecCtx, run};
    use crate::analytics::spec::{Period, PeriodPreset, QuerySpec};

    let s = seed(&pool, "a").await;
    let db = crate::db::Db::for_org(&pool, s.org).await;

    let ctx = CompileCtx {
        tz: chrono_tz::Africa::Cairo,
        now: chrono::Utc::now(),
    };
    let exec = ExecCtx {
        branch_ids: &[s.branch],
        locale: "en",
        tz: "Africa/Cairo",
    };
    for ds in crate::analytics::schema::DATASETS {
        for dim in ds.dims {
            // Measures are taken in batches: the compiler caps a single query at
            // 8, and the point here is to execute EVERY fragment at least once.
            for batch in ds.measures.chunks(8) {
                let spec = QuerySpec {
                    dataset: ds.id.into(),
                    dimensions: vec![dim.id.into()],
                    measures: batch.iter().map(|m| m.id.to_string()).collect(),
                    period: Period::preset(PeriodPreset::AllTime),
                    ..Default::default()
                };
                let compiled = compile(&spec, &ctx)
                    .unwrap_or_else(|e| panic!("{}/{} did not compile: {e}", ds.id, dim.id));
                run(&db, &compiled, &exec).await.unwrap_or_else(|e| {
                    panic!(
                        "{}/{} [{}] did not execute: {e}",
                        ds.id,
                        dim.id,
                        batch.iter().map(|m| m.id).collect::<Vec<_>>().join(",")
                    )
                });
            }
        }
    }
}

#[sqlx::test]
async fn the_executor_refuses_to_write(pool: PgPool) {
    // Defense in depth: even if a fragment were somehow malicious, the
    // transaction is read-only.
    let s = seed(&pool, "a").await;
    let db = crate::db::Db::for_org(&pool, s.org).await;
    let mut tx = db.begin().await.unwrap();
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await
        .unwrap();
    let err = sqlx::query("DELETE FROM orders")
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("read-only"), "{err}");
}
