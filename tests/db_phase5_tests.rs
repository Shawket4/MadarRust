//! DB architecture Phase 5 (DB_ARCHITECTURE_AUDIT_2026-10-16 §8): relational
//! ticket lines (5.1), unified ref counters (5.3), the loyalty membership
//! rename (5.5), the money-width expand (5.6), ledger FKs that restrict and the
//! till soft delete (5.7).

use sqlx::PgPool;
use uuid::Uuid;

struct Shop {
    org: Uuid,
    branch: Uuid,
    teller: Uuid,
    till: Uuid,
    ticket: Uuid,
    round: Uuid,
}

async fn shop(pool: &PgPool) -> Shop {
    let s = Shop {
        org: Uuid::new_v4(),
        branch: Uuid::new_v4(),
        teller: Uuid::new_v4(),
        till: Uuid::new_v4(),
        ticket: Uuid::new_v4(),
        round: Uuid::new_v4(),
    };
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations (id, name, slug) VALUES ('{org}', 'P5', 'p5-{org}');
         INSERT INTO users (id, name, role, org_id, pin_hash) VALUES ('{teller}', 'T', 'teller', '{org}', 'x');
         INSERT INTO branches (id, org_id, name, code) VALUES ('{branch}', '{org}', 'B', 'P5');
         INSERT INTO tills (id, branch_id, teller_id, opening_cash) VALUES ('{till}', '{branch}', '{teller}', 50000);
         INSERT INTO open_tickets (id, org_id, branch_id, opened_by) VALUES ('{ticket}', '{org}', '{branch}', '{teller}');
         INSERT INTO open_ticket_rounds (id, open_ticket_id, round_number, fired_by)
              VALUES ('{round}', '{ticket}', 1, '{teller}');",
        org = s.org,
        teller = s.teller,
        branch = s.branch,
        till = s.till,
        ticket = s.ticket,
        round = s.round,
    ))
    .execute(pool)
    .await
    .unwrap();
    s
}

