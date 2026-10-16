//! DB architecture Phase 2 (DB_ARCHITECTURE_AUDIT_2026-10-16 §8): flat RLS,
//! `org_id NOT NULL`, the index cleanup (per-org uniques instead of global
//! ones) and the convergence of fresh databases with prod's catalog shape.

use sqlx::PgPool;
use uuid::Uuid;

/// The tables 20261018100000 flattened (their policy reached the org through
/// a parent before).
const FLATTENED: &[&str] = &[
    "orders", "tills", "inventory_movements", "branch_stock", "sync_changes",
    "asset_bundle_dirty", "branch_delivery_settings", "category_station_routes",
    "delivery_ref_counters", "delivery_zones", "menu_item_station_routes",
    "order_ref_counters", "ticket_ref_counters", "order_items", "order_payments",
    "order_item_addons", "order_item_optionals", "till_cash_movements",
    "till_reconciliations", "till_spot_views", "open_ticket_rounds", "open_ticket_items",
    "kitchen_ticket_items", "menu_item_sizes", "menu_item_modifier_groups",
    "menu_item_price_epochs", "modifier_options", "recipe_lines", "ingredient_cost_history",
    "stocktake_items", "purchase_order_lines", "goods_receipt_lines", "stock_transfer_lines",
    "packaging_rule_lines", "recipe_base_lines", "menu_price_overrides",
];

struct Shop {
    org: Uuid,
    branch: Uuid,
    till: Uuid,
    teller: Uuid,
    order: Uuid,
    item: Uuid,
    size: Uuid,
}

async fn shop(pool: &PgPool, label: &str) -> Shop {
    let s = Shop {
        org: Uuid::new_v4(),
        branch: Uuid::new_v4(),
        till: Uuid::new_v4(),
        teller: Uuid::new_v4(),
        order: Uuid::new_v4(),
        item: Uuid::new_v4(),
        size: Uuid::new_v4(),
    };
    let menu_item = Uuid::new_v4();
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations (id, name, slug) VALUES ('{org}', 'Org {label}', 'p2-{org}');
         INSERT INTO users (id, name, role, org_id, pin_hash) VALUES ('{teller}', 'T {label}', 'teller', '{org}', 'x');
         INSERT INTO branches (id, org_id, name, code) VALUES ('{branch}', '{org}', 'B {label}', 'SAME');
         INSERT INTO tills (id, branch_id, teller_id) VALUES ('{till}', '{branch}', '{teller}');
         INSERT INTO orders (id, branch_id, till_id, teller_id, order_number, payment_method, order_ref,
                             idempotency_key, subtotal, total_amount)
              VALUES ('{order}', '{branch}', '{till}', '{teller}', 1, 'cash', 'SAME-REF',
                      '00000000-0000-0000-0000-00000000c0de', 300, 300);
         INSERT INTO order_items (id, order_id, item_name, unit_price, quantity, line_total)
              VALUES ('{item}', '{order}', 'Latte', 100, 3, 300);
         INSERT INTO order_item_addons (order_item_id, addon_item_id, addon_name, unit_price, line_total)
              VALUES ('{item}', gen_random_uuid(), 'Oat', 20, 20);
         INSERT INTO menu_items (id, org_id, name) VALUES ('{menu_item}', '{org}', 'Latte');
         INSERT INTO menu_item_sizes (id, menu_item_id, label, price) VALUES ('{size}', '{menu_item}', 'M', 100);
         INSERT INTO delivery_zones (branch_id, name, max_road_distance_meters, fee)
              VALUES ('{branch}', 'Near', 1000, 10);
         -- channel scope: no branch, the org comes from the size it prices
         INSERT INTO menu_price_overrides (scope, channel, target_type, target_id, price)
              VALUES ('channel', 'talabat', 'menu_item_size', '{size}', 120);",
        org = s.org,
        teller = s.teller,
        branch = s.branch,
        till = s.till,
        order = s.order,
        item = s.item,
        size = s.size,
    ))
    .execute(pool)
    .await
    .unwrap();
    s
}

