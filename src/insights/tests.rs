//! Insights tests: ledger ranking + signal arithmetic + honesty rules, target
//! resolution (branch → org → default), decision recording (server baseline),
//! suppression (dismissed quiets a signal until it worsens), and margin-watch.
//!
//! Seeding note: test DBs are built from migrations only — the deploy-time shim
//! is NOT applied, so `menu_item_recipes` here is the REAL legacy table (which
//! `costing::org_sku_costs` reads via the compat name). Unified tables
//! (`menu_item_sizes`, `recipe_lines`) are seeded alongside where the ledger
//! reads them directly. In production the shim serves both from one source.

use actix_web::{App, test, web};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::insights::routes;
use crate::models::UserRole;

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn org_admin_token(user_id: Uuid, org_id: Uuid) -> String {
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

async fn app(
    pool: PgPool,
) -> impl actix_web::dev::Service<
    actix_http::Request,
    Response = actix_web::dev::ServiceResponse,
    Error = actix_web::Error,
> {
    test::init_service(
        App::new()
            .app_data(web::Data::new(pool))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await
}

// ── seed helpers ──────────────────────────────────────────────────────────────

async fn seed_org(pool: &PgPool) -> Uuid {
    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Insights Org', $2)")
        .bind(org_id)
        .bind(format!("ins-org-{org_id}"))
        .execute(pool)
        .await
        .unwrap();
    org_id
}

async fn seed_branch(pool: &PgPool, org_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(org_id)
        .bind(format!("Branch {id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_user(pool: &PgPool, org_id: Uuid, role: &str) -> Uuid {
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, $3, $4, 'hash', $5::user_role)",
    )
    .bind(user_id)
    .bind(org_id)
    .bind(format!("User {user_id}"))
    .bind(format!("u-{user_id}@test.com"))
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
    user_id
}

async fn grant(pool: &PgPool, resource: &str, action: &str) {
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) \
         VALUES ('org_admin'::user_role, $1::permission_resource, $2::permission_action, true) \
         ON CONFLICT DO NOTHING",
    )
    .bind(resource)
    .bind(action)
    .execute(pool)
    .await
    .unwrap();
}

#[sqlx::test]
async fn repricing_suggests_target_price_and_skips_uncosted(pool: PgPool) {
    // Espresso: price 100.00, cost 60.00 → 40% margin, below the default 60%
    // target → suggested 150.00 (uplift 50.00). Croissant: priced but NO recipe
    // cost → must be COUNTED as cost-unknown, never suggested (no guessed cost).
    let org = seed_org(&pool).await;
    seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    grant(&pool, "orders", "read").await;
    let cat = seed_category(&pool, org).await;
    let espresso = seed_item(&pool, org, cat, "Espresso", 10000).await;
    seed_costed_recipe(&pool, org, espresso, 6000.0).await;
    seed_item(&pool, org, cat, "Croissant", 8000).await; // priced, no recipe cost

    let app = app(pool).await;
    let req = test::TestRequest::get()
        .uri(&format!("/insights/branches/{}/repricing", Uuid::nil()))
        .insert_header((
            "Authorization",
            format!("Bearer {}", org_admin_token(user, org)),
        ))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;

    assert_eq!(body["target_pct"], 60.0);
    let sugg = body["suggestions"].as_array().unwrap();
    assert_eq!(sugg.len(), 1, "only the costed, underpriced item");
    assert_eq!(sugg[0]["item_name"], "Espresso");
    assert_eq!(sugg[0]["current_price"], 10000);
    assert_eq!(sugg[0]["cost"], 6000);
    assert_eq!(sugg[0]["suggested_price"], 15000);
    assert_eq!(sugg[0]["uplift"], 5000);
    assert_eq!(sugg[0]["below_cost"], false);
    // Croissant (no cost) is counted as unknown, not suggested.
    assert!(body["skus_cost_unknown"].as_i64().unwrap() >= 1);
}

