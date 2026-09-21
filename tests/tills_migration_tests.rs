//! Tills rework — schema/migration tests (TILLS_CONTRACT.md §8 B1, §10.6, §11.8).
//!
//! Every test builds its own database: migrations up to the last pre-rework
//! version, a fixture seeded on the OLD schema (shifts + drawer entities), then
//! the rework migrations. Runtime `sqlx::query` only (no compile-time macros), so
//! this file does not couple the build to any particular schema.
//! Registered by B2 in `src/lib.rs` as `#[cfg(test)] mod tills_migration_tests;`.

use std::borrow::Cow;

use sqlx::migrate::Migrator;
use sqlx::{PgPool, Row};
use uuid::Uuid;

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Last migration before the rework.
const PRE_VERSION: i64 = 20260913101500;
// 20260915090000 extends the changefeed (feed gaps for offline plan B); the
// rework's down script removes it with the feed it depends on.
const REWORK_VERSIONS: [i64; 9] = [
    20260914090000,
    20260914090100,
    20260914090200,
    20260914090300,
    20260914090400,
    20260914090500,
    20260914090600,
    20260914091000,
    20260915090000,
];

const DOWN_SQL: &str = include_str!("../scripts/tills_rework/down.sql");

// Fixture ids (stable, readable).
const ORG: &str = "00000000-0000-4000-8000-000000000001";
const ORG2: &str = "00000000-0000-4000-8000-000000000002";
const B1: &str = "00000000-0000-4000-8000-0000000000b1";
const B2: &str = "00000000-0000-4000-8000-0000000000b2";
const B3: &str = "00000000-0000-4000-8000-0000000000b3"; // other org
const T1: &str = "00000000-0000-4000-8000-0000000000a1";
const T2: &str = "00000000-0000-4000-8000-0000000000a2";
const U3: &str = "00000000-0000-4000-8000-0000000000a3"; // other org
const E1: &str = "00000000-0000-4000-8000-0000000000e1";
const S1: &str = "00000000-0000-4000-8000-00000000c001";
const S2: &str = "00000000-0000-4000-8000-00000000c002";
const S3: &str = "00000000-0000-4000-8000-00000000c003";
const O1: &str = "00000000-0000-4000-8000-00000000d001";
const O3: &str = "00000000-0000-4000-8000-00000000d003";
const OC1: &str = "00000000-0000-4000-8000-00000000f001";
const OC2: &str = "00000000-0000-4000-8000-00000000f002";
const OC3: &str = "00000000-0000-4000-8000-00000000f003";
const TK_OPEN: &str = "00000000-0000-4000-8000-00000000ab02";
const MI: &str = "00000000-0000-4000-8000-00000000ee01";
const SZ: &str = "00000000-0000-4000-8000-00000000ee02";
const PM_CASH: &str = "00000000-0000-4000-8000-00000000bb01";
const PM_OTHER_ORG: &str = "00000000-0000-4000-8000-00000000bb09";

const FIXTURE: &str = r#"
INSERT INTO organizations (id, name) VALUES
  ('00000000-0000-4000-8000-000000000001', 'Fixture Org'),
  ('00000000-0000-4000-8000-000000000002', 'Other Org');
INSERT INTO branches (id, org_id, name, code) VALUES
  ('00000000-0000-4000-8000-0000000000b1', '00000000-0000-4000-8000-000000000001', 'Branch 1', 'FXB1'),
  ('00000000-0000-4000-8000-0000000000b2', '00000000-0000-4000-8000-000000000001', 'Branch 2', 'FXB2'),
  ('00000000-0000-4000-8000-0000000000b3', '00000000-0000-4000-8000-000000000002', 'Other',    'FXB3');
INSERT INTO users (id, org_id, name, role, pin_hash) VALUES
  ('00000000-0000-4000-8000-0000000000a1', '00000000-0000-4000-8000-000000000001', 'Teller One', 'teller', 'x'),
  ('00000000-0000-4000-8000-0000000000a2', '00000000-0000-4000-8000-000000000001', 'Teller Two', 'teller', 'x'),
  ('00000000-0000-4000-8000-0000000000a3', '00000000-0000-4000-8000-000000000002', 'Other Teller', 'teller', 'x');
INSERT INTO org_payment_methods (id, org_id, name, color, icon, is_cash) VALUES
  ('00000000-0000-4000-8000-00000000bb01', '00000000-0000-4000-8000-000000000001', 'Cash', '#000', 'cash', true),
  ('00000000-0000-4000-8000-00000000bb09', '00000000-0000-4000-8000-000000000002', 'Cash', '#000', 'cash', true);

-- drawer entities: default per branch (B1 with a float), plus a deleted one
INSERT INTO tills (id, org_id, branch_id, name, is_default, standard_float, deleted_at) VALUES
  ('00000000-0000-4000-8000-0000000000e1', '00000000-0000-4000-8000-000000000001', '00000000-0000-4000-8000-0000000000b1', 'Till 1', true, 50000, NULL),
  ('00000000-0000-4000-8000-0000000000e2', '00000000-0000-4000-8000-000000000001', '00000000-0000-4000-8000-0000000000b2', 'Till 1', true, NULL, NULL),
  ('00000000-0000-4000-8000-0000000000e3', '00000000-0000-4000-8000-000000000001', '00000000-0000-4000-8000-0000000000b1', 'Old',    false, 100, now());

INSERT INTO shifts (id, branch_id, teller_id, till_id, status, opening_cash, closing_cash_declared, closing_cash_system, opened_at, closed_at) VALUES
  ('00000000-0000-4000-8000-00000000c001', '00000000-0000-4000-8000-0000000000b1', '00000000-0000-4000-8000-0000000000a1', '00000000-0000-4000-8000-0000000000e1', 'closed', 1000, 3000, 3200, '2026-09-01 09:00+00', '2026-09-01 12:00+00'),
  ('00000000-0000-4000-8000-00000000c002', '00000000-0000-4000-8000-0000000000b1', '00000000-0000-4000-8000-0000000000a1', '00000000-0000-4000-8000-0000000000e1', 'open',   500,  NULL, NULL, '2026-09-01 13:00+00', NULL),
  ('00000000-0000-4000-8000-00000000c003', '00000000-0000-4000-8000-0000000000b2', '00000000-0000-4000-8000-0000000000a2', '00000000-0000-4000-8000-0000000000e2', 'open',   0,    NULL, NULL, '2026-09-01 08:00+00', NULL);

