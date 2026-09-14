//! Shared till-report test vectors (OFFLINE_B_DESIGN §7).
//!
//! The POS computes a till's drawer and Z report on the device from the rows the
//! changefeed delivers. This test seeds scenarios, takes each till's rows exactly
//! as `POST /sync/pull` projects them, and records what THIS backend computes:
//! `compute_system_cash`, `report_figures` and the close preview's per-method
//! totals. The POS core loads the same file (`madar-core` `ledger::report`) and
//! must agree field by field.
//!
//! The file is committed. A change to the drawer formula, the report or the
//! projections fails this test until the vectors are regenerated and copied to
//! the POS:
//!
//! ```sh
//! MADAR_WRITE_TILL_VECTORS=1 cargo nextest run -E 'test(till_report_vectors)'
//! cp tests/fixtures/till_report_vectors.json \
//!    ../madar/rust-core/crates/madar-core/tests/fixtures/till_report_vectors.json
//! ```
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

const PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/till_report_vectors.json");

fn id(label: &str) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_OID, format!("till-vector:{label}").as_bytes())
}

/// One scenario: SQL run with `{org}`, `{branch}`, `{sara}`, `{omar}` and every
/// `{id:<label>}` substituted, and the tills whose figures are recorded.
struct Scenario {
    name: &'static str,
    sql: &'static str,
    tills: &'static [&'static str],
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "cash_card_splits_and_tips",
        tills: &["t1"],
        sql: r#"
INSERT INTO tills (id, branch_id, teller_id, status, opening_cash, opened_at) VALUES ('{id:t1}', '{branch}', '{sara}', 'open', 10000, '2026-09-14 08:00+00');
-- a cash sale
INSERT INTO orders (id, branch_id, till_id, teller_id, order_number, payment_method, order_ref, subtotal, total_amount, created_at)
     VALUES ('{id:a}', '{branch}', '{id:t1}', '{sara}', 1, 'Cash', 'V-A', 5000, 5000, '2026-09-14 09:00+00');
INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ('{id:a}', 'Cash', 5000, true);
-- a split: card + cash
INSERT INTO orders (id, branch_id, till_id, teller_id, order_number, payment_method, order_ref, subtotal, total_amount, created_at)
     VALUES ('{id:b}', '{branch}', '{id:t1}', '{sara}', 2, 'Card', 'V-B', 5000, 5000, '2026-09-14 09:05+00');
INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ('{id:b}', 'Card', 3000, false), ('{id:b}', 'Cash', 2000, true);
-- a card sale with a CASH tip
INSERT INTO orders (id, branch_id, till_id, teller_id, order_number, payment_method, order_ref, subtotal, total_amount, tip_amount, tip_payment_method, tip_is_cash, created_at)
     VALUES ('{id:c}', '{branch}', '{id:t1}', '{sara}', 3, 'Card', 'V-C', 4000, 4000, 500, 'Cash', true, '2026-09-14 09:10+00');
INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ('{id:c}', 'Card', 4000, false);
-- a cash sale with a CARD tip
INSERT INTO orders (id, branch_id, till_id, teller_id, order_number, payment_method, order_ref, subtotal, total_amount, tip_amount, tip_payment_method, tip_is_cash, created_at)
     VALUES ('{id:d}', '{branch}', '{id:t1}', '{sara}', 4, 'Cash', 'V-D', 1000, 1000, 300, 'Card', false, '2026-09-14 09:15+00');
INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ('{id:d}', 'Cash', 1000, true);
"#,
    },
    Scenario {
        name: "voids_and_refunds",
        tills: &["t1"],
        sql: r#"
INSERT INTO tills (id, branch_id, teller_id, status, opening_cash, opened_at) VALUES ('{id:t1}', '{branch}', '{sara}', 'open', 2000, '2026-09-14 08:00+00');
-- voided cash sale: not tendered, counted as voided
INSERT INTO orders (id, branch_id, till_id, teller_id, order_number, payment_method, order_ref, subtotal, total_amount, status, voided_at, voided_by, void_reason, created_at)
     VALUES ('{id:e}', '{branch}', '{id:t1}', '{sara}', 1, 'Cash', 'V-E', 2000, 2000, 'voided', '2026-09-14 09:30+00', '{sara}', 'wrong_order', '2026-09-14 09:00+00');
INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ('{id:e}', 'Cash', 2000, true);
-- cash sale, partly refunded in cash
INSERT INTO orders (id, branch_id, till_id, teller_id, order_number, payment_method, order_ref, subtotal, total_amount, created_at)
     VALUES ('{id:f}', '{branch}', '{id:t1}', '{sara}', 2, 'Cash', 'V-F', 3000, 3000, '2026-09-14 09:05+00');
INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ('{id:f}', 'Cash', 3000, true);
INSERT INTO order_refunds (id, org_id, branch_id, order_id, till_id, amount, method, is_cash, reason, issued_by, issued_at, created_at)
     VALUES ('{id:rf}', '{org}', '{branch}', '{id:f}', '{id:t1}', 1000, 'Cash', true, 'goodwill', '{sara}', '2026-09-14 10:00+00', '2026-09-14 10:00+00');
-- card sale, fully refunded to card (status becomes refunded)
INSERT INTO orders (id, branch_id, till_id, teller_id, order_number, payment_method, order_ref, subtotal, total_amount, created_at)
     VALUES ('{id:g}', '{branch}', '{id:t1}', '{sara}', 3, 'Card', 'V-G', 2500, 2500, '2026-09-14 09:10+00');
INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ('{id:g}', 'Card', 2500, false);
INSERT INTO order_refunds (id, org_id, branch_id, order_id, till_id, amount, method, is_cash, reason, issued_by, issued_at, created_at)
     VALUES ('{id:rg}', '{org}', '{branch}', '{id:g}', '{id:t1}', 2500, 'Card', false, 'wrong_order', '{sara}', '2026-09-14 10:05+00', '2026-09-14 10:05+00');
-- cash sale with a cash tip, fully refunded in cash
INSERT INTO orders (id, branch_id, till_id, teller_id, order_number, payment_method, order_ref, subtotal, total_amount, tip_amount, tip_payment_method, tip_is_cash, created_at)
     VALUES ('{id:h}', '{branch}', '{id:t1}', '{sara}', 4, 'Cash', 'V-H', 1500, 1500, 200, 'Cash', true, '2026-09-14 09:15+00');
INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ('{id:h}', 'Cash', 1500, true);
INSERT INTO order_refunds (id, org_id, branch_id, order_id, till_id, amount, method, is_cash, reason, issued_by, issued_at, created_at)
     VALUES ('{id:rh}', '{org}', '{branch}', '{id:h}', '{id:t1}', 1500, 'Cash', true, 'quality_issue', '{sara}', '2026-09-14 10:10+00', '2026-09-14 10:10+00');
"#,
    },
    Scenario {
        name: "drawer_movements",
        tills: &["t1"],
        sql: r#"
INSERT INTO tills (id, branch_id, teller_id, status, opening_cash, opened_at) VALUES ('{id:t1}', '{branch}', '{sara}', 'open', 5000, '2026-09-14 08:00+00');
INSERT INTO till_cash_movements (id, till_id, amount, note, moved_by, kind, created_at) VALUES
  ('{id:in}',   '{id:t1}',  2000, 'float top-up', '{sara}', 'pay_in',    '2026-09-14 09:00+00'),
  ('{id:out}',  '{id:t1}',  -500, 'milk run',     '{sara}', 'pay_out',   '2026-09-14 09:10+00'),
  ('{id:drop}', '{id:t1}', -3000, 'to the safe',  '{omar}', 'safe_drop', '2026-09-14 09:20+00');
-- a correction of the pay-out lands in the pay-out bucket; a bare one in its own
INSERT INTO till_cash_movements (id, till_id, amount, note, moved_by, kind, corrects_id, created_at) VALUES
  ('{id:fix}',  '{id:t1}',   500, 'milk was free', '{sara}', 'correction', '{id:out}', '2026-09-14 09:30+00'),
  ('{id:adj}',  '{id:t1}',  -200, 'miscount',      '{sara}', 'correction', NULL,       '2026-09-14 09:40+00');
INSERT INTO orders (id, branch_id, till_id, teller_id, order_number, payment_method, order_ref, subtotal, total_amount, created_at)
     VALUES ('{id:a}', '{branch}', '{id:t1}', '{sara}', 1, 'Cash', 'V-MA', 700, 700, '2026-09-14 09:50+00');
INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ('{id:a}', 'Cash', 700, true);
"#,
    },
    Scenario {
        name: "legacy_null_cash_flags",
        tills: &["t1"],
        sql: r#"
INSERT INTO tills (id, branch_id, teller_id, status, opening_cash, opened_at) VALUES ('{id:t1}', '{branch}', '{sara}', 'open', 0, '2026-09-14 08:00+00');
-- is_cash NULL: only the literal method name 'cash' counts as cash
INSERT INTO orders (id, branch_id, till_id, teller_id, order_number, payment_method, order_ref, subtotal, total_amount, tip_amount, created_at)
     VALUES ('{id:a}', '{branch}', '{id:t1}', '{sara}', 1, 'cash', 'V-LA', 1000, 1000, 100, '2026-09-14 09:00+00');
INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ('{id:a}', 'cash', 1000, NULL);
INSERT INTO orders (id, branch_id, till_id, teller_id, order_number, payment_method, order_ref, subtotal, total_amount, tip_amount, created_at)
     VALUES ('{id:b}', '{branch}', '{id:t1}', '{sara}', 2, 'Cash', 'V-LB', 2000, 2000, 150, '2026-09-14 09:05+00');
INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ('{id:b}', 'Cash', 2000, NULL);
-- tip method named but no flag: the name decides
INSERT INTO orders (id, branch_id, till_id, teller_id, order_number, payment_method, order_ref, subtotal, total_amount, tip_amount, tip_payment_method, created_at)
     VALUES ('{id:c}', '{branch}', '{id:t1}', '{sara}', 3, 'cash', 'V-LC', 800, 800, 70, 'Visa', '2026-09-14 09:10+00');
INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ('{id:c}', 'cash', 800, NULL);
-- no legs at all (a pre-legs sale): tendered nothing, still a sale
INSERT INTO orders (id, branch_id, till_id, teller_id, order_number, payment_method, order_ref, subtotal, total_amount, created_at)
     VALUES ('{id:d}', '{branch}', '{id:t1}', '{sara}', 4, 'cash', 'V-LD', 300, 300, '2026-09-14 09:15+00');
"#,
    },
    Scenario {
        name: "refund_from_another_till",
        tills: &["t1", "t2"],
        sql: r#"
INSERT INTO tills (id, branch_id, teller_id, status, opening_cash, opened_at) VALUES ('{id:t1}', '{branch}', '{sara}', 'open', 1000, '2026-09-14 08:00+00');
INSERT INTO tills (id, branch_id, teller_id, status, opening_cash, opened_at) VALUES ('{id:t2}', '{branch}', '{omar}', 'open', 3000, '2026-09-14 08:30+00');
INSERT INTO orders (id, branch_id, till_id, teller_id, order_number, payment_method, order_ref, subtotal, total_amount, created_at)
     VALUES ('{id:a}', '{branch}', '{id:t1}', '{sara}', 1, 'Cash', 'V-RA', 4000, 4000, '2026-09-14 09:00+00');
INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ('{id:a}', 'Cash', 4000, true);
-- the money goes back out of Omar's drawer, not Sara's
INSERT INTO order_refunds (id, org_id, branch_id, order_id, till_id, amount, method, is_cash, reason, issued_by, issued_at, created_at)
     VALUES ('{id:r}', '{org}', '{branch}', '{id:a}', '{id:t2}', 1000, 'Cash', true, 'overcharged', '{omar}', '2026-09-14 11:00+00', '2026-09-14 11:00+00');
"#,
    },
    Scenario {
        name: "closed_till_keeps_its_snapshot",
        tills: &["t1"],
        sql: r#"
INSERT INTO tills (id, branch_id, teller_id, status, opening_cash, opened_at) VALUES ('{id:t1}', '{branch}', '{sara}', 'open', 1000, '2026-09-14 08:00+00');
INSERT INTO orders (id, branch_id, till_id, teller_id, order_number, payment_method, order_ref, subtotal, total_amount, created_at)
     VALUES ('{id:a}', '{branch}', '{id:t1}', '{sara}', 1, 'Cash', 'V-CA', 2000, 2000, '2026-09-14 09:00+00');
INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ('{id:a}', 'Cash', 2000, true);
UPDATE tills SET status = 'closed', closed_at = '2026-09-14 12:00+00', closing_cash_declared = 2900,
                 closing_cash_system = 3000 WHERE id = '{id:t1}';
-- a late sale replayed onto the closed till: the frozen drawer figure stands
INSERT INTO orders (id, branch_id, till_id, teller_id, order_number, payment_method, order_ref, subtotal, total_amount, created_at)
     VALUES ('{id:b}', '{branch}', '{id:t1}', '{sara}', 2, 'Cash', 'V-CB', 500, 500, '2026-09-14 11:59+00');
INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ('{id:b}', 'Cash', 500, true);
"#,
    },
    Scenario {
        name: "empty_till",
        tills: &["t1"],
        sql: r#"
INSERT INTO tills (id, branch_id, teller_id, status, opening_cash, opened_at) VALUES ('{id:t1}', '{branch}', '{sara}', 'open', 0, '2026-09-14 08:00+00');
"#,
    },
];