/// 5.1: a fired line's own fields land in columns and child rows, from the
/// jsonb the code still writes.
#[sqlx::test]
async fn a_ticket_line_is_mirrored_into_columns_and_child_rows(pool: PgPool) {
    let s = shop(&pool).await;
    let (addon, optional) = (Uuid::new_v4(), Uuid::new_v4());
    let line = serde_json::json!({
        "qty": 2, "name": "Latte", "unit_price": 4500, "line_total": 10_000,
        "size_label": "Large", "modifiers": ["Oat milk"],
        "input": {"menu_item_id": null, "quantity": 2, "unit_price": 4500, "size_label": "Large",
                  "notes": null, "bundle_id": null, "bundle_components": [],
                  "addons": [{"addon_item_id": addon, "quantity": 1, "unit_price": 500}],
                  "optional_field_ids": [optional]}
    });
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO open_ticket_items (open_ticket_id, round_id, line, line_total)
         VALUES ($1, $2, $3, 10000) RETURNING id",
    )
    .bind(s.ticket)
    .bind(s.round)
    .bind(&line)
    .fetch_one(&pool)
    .await
    .unwrap();
    let (q, name, price, size, mods): (Option<i32>, Option<String>, Option<i32>, Option<String>, Option<Vec<String>>) =
        sqlx::query_as(
            "SELECT quantity, item_name, unit_price, size_label, modifier_names FROM open_ticket_items WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((q, name.as_deref(), price, size.as_deref()), (Some(2), Some("Latte"), Some(4500), Some("Large")));
    assert_eq!(mods, Some(vec!["Oat milk".to_string()]));
    let addons: Vec<(Uuid, i32, i32)> = sqlx::query_as(
        "SELECT addon_item_id, quantity, unit_price FROM open_ticket_item_addons WHERE item_id = $1",
    )
    .bind(id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(addons, vec![(addon, 1, 500)]);
    let opts: Vec<Uuid> =
        sqlx::query_scalar("SELECT option_id FROM open_ticket_item_optionals WHERE item_id = $1")
            .bind(id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(opts, vec![optional]);
}

/// 5.3: the next number is shared with the old counter table (an older binary
/// in a rolling deploy bumps only that one) and mirrored into ref_counters.
#[sqlx::test]
async fn ref_numbers_never_repeat_across_old_and_new_writers(pool: PgPool) {
    let s = shop(&pool).await;
    let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 17).unwrap();
    let mut conn = pool.acquire().await.unwrap();
    use madar_rust::refs::{Scope, next};
    assert_eq!(next(&mut conn, Scope::Ticket, s.branch, day).await.unwrap(), 1);
    assert_eq!(next(&mut conn, Scope::Ticket, s.branch, day).await.unwrap(), 2);
    // An older binary takes 3 from the old table alone.
    let old: i32 = sqlx::query_scalar(
        "UPDATE ticket_ref_counters SET last_seq = last_seq + 1
          WHERE branch_id = $1 AND business_date = $2 RETURNING last_seq",
    )
    .bind(s.branch)
    .bind(day)
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(old, 3);
    assert_eq!(next(&mut conn, Scope::Ticket, s.branch, day).await.unwrap(), 4);
    let mirrored: i32 = sqlx::query_scalar(
        "SELECT last_seq FROM ref_counters WHERE scope = 'ticket' AND branch_id = $1 AND business_date = $2",
    )
    .bind(s.branch)
    .bind(day)
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(mirrored, 4);
    // Scopes are separate sequences.
    assert_eq!(next(&mut conn, Scope::Delivery, s.branch, day).await.unwrap(), 1);
    assert_eq!(next(&mut conn, Scope::Order, s.branch, day).await.unwrap(), 1);
}

/// 5.5: the table is loyalty_memberships; the old name is a writable view.
#[sqlx::test]
async fn the_old_membership_name_still_reads_and_writes(pool: PgPool) {
    let s = shop(&pool).await;
    let customer = Uuid::new_v4();
    sqlx::query("INSERT INTO customers (id, org_id, name) VALUES ($1, $2, 'Mona')")
        .bind(customer)
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    // An older binary inserts through the old name; defaults still apply.
    sqlx::query("INSERT INTO loyalty_customers (id, org_id, member_token) VALUES ($1, $2, 'tok-p5')")
        .bind(customer)
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let (points, wide): (i32, Option<i64>) = sqlx::query_as(
        "SELECT lifetime_points, lifetime_points_wide FROM loyalty_memberships WHERE id = $1",
    )
    .bind(customer)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((points, wide), (0, Some(0)));
    sqlx::query("UPDATE loyalty_customers SET lifetime_points = 2147483647 WHERE id = $1")
        .bind(customer)
        .execute(&pool)
        .await
        .unwrap();
    let wide: Option<i64> =
        sqlx::query_scalar("SELECT lifetime_points_wide FROM loyalty_memberships WHERE id = $1")
            .bind(customer)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(wide, Some(2_147_483_647), "the wide twin follows every write");
}

/// 5.6: every widened till column has its bigint twin, kept equal.
#[sqlx::test]
async fn till_money_has_its_wide_twin(pool: PgPool) {
    let s = shop(&pool).await;
    sqlx::query("UPDATE tills SET closing_cash_declared = 60000, closing_cash_system = 59000 WHERE id = $1")
        .bind(s.till)
        .execute(&pool)
        .await
        .unwrap();
    let row: (Option<i64>, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT opening_cash_wide, closing_cash_declared_wide, closing_cash_system_wide FROM tills WHERE id = $1",
    )
    .bind(s.till)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row, (Some(50_000), Some(60_000), Some(59_000)));
}

/// 5.7: deleting a branch or an org can no longer take its ledger with it.
#[sqlx::test]
async fn a_branch_with_a_ledger_cannot_be_hard_deleted(pool: PgPool) {
    let s = shop(&pool).await;
    // The open ticket used to cascade away with the branch.
    let err = sqlx::query("DELETE FROM branches WHERE id = $1")
        .bind(s.branch)
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(err.as_database_error().and_then(|e| e.code()).as_deref(), Some("23503"));
    let still: i64 = sqlx::query_scalar("SELECT count(*) FROM open_tickets WHERE id = $1")
        .bind(s.ticket)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(still, 1);
}

/// 5.7: the demo sweeper still purges an expired DEMO org, ledgers included.
#[sqlx::test]
async fn the_demo_sweeper_still_purges_an_expired_demo_org(pool: PgPool) {
    let s = shop(&pool).await;
    sqlx::query("UPDATE organizations SET is_demo = true, demo_expires_at = now() - interval '1 day' WHERE id = $1")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    madar_rust::demo::sweeper::purge_org(&pool, s.org).await.unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM organizations WHERE id = $1")
        .bind(s.org)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}
