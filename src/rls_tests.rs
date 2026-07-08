//! Row-Level Security verification.
//!
//! These tests prove the DATABASE enforces tenant isolation independently of any
//! application `WHERE org_id = …` filter. They seed two orgs' full data chains
//! through the base pool (which connects as the table owner and bypasses RLS),
//! then read and write through [`crate::db::Db`] tenant pools — the exact scoped
//! path production handlers use — and assert one tenant can neither see nor
//! touch the other's rows.

use crate::db::Db;
use sqlx::PgPool;
use uuid::Uuid;

/// Seed one org with a complete chain: org → user(teller) → branch → till →
/// shift → category → menu_item → order → order_item. Uses the owner pool, so
/// RLS is bypassed for setup. Returns the ids a test needs to probe.
struct Seed {
    org: Uuid,
    branch: Uuid,
    menu_item: Uuid,
    order: Uuid,
    order_item: Uuid,
}

async fn seed_org(pool: &PgPool, label: &str) -> Seed {
    let org = Uuid::new_v4();
    let teller = Uuid::new_v4();
    let branch = Uuid::new_v4();
    let till = Uuid::new_v4();
    let shift = Uuid::new_v4();
    let category = Uuid::new_v4();
    let menu_item = Uuid::new_v4();
    let order = Uuid::new_v4();
    let order_item = Uuid::new_v4();

    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, $2, $3)")
        .bind(org)
        .bind(format!("Org {label}"))
        .bind(format!("org-{}", org.simple()))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO users (id, name, role, org_id, pin_hash) VALUES ($1, $2, 'teller', $3, 'x')",
    )
    .bind(teller)
    .bind(format!("Teller {label}"))
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO branches (id, org_id, name, code) VALUES ($1, $2, $3, $4)")
        .bind(branch)
        .bind(org)
        .bind(format!("Branch {label}"))
        .bind(org.simple().to_string()[..6].to_uppercase())
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO tills (id, org_id, branch_id, name) VALUES ($1, $2, $3, 'Till')")
        .bind(till)
        .bind(org)
        .bind(branch)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO shifts (id, branch_id, teller_id, till_id) VALUES ($1, $2, $3, $4)")
        .bind(shift)
        .bind(branch)
        .bind(teller)
        .bind(till)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'Cat')")
        .bind(category)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO menu_items (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(menu_item)
        .bind(org)
        .bind(format!("Item {label}"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO orders (id, branch_id, shift_id, teller_id, order_number, payment_method, order_ref)
         VALUES ($1, $2, $3, $4, 1, 'cash', $5)",
    )
    .bind(order)
    .bind(branch)
    .bind(shift)
    .bind(teller)
    .bind(format!("REF-{}", &order.simple().to_string()[..8]))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO order_items (id, order_id, item_name, unit_price, line_total)
         VALUES ($1, $2, 'Line', 100, 100)",
    )
    .bind(order_item)
    .bind(order)
    .execute(pool)
    .await
    .unwrap();

    Seed {
        org,
        branch,
        menu_item,
        order,
        order_item,
    }
}

async fn count(db: &Db, sql: &str, id: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .bind(id)
        .fetch_one(db.get_ref())
        .await
        .unwrap()
}

/// Coverage: every public base table must enforce row security. The only
/// sanctioned exemption is `_sqlx_migrations` (owner-only bookkeeping). A new
/// table shipped without a policy fails here.
#[sqlx::test]
async fn rls_coverage_every_table_enforced(pool: PgPool) {
    let unprotected: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname FROM pg_class c
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = 'public' AND c.relkind = 'r'
           AND c.relname <> '_sqlx_migrations'
           AND NOT c.relrowsecurity
         ORDER BY c.relname",
    )
    .fetch_all(&pool)
    .await
    .unwrap();

    assert!(
        unprotected.is_empty(),
        "these public tables have no row-level security: {unprotected:?}"
    );
}

