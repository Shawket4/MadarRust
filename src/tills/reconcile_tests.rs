//! B3 tests: close-till reconciliation (TILLS_CONTRACT.md §8 B3).
use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;
use crate::tills::reconcile::*;

// ── pure planning ────────────────────────────────────────────────

fn mt(method: &str, is_cash: bool, total: i64) -> MethodTotal {
    MethodTotal { method: method.into(), payment_method_id: None, is_cash, system_total: total, order_count: 1 }
}

fn input(method: &str, status: &str, amount: Option<i32>, note: Option<&str>) -> ReconciliationInput {
    ReconciliationInput { method: method.into(), status: status.into(), declared_amount: amount, note: note.map(Into::into) }
}

fn msg(e: AppError) -> String {
    e.to_string()
}

#[test]
fn reconciliation_status_rollup() {
    assert_eq!(rollup_status(["checked", "checked"]), "clean");
    assert_eq!(rollup_status(Vec::<&str>::new()), "clean");
    assert_eq!(rollup_status(["checked", "unreviewed"]), "unreviewed");
    assert_eq!(rollup_status(["unreviewed", "disagreed", "checked"]), "disagreed");
}

#[test]
fn plan_live_disagreed_needs_amount_and_note() {
    let totals = vec![mt("cash", true, 500), mt("card", false, 900)];
    let e = plan_lines(&totals, 500, 500, None, &[input("card", "disagreed", None, Some("x"))], false).unwrap_err();
    assert!(msg(e).contains(CODE_AMOUNT_REQUIRED));
    let e = plan_lines(&totals, 500, 500, None, &[input("card", "disagreed", Some(800), Some("  "))], false).unwrap_err();
    assert!(msg(e).contains(CODE_NOTE_REQUIRED));
    let e = plan_lines(&totals, 500, 500, None, &[input("card", "maybe", None, None)], false).unwrap_err();
    assert!(matches!(e, AppError::BadRequest(_)));
}

#[test]
fn plan_replay_never_fails() {
    let totals = vec![mt("cash", true, 500), mt("card", false, 900)];
    let lines = plan_lines(&totals, 450, 500, None, &[input("card", "disagreed", None, None)], true).unwrap();
    assert_eq!(lines[0].status, "disagreed"); // cash: declared != system, no note needed
    assert_eq!(lines[0].note, None);
    assert_eq!(lines[1].status, "disagreed");
    assert_eq!(lines[1].declared_amount, Some(900));
    assert_eq!(lines[1].note.as_deref(), Some(REPLAY_MISSING_NOTE));
    let lines = plan_lines(&totals, 500, 500, None, &[input("card", "maybe", None, None)], true).unwrap();
    assert_eq!(lines[1].status, "unreviewed");
}

#[test]
fn plan_unlisted_unreviewed_and_unused_input_stored_zero() {
    let totals = vec![mt("cash", true, 0), mt("card", false, 900), mt("wallet", false, 100)];
    let lines = plan_lines(
        &totals,
        0,
        0,
        Some("fine"),
        &[input("card", "checked", Some(1), None), input("instapay", "checked", None, None), input("cash", "disagreed", None, None)],
        false,
    )
    .unwrap();
    let by = |m: &str| lines.iter().find(|l| l.method == m).unwrap().clone();
    assert_eq!(by("cash").status, "checked"); // cash input ignored; drawer count decides
    assert_eq!(by("cash").note.as_deref(), Some("fine"));
    assert_eq!(by("card").status, "checked");
    assert_eq!(by("card").declared_amount, None);
    assert_eq!(by("wallet").status, "unreviewed");
    assert_eq!(by("instapay").system_total, 0);
    assert_eq!(lines.len(), 4);
}

// ── DB ───────────────────────────────────────────────────────────

pub(crate) struct Fx {
    pub org: Uuid,
    pub branch: Uuid,
    pub teller: Uuid,
    pub till: Uuid,
}