async fn seed_category(pool: &PgPool, org_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'Drinks')")
        .bind(id)
        .bind(org_id)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_item(pool: &PgPool, org_id: Uuid, cat: Uuid, name: &str, price: i64) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) \
         VALUES ($1, $2, $3, $4, $5, true)",
    )
    .bind(id)
    .bind(org_id)
    .bind(cat)
    .bind(name)
    .bind(price)
    .execute(pool)
    .await
    .unwrap();
    // Unified catalog SKU row (the ledger's catalog side).
    sqlx::query(
        "INSERT INTO menu_item_sizes (menu_item_id, label, price, sort, is_active) \
         VALUES ($1, 'one_size', $2, 0, true)",
    )
    .bind(id)
    .bind(price)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// A priced ingredient + a LEGACY recipe row (what org_sku_costs reads in test
/// DBs) so the SKU's CURRENT cost resolves and `recipe_incomplete` stays quiet.
async fn seed_costed_recipe(pool: &PgPool, org_id: Uuid, item: Uuid, cost: f64) -> Uuid {
    let ing: Uuid = sqlx::query_scalar(
        "INSERT INTO org_ingredients (org_id, name, unit, cost_per_unit, category) \
         VALUES ($1, $2, 'g'::inventory_unit, $3, 'general') RETURNING id",
    )
    .bind(org_id)
    .bind(format!("Ing-{item}"))
    .bind(cost)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO menu_item_recipes (menu_item_id, size_label, quantity_used, \
             ingredient_name, ingredient_unit, org_ingredient_id) \
         VALUES ($1, 'one_size', 1.0, 'x', 'g', $2)",
    )
    .bind(item)
    .bind(ing)
    .execute(pool)
    .await
    .unwrap();
    ing
}

async fn seed_order(pool: &PgPool, branch_id: Uuid, org_id: Uuid) -> Uuid {
    // Reuse the branch's open shift if one exists (one open shift per till).
    let existing: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT id, teller_id FROM shifts WHERE branch_id = $1 AND status = 'open' LIMIT 1",
    )
    .bind(branch_id)
    .fetch_optional(pool)
    .await
    .unwrap();
    if let Some((shift, teller)) = existing {
        return sqlx::query_scalar(
            "INSERT INTO orders (branch_id, teller_id, shift_id, idempotency_key, subtotal, \
                 discount_amount, tax_amount, total_amount, status, order_number, payment_method, \
                 order_ref) \
             VALUES ($1, $2, $3, gen_random_uuid(), 0, 0, 0, 0, 'completed', \
                 COALESCE((SELECT MAX(order_number) + 1 FROM orders WHERE shift_id = $3), 1), \
                 'cash', gen_random_uuid()::text) RETURNING id",
        )
        .bind(branch_id)
        .bind(teller)
        .bind(shift)
        .fetch_one(pool)
        .await
        .unwrap();
    }
    let teller = seed_user(pool, org_id, "teller").await;
    let shift: Uuid = sqlx::query_scalar(
        "INSERT INTO shifts (branch_id, teller_id, status, opening_cash) \
         VALUES ($1, $2, 'open', 0) RETURNING id",
    )
    .bind(branch_id)
    .bind(teller)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO orders (branch_id, teller_id, shift_id, idempotency_key, subtotal, \
             discount_amount, tax_amount, total_amount, status, order_number, payment_method, \
             order_ref) \
         VALUES ($1, $2, $3, gen_random_uuid(), 0, 0, 0, 0, 'completed', 1, 'cash', \
             gen_random_uuid()::text) RETURNING id",
    )
    .bind(branch_id)
    .bind(teller)
    .bind(shift)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[allow(clippy::too_many_arguments)]
