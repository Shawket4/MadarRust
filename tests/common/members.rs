//! A loyalty member under the unified customers model.

use sqlx::PgPool;
use uuid::Uuid;

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