pub(crate) async fn fixture(pool: &PgPool) -> Fx {
    let org = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(org).bind(format!("o-{org}")).execute(pool).await.unwrap();
    let branch = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(branch).bind(org).bind(format!("b-{branch}")).execute(pool).await.unwrap();
    let teller = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES ($1, $2, 'T', $3, 'h', 'teller'::user_role)")
        .bind(teller).bind(org).bind(format!("{teller}@t.com")).execute(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, is_active) VALUES
         ($1, 'cash', '{}', 'c', 'i', true, true), ($1, 'card', '{}', 'c', 'i', false, true), ($1, 'wallet', '{}', 'c', 'i', false, true)",
    )
    .bind(org).execute(pool).await.unwrap();
    let till = Uuid::new_v4();
    sqlx::query("INSERT INTO tills (id, branch_id, teller_id, opening_cash) VALUES ($1, $2, $3, 100)")
        .bind(till).bind(branch).bind(teller).execute(pool).await.unwrap();
    Fx { org, branch, teller, till }
}

static ORDER_NO: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1);

/// An order with the given legs `(method, amount, is_cash)` and optional tip.
pub(crate) async fn order(pool: &PgPool, fx: &Fx, legs: &[(&str, i32, bool)], tip: Option<(&str, i32, bool)>) -> Uuid {
    let id = Uuid::new_v4();
    let total: i32 = legs.iter().map(|l| l.1).sum();
    let pm = if legs.len() == 1 { legs[0].0.to_string() } else { "mixed".into() };
    sqlx::query(
        "INSERT INTO orders (id, branch_id, teller_id, till_id, idempotency_key, subtotal, tax_amount, total_amount, status,
                             order_number, payment_method, order_ref, tip_amount, tip_payment_method, tip_is_cash)
         VALUES ($1, $2, $3, $4, gen_random_uuid(), $5, 0, $5, 'completed', $6, $7, gen_random_uuid()::text, $8, $9, $10)",
    )
    .bind(id).bind(fx.branch).bind(fx.teller).bind(fx.till).bind(total)
    .bind(ORDER_NO.fetch_add(1, std::sync::atomic::Ordering::Relaxed)).bind(pm)
    .bind(tip.map(|t| t.1).unwrap_or(0)).bind(tip.map(|t| t.0.to_string())).bind(tip.map(|t| t.2))
    .execute(pool).await.unwrap();
    for (m, a, c) in legs {
        sqlx::query("INSERT INTO order_payments (order_id, method, amount, is_cash, till_id) VALUES ($1, $2, $3, $4, $5)")
            .bind(id).bind(*m).bind(*a).bind(*c).bind(fx.till).execute(pool).await.unwrap();
    }
    id
}

async fn close_status(pool: &PgPool, till: Uuid) {
    sqlx::query("UPDATE tills SET status = 'closed', closed_at = now() WHERE id = $1")
        .bind(till).execute(pool).await.unwrap();
}

async fn totals(pool: &PgPool, till: Uuid, expected: i64) -> Vec<MethodTotal> {
    let mut c = pool.acquire().await.unwrap();
    system_totals_by_method(&mut c, till, expected).await.unwrap()
}

#[sqlx::test]
async fn system_totals_by_method_matches_report_payment_summary(pool: PgPool) {
    let fx = fixture(&pool).await;
    order(&pool, &fx, &[("card", 700, false)], None).await;
    order(&pool, &fx, &[("card", 300, false), ("wallet", 200, false), ("cash", 50, true)], None).await;
    let t = totals(&pool, fx.till, 150).await;
    // Same numbers as the report's payment_summary query (goods legs by method).
    let summary: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT op.method, SUM(op.amount)::bigint, COUNT(DISTINCT op.order_id)::bigint FROM order_payments op
         JOIN orders o ON o.id = op.order_id WHERE o.till_id = $1 AND o.status NOT IN ('voided','refunded')
         AND NOT op.is_cash GROUP BY op.method ORDER BY op.method",
    )
    .bind(fx.till).fetch_all(&pool).await.unwrap();
    let non_cash: Vec<(String, i64, i64)> =
        t.iter().filter(|m| !m.is_cash).map(|m| (m.method.clone(), m.system_total, m.order_count)).collect();
    assert_eq!(non_cash, summary);
    assert_eq!(non_cash[0], ("card".into(), 1000, 2));
    assert!(t.iter().find(|m| m.method == "card").unwrap().payment_method_id.is_some());
}