async fn seed_line(
    pool: &PgPool,
    order: Uuid,
    item: Uuid,
    name: &str,
    unit_price: i64,
    qty: i64,
    unit_cost: Option<i64>,
) {
    sqlx::query(
        "INSERT INTO order_items (order_id, menu_item_id, item_name, unit_price, quantity, \
             line_total, line_cost, unit_cost, cost_missing) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(order)
    .bind(item)
    .bind(name)
    .bind(unit_price)
    .bind(qty)
    .bind(unit_price * qty)
    .bind(unit_cost.map(|c| c * qty))
    .bind(unit_cost)
    .bind(unit_cost.is_none())
    .execute(pool)
    .await
    .unwrap();
}

async fn get_json(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    token: &str,
    uri: &str,
) -> serde_json::Value {
    let resp = test::call_service(
        app,
        test::TestRequest::get()
            .uri(uri)
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success(), "GET {uri} → {}", resp.status());
    test::read_body_json(resp).await
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_ledger_ranking_signals_and_honesty(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let admin = seed_user(&pool, org, "org_admin").await;
    grant(&pool, "orders", "read").await;
    let token = org_admin_token(admin, org);
    let cat = seed_category(&pool, org).await;

    // Star: 10 × 1000, cost 200/unit → margin 80% (above the 60% default).
    let star = seed_item(&pool, org, cat, "Star", 1000).await;
    seed_costed_recipe(&pool, org, star, 200.0).await;
    // Thin: 8 × 1000, cost 700/unit → margin 30% — top-quartile seller below
    // target-buffer ⇒ below_target + price_candidate (suggest 700/(1-.6)=1750→1800).
    let thin = seed_item(&pool, org, cat, "Thin", 1000).await;
    seed_costed_recipe(&pool, org, thin, 700.0).await;
    // Mystery: sold with UNKNOWN sale-time cost + no current recipe ⇒ margin
    // null (never 0) + recipe_incomplete.
    let mystery = seed_item(&pool, org, cat, "Mystery", 500).await;
    // Sleeper: on the menu, zero sales ⇒ removal_candidate.
    let sleeper = seed_item(&pool, org, cat, "Sleeper", 800).await;

    let order = seed_order(&pool, branch, org).await;
    seed_line(&pool, order, star, "Star", 1000, 10, Some(200)).await;
    seed_line(&pool, order, thin, "Thin", 1000, 8, Some(700)).await;
    seed_line(&pool, order, mystery, "Mystery", 500, 3, None).await;

    let app = app(pool.clone()).await;
    let from = (chrono::Utc::now() - chrono::Duration::days(7))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let body = get_json(
        &app,
        &token,
        &format!("/insights/branches/{branch}/menu-margin?from={from}"),
    )
    .await;

    // Ranking: Star (margin 8000) before Thin (2400); Mystery margin is null.
    let rows = body["rows"].as_array().unwrap();
    let idx = |name: &str| rows.iter().position(|r| r["item_name"] == name).unwrap();
    assert!(idx("Star") < idx("Thin"), "ranked by known margin desc");
    let mystery_row = &rows[idx("Mystery")];
    assert!(
        mystery_row["cost"].is_null(),
        "unknown cost is null, never 0"
    );
    assert!(mystery_row["margin"].is_null());

    // Signals.
    let flags = |name: &str| -> Vec<String> {
        rows[idx(name)]["flags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["kind"].as_str().unwrap().to_string())
            .collect()
    };
    assert!(flags("Thin").contains(&"below_target".into()));
    assert!(flags("Thin").contains(&"price_candidate".into()));
    let suggested = rows[idx("Thin")]["flags"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["kind"] == "price_candidate")
        .unwrap()["params"]["suggested_price"]
        .as_i64()
        .unwrap();
    assert_eq!(suggested, 1800, "700/(1-0.60)=1750 rounds UP to whole EGP");
    assert!(flags("Sleeper").contains(&"removal_candidate".into()));
    assert!(flags("Mystery").contains(&"recipe_incomplete".into()));
    assert!(
        !flags("Star").contains(&"below_target".into()),
        "80% margin is above target"
    );

    // Classic menu-engineering class rides along as a secondary lens:
    // Star (pop 10/18, unit profit 800 > avg ≈577) → star;
    // Thin (pop 8/18 high, unit profit 300 < avg) → workhorse;
    // Mystery (unknown cost) + Sleeper (no sales) stay unclassified.
    assert_eq!(rows[idx("Star")]["class"], "star");
    assert_eq!(rows[idx("Thin")]["class"], "workhorse");
    assert!(rows[idx("Mystery")]["class"].is_null());
    assert!(rows[idx("Sleeper")]["class"].is_null());
    assert!(rows[idx("Star")]["popularity_pct"].as_f64().unwrap() > 50.0);

    // Totals honesty: unknown-cost revenue reported separately, not zeroed in.
    assert_eq!(
        body["totals"]["revenue"].as_i64().unwrap(),
        10_000 + 8_000 + 1_500
    );
    assert_eq!(
        body["totals"]["revenue_cost_unknown"].as_i64().unwrap(),
        1_500
    );
    assert_eq!(body["rows_cost_unknown"].as_i64().unwrap(), 1);
    assert!(body["totals"]["below_target_gap"].as_i64().unwrap() > 0);
    assert_eq!(body["target_pct"].as_f64().unwrap(), 60.0);
    assert_eq!(body["target_source"], "default");
}

#[sqlx::test]
async fn test_target_resolution_branch_over_org_over_default(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let admin = seed_user(&pool, org, "org_admin").await;
    grant(&pool, "orders", "read").await;
    grant(&pool, "menu_items", "update").await;
    let token = org_admin_token(admin, org);
    let app = app(pool.clone()).await;

    // PUT the org default, then a branch override.
    for (b, pct) in [(None::<Uuid>, 55.0), (Some(branch), 70.0)] {
        let resp = test::call_service(
            &app,
            test::TestRequest::put()
                .uri(&format!("/insights/margin-target?org_id={org}"))
                .insert_header(("Authorization", format!("Bearer {token}")))
                .set_json(json!({ "branch_id": b, "target_pct": pct }))
                .to_request(),
        )
        .await;
        assert!(resp.status().is_success());
    }

    let branch_led = get_json(
        &app,
        &token,
        &format!("/insights/branches/{branch}/menu-margin"),
    )
    .await;
    assert_eq!(branch_led["target_pct"].as_f64().unwrap(), 70.0);
    assert_eq!(branch_led["target_source"], "branch");

    let org_led = get_json(
        &app,
        &token,
        &format!("/insights/branches/{}/menu-margin", Uuid::nil()),
    )
    .await;
    assert_eq!(org_led["target_pct"].as_f64().unwrap(), 55.0);
    assert_eq!(org_led["target_source"], "org");

    // Invalid pct rejected.
    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!("/insights/margin-target?org_id={org}"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(json!({ "target_pct": 400.0 }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400);
}

#[sqlx::test]
async fn test_decision_records_baseline_and_suppresses_signal(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let admin = seed_user(&pool, org, "org_admin").await;
    grant(&pool, "orders", "read").await;
    grant(&pool, "menu_items", "update").await;
    let token = org_admin_token(admin, org);
    let cat = seed_category(&pool, org).await;

    let thin = seed_item(&pool, org, cat, "Thin", 1000).await;
    seed_costed_recipe(&pool, org, thin, 700.0).await;
    let order = seed_order(&pool, branch, org).await;
    seed_line(&pool, order, thin, "Thin", 1000, 8, Some(700)).await;

    let app = app(pool.clone()).await;
    let from = (chrono::Utc::now() - chrono::Duration::days(7))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let uri = format!("/insights/branches/{branch}/menu-margin?from={from}");

    let before = get_json(&app, &token, &uri).await;
    let has_below_target = |body: &serde_json::Value| {
        body["rows"].as_array().unwrap().iter().any(|r| {
            r["item_name"] == "Thin"
                && r["flags"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|f| f["kind"] == "below_target")
        })
    };
    assert!(has_below_target(&before));

    // Dismiss it. Baseline must be computed server-side from real history.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/insights/decisions?org_id={org}"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(json!({
                "branch_id": branch,
                "menu_item_id": thin,
                "signal_kind": "below_target",
                "action": "dismissed",
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let decision: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(decision["baseline"]["quantity"].as_i64().unwrap(), 8);
    assert_eq!(decision["baseline"]["margin_pct"].as_f64().unwrap(), 30.0);

    // Signal now suppressed (margin hasn't worsened).
    let after = get_json(&app, &token, &uri).await;
    assert!(!has_below_target(&after), "dismissed ⇒ suppressed");

    // The decision log lists it; impact not yet measurable (<1 day of after-data).
    let log = get_json(&app, &token, &format!("/insights/decisions?org_id={org}")).await;
    let entry = &log.as_array().unwrap()[0];
    assert_eq!(entry["action"], "dismissed");
    assert_eq!(entry["item_name"], "Thin");
    assert!(entry["impact"].is_null());
    assert_eq!(entry["impact_complete"], false);

    // A decision whose baseline margin was MUCH higher (evidence has since
    // worsened by >5pts) does NOT suppress — the signal re-raises.
    sqlx::query(
        "UPDATE menu_decisions SET baseline = jsonb_set(baseline, '{margin_pct}', '55.0') \
         WHERE org_id = $1",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    let worsened = get_json(&app, &token, &uri).await;
    assert!(
        has_below_target(&worsened),
        "margin 30% vs baseline 55% ⇒ worsened ⇒ re-raised"
    );
}

#[sqlx::test]
async fn test_suppression_is_branch_scoped(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org).await;
    let branch_b = seed_branch(&pool, org).await;
    let admin = seed_user(&pool, org, "org_admin").await;
    grant(&pool, "orders", "read").await;
    grant(&pool, "menu_items", "update").await;
    let token = org_admin_token(admin, org);
    let cat = seed_category(&pool, org).await;

    // The same thin-margin SKU sells at BOTH branches.
    let thin = seed_item(&pool, org, cat, "Thin", 1000).await;
    seed_costed_recipe(&pool, org, thin, 700.0).await;
    for br in [branch_a, branch_b] {
        let order = seed_order(&pool, br, org).await;
        seed_line(&pool, order, thin, "Thin", 1000, 8, Some(700)).await;
    }

    let app = app(pool.clone()).await;
    let from = (chrono::Utc::now() - chrono::Duration::days(7))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let has_flag = |body: &serde_json::Value| {
        body["rows"].as_array().unwrap().iter().any(|r| {
            r["item_name"] == "Thin"
                && r["flags"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|f| f["kind"] == "below_target")
        })
    };

    // Dismiss at branch A ONLY.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/insights/decisions?org_id={org}"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(json!({
                "branch_id": branch_a,
                "menu_item_id": thin,
                "signal_kind": "below_target",
                "action": "dismissed",
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201);

    let led_a = get_json(
        &app,
        &token,
        &format!("/insights/branches/{branch_a}/menu-margin?from={from}"),
    )
    .await;
    let led_b = get_json(
        &app,
        &token,
        &format!("/insights/branches/{branch_b}/menu-margin?from={from}"),
    )
    .await;
    assert!(!has_flag(&led_a), "dismissed at A ⇒ suppressed at A");
    assert!(
        has_flag(&led_b),
        "branches differ — a dismissal at A must NOT silence B"
    );
}

#[sqlx::test]
async fn test_price_candidate_learns_from_prior_outcome(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let admin = seed_user(&pool, org, "org_admin").await;
    grant(&pool, "orders", "read").await;
    let token = org_admin_token(admin, org);
    let cat = seed_category(&pool, org).await;

    // Thin-margin top seller in the CURRENT window ⇒ price_candidate territory.
    let thin = seed_item(&pool, org, cat, "Thin", 1000).await;
    seed_costed_recipe(&pool, org, thin, 700.0).await;
    let order = seed_order(&pool, branch, org).await;
    seed_line(&pool, order, thin, "Thin", 1000, 8, Some(700)).await;

    // A PRIOR acted pricing decision 10 days ago whose baseline was much
    // healthier than what actually happened after (margin/day collapsed):
    // baseline 28d window: qty 280, margin 84000 ⇒ 3000/day vs the after
    // window's ~8 units total ⇒ the change measurably hurt.
    sqlx::query(
        "INSERT INTO menu_decisions \
             (org_id, branch_id, menu_item_id, size_label, signal_kind, action, baseline, created_at) \
         VALUES ($1, $2, $3, 'one_size', 'price_candidate', 'acted', \
                 '{\"window_days\":28,\"quantity\":280,\"revenue\":280000,\"cost\":196000, \
                   \"margin\":84000,\"margin_pct\":30.0,\"qty_per_day\":10.0}', \
                 now() - interval '10 days')",
    )
    .bind(org)
    .bind(branch)
    .bind(thin)
    .execute(&pool)
    .await
    .unwrap();

    let app = app(pool.clone()).await;
    let from = (chrono::Utc::now() - chrono::Duration::days(7))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let body = get_json(
        &app,
        &token,
        &format!("/insights/branches/{branch}/menu-margin?from={from}"),
    )
    .await;

    let pc = body["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["item_name"] == "Thin")
        .unwrap()["flags"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["kind"] == "price_candidate")
        .expect("still a price candidate (acted 10d ago > suppression handles acted? no — acted suppresses for 30d)")
        .clone();
    assert_eq!(
        pc["params"]["caution"], true,
        "prior change hurt ⇒ caution, not a higher price"
    );
    assert!(
        pc["params"]["suggested_price"].is_null(),
        "no escalated suggestion after a failed change"
    );
    assert!(pc["params"]["last_margin_per_day_delta"].as_f64().unwrap() < 0.0);
}

#[sqlx::test]
async fn test_margin_watch_top_bottom_and_counts(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let admin = seed_user(&pool, org, "org_admin").await;
    grant(&pool, "orders", "read").await;
    let token = org_admin_token(admin, org);
    let cat = seed_category(&pool, org).await;

    let mut items = Vec::new();
    for (name, price, cost, qty) in [
        ("A", 1000_i64, 200_i64, 10_i64),
        ("B", 1000, 500, 6),
        ("C", 1000, 900, 4),
        ("D", 1000, 950, 2),
    ] {
        let it = seed_item(&pool, org, cat, name, price).await;
        seed_costed_recipe(&pool, org, it, cost as f64).await;
        items.push((it, name, price, cost, qty));
    }
    let order = seed_order(&pool, branch, org).await;
    for (it, name, price, cost, qty) in &items {
        seed_line(&pool, order, *it, name, *price, *qty, Some(*cost)).await;
    }

    let app = app(pool.clone()).await;
    let from = (chrono::Utc::now() - chrono::Duration::days(7))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let body = get_json(
        &app,
        &token,
        &format!("/insights/branches/{branch}/margin-watch?from={from}"),
    )
    .await;

    let top = body["top"].as_array().unwrap();
    let bottom = body["bottom"].as_array().unwrap();
    assert!(top.len() <= 3 && !top.is_empty());
    assert!(bottom.len() <= 3 && !bottom.is_empty());
    assert_eq!(top[0]["item_name"], "A", "largest known margin first");
    assert_eq!(bottom[0]["item_name"], "D", "smallest known margin first");
    assert!(
        body["open_signals"].as_i64().unwrap() > 0,
        "C/D are below target"
    );
    assert_eq!(body["target_pct"].as_f64().unwrap(), 60.0);
}

#[sqlx::test]
async fn test_elasticity_tempers_the_suggested_price(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let admin = seed_user(&pool, org, "org_admin").await;
    grant(&pool, "orders", "read").await;
    let token = org_admin_token(admin, org);
    let cat = seed_category(&pool, org).await;

    // Current window: 6 sold @1200 (unit cost 700) → margin 41.7%, under the
    // 55% price-candidate bar. The NAIVE target-restoring price would be
    // 700/(1-0.6) = 1750 → 1800.
    let item = seed_item(&pool, org, cat, "Learned", 1200).await;
    seed_costed_recipe(&pool, org, item, 700.0).await;
    let order = seed_order(&pool, branch, org).await;
    seed_line(&pool, order, item, "Learned", 1200, 6, Some(700)).await;

    // A measured prior change 40 days ago (outside the 30-day acted-quiet
    // window, so the signal may speak again) that WORKED, whose (price,
    // volume) pair teaches elasticity: baseline 28 units @avg 1000 (qpd 1.0,
    // margin/day 200) → after-window [40d, 12d ago]: 17 units @1200 over 28
    // days (qpd 0.607, margin/day ≈303). e = ln(0.607)/ln(1.2) ≈ −2.74.
    sqlx::query(
        "INSERT INTO menu_decisions \
             (org_id, branch_id, menu_item_id, size_label, signal_kind, action, baseline, created_at) \
         VALUES ($1, $2, $3, 'one_size', 'price_candidate', 'acted', \
                 '{\"window_days\":28,\"quantity\":28,\"revenue\":28000,\"cost\":22400, \
                   \"margin\":5600,\"margin_pct\":20.0,\"qty_per_day\":1.0}', \
                 now() - interval '40 days')",
    )
    .bind(org)
    .bind(branch)
    .bind(item)
    .execute(&pool)
    .await
    .unwrap();
    // The after-window sales that teach the elasticity (backdated 20 days).
    let order_after = seed_order(&pool, branch, org).await;
    seed_line(&pool, order_after, item, "Learned", 1200, 17, Some(700)).await;
    sqlx::query("UPDATE orders SET created_at = now() - interval '20 days' WHERE id = $1")
        .bind(order_after)
        .execute(&pool)
        .await
        .unwrap();

    let app = app(pool.clone()).await;
    let from = (chrono::Utc::now() - chrono::Duration::days(7))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let body = get_json(
        &app,
        &token,
        &format!("/insights/branches/{branch}/menu-margin?from={from}"),
    )
    .await;
    let pc = body["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["item_name"] == "Learned")
        .unwrap()["flags"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["kind"] == "price_candidate")
        .expect("price candidate fires (worked ⇒ no caution)")
        .clone();

    assert_eq!(pc["params"]["last_worked"], true);
    let e = pc["params"]["elasticity"]
        .as_f64()
        .expect("elasticity learned");
    assert!((-3.0..=-2.6).contains(&e), "e≈−2.8, got {e}");
    // With demand this elastic, raising price LOSES margin/day — the optimizer
    // holds at the current realized price instead of the naive 1800.
    assert_eq!(pc["params"]["suggested_price"].as_i64().unwrap(), 1_200);
    assert_eq!(
        pc["params"]["expected_margin_per_day_delta"]
            .as_f64()
            .unwrap(),
        0.0
    );
}

#[sqlx::test]
async fn test_adaptive_bar_and_min_volume_floor(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let admin = seed_user(&pool, org, "org_admin").await;
    grant(&pool, "orders", "read").await;
    let token = org_admin_token(admin, org);
    let cat = seed_category(&pool, org).await;

    // Three DISTINCT SKUs' below_target signals were dismissed recently and
    // none acted → the kind's bar rises by (3−0−2)×2 = 2 points.
    for i in 0..3 {
        let it = seed_item(&pool, org, cat, &format!("Dismissed{i}"), 1000).await;
        sqlx::query(
            "INSERT INTO menu_decisions \
                 (org_id, branch_id, menu_item_id, size_label, signal_kind, action, baseline) \
             VALUES ($1, $2, $3, 'one_size', 'below_target', 'dismissed', '{\"margin_pct\": 99.0}')",
        )
        .bind(org)
        .bind(branch)
        .bind(it)
        .execute(&pool)
        .await
        .unwrap();
    }

    // SmallGap: 59% margin — under the 60% target but INSIDE the raised bar
    // (60−2=58) ⇒ no flag. BigGap: 40% ⇒ flags, with the bar disclosed.
    // TinyQty: 30% margin but only 2 sold ⇒ under the volume floor, no flag.
    let small = seed_item(&pool, org, cat, "SmallGap", 1000).await;
    seed_costed_recipe(&pool, org, small, 410.0).await;
    let big = seed_item(&pool, org, cat, "BigGap", 1000).await;
    seed_costed_recipe(&pool, org, big, 600.0).await;
    let tiny = seed_item(&pool, org, cat, "TinyQty", 1000).await;
    seed_costed_recipe(&pool, org, tiny, 700.0).await;
    let order = seed_order(&pool, branch, org).await;
    seed_line(&pool, order, small, "SmallGap", 1000, 6, Some(410)).await;
    seed_line(&pool, order, big, "BigGap", 1000, 6, Some(600)).await;
    seed_line(&pool, order, tiny, "TinyQty", 1000, 2, Some(700)).await;

    let app = app(pool.clone()).await;
    let from = (chrono::Utc::now() - chrono::Duration::days(7))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let body = get_json(
        &app,
        &token,
        &format!("/insights/branches/{branch}/menu-margin?from={from}"),
    )
    .await;
    let flags_of = |name: &str| -> Vec<String> {
        body["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["item_name"] == name)
            .unwrap()["flags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["kind"].as_str().unwrap().to_string())
            .collect()
    };
    assert!(
        !flags_of("SmallGap").contains(&"below_target".into()),
        "59% vs raised bar 58 ⇒ quiet"
    );
    assert!(flags_of("BigGap").contains(&"below_target".into()));
    let bt = body["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["item_name"] == "BigGap")
        .unwrap()["flags"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["kind"] == "below_target")
        .unwrap()
        .clone();
    assert_eq!(
        bt["params"]["adaptive_bar"].as_f64().unwrap(),
        2.0,
        "bar disclosed"
    );
    assert!(
        !flags_of("TinyQty").contains(&"below_target".into()),
        "2 sold is noise, not evidence"
    );
}

#[sqlx::test]
async fn test_prev_period_revenue_in_totals(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let admin = seed_user(&pool, org, "org_admin").await;
    grant(&pool, "orders", "read").await;
    let token = org_admin_token(admin, org);
    let cat = seed_category(&pool, org).await;

    let item = seed_item(&pool, org, cat, "Steady", 1000).await;
    seed_costed_recipe(&pool, org, item, 200.0).await;
    // Current-window sale + a PREVIOUS-window sale (backdated 10 days).
    let order_now = seed_order(&pool, branch, org).await;
    seed_line(&pool, order_now, item, "Steady", 1000, 3, Some(200)).await;
    let order_prev = seed_order(&pool, branch, org).await;
    seed_line(&pool, order_prev, item, "Steady", 1000, 5, Some(200)).await;
    sqlx::query("UPDATE orders SET created_at = now() - interval '10 days' WHERE id = $1")
        .bind(order_prev)
        .execute(&pool)
        .await
        .unwrap();

    let app = app(pool.clone()).await;
    let from = (chrono::Utc::now() - chrono::Duration::days(7))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let body = get_json(
        &app,
        &token,
        &format!("/insights/branches/{branch}/menu-margin?from={from}"),
    )
    .await;

    assert_eq!(body["totals"]["revenue"].as_i64().unwrap(), 3_000);
    assert_eq!(
        body["totals"]["prev_revenue"].as_i64().unwrap(),
        5_000,
        "previous equal-length window revenue feeds the header trend"
    );
    let row = body["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["item_name"] == "Steady")
        .unwrap();
    assert_eq!(row["prev_quantity"].as_i64().unwrap(), 5);
}