INSERT INTO orders (id, branch_id, shift_id, teller_id, order_number, payment_method, order_ref, subtotal, total_amount) VALUES
  ('00000000-0000-4000-8000-00000000d001', '00000000-0000-4000-8000-0000000000b1', '00000000-0000-4000-8000-00000000c001', '00000000-0000-4000-8000-0000000000a1', 1, 'Cash', 'FX-1', 1000, 1000),
  ('00000000-0000-4000-8000-00000000d002', '00000000-0000-4000-8000-0000000000b1', '00000000-0000-4000-8000-00000000c001', '00000000-0000-4000-8000-0000000000a1', 2, 'Card', 'FX-2', 2500, 2500),
  ('00000000-0000-4000-8000-00000000d003', '00000000-0000-4000-8000-0000000000b1', '00000000-0000-4000-8000-00000000c002', '00000000-0000-4000-8000-0000000000a1', 1, 'Cash', 'FX-3', 700, 700);
INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES
  ('00000000-0000-4000-8000-00000000d001', 'Cash', 1000, true),
  ('00000000-0000-4000-8000-00000000d002', 'Card', 1500, false),
  ('00000000-0000-4000-8000-00000000d002', 'Cash', 1000, true),
  ('00000000-0000-4000-8000-00000000d003', 'Cash', 700, true);
INSERT INTO shift_cash_movements (shift_id, amount, note, moved_by, kind) VALUES
  ('00000000-0000-4000-8000-00000000c001', 500, 'float top-up', '00000000-0000-4000-8000-0000000000a1', 'pay_in'),
  ('00000000-0000-4000-8000-00000000c002', -300, 'to safe', '00000000-0000-4000-8000-0000000000a1', 'safe_drop');
INSERT INTO order_refunds (org_id, branch_id, order_id, shift_id, amount, method, is_cash, reason, issued_by) VALUES
  ('00000000-0000-4000-8000-000000000001', '00000000-0000-4000-8000-0000000000b1', '00000000-0000-4000-8000-00000000d002', '00000000-0000-4000-8000-00000000c002', 200, 'Card', false, 'goodwill', '00000000-0000-4000-8000-0000000000a1');

INSERT INTO open_tickets (id, org_id, branch_id, opened_by, status, settled_at, settled_by, settled_shift_id) VALUES
  ('00000000-0000-4000-8000-00000000ab01', '00000000-0000-4000-8000-000000000001', '00000000-0000-4000-8000-0000000000b1', '00000000-0000-4000-8000-0000000000a1', 'settled', '2026-09-01 11:30+00', '00000000-0000-4000-8000-0000000000a1', '00000000-0000-4000-8000-00000000c001');
INSERT INTO open_tickets (id, org_id, branch_id, opened_by) VALUES
  ('00000000-0000-4000-8000-00000000ab02', '00000000-0000-4000-8000-000000000001', '00000000-0000-4000-8000-0000000000b1', '00000000-0000-4000-8000-0000000000a1');

INSERT INTO branch_tables (id, org_id, branch_id, label) VALUES
  ('00000000-0000-4000-8000-00000000fa01', '00000000-0000-4000-8000-000000000001', '00000000-0000-4000-8000-0000000000b1', 'T1'),
  ('00000000-0000-4000-8000-00000000fa02', '00000000-0000-4000-8000-000000000001', '00000000-0000-4000-8000-0000000000b1', 'T2');
INSERT INTO table_occupancies (id, org_id, branch_id, table_id, held_by, started_at, started_by, started_till_id, ended_at, ended_by, ended_till_id, end_reason) VALUES
  ('00000000-0000-4000-8000-00000000f001', '00000000-0000-4000-8000-000000000001', '00000000-0000-4000-8000-0000000000b1', '00000000-0000-4000-8000-00000000fa01', 'party',
   '2026-09-01 10:00+00', '00000000-0000-4000-8000-0000000000a1', '00000000-0000-4000-8000-0000000000e1', '2026-09-01 11:00+00', '00000000-0000-4000-8000-0000000000a1', '00000000-0000-4000-8000-0000000000e1', 'released'),
  -- T2 never had a session at B1: remap must be NULL
  ('00000000-0000-4000-8000-00000000f002', '00000000-0000-4000-8000-000000000001', '00000000-0000-4000-8000-0000000000b1', '00000000-0000-4000-8000-00000000fa01', 'party',
   '2026-09-01 12:30+00', '00000000-0000-4000-8000-0000000000a2', '00000000-0000-4000-8000-0000000000e1', NULL, NULL, NULL, NULL),
  ('00000000-0000-4000-8000-00000000f003', '00000000-0000-4000-8000-000000000001', '00000000-0000-4000-8000-0000000000b1', '00000000-0000-4000-8000-00000000fa02', 'party',
   '2026-09-01 13:30+00', '00000000-0000-4000-8000-0000000000a1', NULL, NULL, NULL, NULL, NULL);

INSERT INTO permissions (user_id, resource, action) VALUES
  ('00000000-0000-4000-8000-0000000000a1', 'shifts', 'read'),
  ('00000000-0000-4000-8000-0000000000a1', 'shift_counts', 'read');

INSERT INTO menu_items (id, org_id, name) VALUES
  ('00000000-0000-4000-8000-00000000ee01', '00000000-0000-4000-8000-000000000001', 'Latte');
INSERT INTO menu_item_sizes (id, menu_item_id, label, price) VALUES
  ('00000000-0000-4000-8000-00000000ee02', '00000000-0000-4000-8000-00000000ee01', 'M', 1000);
"#;

// ── helpers ────────────────────────────────────────────────────────────────────

fn subset(pred: impl Fn(i64) -> bool) -> Migrator {
    let migrations: Vec<_> = MIGRATOR
        .iter()
        .filter(|m| pred(m.version))
        .cloned()
        .collect();
    Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: true,
        locking: true,
        no_tx: false,
    }
}

/// A brand-new database cloned from `template0`: the test cluster's `template1`
/// may be pre-migrated, and these tests must seed the OLD schema. The returned
/// guard drops the database when the test ends (pass or panic).
async fn fresh(pool: &PgPool) -> (PgPool, FreshDb) {
    let name = format!("_sqlx_test_t0_{}", Uuid::new_v4().simple());
    sqlx::raw_sql(&format!("CREATE DATABASE \"{name}\" TEMPLATE template0"))
        .execute(pool)
        .await
        .expect("create fresh database");
    let base = pool.connect_options().as_ref().clone();
    let fresh = sqlx::pool::PoolOptions::new()
        .max_connections(4)
        .connect_with(base.clone().database(&name))
        .await
        .expect("connect fresh database");
    (fresh, FreshDb { name, base })
}

struct FreshDb {
    name: String,
    base: sqlx::postgres::PgConnectOptions,
}