#[sqlx::test]
async fn cash_row_uses_expected_cash(pool: PgPool) {
    let fx = fixture(&pool).await;
    // No sales at all: cash row still present, first.
    let t = totals(&pool, fx.till, 100).await;
    assert_eq!(t.len(), 1);
    assert!(t[0].is_cash);
    assert_eq!((t[0].method.as_str(), t[0].system_total), ("cash", 100));
    order(&pool, &fx, &[("cash", 400, true)], Some(("card", 30, false))).await;
    let t = totals(&pool, fx.till, 12345).await;
    assert_eq!(t[0].system_total, 12345);
    assert_eq!(t[0].order_count, 1);
    // Tips on a card count toward the card terminal's total.
    assert_eq!(t.iter().find(|m| m.method == "card").unwrap().system_total, 30);
}

#[sqlx::test]
async fn refunds_issued_reduce_method_total(pool: PgPool) {
    let fx = fixture(&pool).await;
    let o = order(&pool, &fx, &[("card", 500, false)], None).await;
    sqlx::query(
        "INSERT INTO order_refunds (org_id, branch_id, order_id, till_id, amount, method, is_cash, reason, issued_by)
         VALUES ($1, $2, $3, $4, 200, 'card', false, 'goodwill', $5)",
    )
    .bind(fx.org).bind(fx.branch).bind(o).bind(fx.till).bind(fx.teller).execute(&pool).await.unwrap();
    let t = totals(&pool, fx.till, 0).await;
    assert_eq!(t.iter().find(|m| m.method == "card").unwrap().system_total, 300);
}

#[sqlx::test]
async fn close_writes_checked_disagreed_unreviewed(pool: PgPool) {
    let fx = fixture(&pool).await;
    order(&pool, &fx, &[("card", 700, false)], None).await;
    order(&pool, &fx, &[("wallet", 200, false)], None).await;
    order(&pool, &fx, &[("cash", 50, true)], None).await;
    close_status(&pool, fx.till).await;
    let mut tx = pool.begin().await.unwrap();

    // Live validation fails BEFORE anything is written.
    let bad = write_close_reconciliation(&mut tx, fx.till, fx.teller, 150, 150, None,
        &[input("card", "disagreed", Some(650), None)], false).await.unwrap_err();
    assert!(bad.to_string().contains(CODE_NOTE_REQUIRED));
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM till_reconciliations WHERE till_id = $1")
        .bind(fx.till).fetch_one(&mut *tx).await.unwrap();
    assert_eq!(n, 0);

    let (lines, status) = write_close_reconciliation(&mut tx, fx.till, fx.teller, 140, 150, Some("short 10"),
        &[input("card", "disagreed", Some(650), Some("one slip missing"))], false).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(status, "disagreed");
    let by = |m: &str| lines.iter().find(|l| l.method == m).unwrap().clone();
    assert_eq!(lines[0].method, "cash");
    assert_eq!((by("cash").status.as_str(), by("cash").declared_amount, by("cash").system_total), ("disagreed", Some(140), 150));
    assert_eq!((by("card").status.as_str(), by("card").declared_amount), ("disagreed", Some(650)));
    assert_eq!(by("wallet").status, "unreviewed");
    assert!(lines.iter().all(|l| !l.changed_after_close && l.reconciled_by == Some(fx.teller)));

    // Flags queryable for the dashboard.
    let (rs, dc): (Option<String>, i64) = sqlx::query_as(
        "SELECT t.reconciliation_status, (SELECT COUNT(*) FROM till_reconciliations r WHERE r.till_id = t.id AND r.status = 'disagreed')
         FROM tills t WHERE t.id = $1 AND (t.opened_while_another_open OR t.reconciliation_status = 'disagreed')",
    )
    .bind(fx.till).fetch_one(&pool).await.unwrap();
    assert_eq!((rs.as_deref(), dc), (Some("disagreed"), 2));

    // Idempotent: a replayed close does not rewrite.
    let mut c = pool.acquire().await.unwrap();
    let (again, st2) = write_close_reconciliation(&mut c, fx.till, fx.teller, 999, 150, None,
        &[input("card", "checked", None, None)], true).await.unwrap();
    assert_eq!(st2, "disagreed");
    assert_eq!(again, lines);
    assert_eq!(lines_for_till(&pool, fx.till).await.unwrap(), lines);
}