/// Reads through a tenant pool return only that tenant's rows — the other org's
/// org/branch/menu_item/order/order_item are all invisible.
#[sqlx::test]
async fn rls_cross_tenant_read_isolation(pool: PgPool) {
    let a = seed_org(&pool, "A").await;
    let b = seed_org(&pool, "B").await;

    let dba = Db::for_org(&pool, a.org).await;

    // A sees its own chain…
    assert_eq!(
        count(
            &dba,
            "SELECT count(*) FROM organizations WHERE id=$1",
            a.org
        )
        .await,
        1
    );
    assert_eq!(
        count(&dba, "SELECT count(*) FROM branches WHERE id=$1", a.branch).await,
        1
    );
    assert_eq!(
        count(
            &dba,
            "SELECT count(*) FROM menu_items WHERE id=$1",
            a.menu_item
        )
        .await,
        1
    );
    assert_eq!(
        count(&dba, "SELECT count(*) FROM orders WHERE id=$1", a.order).await,
        1
    );
    assert_eq!(
        count(
            &dba,
            "SELECT count(*) FROM order_items WHERE id=$1",
            a.order_item
        )
        .await,
        1
    );

    // …and NONE of B's, at every level of the chain (org, branch, child, grandchild).
    assert_eq!(
        count(
            &dba,
            "SELECT count(*) FROM organizations WHERE id=$1",
            b.org
        )
        .await,
        0
    );
    assert_eq!(
        count(&dba, "SELECT count(*) FROM branches WHERE id=$1", b.branch).await,
        0
    );
    assert_eq!(
        count(
            &dba,
            "SELECT count(*) FROM menu_items WHERE id=$1",
            b.menu_item
        )
        .await,
        0
    );
    assert_eq!(
        count(&dba, "SELECT count(*) FROM orders WHERE id=$1", b.order).await,
        0
    );
    assert_eq!(
        count(
            &dba,
            "SELECT count(*) FROM order_items WHERE id=$1",
            b.order_item
        )
        .await,
        0
    );

    // Unqualified aggregate (the "forgot the WHERE org_id" case) still only
    // counts A's single order — RLS is the backstop, not the filter.
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM orders")
            .fetch_one(dba.get_ref())
            .await
            .unwrap(),
        1
    );
}

/// Writes cannot cross the tenant boundary: no UPDATE reaches another org's
/// rows, and no INSERT can place a row into — or point a child FK at — another
/// org (WITH CHECK defaults to the USING predicate).
#[sqlx::test]
async fn rls_cross_tenant_write_rejected(pool: PgPool) {
    let a = seed_org(&pool, "A").await;
    let b = seed_org(&pool, "B").await;
    let dba = Db::for_org(&pool, a.org).await;

    // UPDATE targeting B's order from A's pool touches zero rows (invisible).
    let updated = sqlx::query("UPDATE orders SET notes = 'x' WHERE id = $1")
        .bind(b.order)
        .execute(dba.get_ref())
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(updated, 0, "A must not be able to update B's order");
    // Confirm via the owner pool that B's order is genuinely untouched.
    let leaked: Option<String> = sqlx::query_scalar("SELECT notes FROM orders WHERE id = $1")
        .bind(b.order)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(leaked, None);

    // INSERT a branch into B's org from A's pool → WITH CHECK violation.
    let forged_branch =
        sqlx::query("INSERT INTO branches (org_id, name, code) VALUES ($1, 'evil', 'EVIL01')")
            .bind(b.org)
            .execute(dba.get_ref())
            .await;
    assert!(
        forged_branch.is_err(),
        "A must not insert a branch into B's org"
    );

    // INSERT an order_item pointing at B's order from A's pool → the child
    // policy's parent EXISTS fails, so WITH CHECK rejects it.
    let forged_item = sqlx::query(
        "INSERT INTO order_items (order_id, item_name, unit_price, line_total)
         VALUES ($1, 'evil', 1, 1)",
    )
    .bind(b.order)
    .execute(dba.get_ref())
    .await;
    assert!(
        forged_item.is_err(),
        "A must not attach a line to B's order"
    );
}

/// A pool scoped to an org that owns nothing (deny-by-default) sees no rows.
#[sqlx::test]
async fn rls_unknown_org_sees_nothing(pool: PgPool) {
    seed_org(&pool, "A").await;
    let stranger = Db::for_org(&pool, Uuid::new_v4()).await;

    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM orders")
            .fetch_one(stranger.get_ref())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM menu_items")
            .fetch_one(stranger.get_ref())
            .await
            .unwrap(),
        0
    );
}
