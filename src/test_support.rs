//! Test-only helpers for the size model.
//!
//! Price lives in `menu_item_sizes`, and every item is born with a synthetic
//! `one_size` row carrying the price it was created with. A fixture that wants
//! to author an item's REAL sizes has to displace that row, exactly as the
//! editor does.

use sqlx::PgPool;
use uuid::Uuid;

/// Add one AUTHORED size to an item and return its id.
///
/// In one transaction it first drops the synthetic `one_size` row the item was
/// born with, then inserts the requested size under a fresh id. Two
/// consequences matter to fixtures:
///
///   * the first authored size replaces the sentinel rather than sitting beside
///     it, so the item does not silently become multi-size with a phantom
///     cheapest size;
///   * a fixture may author a size genuinely LABELLED `one_size` — it gets a
///     fresh id, so it is a real size a customer picks, not the sentinel, and
///     nothing later retires it.
pub(crate) async fn seed_real_size(
    pool: &PgPool,
    item: Uuid,
    label: &str,
    price: i32,
    sort: i32,
) -> Uuid {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "DELETE FROM menu_item_sizes z
          WHERE z.menu_item_id = $1
            AND z.label = 'one_size'
            AND z.id = (md5(z.menu_item_id::text || ':one_size'))::uuid",
    )
    .bind(item)
    .execute(&mut *tx)
    .await
    .unwrap();
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO menu_item_sizes (menu_item_id, label, price, sort, is_active)
         VALUES ($1, $2, $3, $4, true)
         ON CONFLICT (menu_item_id, label) DO UPDATE
             SET price = EXCLUDED.price, sort = EXCLUDED.sort, is_active = true
         RETURNING id",
    )
    .bind(item)
    .bind(label)
    .bind(price)
    .bind(sort)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    id
}

/// A loyalty member, the way the unified model stores one: a `customers` row
/// (the person) and a `loyalty_customers` row under THE SAME id (the card).
/// `phone` is kept as typed and keyed through the database's own
/// `phone_canonical`, so a deliberately bad phone seeds a member with no key.
pub async fn seed_loyalty_member(
    pool: &PgPool,
    org: Uuid,
    phone: &str,
    name: &str,
    token: &str,
) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO customers (org_id, name, phone, phone_key, source) \
         VALUES ($1, $2, $3, phone_canonical($3), 'loyalty') RETURNING id",
    )
    .bind(org)
    .bind(name)
    .bind(phone)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO loyalty_customers (id, org_id, member_token) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(org)
        .bind(token)
        .execute(pool)
        .await
        .unwrap();
    id
}