#[sqlx::test]
async fn close_all_checked_is_clean_and_replay_stores_no_note(pool: PgPool) {
    let fx = fixture(&pool).await;
    order(&pool, &fx, &[("card", 700, false)], None).await;
    close_status(&pool, fx.till).await;
    let mut c = pool.acquire().await.unwrap();
    let (_, s) = write_close_reconciliation(&mut c, fx.till, fx.teller, 100, 100, None,
        &[input("card", "checked", None, None)], false).await.unwrap();
    assert_eq!(s, "clean");

    let fx2 = fixture(&pool).await;
    order(&pool, &fx2, &[("card", 700, false)], None).await;
    close_status(&pool, fx2.till).await;
    let (lines, s) = write_close_reconciliation(&mut c, fx2.till, fx2.teller, 100, 100, None,
        &[input("card", "disagreed", Some(600), None)], true).await.unwrap();
    assert_eq!(s, "disagreed");
    assert_eq!(lines[1].note.as_deref(), Some(REPLAY_MISSING_NOTE));
}

#[sqlx::test]
async fn late_replay_updates_current_total_only(pool: PgPool) {
    let fx = fixture(&pool).await;
    order(&pool, &fx, &[("card", 700, false)], None).await;
    close_status(&pool, fx.till).await;
    let mut c = pool.acquire().await.unwrap();
    let (before, s) = write_close_reconciliation(&mut c, fx.till, fx.teller, 100, 100, None,
        &[input("card", "checked", None, None)], false).await.unwrap();
    assert_eq!(s, "clean");

    order(&pool, &fx, &[("card", 50, false)], None).await;
    order(&pool, &fx, &[("wallet", 20, false)], None).await;
    recompute_after_late_replay(&mut c, fx.till, 130).await.unwrap();
    recompute_after_late_replay(&mut c, fx.till, 130).await.unwrap(); // replay-safe
    let after = lines_for_till(&pool, fx.till).await.unwrap();
    let by = |m: &str| after.iter().find(|l| l.method == m).unwrap().clone();
    assert_eq!((by("card").system_total, by("card").current_system_total, by("card").status.as_str()), (700, 750, "checked"));
    assert!(by("card").changed_after_close);
    assert_eq!((by("cash").system_total, by("cash").current_system_total), (100, 130));
    assert_eq!((by("wallet").system_total, by("wallet").current_system_total, by("wallet").status.as_str()), (0, 20, "unreviewed"));
    assert_eq!(before.len() + 1, after.len());
    let rs: Option<String> = sqlx::query_scalar("SELECT reconciliation_status FROM tills WHERE id = $1")
        .bind(fx.till).fetch_one(&pool).await.unwrap();
    assert_eq!(rs.as_deref(), Some("clean"));

    // A pre-reconciliation till (no lines) is left alone.
    let fx2 = fixture(&pool).await;
    close_status(&pool, fx2.till).await;
    recompute_after_late_replay(&mut c, fx2.till, 0).await.unwrap();
    assert!(lines_for_till(&pool, fx2.till).await.unwrap().is_empty());
}