/// Rows of `table` a tenant session of `org` sees.
async fn visible(pool: &PgPool, org: Uuid, table: &str) -> i64 {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.org_id', $1, true)")
        .bind(org.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SET LOCAL ROLE madar_app")
        .execute(&mut *tx)
        .await
        .unwrap();
    let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    n
}

#[sqlx::test]
async fn every_flattened_policy_is_the_flat_non_indexable_form(pool: PgPool) {
    let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT tablename::text, qual, with_check FROM pg_policies
          WHERE schemaname = 'public' AND policyname = 'tenant_isolation' AND tablename = ANY($1)",
    )
    .bind(FLATTENED.iter().map(|t| t.to_string()).collect::<Vec<_>>())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), FLATTENED.len(), "one tenant_isolation policy per table");
    let flat = "((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)) IS TRUE)";
    for (t, qual, check) in rows {
        assert_eq!(qual.as_deref(), Some(flat), "{t} USING");
        assert_eq!(check.as_deref(), Some(flat), "{t} WITH CHECK");
    }
    // ...and every one of those org_id columns is NOT NULL.
    let nullable: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.columns
          WHERE table_schema = 'public' AND column_name = 'org_id' AND is_nullable = 'YES'
            AND table_name = ANY($1)",
    )
    .bind(FLATTENED.iter().map(|t| t.to_string()).collect::<Vec<_>>())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(nullable.is_empty(), "{nullable:?}");
}

#[sqlx::test]
async fn a_tenant_sees_only_its_own_rows_through_the_flat_policies(pool: PgPool) {
    let a = shop(&pool, "A").await;
    let b = shop(&pool, "B").await;
    for t in [
        "orders", "tills", "order_items", "order_item_addons", "menu_item_sizes",
        "delivery_zones", "menu_price_overrides",
    ] {
        assert_eq!(visible(&pool, a.org, t).await, 1, "{t} as A");
        assert_eq!(visible(&pool, b.org, t).await, 1, "{t} as B");
    }
    // No org stamped: nothing (unset) or an error ('' fails the uuid cast),
    // never everything.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL ROLE madar_app").execute(&mut *tx).await.unwrap();
    match sqlx::query_scalar::<_, i64>("SELECT count(*) FROM order_items")
        .fetch_one(&mut *tx)
        .await
    {
        Ok(n) => assert_eq!(n, 0),
        Err(e) => assert_eq!(e.as_database_error().and_then(|d| d.code()).as_deref(), Some("22P02")),
    }
    tx.rollback().await.unwrap();
}

#[sqlx::test]
async fn the_new_org_id_columns_are_filled_from_the_parent(pool: PgPool) {
    let a = shop(&pool, "A").await;
    let zone: Uuid = sqlx::query_scalar("SELECT org_id FROM delivery_zones WHERE branch_id = $1")
        .bind(a.branch)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(zone, a.org);
    let mpo: Uuid =
        sqlx::query_scalar("SELECT org_id FROM menu_price_overrides WHERE target_id = $1")
            .bind(a.size)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(mpo, a.org, "a channel-scoped override takes the org of its size");
    // A tenant cannot write a row into another org: the parent is invisible,
    // so the fill trigger finds no org and refuses.
    let b = shop(&pool, "B").await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.org_id', $1, true)")
        .bind(a.org.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SET LOCAL ROLE madar_app").execute(&mut *tx).await.unwrap();
    let err = sqlx::query(
        "INSERT INTO delivery_zones (branch_id, name, max_road_distance_meters, fee) VALUES ($1, 'x', 5, 1)",
    )
    .bind(b.branch)
    .execute(&mut *tx)
    .await
    .unwrap_err();
    assert_eq!(err.as_database_error().and_then(|e| e.code()).as_deref(), Some("23503"));
}

/// Refs and idempotency keys are minted per device / branch and codes repeat
/// across orgs: uniqueness is per org (Phase 2.3), never global.
#[sqlx::test]
async fn keys_and_refs_are_unique_per_org_not_globally(pool: PgPool) {
    // Both shops use branch code SAME, order_ref SAME-REF and the same
    // idempotency key: two orgs, no conflict.
    let a = shop(&pool, "A").await;
    let _b = shop(&pool, "B").await;
    // Within one org the same key still conflicts, on an index the race
    // handlers recognise ("idem" / "order_ref").
    for (col, val, needle) in [
        ("idempotency_key", "'00000000-0000-0000-0000-00000000c0de'::uuid", "idem"),
        ("order_ref", "'SAME-REF'", "order_ref"),
    ] {
        let other = if col == "order_ref" { "idempotency_key" } else { "order_ref" };
        let other_val = if col == "order_ref" { "gen_random_uuid()" } else { "'OTHER'" };
        let err = sqlx::query(&format!(
            "INSERT INTO orders (branch_id, till_id, teller_id, order_number, payment_method,
                                 {col}, {other}, subtotal, total_amount)
             VALUES ($1, $2, $3, 2, 'cash', {val}, {other_val}, 1, 1)"
        ))
        .bind(a.branch)
        .bind(a.till)
        .bind(a.teller)
        .execute(&pool)
        .await
        .unwrap_err();
        let db = err.as_database_error().unwrap();
        assert_eq!(db.code().as_deref(), Some("23505"), "{col}");
        assert!(db.constraint().is_some_and(|c| c.contains(needle)), "{col}: {:?}", db.constraint());
    }
}

