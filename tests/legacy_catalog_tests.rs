//! `tests/common/legacy_catalog` writes the unified tables; each helper must
//! read back through its compat view exactly as the old row it replaces.

mod common;

use common::legacy_catalog as lc;
use sqlx::PgPool;
use uuid::Uuid;

struct Shop {
    org: Uuid,
    branch: Uuid,
    item: Uuid,
    sized: Uuid,
    ing: Uuid,
}

async fn shop(pool: &PgPool) -> Shop {
    let (org, branch, item, sized, ing) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations (id, name, slug) VALUES ('{org}', 'LC', 'lc-{org}');
         INSERT INTO branches (id, org_id, name) VALUES ('{branch}', '{org}', 'Main');
         INSERT INTO menu_items (id, org_id, name, base_price) VALUES ('{item}', '{org}', 'Tea', 2000);
         INSERT INTO menu_items (id, org_id, name, base_price) VALUES ('{sized}', '{org}', 'Latte', 3000);
         INSERT INTO menu_item_sizes (menu_item_id, label, price, sort) VALUES ('{sized}', 'Large', 4000, 1);
         DELETE FROM menu_item_sizes WHERE menu_item_id = '{sized}' AND label = 'one_size';
         INSERT INTO org_ingredients (id, org_id, name, unit) VALUES ('{ing}', '{org}', 'Milk', 'ml');"
    ))
    .execute(pool)
    .await
    .unwrap();
    Shop { org, branch, item, sized, ing }
}

#[sqlx::test]
async fn addons_and_their_lines_read_back_through_the_views(pool: PgPool) {
    let s = shop(&pool).await;
    let id = Uuid::new_v4();
    assert_eq!(lc::addon_item(&pool, Some(id), s.org, "Oat", "milk_type", 700).await, id);
    let other = lc::addon_item(&pool, None, s.org, "Soy", "milk_type", 600).await;
    lc::addon_ingredient(&pool, id, s.ing, 200, "ml").await;

    let row: (Uuid, String, Option<String>, i32, bool) = sqlx::query_as(
        "SELECT org_id, name, type, default_price, is_active FROM addon_items WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row, (s.org, "Oat".into(), Some("milk_type".into()), 700, true));
    // Same type, same group.
    let groups: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT group_id) FROM modifier_options WHERE id = ANY($1)",
    )
    .bind(vec![id, other])
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(groups, 1);
    let line: (Uuid, f64, String, String) = sqlx::query_as(
        "SELECT org_ingredient_id, quantity_used::float8, ingredient_name, ingredient_unit
           FROM addon_item_ingredients WHERE addon_item_id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(line, (s.ing, 200.0, "Milk".into(), "ml".into()));
}

#[sqlx::test]
async fn recipes_optionals_slots_and_allowlists_read_back(pool: PgPool) {
    let s = shop(&pool).await;
    lc::item_recipe(&pool, s.item, "one_size", s.ing, 150, "ml").await;
    let r: (String, f64) = sqlx::query_as(
        "SELECT size_label, quantity_used::float8 FROM menu_item_recipes WHERE menu_item_id = $1",
    )
    .bind(s.item)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(r, ("one_size".into(), 150.0));

    let f = lc::optional_field(
        &pool,
        lc::Optional::new(s.item, "Extra hot", 50).recipe(s.ing, 10, "ml"),
    )
    .await;
    let o: (Uuid, String, i32, Option<Uuid>, Option<f64>) = sqlx::query_as(
        "SELECT menu_item_id, name, price, org_ingredient_id, quantity_used::float8
           FROM menu_item_optional_fields WHERE id = $1",
    )
    .bind(f)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(o, (s.item, "Extra hot".into(), 50, Some(s.ing), Some(10.0)));

    let a = lc::addon_item(&pool, None, s.org, "Vanilla", "syrup", 100).await;
    let b = lc::addon_item(&pool, None, s.org, "Caramel", "syrup", 100).await;
    let slot = lc::addon_slot(&pool, s.org, s.item, "syrup", true, 1, Some(2)).await;
    let sl: (Uuid, String, bool, i32, Option<i32>) = sqlx::query_as(
        "SELECT id, addon_type, is_required, min_selections, max_selections
           FROM menu_item_addon_slots WHERE menu_item_id = $1",
    )
    .bind(s.item)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(sl, (slot, "syrup".into(), true, 1, Some(2)));
    lc::allowed_addons(&pool, s.item, &[b, a]).await;
    let allowed: Vec<Uuid> = sqlx::query_scalar(
        "SELECT addon_item_id FROM menu_item_allowed_addons WHERE menu_item_id = $1 ORDER BY sort_order",
    )
    .bind(s.item)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(allowed, vec![b, a]);
}