impl Drop for FreshDb {
    fn drop(&mut self) {
        let name = std::mem::take(&mut self.name);
        let opts = self.base.clone().database("postgres");
        // Drop runs outside any async context guarantee: use a private runtime.
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                use sqlx::Connection;
                if let Ok(mut conn) = sqlx::PgConnection::connect_with(&opts).await {
                    let _ =
                        sqlx::raw_sql(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
                            .execute(&mut conn)
                            .await;
                }
            });
        })
        .join();
    }
}

async fn migrate_pre(pool: &PgPool) {
    subset(|v| v <= PRE_VERSION)
        .run(pool)
        .await
        .expect("pre-rework migrations");
}

async fn migrate_rest(pool: &PgPool) {
    subset(|_| true).run(pool).await.expect("rework migrations");
}

/// Old schema + fixture + rework migrations.
async fn setup(pool: &PgPool) {
    migrate_pre(pool).await;
    sqlx::raw_sql(FIXTURE).execute(pool).await.expect("fixture");
    migrate_rest(pool).await;
}

async fn i64_of(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

async fn uuid_opt(pool: &PgPool, sql: &str) -> Option<Uuid> {
    sqlx::query_scalar::<_, Option<Uuid>>(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn u(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap()
}

const PER_TILL_OLD: &str = r#"
SELECT s.id::text || '|' ||
  (SELECT count(*) FROM orders o WHERE o.shift_id = s.id) || ',' ||
  (SELECT coalesce(sum(total_amount),0) FROM orders o WHERE o.shift_id = s.id) || ',' ||
  (SELECT count(*) FROM order_payments p JOIN orders o ON o.id = p.order_id WHERE o.shift_id = s.id) || ',' ||
  (SELECT coalesce(sum(p.amount),0) FROM order_payments p JOIN orders o ON o.id = p.order_id WHERE o.shift_id = s.id) || ',' ||
  (SELECT coalesce(sum(amount),0) FROM shift_cash_movements m WHERE m.shift_id = s.id) || ',' ||
  (SELECT coalesce(sum(amount),0) FROM order_refunds r WHERE r.shift_id = s.id) || ',' ||
  (SELECT count(*) FROM open_tickets t WHERE t.settled_shift_id = s.id) || ',' ||
  s.status || ',' || coalesce(s.closing_cash_system::text,'-')
FROM shifts s ORDER BY s.id"#;

const PER_TILL_NEW: &str = r#"
SELECT s.id::text || '|' ||
  (SELECT count(*) FROM orders o WHERE o.till_id = s.id) || ',' ||
  (SELECT coalesce(sum(total_amount),0) FROM orders o WHERE o.till_id = s.id) || ',' ||
  (SELECT count(*) FROM order_payments p WHERE p.till_id = s.id) || ',' ||
  (SELECT coalesce(sum(p.amount),0) FROM order_payments p WHERE p.till_id = s.id) || ',' ||
  (SELECT coalesce(sum(amount),0) FROM till_cash_movements m WHERE m.till_id = s.id) || ',' ||
  (SELECT coalesce(sum(amount),0) FROM order_refunds r WHERE r.till_id = s.id) || ',' ||
  (SELECT count(*) FROM open_tickets t WHERE t.settled_till_id = s.id) || ',' ||
  s.status || ',' || coalesce(s.closing_cash_system::text,'-')
FROM tills s ORDER BY s.id"#;

async fn lines(pool: &PgPool, sql: &str) -> Vec<String> {
    sqlx::query_scalar::<_, String>(sql)
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn max_seq(pool: &PgPool) -> i64 {
    i64_of(pool, "SELECT coalesce(max(seq),0) FROM sync_changes").await
}

async fn feed_op(pool: &PgPool, branch: &str, ty: &str, id: &str) -> Option<(String, i64)> {
    sqlx::query(
        "SELECT op, seq FROM sync_changes WHERE branch_id = $1 AND type = $2 AND entity_id = $3",
    )
    .bind(u(branch))
    .bind(ty)
    .bind(u(id))
    .fetch_optional(pool)
    .await
    .unwrap()
    .map(|r| (r.get::<String, _>(0), r.get::<i64, _>(1)))
}

// ── §8 B1 schema / migration ─────────────────────────────────────────────────

#[sqlx::test(migrations = false)]
async fn migration_preserves_counts_and_sums_per_till(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    migrate_pre(&pool).await;
    sqlx::raw_sql(FIXTURE).execute(&pool).await.unwrap();
    let before = lines(&pool, PER_TILL_OLD).await;
    migrate_rest(&pool).await;
    let after = lines(&pool, PER_TILL_NEW).await;
    assert_eq!(before.len(), 3);
    assert_eq!(before, after);
    assert_eq!(
        i64_of(&pool, "SELECT count(*) FROM tills WHERE status = 'open'").await,
        2
    );
    assert_eq!(
        i64_of(
            &pool,
            "SELECT count(*) FROM tills WHERE verification = 'legacy'"
        )
        .await,
        3
    );
}

#[sqlx::test(migrations = false)]
async fn migration_archives_till_entities_and_bindings(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    assert_eq!(
        i64_of(&pool, "SELECT count(*) FROM archive.till_entities").await,
        3
    );
    assert_eq!(
        i64_of(&pool, "SELECT count(*) FROM archive.shift_till_bindings").await,
        3
    );
    let bound = uuid_opt(
        &pool,
        &format!("SELECT till_entity_id FROM archive.shift_till_bindings WHERE shift_id = '{S3}'"),
    )
    .await;
    assert_eq!(bound, Some(u("00000000-0000-4000-8000-0000000000e2")));
    // `tills` is now the session table; the entity's columns are gone.
    assert_eq!(i64_of(&pool, "SELECT count(*) FROM information_schema.columns WHERE table_schema='public' AND table_name='tills' AND column_name IN ('teller_id','name','is_default')").await, 1);
    // standard_float moved to the branch (default, live entities only).
    assert_eq!(
        i64_of(
            &pool,
            &format!("SELECT coalesce(standard_float,-1)::bigint FROM branches WHERE id = '{B1}'")
        )
        .await,
        50000
    );
    assert_eq!(
        i64_of(
            &pool,
            &format!("SELECT coalesce(standard_float,-1)::bigint FROM branches WHERE id = '{B2}'")
        )
        .await,
        -1
    );
}

#[sqlx::test(migrations = false)]
async fn migration_remaps_occupancy_till_refs_to_covering_session(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    let row =
        sqlx::query("SELECT started_till_id, ended_till_id FROM table_occupancies WHERE id = $1")
            .bind(u(OC1))
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.get::<Option<Uuid>, _>(0), Some(u(S1)));
    assert_eq!(row.get::<Option<Uuid>, _>(1), Some(u(S1)));
    let arch = uuid_opt(&pool, &format!("SELECT started_till_entity_id FROM archive.occupancy_till_refs WHERE occupancy_id = '{OC1}'")).await;
    assert_eq!(arch, Some(u(E1)));
}

#[sqlx::test(migrations = false)]
async fn migration_remap_null_when_no_covering_session(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    assert_eq!(
        uuid_opt(
            &pool,
            &format!("SELECT started_till_id FROM table_occupancies WHERE id = '{OC2}'")
        )
        .await,
        None
    );
    assert_eq!(uuid_opt(&pool, &format!("SELECT started_till_entity_id FROM archive.occupancy_till_refs WHERE occupancy_id = '{OC2}'")).await, Some(u(E1)));
    // A NULL ref is not invented into an attribution.
    assert_eq!(
        uuid_opt(
            &pool,
            &format!("SELECT started_till_id FROM table_occupancies WHERE id = '{OC3}'")
        )
        .await,
        None
    );
    assert_eq!(
        i64_of(
            &pool,
            &format!(
                "SELECT count(*) FROM archive.occupancy_till_refs WHERE occupancy_id = '{OC3}'"
            )
        )
        .await,
        0
    );
}

#[sqlx::test(migrations = false)]
async fn migration_backfills_order_payments_till_id(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    assert_eq!(
        i64_of(&pool, "SELECT count(*) FROM order_payments").await,
        4
    );
    assert_eq!(i64_of(&pool, "SELECT count(*) FROM order_payments p JOIN orders o ON o.id = p.order_id WHERE p.till_id IS DISTINCT FROM o.till_id").await, 0);
    assert_eq!(
        i64_of(
            &pool,
            &format!("SELECT count(*) FROM order_payments WHERE till_id = '{S1}'")
        )
        .await,
        3
    );
}

#[sqlx::test(migrations = false)]
async fn order_payments_fill_till_trigger_sets_missing_till(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    let till: Uuid = sqlx::query_scalar("INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ($1, 'Cash', 5, true) RETURNING till_id")
        .bind(u(O3))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(till, u(S2));
}

#[sqlx::test(migrations = false)]
async fn refund_trigger_references_tills_and_rejects_cross_branch(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    let body: String = sqlx::query_scalar(
        "SELECT prosrc FROM pg_proc WHERE proname = 'order_refunds_before_insert'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(body.contains("FROM tills WHERE id = NEW.till_id"));
    // O1 was sold at B1; S3 is a till at B2.
    let err = sqlx::query("INSERT INTO order_refunds (order_id, till_id, amount, method, is_cash, reason, issued_by) VALUES ($1, $2, 100, 'Cash', true, 'goodwill', $3)")
        .bind(u(O1))
        .bind(u(S3))
        .bind(u(T1))
        .execute(&pool)
        .await
        .expect_err("cross-branch refund must be refused");
    assert!(err.to_string().contains("refund: till"), "{err}");
    // Same-branch refund in the open till works.
    sqlx::query("INSERT INTO order_refunds (order_id, till_id, amount, method, is_cash, reason, issued_by) VALUES ($1, $2, 100, 'Cash', true, 'goodwill', $3)")
        .bind(u(O1))
        .bind(u(S2))
        .bind(u(T1))
        .execute(&pool)
        .await
        .unwrap();
}

#[sqlx::test(migrations = false)]
async fn no_function_body_mentions_shifts(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    let n = i64_of(&pool, r"SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
        WHERE n.nspname = 'public' AND regexp_replace(p.prosrc, 'work_shift', '', 'g') ~ '(\mshifts\M|shift_id)'").await;
    assert_eq!(n, 0);
}

#[sqlx::test(migrations = false)]
async fn schema_has_no_stray_shift_identifiers(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    let stray = lines(&pool, r"
        SELECT k || ':' || name FROM (
          SELECT 'rel' k, relname::text name FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE nspname = 'public' AND relname ~ 'shift'
          UNION ALL SELECT 'col', c.relname || '.' || a.attname FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace
                     WHERE nspname = 'public' AND a.attnum > 0 AND NOT a.attisdropped AND a.attname ~ 'shift'
          UNION ALL SELECT 'con', conname::text FROM pg_constraint c JOIN pg_namespace n ON n.oid = c.connamespace WHERE nspname = 'public' AND conname ~ 'shift'
          UNION ALL SELECT 'trg', tgname::text FROM pg_trigger WHERE tgname ~ 'shift'
          UNION ALL SELECT 'fn', proname::text FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE nspname = 'public' AND proname ~ 'shift'
          UNION ALL SELECT 'type', typname::text FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace WHERE nspname = 'public' AND typname ~ 'shift'
          UNION ALL SELECT 'enum', enumlabel::text FROM pg_enum WHERE enumlabel ~ 'shift'
          UNION ALL SELECT 'policy', tablename || '.' || policyname FROM pg_policies WHERE schemaname = 'public' AND (qual ~ 'shift' OR policyname ~ 'shift')
          UNION ALL SELECT 'view', viewname::text FROM pg_views WHERE schemaname = 'public' AND regexp_replace(definition, 'work_shift', '', 'g') ~ 'shift'
        ) x
        WHERE name !~ 'work_shift' AND name !~ '^staff_schedules' AND name <> 'shift_counts'
        ORDER BY 1").await;
    assert!(stray.is_empty(), "stray shift identifiers: {stray:?}");
}

#[sqlx::test(migrations = false)]
async fn two_open_tills_same_teller_allowed(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    sqlx::query("INSERT INTO tills (branch_id, teller_id, status, verification) VALUES ($1, $2, 'open', 'unverified')")
        .bind(u(B1))
        .bind(u(T1))
        .execute(&pool)
        .await
        .expect("no unique constraint on one open till per person");
    assert_eq!(
        i64_of(
            &pool,
            &format!("SELECT count(*) FROM tills WHERE teller_id = '{T1}' AND status = 'open'")
        )
        .await,
        2
    );
    // A flag must link the other till.
    let err = sqlx::query(
        "INSERT INTO tills (branch_id, teller_id, opened_while_another_open) VALUES ($1, $2, true)",
    )
    .bind(u(B1))
    .bind(u(T1))
    .execute(&pool)
    .await;
    assert!(err.is_err());
}

#[sqlx::test(migrations = false)]
async fn permission_rows_follow_enum_rename(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    migrate_pre(&pool).await;
    sqlx::raw_sql(FIXTURE).execute(&pool).await.unwrap();
    let before_user = i64_of(
        &pool,
        "SELECT count(*) FROM permissions WHERE resource::text = 'shifts'",
    )
    .await;
    let before_role = i64_of(
        &pool,
        "SELECT count(*) FROM role_permissions WHERE resource::text = 'shifts'",
    )
    .await;
    migrate_rest(&pool).await;
    assert_eq!(
        i64_of(
            &pool,
            "SELECT count(*) FROM permissions WHERE resource::text = 'tills'"
        )
        .await,
        before_user
    );
    assert_eq!(
        i64_of(
            &pool,
            "SELECT count(*) FROM role_permissions WHERE resource::text = 'tills'"
        )
        .await,
        before_role
    );
    assert_eq!(i64_of(&pool, "SELECT count(*) FROM pg_enum e JOIN pg_type t ON t.oid = e.enumtypid WHERE t.typname = 'permission_resource' AND e.enumlabel = 'shifts'").await, 0);
    // The dead value survives, with its row.
    assert_eq!(
        i64_of(
            &pool,
            "SELECT count(*) FROM permissions WHERE resource::text = 'shift_counts'"
        )
        .await,
        1
    );
}

async fn insert_device(pool: &PgPool, id: Uuid, org: &str, branch: &str, code: &str) {
    sqlx::query("INSERT INTO devices (id, org_id, branch_id, code) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(u(org))
        .bind(u(branch))
        .bind(code)
        .execute(pool)
        .await
        .unwrap();
}

async fn insert_order(
    pool: &PgPool,
    till: &str,
    number: i32,
    device: Option<(Uuid, &str)>,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO orders (branch_id, till_id, teller_id, order_number, payment_method, order_ref, device_id, device_code)
                 VALUES ($1, $2, $3, $4, 'Cash', $5, $6, $7)")
        .bind(u(B1))
        .bind(u(till))
        .bind(u(T1))
        .bind(number)
        .bind(format!("REF-{}", Uuid::new_v4()))
        .bind(device.map(|d| d.0))
        .bind(device.map(|d| d.1))
        .execute(pool)
        .await
        .map(|_| ())
}

#[sqlx::test(migrations = false)]
async fn legacy_numbered_orders_unique_per_till(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    let err = insert_order(&pool, S2, 1, None)
        .await
        .expect_err("server-numbered #1 already exists in S2");
    assert!(
        err.to_string().contains("uq_orders_till_legacy_number"),
        "{err}"
    );
}

#[sqlx::test(migrations = false)]
async fn device_numbered_orders_not_unique_per_till(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    let (d1, d2) = (Uuid::new_v4(), Uuid::new_v4());
    insert_device(&pool, d1, ORG, B1, "36B").await;
    insert_device(&pool, d2, ORG, B1, "36B").await; // code clash never fails
    insert_order(&pool, S2, 1, Some((d1, "36B"))).await.unwrap();
    insert_order(&pool, S2, 1, Some((d2, "36B"))).await.unwrap();
    // device-numbered rows must carry a code
    let bad = sqlx::query("INSERT INTO orders (branch_id, till_id, teller_id, order_number, payment_method, order_ref, device_id) VALUES ($1,$2,$3,9,'Cash','REF-nocode',$4)")
        .bind(u(B1)).bind(u(S2)).bind(u(T1)).bind(d1)
        .execute(&pool).await;
    assert!(bad.is_err());
}

#[sqlx::test(migrations = false)]
async fn reconciliation_checks_enforced(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    let ins = |method: &'static str,
               is_cash: bool,
               status: &'static str,
               amount: Option<i32>,
               note: Option<&'static str>| {
        let pool = pool.clone();
        async move {
            sqlx::query("INSERT INTO till_reconciliations (till_id, method, is_cash, system_total, current_system_total, status, declared_amount, note)
                         VALUES ($1, $2, $3, 100, 100, $4, $5, $6)")
                .bind(u(S1)).bind(method).bind(is_cash).bind(status).bind(amount).bind(note)
                .execute(&pool).await
        }
    };
    assert!(
        ins("Card", false, "disagreed", None, Some("x"))
            .await
            .is_err(),
        "disagreed needs an amount"
    );
    assert!(
        ins("Card", false, "disagreed", Some(90), None)
            .await
            .is_err(),
        "non-cash disagreed needs a note"
    );
    assert!(
        ins("Cash", true, "disagreed", Some(90), None).await.is_ok(),
        "cash disagreed: note optional"
    );
    assert!(ins("Card", false, "checked", None, None).await.is_ok());
    assert!(
        ins("Card", false, "checked", None, None).await.is_err(),
        "one line per method"
    );
    // an open till carries no reconciliation status
    assert!(
        sqlx::query("UPDATE tills SET reconciliation_status = 'clean' WHERE id = $1")
            .bind(u(S2))
            .execute(&pool)
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = false)]
async fn availability_same_org_trigger(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    sqlx::query("INSERT INTO branch_payment_methods (branch_id, payment_method_id, org_id) VALUES ($1, $2, $3)")
        .bind(u(B1)).bind(u(PM_CASH)).bind(u(ORG))
        .execute(&pool).await.unwrap();
    // method of another org
    assert!(sqlx::query("INSERT INTO branch_payment_methods (branch_id, payment_method_id, org_id) VALUES ($1, $2, $3)")
        .bind(u(B2)).bind(u(PM_OTHER_ORG)).bind(u(ORG)).execute(&pool).await.is_err());
    // owner of another org
    assert!(sqlx::query("INSERT INTO user_payment_methods (user_id, payment_method_id, org_id) VALUES ($1, $2, $3)")
        .bind(u(U3)).bind(u(PM_CASH)).bind(u(ORG)).execute(&pool).await.is_err());
    let d = Uuid::new_v4();
    insert_device(&pool, d, ORG, B1, "A1").await;
    sqlx::query("INSERT INTO device_payment_methods (device_id, payment_method_id, org_id) VALUES ($1, $2, $3)")
        .bind(d).bind(u(PM_CASH)).bind(u(ORG))
        .execute(&pool).await.unwrap();
    let _ = B3;
}

#[sqlx::test(migrations = false)]
async fn down_script_round_trip(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    migrate_pre(&pool).await;
    sqlx::raw_sql(FIXTURE).execute(&pool).await.unwrap();
    let before = lines(&pool, PER_TILL_OLD).await;
    let entities_before = lines(
        &pool,
        "SELECT id::text || name || is_default FROM tills ORDER BY id",
    )
    .await;
    let bindings_before = lines(&pool, "SELECT id::text || till_id FROM shifts ORDER BY id").await;
    let occ_before = lines(&pool, "SELECT id::text || coalesce(started_till_id::text,'-') || coalesce(ended_till_id::text,'-') FROM table_occupancies ORDER BY id").await;
    migrate_rest(&pool).await;

    // psql meta-commands are not SQL; the script is otherwise plain.
    let down: String = DOWN_SQL
        .lines()
        .filter(|l| !l.trim_start().starts_with('\\'))
        .collect::<Vec<_>>()
        .join("\n");
    let mut tx = pool.begin().await.unwrap();
    sqlx::raw_sql(&down)
        .execute(&mut *tx)
        .await
        .expect("down.sql");
    tx.commit().await.unwrap();

    assert_eq!(lines(&pool, PER_TILL_OLD).await, before, "I1–I3 reversed");
    assert_eq!(
        lines(
            &pool,
            "SELECT id::text || name || is_default FROM tills ORDER BY id"
        )
        .await,
        entities_before
    );
    assert_eq!(
        lines(&pool, "SELECT id::text || till_id FROM shifts ORDER BY id").await,
        bindings_before
    );
    assert_eq!(lines(&pool, "SELECT id::text || coalesce(started_till_id::text,'-') || coalesce(ended_till_id::text,'-') FROM table_occupancies ORDER BY id").await, occ_before);
    let rework_in = REWORK_VERSIONS
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(
        i64_of(
            &pool,
            &format!("SELECT count(*) FROM _sqlx_migrations WHERE version IN ({rework_in})")
        )
        .await,
        0
    );
    // The old one-open-per-teller rule is back.
    assert!(
        sqlx::query("INSERT INTO tills (branch_id, teller_id, status) VALUES ($1, $2, 'open')")
            .bind(u(B1))
            .bind(u(T1))
            .execute(&pool)
            .await
            .is_err()
    );

    // up → down → up
    migrate_rest(&pool).await;
    assert_eq!(lines(&pool, PER_TILL_NEW).await, before);
    let applied: Vec<i64> = sqlx::query_scalar(
        "SELECT version FROM _sqlx_migrations WHERE version BETWEEN 20260914090000 AND 20260915090000 AND version NOT IN (20260914090700, 20260914090800) ORDER BY version",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    // client_seen (090700) and grants (090800) are not part of the rework and stay applied across its down script;
    // migrations after the rework's last one are not its business either.
    assert_eq!(applied, REWORK_VERSIONS.to_vec());
}

// ── §10.6 changefeed ─────────────────────────────────────────────────────────

/// 20260914091000: the backfill no longer stamps history as "changed now". A
/// backfilled LEDGER row carries its entity's own business time, so a full pull
/// right after deploy ships the 48 h window + open tills, not all history;
/// a row changed after the feed existed keeps its real stamp.
#[sqlx::test(migrations = false)]
async fn changefeed_backfill_restamped_to_business_time(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    migrate_pre(&pool).await;
    sqlx::raw_sql(FIXTURE)
        .execute(&pool)
        .await
        .expect("fixture");
    // History is old: every ledger row happened on 2026-09-01.
    sqlx::raw_sql(
        "SET session_replication_role = replica;
         UPDATE orders SET created_at = '2026-09-01 10:00+00', updated_at = '2026-09-01 10:30+00';
         UPDATE shift_cash_movements SET created_at = '2026-09-01 10:00+00';
         UPDATE order_refunds SET created_at = '2026-09-01 11:00+00', issued_at = '2026-09-01 11:00+00';
         SET session_replication_role = origin;",
    )
    .execute(&pool)
    .await
    .unwrap();
    // Up to (and including) the changefeed, but NOT the restamp yet: this is
    // what a rehearsal database looks like today.
    subset(|v| v <= 20260914090600)
        .run(&pool)
        .await
        .expect("rework up to the feed");
    let stamp =
        format!("(SELECT installed_on FROM _sqlx_migrations WHERE version = 20260914090300)");
    let ledger_at_stamp = format!(
        "SELECT count(*) FROM sync_changes WHERE type IN ('till','cash_movement','order','refund') AND changed_at = {stamp}"
    );
    let backfilled = i64_of(&pool, &ledger_at_stamp).await;
    assert!(
        backfilled >= 8,
        "the bug: every ledger row stamped as changed at migration time ({backfilled})"
    );
    // Counted as ROWS, not by stamp: the point is that the ledger backfill does
    // not WINDOW state rows away. A later migration may legitimately re-stamp a
    // catalog row — the price-lives-in-sizes migration mirrors every item's
    // price from its sizes, and a till must re-pull the catalog when it does —
    // so the stamp is not the invariant here; the row surviving is.
    let state_before = i64_of(
        &pool,
        "SELECT count(*) FROM sync_changes WHERE type = 'menu_item'",
    )
    .await;
    // A change made after the feed exists keeps its real stamp.
    sqlx::query(&format!(
        "UPDATE orders SET total_amount = total_amount WHERE id = '{O1}'"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let o1_changed: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(&format!(
        "SELECT changed_at FROM sync_changes WHERE type = 'order' AND entity_id = '{O1}'"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();

    migrate_rest(&pool).await;

    assert_eq!(
        i64_of(&pool, &ledger_at_stamp).await,
        0,
        "no backfilled ledger row keeps the migration-time stamp"
    );
    let o3: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(&format!(
        "SELECT changed_at FROM sync_changes WHERE type = 'order' AND entity_id = '{O3}'"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        o3.to_rfc3339(),
        "2026-09-01T10:00:00+00:00",
        "an order takes its own business time, not updated_at"
    );
    let o1_after: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(&format!(
        "SELECT changed_at FROM sync_changes WHERE type = 'order' AND entity_id = '{O1}'"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(o1_after, o1_changed, "a real post-feed change is untouched");
    let closed_till: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(&format!(
        "SELECT changed_at FROM sync_changes WHERE type = 'till' AND entity_id = '{S1}'"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(closed_till.to_rfc3339(), "2026-09-01T12:00:00+00:00");
    assert_eq!(
        i64_of(
            &pool,
            "SELECT count(*) FROM sync_changes WHERE type = 'menu_item'"
        )
        .await,
        state_before,
        "state rows are not windowed away by the ledger backfill"
    );

    // The full pull: the closed till's old history is out of the window; the
    // open till's whole history and the just-changed order are in.
    let org = u(ORG);
    let req = madar_rust::sync::pull::PullRequest {
        branch_id: u(B1),
        device_id: None,
        types: None,
        limit: None,
        ledger_page_size: None,
        snapshot_cursor: None,
    };
    let resp = madar_rust::sync::pull::pull_core(&pool, org, &req, None)
        .await
        .unwrap();
    let ids = |ty: &str| -> Vec<String> {
        resp.data[ty]
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect()
    };
    let orders = ids("order");
    assert!(
        orders.contains(&O3.to_string()),
        "open till's order is in: {orders:?}"
    );
    assert!(
        orders.contains(&O1.to_string()),
        "changed-now order is in: {orders:?}"
    );
    assert!(
        !orders.contains(&"00000000-0000-4000-8000-00000000d002".to_string()),
        "old closed-till order is out: {orders:?}"
    );
    let tills = ids("till");
    assert!(
        tills.contains(&S2.to_string()) && !tills.contains(&S1.to_string()),
        "{tills:?}"
    );
    assert_eq!(
        ids("refund").len(),
        1,
        "the refund was issued from the open till S2, so its history is in"
    );
}

#[sqlx::test(migrations = false)]
async fn changefeed_backfill_matches_live_sets(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    let diff = i64_of(&pool, "SELECT count(*) FROM (
        (SELECT branch_id, type, entity_id FROM sync_changes WHERE op = 'upsert' EXCEPT SELECT * FROM sync_live_rows())
        UNION ALL
        (SELECT * FROM sync_live_rows() EXCEPT SELECT branch_id, type, entity_id FROM sync_changes WHERE op = 'upsert')) d").await;
    assert_eq!(diff, 0);
    assert_eq!(
        i64_of(
            &pool,
            &format!(
                "SELECT count(*) FROM sync_changes WHERE type = 'till' AND branch_id = '{B1}'"
            )
        )
        .await,
        2
    );
    assert_eq!(i64_of(&pool, &format!("SELECT count(*) FROM sync_changes WHERE type = 'open_ticket' AND branch_id = '{B1}'")).await, 1);
    assert_eq!(
        i64_of(
            &pool,
            "SELECT count(*) FROM sync_feed_watermarks WHERE purged_through_seq = 0"
        )
        .await,
        3
    );
}

#[sqlx::test(migrations = false)]
async fn sync_emit_compacts_and_advances_seq(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    let (_, s0) = feed_op(&pool, B1, "till", S2).await.unwrap();
    let rows_before = i64_of(&pool, "SELECT count(*) FROM sync_changes").await;
    sqlx::query("UPDATE tills SET notes = 'x' WHERE id = $1")
        .bind(u(S2))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE tills SET notes = 'y' WHERE id = $1")
        .bind(u(S2))
        .execute(&pool)
        .await
        .unwrap();
    let (op, s1) = feed_op(&pool, B1, "till", S2).await.unwrap();
    assert_eq!(op, "upsert");
    assert!(s1 > s0);
    assert_eq!(s1, max_seq(&pool).await);
    assert_eq!(
        i64_of(&pool, "SELECT count(*) FROM sync_changes").await,
        rows_before,
        "compacted: one row per entity"
    );
    // horizon: nothing in flight → head of the branch
    let h: i64 = sqlx::query_scalar("SELECT sync_safe_horizon($1, 0)")
        .bind(u(B1))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(h, s1);
}

#[sqlx::test(migrations = false)]
async fn safe_horizon_waits_out_uncommitted_emitters(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    let mut writer = pool.begin().await.unwrap();
    sqlx::query("UPDATE tills SET notes = 'in flight' WHERE id = $1")
        .bind(u(S2))
        .execute(&mut *writer)
        .await
        .unwrap();
    let head_before: i64 = i64_of(
        &pool,
        &format!("SELECT max(seq) FROM sync_changes WHERE branch_id = '{B1}'"),
    )
    .await;
    // The writer holds the branch key shared: no progress past `since`.
    let h: i64 = sqlx::query_scalar("SELECT sync_safe_horizon($1, 7, 50)")
        .bind(u(B1))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(h, 7);
    writer.commit().await.unwrap();
    let h: i64 = sqlx::query_scalar("SELECT sync_safe_horizon($1, 7)")
        .bind(u(B1))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(h > head_before);
}

#[sqlx::test(migrations = false)]
async fn ticket_settle_emits_delete(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    assert_eq!(
        feed_op(&pool, B1, "open_ticket", TK_OPEN).await.unwrap().0,
        "upsert"
    );
    sqlx::query("UPDATE open_tickets SET status = 'settled', settled_at = now(), settled_by = $2, settled_till_id = $3 WHERE id = $1")
        .bind(u(TK_OPEN)).bind(u(T1)).bind(u(S2))
        .execute(&pool).await.unwrap();
    assert_eq!(
        feed_op(&pool, B1, "open_ticket", TK_OPEN).await.unwrap().0,
        "delete"
    );
}

#[sqlx::test(migrations = false)]
async fn occupancy_needs_bussing_stays_live_until_cleared(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    assert_eq!(
        feed_op(&pool, B1, "table_occupancy", OC3).await.unwrap().0,
        "upsert"
    );
    sqlx::query("UPDATE table_occupancies SET ended_at = now(), ended_by = $2, end_reason = 'released', needs_bussing = true WHERE id = $1")
        .bind(u(OC3)).bind(u(T1)).execute(&pool).await.unwrap();
    assert_eq!(
        feed_op(&pool, B1, "table_occupancy", OC3).await.unwrap().0,
        "upsert"
    );
    sqlx::query("UPDATE table_occupancies SET cleared_at = now(), cleared_by = $2 WHERE id = $1")
        .bind(u(OC3))
        .bind(u(T1))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        feed_op(&pool, B1, "table_occupancy", OC3).await.unwrap().0,
        "delete"
    );
}

#[sqlx::test(migrations = false)]
async fn org_type_fans_out_to_all_branches(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    let s0 = max_seq(&pool).await;
    sqlx::query("UPDATE org_payment_methods SET color = '#fff' WHERE id = $1")
        .bind(u(PM_CASH))
        .execute(&pool)
        .await
        .unwrap();
    let branches = lines(&pool, &format!("SELECT branch_id::text FROM sync_changes WHERE seq > {s0} AND type = 'payment_method' ORDER BY 1")).await;
    assert_eq!(
        branches,
        vec![B1.to_string(), B2.to_string()],
        "both branches of the org, not the other org's"
    );
}

#[sqlx::test(migrations = false)]
async fn child_row_change_reemits_parent_menu_item(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    let (_, s0) = feed_op(&pool, B1, "menu_item", MI).await.unwrap();
    sqlx::query("UPDATE menu_item_sizes SET price = 1200 WHERE id = $1")
        .bind(u(SZ))
        .execute(&pool)
        .await
        .unwrap();
    let (op, s1) = feed_op(&pool, B1, "menu_item", MI).await.unwrap();
    assert_eq!(op, "upsert");
    assert!(s1 > s0);
    // branch-scoped price override re-emits for that branch only
    let (_, b2_before) = feed_op(&pool, B2, "menu_item", MI).await.unwrap();
    sqlx::query("INSERT INTO menu_price_overrides (scope, branch_id, target_type, target_id, price) VALUES ('branch', $1, 'menu_item_size', $2, 900)")
        .bind(u(B1)).bind(u(SZ)).execute(&pool).await.unwrap();
    assert!(feed_op(&pool, B1, "menu_item", MI).await.unwrap().1 > s1);
    assert_eq!(
        feed_op(&pool, B2, "menu_item", MI).await.unwrap().1,
        b2_before
    );
    // deactivating the item leaves the live set
    sqlx::query("UPDATE menu_items SET is_active = false WHERE id = $1")
        .bind(u(MI))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        feed_op(&pool, B2, "menu_item", MI).await.unwrap().0,
        "delete"
    );
}

#[sqlx::test(migrations = false)]
async fn every_projection_source_table_has_emitter(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    // The changefeed's header plus every later migration that adds source tables.
    let header: Vec<String> = [
        include_str!("../migrations/20260914090300_sync_changefeed.sql"),
        include_str!("../migrations/20260915090000_sync_feed_gaps.sql"),
        include_str!("../migrations/20260917090000_sync_feed_branch_reads.sql"),
        include_str!("../migrations/20260918110000_authz_enforce.sql"),
        include_str!("../migrations/20260918200000_customers.sql"),
        include_str!("../migrations/20260921020000_till_spot_views.sql"),
        include_str!("../migrations/20260922030000_staff_drinks_pool.sql"),
        include_str!("../migrations/20260925030000_customers_shared_key.sql"),
    ]
    .iter()
    .flat_map(|sql| {
        sql.lines()
            .skip_while(|l| !l.starts_with("-- SOURCE TABLES"))
            .skip(2)
            .take_while(|l| l.contains("->"))
            .map(|l| {
                l.trim_start_matches("--")
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<_>>()
    })
    .collect();
    assert!(header.len() > 40, "header parsed: {header:?}");
    let mut registry = lines(
        &pool,
        "SELECT source_table FROM sync_source_tables() ORDER BY 1",
    )
    .await;
    let mut h = header.clone();
    h.sort();
    registry.sort();
    assert_eq!(
        h, registry,
        "migration header and sync_source_tables() disagree"
    );
    let missing = lines(&pool, "SELECT s.source_table FROM sync_source_tables() s
        WHERE NOT EXISTS (SELECT 1 FROM pg_trigger t JOIN pg_proc p ON p.oid = t.tgfoid
                           WHERE t.tgrelid = to_regclass('public.' || s.source_table) AND t.tgname = 'sync_emit'
                             AND p.proname = 'sync_emit_' || s.source_table)").await;
    assert!(missing.is_empty(), "tables without emitter: {missing:?}");
}

// ── §11.8 assets ─────────────────────────────────────────────────────────────

async fn insert_group_and_asset(
    pool: &PgPool,
    org: Option<&str>,
    hash: &str,
) -> Result<Uuid, sqlx::Error> {
    let group: Uuid = sqlx::query_scalar("INSERT INTO asset_groups (org_id, kind, source_hash, encoder) VALUES ($1, 'image', $2, 'test/1') RETURNING id")
        .bind(org.map(u))
        .bind(format!("{:0>64}", Uuid::new_v4().simple().to_string()))
        .fetch_one(pool)
        .await?;
    sqlx::query("INSERT INTO assets (org_id, hash, group_id, encoder, kind, variant, ext, content_type, bytes, source_hash, source_kind)
                 VALUES ($1, $2, $3, 'test/1', 'image', 'tile', 'webp', 'image/webp', 10, $2, 'upload')")
        .bind(org.map(u))
        .bind(hash)
        .bind(group)
        .execute(pool)
        .await?;
    Ok(group)
}

#[sqlx::test(migrations = false)]
async fn assets_unique_per_org_not_global(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    let hash = "a".repeat(64);
    let g = insert_group_and_asset(&pool, Some(ORG), &hash)
        .await
        .unwrap();
    insert_group_and_asset(&pool, Some(ORG2), &hash)
        .await
        .expect("same bytes in another org is a separate row");
    // 090600: a file is shared by (org, hash); a row is unique per (group, variant).
    insert_group_and_asset(&pool, Some(ORG), &hash)
        .await
        .expect("another group in the same org may reference the same file");
    assert!(
        sqlx::query("INSERT INTO assets (org_id, hash, group_id, encoder, kind, variant, ext, content_type, bytes, source_hash, source_kind)
                     VALUES ($1, $2, $3, 'test/1', 'image', 'tile', 'webp', 'image/webp', 10, $2, 'upload')")
            .bind(u(ORG))
            .bind("b".repeat(64))
            .bind(g)
            .execute(&pool)
            .await
            .is_err(),
        "one row per variant within a group"
    );
}

#[sqlx::test(migrations = false)]
async fn asset_reference_columns_fk_set_null(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    let g = insert_group_and_asset(&pool, Some(ORG), &"b".repeat(64))
        .await
        .unwrap();
    sqlx::query("UPDATE menu_items SET image_group_id = $1 WHERE id = $2")
        .bind(g)
        .bind(u(MI))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM asset_groups WHERE id = $1")
        .bind(g)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        uuid_opt(
            &pool,
            &format!("SELECT image_group_id FROM menu_items WHERE id = '{MI}'")
        )
        .await,
        None
    );
    assert_eq!(
        i64_of(
            &pool,
            &format!("SELECT count(*) FROM assets WHERE group_id = '{g}'")
        )
        .await,
        0
    );
}

#[sqlx::test(migrations = false)]
async fn asset_ref_change_marks_bundle_dirty_and_emits_row(pool: PgPool) {
    let (pool, _fresh) = fresh(&pool).await;
    setup(&pool).await;
    let g = insert_group_and_asset(&pool, Some(ORG), &"c".repeat(64))
        .await
        .unwrap();
    let (_, s0) = feed_op(&pool, B1, "menu_item", MI).await.unwrap();
    sqlx::query("UPDATE menu_items SET image_group_id = $1 WHERE id = $2")
        .bind(g)
        .bind(u(MI))
        .execute(&pool)
        .await
        .unwrap();
    assert!(feed_op(&pool, B1, "menu_item", MI).await.unwrap().1 > s0);
    assert_eq!(
        i64_of(&pool, "SELECT count(*) FROM asset_bundle_dirty").await,
        2
    );
    // org logo re-emits branch_settings for every branch of the org
    let (_, bs0) = feed_op(&pool, B2, "branch_settings", B2).await.unwrap();
    sqlx::query("UPDATE organizations SET logo_group_id = $1 WHERE id = $2")
        .bind(g)
        .bind(u(ORG))
        .execute(&pool)
        .await
        .unwrap();
    assert!(feed_op(&pool, B2, "branch_settings", B2).await.unwrap().1 > bs0);
}