/// L2: a deleted branch no longer holds its code for ever.
#[sqlx::test]
async fn a_deleted_branchs_code_can_be_used_again(pool: PgPool) {
    let a = shop(&pool, "A").await;
    sqlx::query("UPDATE branches SET deleted_at = now() WHERE id = $1")
        .bind(a.branch)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO branches (org_id, name, code) VALUES ($1, 'Again', 'SAME')")
        .bind(a.org)
        .execute(&pool)
        .await
        .unwrap();
    // Two LIVE branches still cannot share it.
    let err = sqlx::query("INSERT INTO branches (org_id, name, code) VALUES ($1, 'Third', 'SAME')")
        .bind(a.org)
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(err.as_database_error().and_then(|e| e.code()).as_deref(), Some("23505"));
}

/// Phase 2.4: a database built from migrations/ has prod's catalog shape.
#[sqlx::test]
async fn the_legacy_catalog_is_prods_views_and_the_old_tables_are_archived(pool: PgPool) {
    let views: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.relname::text, c.relkind::text FROM pg_class c
          WHERE c.relnamespace = 'public'::regnamespace AND c.relname = ANY($1) ORDER BY 1",
    )
    .bind(
        [
            "addon_item_ingredients", "addon_items", "branch_addon_overrides",
            "branch_channel_addon_overrides", "branch_channel_menu_overrides",
            "branch_menu_overrides", "branch_menu_size_overrides", "item_sizes",
            "menu_item_addon_overrides", "menu_item_addon_slots", "menu_item_allowed_addons",
            "menu_item_optional_fields", "menu_item_recipes",
        ]
        .map(String::from)
        .to_vec(),
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(views.len(), 12, "every name but menu_item_addon_overrides: {views:?}");
    assert!(views.iter().all(|(_, k)| k == "v"), "{views:?}");
    let invoker: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_class WHERE relnamespace = 'public'::regnamespace AND relkind = 'v'
            AND relname = ANY($1) AND 'security_invoker=true' = ANY(reloptions)",
    )
    .bind(views.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(invoker, 12, "RLS applies through every compat view");
    // The tables were moved, not dropped, and tenants cannot reach them.
    let archived: Vec<String> = sqlx::query_scalar(
        "SELECT relname::text FROM pg_class WHERE relnamespace = 'archive'::regnamespace AND relkind = 'r'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(archived.len(), 12, "{archived:?}");
    for t in &archived {
        let can: bool = sqlx::query_scalar("SELECT has_table_privilege('madar_app', $1, 'SELECT')")
            .bind(format!("archive.{t}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!can, "madar_app reads archive.{t}");
    }
}