#[sqlx::test]
async fn overrides_read_back_through_the_legacy_views(pool: PgPool) {
    let s = shop(&pool).await;
    // Size-less item: the menu price lands on one_size.
    lc::branch_menu_override(&pool, s.branch, s.item, Some(2500), true).await;
    let p: (Option<i32>, bool) = sqlx::query_as(
        "SELECT price_override, is_available FROM branch_menu_overrides WHERE branch_id = $1 AND menu_item_id = $2",
    )
    .bind(s.branch)
    .bind(s.item)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(p, (Some(2500), true));
    // Sized item: no one_size, so a menu-level price is not applied (as the
    // backfill did); a size price is; a missing size prices nothing.
    lc::branch_menu_override(&pool, s.branch, s.sized, Some(9999), true).await;
    lc::branch_size_override(&pool, s.branch, s.sized, "Large", 4200).await;
    lc::branch_size_override(&pool, s.branch, s.sized, "Jumbo", 5000).await;
    let sizes: Vec<(String, Option<i32>)> = sqlx::query_as(
        "SELECT size_label, price_override FROM branch_menu_size_overrides
          WHERE branch_id = $1 AND menu_item_id = $2 ORDER BY 1",
    )
    .bind(s.branch)
    .bind(s.sized)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(sizes, vec![("Large".into(), Some(4200))]);
    // Off at the branch.
    lc::branch_menu_override(&pool, s.branch, s.sized, None, false).await;
    let off: bool = sqlx::query_scalar(
        "SELECT is_available FROM branch_menu_overrides WHERE branch_id = $1 AND menu_item_id = $2",
    )
    .bind(s.branch)
    .bind(s.sized)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!off);
    lc::clear_branch_menu_override(&pool, s.branch, s.sized).await;
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM branch_menu_overrides WHERE menu_item_id = $1",
    )
    .bind(s.sized)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 0);

    let addon = lc::addon_item(&pool, None, s.org, "Shot", "extra", 100).await;
    lc::branch_addon_override(&pool, s.branch, addon, Some(150), true).await;
    let a: (Option<i32>, bool) = sqlx::query_as(
        "SELECT price_override, is_available FROM branch_addon_overrides WHERE addon_item_id = $1",
    )
    .bind(addon)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(a, (Some(150), true));
    lc::branch_addon_override(&pool, s.branch, addon, None, false).await;
    let a: (Option<i32>, bool) = sqlx::query_as(
        "SELECT price_override, is_available FROM branch_addon_overrides WHERE addon_item_id = $1",
    )
    .bind(addon)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(a, (None, false));

    lc::channel_menu_override(&pool, s.branch, s.item, "in_mall", Some(2700), Some(true)).await;
    let c: (Option<i32>, Option<bool>) = sqlx::query_as(
        "SELECT price_override, is_available FROM branch_channel_menu_overrides
          WHERE branch_id = $1 AND menu_item_id = $2 AND channel = 'in_mall'",
    )
    .bind(s.branch)
    .bind(s.item)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(c, (Some(2700), Some(true)));
    lc::channel_addon_override(&pool, s.branch, addon, "in_mall", Some(130), None).await;
    let c: (Option<i32>, Option<bool>) = sqlx::query_as(
        "SELECT price_override, is_available FROM branch_channel_addon_overrides WHERE addon_item_id = $1",
    )
    .bind(addon)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(c, (Some(130), None));
}
