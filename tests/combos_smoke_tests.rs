//! The combos fixture seeds cleanly through every guard trigger.
mod common;

use sqlx::PgPool;

#[sqlx::test]
async fn the_shop_seeds(pool: PgPool) {
    let s = common::combos::shop(&pool).await;
    let kind: String = sqlx::query_scalar("SELECT kind FROM menu_items WHERE id = $1")
        .bind(s.combo)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kind, "combo");
    let p: i32 = sqlx::query_scalar(
        "SELECT price FROM menu_item_sizes WHERE menu_item_id = $1 AND label = 'one_size'",
    )
    .bind(s.combo)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(p, 15000);
    // a combo inside a combo is refused by the guard
    let err = sqlx::query(
        "INSERT INTO combo_slot_choices (org_id, slot_id, menu_item_id) VALUES ($1, $2, $3)",
    )
    .bind(s.org)
    .bind(s.slot_main)
    .bind(s.combo)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(err.to_string().contains("COMBO_NESTED"), "{err}");
    // a combo has no sizes of its own
    let err = sqlx::query(
        "INSERT INTO menu_item_sizes (menu_item_id, label, price) VALUES ($1, 'Large', 100)",
    )
    .bind(s.combo)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(err.to_string().contains("COMBO_NO_RECIPE"), "{err}");
}
