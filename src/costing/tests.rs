#![allow(unused_imports)]
use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;

use super::service::{AddonCost, SkuCost};

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

async fn seed_basics(pool: &PgPool) -> (Uuid, Uuid, String) {
    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(org_id)
        .bind(format!("costing-{org_id}"))
        .execute(pool)
        .await
        .unwrap();
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, 'U', $3, 'h', 'org_admin'::user_role)",
    )
    .bind(user_id)
    .bind(org_id)
    .bind(format!("u-{user_id}@t.com"))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) \
         VALUES ('org_admin'::user_role, 'orders'::permission_resource, 'read'::permission_action, true) \
         ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
    let token = admin_token(user_id, org_id);
    (org_id, user_id, token)
}

#[sqlx::test]
async fn test_sku_costs_rollup_and_missing(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(super::routes::configure),
    )
    .await;
    let (org_id, _user, token) = seed_basics(&pool).await;

    let cat_id = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'Drinks')")
        .bind(cat_id)
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();

    // Costed item: 10 g @ 250 piastres/g → 2 500 piastres.
    let costed = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) VALUES ($1, $2, $3, 'Latte', 7000, true)")
        .bind(costed).bind(org_id).bind(cat_id).execute(&pool).await.unwrap();
    let ing = Uuid::new_v4();
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category) VALUES ($1, $2, 'Beans', 'g'::inventory_unit, 250, 'coffee_bean')")
        .bind(ing).bind(org_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO menu_item_recipes (menu_item_id, org_ingredient_id, quantity_used, size_label, ingredient_name, ingredient_unit) VALUES ($1, $2, 10.0, 'one_size', 'Beans', 'g')")
        .bind(costed).bind(ing).execute(&pool).await.unwrap();

    // Recipe-less item: cost must be NULL, never zero.
    let bare = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) VALUES ($1, $2, $3, 'Water', 1000, true)")
        .bind(bare).bind(org_id).bind(cat_id).execute(&pool).await.unwrap();

    // Partially-costed item: one PRICED ingredient (Beans, 10 g → 2 500) plus one
    // UNPRICED ingredient (Milk, no cost_per_unit). The rollup sums the priced
    // part and flags it incomplete — this is the genuine `cost_missing == true`
    // case under the partial-tolerant semantics.
    let partial = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) VALUES ($1, $2, $3, 'Mocha', 9000, true)")
        .bind(partial).bind(org_id).bind(cat_id).execute(&pool).await.unwrap();
    let unpriced = Uuid::new_v4();
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category) VALUES ($1, $2, 'Milk', 'g'::inventory_unit, NULL, 'milk')")
        .bind(unpriced).bind(org_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO menu_item_recipes (menu_item_id, org_ingredient_id, quantity_used, size_label, ingredient_name, ingredient_unit) VALUES ($1, $2, 10.0, 'one_size', 'Beans', 'g')")
        .bind(partial).bind(ing).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO menu_item_recipes (menu_item_id, org_ingredient_id, quantity_used, size_label, ingredient_name, ingredient_unit) VALUES ($1, $2, 200.0, 'one_size', 'Milk', 'g')")
        .bind(partial).bind(unpriced).execute(&pool).await.unwrap();

    let req = test::TestRequest::get()
        .uri(&format!("/costing/menu-items?org_id={org_id}"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "got {:?}", resp.status());
    let rows: Vec<SkuCost> = test::read_body_json(resp).await;

    let latte = rows.iter().find(|r| r.menu_item_id == costed).unwrap();
    assert_eq!(latte.cost, Some(2_500));
    assert!(!latte.cost_missing);
    assert!((latte.food_cost_pct.unwrap() - 2_500.0 / 7_000.0).abs() < 1e-9);

    // Recipe-less ⇒ no cost, but NOT "missing": cost_missing flags an incomplete
    // *recipe* rollup, and Water has no recipe at all (see SkuCost::cost_missing).
    let water = rows.iter().find(|r| r.menu_item_id == bare).unwrap();
    assert_eq!(water.cost, None);
    assert!(!water.cost_missing);

    // Partial recipe ⇒ priced part summed (2 500), flagged incomplete, and no
    // food-cost % graded on a partial figure.
    let mocha = rows.iter().find(|r| r.menu_item_id == partial).unwrap();
    assert_eq!(mocha.cost, Some(2_500));
    assert!(mocha.cost_missing);
    assert!(mocha.food_cost_pct.is_none());
}

#[sqlx::test]
async fn test_addon_costs_rollup(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(super::routes::configure),
    )
    .await;
    let (org_id, _user, token) = seed_basics(&pool).await;

    let ing = Uuid::new_v4();
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category) VALUES ($1, $2, 'Oat Milk', 'ml'::inventory_unit, 10, 'milk')")
        .bind(ing).bind(org_id).execute(&pool).await.unwrap();
    let addon = Uuid::new_v4();
    sqlx::query("INSERT INTO addon_items (id, org_id, name, type, default_price) VALUES ($1, $2, 'Oat', 'milk_type', 1500)")
        .bind(addon).bind(org_id).execute(&pool).await.unwrap();
    // 200 ml @ 10 piastres/ml → 2 000 piastres.
    sqlx::query("INSERT INTO addon_item_ingredients (addon_item_id, org_ingredient_id, quantity_used, ingredient_name, ingredient_unit) VALUES ($1, $2, 200.0, 'Oat Milk', 'ml')")
        .bind(addon).bind(ing).execute(&pool).await.unwrap();

    let req = test::TestRequest::get()
        .uri(&format!("/costing/addon-items?org_id={org_id}"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let rows: Vec<AddonCost> = test::read_body_json(resp).await;
    let oat = rows.iter().find(|r| r.addon_item_id == addon).unwrap();
    assert_eq!(oat.cost, Some(2_000));
    assert!(!oat.cost_missing);
}

// ─────────────────────────────────────────────────────────────────────
// Backfill: reprice order snapshots at current ingredient costs
// ─────────────────────────────────────────────────────────────────────

mod backfill_tests {
    use super::super::backfill::{BackfillScope, backfill_cost_snapshots};
    use sqlx::PgPool;
    use uuid::Uuid;

    struct Seeded {
        branch: Uuid,
        order: Uuid,
    }

    async fn seed_org_branch_order(pool: &PgPool) -> (Uuid, Seeded) {
        let org = Uuid::new_v4();
        sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
            .bind(org)
            .bind(format!("bf-{org}"))
            .execute(pool)
            .await
            .unwrap();
        let branch = Uuid::new_v4();
        sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, $3)")
            .bind(branch)
            .bind(org)
            .bind(format!("Branch {branch}"))
            .execute(pool)
            .await
            .unwrap();
        let teller = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO users (id, org_id, name, email, password_hash, role) \
             VALUES ($1, $2, 'T', $3, 'h', 'teller'::user_role)",
        )
        .bind(teller)
        .bind(org)
        .bind(format!("t-{teller}@t.com"))
        .execute(pool)
        .await
        .unwrap();
        let shift: Uuid = sqlx::query_scalar(
            "INSERT INTO shifts (branch_id, teller_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(branch)
        .bind(teller)
        .fetch_one(pool)
        .await
        .unwrap();
        let order: Uuid = sqlx::query_scalar(
            "INSERT INTO orders (branch_id, shift_id, teller_id, order_number, status, \
                                 payment_method, subtotal, total_amount, order_ref) \
             VALUES ($1, $2, $3, 1, 'completed', 'cash', 1000, 1000, gen_random_uuid()::text) RETURNING id",
        )
        .bind(branch)
        .bind(shift)
        .bind(teller)
        .fetch_one(pool)
        .await
        .unwrap();
        (org, Seeded { branch, order })
    }

    /// Ingredient whose CURRENT cost resolves to 100 piastres via an open
    /// history epoch (catalog deliberately differs to prove epoch priority).
    async fn seed_ingredient_at_100(pool: &PgPool, org: Uuid) -> Uuid {
        let ing: Uuid = sqlx::query_scalar(
            "INSERT INTO org_ingredients (org_id, name, unit, cost_per_unit) \
             VALUES ($1, $2, 'g'::inventory_unit, 77) RETURNING id",
        )
        .bind(org)
        .bind(format!("ing-{}", Uuid::new_v4()))
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO ingredient_cost_history (org_ingredient_id, cost_per_unit, effective_from) \
             VALUES ($1, 100, now() - interval '1 day')",
        )
        .bind(ing)
        .execute(pool)
        .await
        .unwrap();
        ing
    }

    /// Menu item with a current recipe of `qty` units of `ing`.
    async fn seed_item_with_recipe(pool: &PgPool, org: Uuid, ing: Uuid, qty: f64) -> Uuid {
        let id = seed_bare_item(pool, org).await;
        sqlx::query(
            "INSERT INTO menu_item_recipes (menu_item_id, size_label, quantity_used, \
                                            ingredient_name, ingredient_unit, org_ingredient_id) \
             VALUES ($1, 'one_size', $2, 'ing', 'g', $3)",
        )
        .bind(id)
        .bind(rust_decimal::Decimal::try_from(qty).unwrap())
        .bind(ing)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    async fn seed_bare_item(pool: &PgPool, org: Uuid) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO menu_items (id, org_id, name, base_price, is_active) \
             VALUES ($1, $2, 'Item', 1000, true)",
        )
        .bind(id)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_line(
        pool: &PgPool,
        order: Uuid,
        menu_item: Option<Uuid>,
        bundle_id: Option<Uuid>,
        quantity: i32,
        stale_line_cost: Option<i64>,
        stale_unit_cost: Option<i64>,
        cost_missing: bool,
    ) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO order_items (order_id, menu_item_id, bundle_id, item_name, unit_price, \
                                      quantity, line_total, line_cost, unit_cost, cost_missing) \
             VALUES ($1, $2, $3, 'x', 1000, $4, 1000, $5, $6, $7) RETURNING id",
        )
        .bind(order)
        .bind(menu_item)
        .bind(bundle_id)
        .bind(quantity)
        .bind(stale_line_cost)
        .bind(stale_unit_cost)
        .bind(cost_missing)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn line_costs(pool: &PgPool, id: Uuid) -> (Option<i64>, Option<i64>, bool) {
        sqlx::query_as::<_, (Option<i64>, Option<i64>, bool)>(
            "SELECT line_cost, unit_cost, cost_missing FROM order_items WHERE id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn backfill_reprices_from_current_recipes(pool: PgPool) {
        let (org, s) = seed_org_branch_order(&pool).await;
        let ing = seed_ingredient_at_100(&pool, org).await;

        // L1: qty 2 of an item whose CURRENT recipe is 1 × ing(100), plus a
        // costed addon (1 × ing per addon unit) and an optional consuming
        // 0.5 × ing per parent unit. Stale stored costs are garbage.
        let m1 = seed_item_with_recipe(&pool, org, ing, 1.0).await;
        let addon_item: Uuid = sqlx::query_scalar(
            "INSERT INTO addon_items (org_id, name, type, default_price) \
             VALUES ($1, 'Syrup', 'extra', 100) RETURNING id",
        )
        .bind(org)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO addon_item_ingredients (addon_item_id, org_ingredient_id, quantity_used, ingredient_name, ingredient_unit) \
             VALUES ($1, $2, 1.0, 'ing', 'g')",
        )
        .bind(addon_item)
        .bind(ing)
        .execute(&pool)
        .await
        .unwrap();
        let l1 = insert_line(
            &pool,
            s.order,
            Some(m1),
            None,
            2,
            Some(99_999),
            Some(99_999),
            false,
        )
        .await;
        let addon_row: Uuid = sqlx::query_scalar(
            "INSERT INTO order_item_addons (order_item_id, addon_item_id, addon_name, \
                                            unit_price, quantity, line_total, line_cost) \
             VALUES ($1, $2, 'Syrup', 100, 1, 100, 88888) RETURNING id",
        )
        .bind(l1)
        .bind(addon_item)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO order_item_optionals (order_item_id, field_name, price, \
                                               org_ingredient_id, ingredient_name, ingredient_unit, \
                                               quantity_deducted, cost) \
             VALUES ($1, 'Extra shot', 0, $2, 'ing', 'g', 0.5, 7777)",
        )
        .bind(l1)
        .bind(ing)
        .execute(&pool)
        .await
        .unwrap();

        // L2: bundle line — one component whose current recipe is 3 × ing.
        let m2 = seed_item_with_recipe(&pool, org, ing, 3.0).await;
        let bundle: Uuid = sqlx::query_scalar(
            "INSERT INTO bundles (org_id, name, price) VALUES ($1, 'B', 900) RETURNING id",
        )
        .bind(org)
        .fetch_one(&pool)
        .await
        .unwrap();
        let l2 = insert_line(
            &pool,
            s.order,
            None,
            Some(bundle),
            1,
            Some(22_222),
            Some(5_555),
            false,
        )
        .await;
        sqlx::query(
            "INSERT INTO order_line_bundle_components (order_line_id, item_id, quantity, line_cost) \
             VALUES ($1, $2, 1, 11111)",
        )
        .bind(l2)
        .bind(m2)
        .execute(&pool)
        .await
        .unwrap();

        // L3: item with NO recipe today → unknowable.
        let m3 = seed_bare_item(&pool, org).await;
        let l3 = insert_line(
            &pool,
            s.order,
            Some(m3),
            None,
            1,
            Some(4_444),
            Some(4_444),
            false,
        )
        .await;

        // L4: costed recipe BUT an addon with no ingredient links → the
        // line's full cost is unknowable; recipe-scope unit_cost resolves.
        let uncosted_addon: Uuid = sqlx::query_scalar(
            "INSERT INTO addon_items (org_id, name, type, default_price) \
             VALUES ($1, 'NoIng', 'extra', 100) RETURNING id",
        )
        .bind(org)
        .fetch_one(&pool)
        .await
        .unwrap();
        let l4 = insert_line(&pool, s.order, Some(m1), None, 1, None, None, true).await;
        sqlx::query(
            "INSERT INTO order_item_addons (order_item_id, addon_item_id, addon_name, \
                                            unit_price, quantity, line_total, line_cost) \
             VALUES ($1, $2, 'NoIng', 100, 1, 100, NULL)",
        )
        .bind(l4)
        .bind(uncosted_addon)
        .execute(&pool)
        .await
        .unwrap();

        // L5: rounding tie — recipe rolls up to 46.5 piastres, which must
        // round HALF AWAY FROM ZERO (47) like the costing service, not
        // half-to-even (46) as float8 SQL would.
        let tie_ing: Uuid = sqlx::query_scalar(
            "INSERT INTO org_ingredients (org_id, name, unit, cost_per_unit) \
             VALUES ($1, $2, 'g'::inventory_unit, 93) RETURNING id",
        )
        .bind(org)
        .bind(format!("tie-{}", Uuid::new_v4()))
        .fetch_one(&pool)
        .await
        .unwrap();
        let m_tie = seed_item_with_recipe(&pool, org, tie_ing, 0.5).await;
        let l5 = insert_line(&pool, s.order, Some(m_tie), None, 1, None, None, true).await;

        // Other org: must be untouched by a branch-scoped run.
        let (_org_b, sb) = seed_org_branch_order(&pool).await;
        let lb = insert_line(
            &pool,
            sb.order,
            None,
            None,
            1,
            Some(7_777),
            Some(7_777),
            false,
        )
        .await;

        let summary = backfill_cost_snapshots(&pool, BackfillScope::Branch(s.branch), false)
            .await
            .unwrap();
        assert_eq!(summary.branches, 1);
        assert_eq!(summary.order_lines_in_scope, 5);
        assert_eq!(summary.order_lines_updated, 5);
        assert!(!summary.dry_run);

        // L1: recipe 100×2 + addon 100×1×2 + optional 50×2 = 500; unit = 100.
        assert_eq!(line_costs(&pool, l1).await, (Some(500), Some(100), false));
        let addon_cost: Option<i64> =
            sqlx::query_scalar("SELECT line_cost FROM order_item_addons WHERE id = $1")
                .bind(addon_row)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(addon_cost, Some(200)); // 100 × addon qty 1 × line qty 2
        let opt_cost: Option<i64> =
            sqlx::query_scalar("SELECT cost FROM order_item_optionals WHERE order_item_id = $1")
                .bind(l1)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(opt_cost, Some(50)); // 0.5 × 100, per parent unit

        // L2: bundle — component 3×100 = 300; unit_cost NULL by definition.
        assert_eq!(line_costs(&pool, l2).await, (Some(300), None, false));
        let comp_cost: Option<i64> = sqlx::query_scalar(
            "SELECT line_cost FROM order_line_bundle_components WHERE order_line_id = $1",
        )
        .bind(l2)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(comp_cost, Some(300));

        // L3: no current recipe → unknowable.
        assert_eq!(line_costs(&pool, l3).await, (None, None, true));

        // L4: uncosted addon keeps line_cost NULL; recipe unit_cost resolves.
        assert_eq!(line_costs(&pool, l4).await, (None, Some(100), true));

        // L5: 0.5 × 93 = 46.5 → 47 (half away from zero, matching Decimal).
        assert_eq!(line_costs(&pool, l5).await, (Some(47), Some(47), false));

        // Other org untouched by the branch scope.
        assert_eq!(
            line_costs(&pool, lb).await,
            (Some(7_777), Some(7_777), false)
        );
    }

    #[sqlx::test]
    async fn backfill_dry_run_rolls_back_and_org_scope_commits(pool: PgPool) {
        let (org, s) = seed_org_branch_order(&pool).await;
        let ing = seed_ingredient_at_100(&pool, org).await;
        let m1 = seed_item_with_recipe(&pool, org, ing, 2.0).await;
        let l1 = insert_line(
            &pool,
            s.order,
            Some(m1),
            None,
            1,
            Some(9_999),
            Some(9_999),
            false,
        )
        .await;

        // Dry run: summary reflects the would-be state, data unchanged.
        let dry = backfill_cost_snapshots(&pool, BackfillScope::Org(org), true)
            .await
            .unwrap();
        assert!(dry.dry_run);
        assert_eq!(dry.line_cost_total_before, 9_999);
        assert_eq!(dry.line_cost_total_after, 200); // 2 × 100 × qty 1
        assert_eq!(
            line_costs(&pool, l1).await,
            (Some(9_999), Some(9_999), false)
        );

        // Live org-scoped run commits.
        let live = backfill_cost_snapshots(&pool, BackfillScope::Org(org), false)
            .await
            .unwrap();
        assert!(!live.dry_run);
        assert_eq!(line_costs(&pool, l1).await, (Some(200), Some(200), false));

        // Unknown org → NotFound.
        let err = backfill_cost_snapshots(&pool, BackfillScope::Org(Uuid::new_v4()), true).await;
        assert!(matches!(err, Err(crate::errors::AppError::NotFound(_))));
    }
}