async fn run_scenario(pool: &PgPool, sc: &Scenario) -> Value {
    let org = id(&format!("{}:org", sc.name));
    let branch = id(&format!("{}:branch", sc.name));
    let sara = id(&format!("{}:sara", sc.name));
    let omar = id(&format!("{}:omar", sc.name));
    let setup = format!(
        "INSERT INTO organizations (id, name, slug) VALUES ('{org}', 'Vector {n}', 'vector-{n}');
         INSERT INTO branches (id, org_id, name, code, timezone) VALUES ('{branch}', '{org}', 'Vector', 'VEC', 'Africa/Cairo');
         INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES
           ('{sara}', '{org}', 'Sara', 'sara-{n}@vector.test', 'x', 'teller'),
           ('{omar}', '{org}', 'Omar', 'omar-{n}@vector.test', 'x', 'teller');
         INSERT INTO org_payment_methods (id, org_id, name, color, icon, is_cash, created_at) VALUES
           ('{cash}', '{org}', 'Cash', '#000', 'cash', true, '2026-01-01 00:00+00'),
           ('{card}', '{org}', 'Card', '#00f', 'card', false, '2026-01-01 00:00+00');",
        n = sc.name,
        cash = id(&format!("{}:pm:cash", sc.name)),
        card = id(&format!("{}:pm:card", sc.name)),
    );
    sqlx::raw_sql(&setup).execute(pool).await.expect("setup");
    let mut sql = sc.sql.replace("{org}", &org.to_string()).replace("{branch}", &branch.to_string());
    sql = sql.replace("{sara}", &sara.to_string()).replace("{omar}", &omar.to_string());
    while let Some(start) = sql.find("{id:") {
        let end = start + sql[start..].find('}').unwrap();
        let label = sql[start + 4..end].to_string();
        sql.replace_range(start..=end, &id(&format!("{}:{label}", sc.name)).to_string());
    }
    sqlx::raw_sql(&sql).execute(pool).await.unwrap_or_else(|e| panic!("{}: {e}", sc.name));

    let body = crate::sync::pull::PullRequest { branch_id: branch, device_id: None, types: None, limit: None, ledger_page_size: None, snapshot_cursor: None };
    let full = crate::sync::pull::pull_core(pool, org, &body, None).await.unwrap();
    // Rows exactly as the device receives them, minus the per-database `seq`.
    let rows = |ty: &str| -> Vec<Value> {
        let mut v: Vec<Value> = full.data.get(ty).cloned().unwrap_or_default();
        for r in v.iter_mut() {
            if let Value::Object(m) = r {
                m.remove("seq");
            }
        }
        v.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        v
    };

    let mut tills = serde_json::Map::new();
    for label in sc.tills {
        let till_id = id(&format!("{}:{label}", sc.name));
        let till = crate::tills::handlers::fetch_till_or_404(pool, till_id).await.unwrap();
        let system_cash = crate::tills::handlers::compute_system_cash(pool, till_id).await.unwrap();
        let f = crate::tills::handlers::report_figures(pool, &till).await.unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let methods = crate::tills::reconcile::system_totals_by_method(&mut conn, till_id, f.expected_cash)
            .await
            .unwrap();
        tills.insert(
            till_id.to_string(),
            json!({
                "system_cash": system_cash,
                "expected_cash": f.expected_cash,
                "payment_summary": f.payment_summary.iter().map(|p| json!({
                    "payment_method": p.payment_method, "is_cash": p.is_cash, "total": p.total, "order_count": p.order_count,
                })).collect::<Vec<_>>(),
                "total_payments": f.total_payments,
                "voided_amount": f.voided_amount,
                "net_payments": f.net_payments,
                "total_tips": f.total_tips,
                "cash_tips": f.cash_tips,
                "non_cash_tips": f.non_cash_tips,
                "cash_movements_in": f.cash_movements_in,
                "cash_movements_out": f.cash_movements_out,
                "safe_drops": f.safe_drops,
                "cash_adjustments": f.cash_adjustments,
                "cash_movements_net": f.cash_movements_net,
                "cash_movement_ids": f.cash_movements.iter().map(|m| m.id.to_string()).collect::<Vec<_>>(),
                "refunds_issued_count": f.refunds_issued_count,
                "refunds_issued_amount": f.refunds_issued_amount,
                "refunds_issued_cash": f.refunds_issued_cash,
                "cash_in_refunded_sales": f.cash_in_refunded_sales,
                "close_methods": methods.iter().map(|m| json!({
                    "method": m.method, "is_cash": m.is_cash, "system_total": m.system_total, "order_count": m.order_count,
                    "payment_method_id": m.payment_method_id,
                })).collect::<Vec<_>>(),
            }),
        );
    }
    json!({
        "name": sc.name,
        "branch_id": branch,
        "rows": {
            "till": rows("till"),
            "order": rows("order"),
            "cash_movement": rows("cash_movement"),
            "refund": rows("refund"),
            "payment_method": rows("payment_method"),
        },
        "expected": tills,
    })
}

