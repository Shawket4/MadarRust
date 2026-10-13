//! `sync_live_rows_for_branch` (migration `20261011120000_signup_scale`) is
//! `sync_live_rows()` restricted to one branch, so a new branch's backfill no
//! longer reads every org's data. It is a second copy of the arms, so these
//! tests hold the two together: a type added to one and not the other, or an
//! arm whose branch filter drops rows, fails here.

use sqlx::PgPool;
use std::collections::BTreeSet;
use uuid::Uuid;

/// The `'type'` literals a function's arms emit, read from its definition.
async fn arm_types(pool: &PgPool, func: &str) -> BTreeSet<String> {
    let def: String = sqlx::query_scalar("SELECT pg_get_functiondef($1::regproc)")
        .bind(func)
        .fetch_one(pool)
        .await
        .unwrap();
    let re = regex::Regex::new(r"SELECT (?:DISTINCT )?\S+, '([a-z_]+)', ").unwrap();
    re.captures_iter(&def).map(|c| c[1].to_string()).collect()
}

#[sqlx::test]
async fn both_functions_list_the_same_types(pool: PgPool) {
    let whole = arm_types(&pool, "sync_live_rows").await;
    let one = arm_types(&pool, "sync_live_rows_for_branch").await;
    assert!(whole.len() > 20, "parsed too few arms: {whole:?}");
    assert_eq!(
        whole, one,
        "sync_live_rows and sync_live_rows_for_branch must list the same types; \
         add the new arm to both"
    );
}

async fn seed_shop(pool: &PgPool, label: &str) -> (Uuid, Uuid) {
    let org = Uuid::new_v4();
    let branch = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, $2, $3)")
        .bind(org)
        .bind(format!("Org {label}"))
        .bind(format!("org-{}", org.simple()))
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
    let cat = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'Cat')")
        .bind(cat)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    for i in 0..3 {
        sqlx::query(
            "INSERT INTO menu_items (id, org_id, category_id, name, base_price) VALUES ($1, $2, $3, $4, 1000)",
        )
        .bind(Uuid::new_v4())
        .bind(org)
        .bind(cat)
        .bind(format!("Item {label}{i}"))
        .execute(pool)
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO org_payment_methods (org_id, name, color, icon, is_cash) VALUES ($1, 'cash', '#22c55e', 'cash', true)")
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO users (id, name, role, org_id, pin_hash) VALUES ($1, $2, 'teller', $3, 'x')",
    )
    .bind(Uuid::new_v4())
    .bind(format!("Teller {label}"))
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    (org, branch)
}

type Row = (Uuid, String, Uuid);

#[sqlx::test]
async fn one_branch_gets_exactly_its_slice_of_the_whole_feed(pool: PgPool) {
    let (_, a) = seed_shop(&pool, "A").await;
    let (_, b) = seed_shop(&pool, "B").await;
    for branch in [a, b] {
        let whole: BTreeSet<Row> = sqlx::query_as::<_, Row>(
            "SELECT branch_id, type, entity_id FROM sync_live_rows() WHERE branch_id = $1",
        )
        .bind(branch)
        .fetch_all(&pool)
        .await
        .unwrap()
        .into_iter()
        .collect();
        let one: BTreeSet<Row> = sqlx::query_as::<_, Row>(
            "SELECT branch_id, type, entity_id FROM sync_live_rows_for_branch($1)",
        )
        .bind(branch)
        .fetch_all(&pool)
        .await
        .unwrap()
        .into_iter()
        .collect();
        assert!(one.len() >= 6, "the seed should produce rows, got {one:?}");
        assert_eq!(whole, one, "branch {branch}");
    }
}

#[sqlx::test]
async fn a_new_branch_is_backfilled_with_its_orgs_catalogue(pool: PgPool) {
    let (org, _) = seed_shop(&pool, "A").await;
    let second = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO branches (id, org_id, name, code) VALUES ($1, $2, 'Second', 'SECOND')",
    )
    .bind(second)
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    let items: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sync_changes WHERE branch_id = $1 AND type = 'menu_item'",
    )
    .bind(second)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        items, 3,
        "the trigger's backfill should seed the org's three items"
    );
}
