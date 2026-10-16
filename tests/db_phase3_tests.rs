//! DB architecture Phase 3 (DB_ARCHITECTURE_AUDIT_2026-10-16 §8): the
//! attachment option table that replaces `included_option_ids` (3.4), and the
//! `compat` schema (3.3).

use sqlx::PgPool;
use uuid::Uuid;

struct Cat {
    org: Uuid,
    mimg: Uuid,
    opts: [Uuid; 3],
}

async fn catalog(pool: &PgPool) -> Cat {
    let (org, item, group, mimg) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let opts = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations (id, name, slug) VALUES ('{org}', 'P3', 'p3-{org}');
         INSERT INTO menu_items (id, org_id, name) VALUES ('{item}', '{org}', 'Latte');
         INSERT INTO modifier_groups (id, org_id, name, legacy_addon_type) VALUES ('{group}', '{org}', 'Milk', 'milk_type');
         INSERT INTO modifier_options (id, group_id, name, legacy_source, sort)
              VALUES ('{a}', '{group}', 'Oat', 'addon', 1), ('{b}', '{group}', 'Soy', 'addon', 2),
                     ('{c}', '{group}', 'Whole', 'addon', 3);
         INSERT INTO menu_item_modifier_groups (id, menu_item_id, group_id, included_option_ids)
              VALUES ('{mimg}', '{item}', '{group}', ARRAY['{c}', '{a}']::uuid[]);",
        a = opts[0],
        b = opts[1],
        c = opts[2],
    ))
    .execute(pool)
    .await
    .unwrap();
    Cat { org, mimg, opts }
}

async fn join_rows(pool: &PgPool, mimg: Uuid) -> Vec<(Uuid, i32)> {
    sqlx::query_as(
        "SELECT option_id, sort FROM menu_item_modifier_group_options WHERE mimg_id = $1 ORDER BY sort",
    )
    .bind(mimg)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn as_array(pool: &PgPool, mimg: Uuid) -> Option<Vec<Uuid>> {
    sqlx::query_scalar(
        "SELECT mimg_included_options(id, option_allowlist) FROM menu_item_modifier_groups WHERE id = $1",
    )
    .bind(mimg)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn every_write_of_the_array_is_mirrored_into_the_join_table(pool: PgPool) {
    let c = catalog(&pool).await;
    let [a, b, cc] = c.opts;
    // Insert: the allowlist, in its order.
    assert_eq!(join_rows(&pool, c.mimg).await, vec![(cc, 0), (a, 1)]);
    assert_eq!(as_array(&pool, c.mimg).await, Some(vec![cc, a]));

    // Update: replaced, not merged.
    sqlx::query("UPDATE menu_item_modifier_groups SET included_option_ids = $2 WHERE id = $1")
        .bind(c.mimg)
        .bind(vec![b])
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(join_rows(&pool, c.mimg).await, vec![(b, 0)]);

    // An empty allowlist is not "all options".
    sqlx::query("UPDATE menu_item_modifier_groups SET included_option_ids = '{}' WHERE id = $1")
        .bind(c.mimg)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(join_rows(&pool, c.mimg).await, vec![]);
    assert_eq!(as_array(&pool, c.mimg).await, Some(vec![]));

    // NULL is never stored: `mimg_fill_provenance` (BEFORE, and before the flag
    // trigger) turns it into the group's whole option list, so "every option"
    // arrives as an explicit allowlist of all of them.
    sqlx::query("UPDATE menu_item_modifier_groups SET included_option_ids = NULL WHERE id = $1")
        .bind(c.mimg)
        .execute(&pool)
        .await
        .unwrap();
    let stored: Option<Vec<Uuid>> =
        sqlx::query_scalar("SELECT included_option_ids FROM menu_item_modifier_groups WHERE id = $1")
            .bind(c.mimg)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(join_rows(&pool, c.mimg).await.len(), 3);
    assert_eq!(as_array(&pool, c.mimg).await, stored);
    let flag: bool =
        sqlx::query_scalar("SELECT option_allowlist FROM menu_item_modifier_groups WHERE id = $1")
            .bind(c.mimg)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(flag);

    // An option that goes takes its join row with it.
    sqlx::query("UPDATE menu_item_modifier_groups SET included_option_ids = $2 WHERE id = $1")
        .bind(c.mimg)
        .bind(vec![a, b])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM modifier_options WHERE id = $1")
        .bind(a)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(join_rows(&pool, c.mimg).await, vec![(b, 1)]);
}

#[sqlx::test]
async fn the_join_table_is_tenant_scoped(pool: PgPool) {
    let c = catalog(&pool).await;
    let other = catalog(&pool).await;
    let org: Uuid = sqlx::query_scalar("SELECT org_id FROM menu_item_modifier_group_options WHERE mimg_id = $1 LIMIT 1")
        .bind(c.mimg)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(org, c.org, "fill_org_id takes the attachment's org");
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.org_id', $1, true)")
        .bind(c.org.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SET LOCAL ROLE madar_app").execute(&mut *tx).await.unwrap();
    let mimgs: Vec<Uuid> =
        sqlx::query_scalar("SELECT DISTINCT mimg_id FROM menu_item_modifier_group_options")
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    assert_eq!(mimgs, vec![c.mimg]);
    assert!(!mimgs.contains(&other.mimg));
}

/// 3.3: the compat schema holds a copy of every old-shape view, and each one
/// answers exactly what its public twin does.
#[sqlx::test]
async fn compat_views_answer_like_the_public_ones(pool: PgPool) {
    let _c = catalog(&pool).await;
    for v in [
        "addon_item_ingredients", "addon_items", "branch_addon_overrides",
        "branch_channel_addon_overrides", "branch_channel_menu_overrides",
        "branch_menu_overrides", "branch_menu_size_overrides", "item_sizes",
        "menu_item_addon_slots", "menu_item_optional_fields", "menu_item_recipes",
        "loyalty_members_v",
    ] {
        let diff: i64 = sqlx::query_scalar(&format!(
            "SELECT (SELECT count(*) FROM (SELECT * FROM public.{v} EXCEPT ALL SELECT * FROM compat.{v}) a)
                  + (SELECT count(*) FROM (SELECT * FROM compat.{v} EXCEPT ALL SELECT * FROM public.{v}) b)"
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(diff, 0, "compat.{v}");
    }
    let diff: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM (SELECT menu_item_id, addon_item_id, sort_order FROM public.menu_item_allowed_addons
                                       EXCEPT ALL SELECT menu_item_id, addon_item_id, sort_order FROM compat.menu_item_allowed_addons) a)
              + (SELECT count(*) FROM (SELECT menu_item_id, addon_item_id, sort_order FROM compat.menu_item_allowed_addons
                                       EXCEPT ALL SELECT menu_item_id, addon_item_id, sort_order FROM public.menu_item_allowed_addons) b)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(diff, 0, "compat.menu_item_allowed_addons (join table) = the array view");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM compat.menu_item_allowed_addons")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 2);
}