/// Fields that depend on when the test ran, not on the scenario.
fn scrub(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for k in ["changed_at", "updated_at", "printed_at"] {
                m.remove(k);
            }
            // Legs are ordered by their random row id on the wire; the order
            // carries no meaning, so the vector pins one.
            if let Some(Value::Array(legs)) = m.get_mut("payment_legs") {
                legs.sort_by_key(|l| (l["method"].as_str().unwrap_or("").to_string(), l["amount"].as_i64()));
            }
            m.values_mut().for_each(scrub);
        }
        Value::Array(a) => a.iter_mut().for_each(scrub),
        _ => {}
    }
}

#[sqlx::test]
async fn till_report_vectors(pool: PgPool) {
    let mut scenarios = Vec::new();
    for sc in SCENARIOS {
        let mut v = run_scenario(&pool, sc).await;
        scrub(&mut v);
        scenarios.push(v);
    }
    let doc = json!({
        "about": "Till drawer + Z report vectors generated by MadarRust src/tills/report_vectors_tests.rs; \
                  rows are /sync/pull projections, expected is the backend's own computation.",
        "scenarios": scenarios,
    });
    let text = serde_json::to_string_pretty(&doc).unwrap() + "\n";
    if std::env::var("MADAR_WRITE_TILL_VECTORS").is_ok() {
        std::fs::create_dir_all(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures")).unwrap();
        std::fs::write(PATH, &text).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(PATH).expect("tests/fixtures/till_report_vectors.json (regenerate: see module docs)");
    let committed: Value = serde_json::from_str(&committed).unwrap();
    assert_eq!(
        committed, doc,
        "the till report or its projections changed: regenerate the vectors and copy them to the POS (module docs)"
    );
}
