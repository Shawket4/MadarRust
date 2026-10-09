//! A till's rows, loaded for madar-shared's fold (`madar_till::report`).
//!
//! The drawer (`compute_system_cash`), the Z report (`report_figures`) and the
//! close preview's per-method totals (`reconcile::system_totals_by_method`)
//! used to be SQL aggregates, pinned to the fold the POS core runs by the
//! shared till vectors. They are the fold now: this module only LOADS the
//! rows — the till, its sales and their tenders, its drawer movements, the
//! refunds issued from it and the org's payment methods — in the shapes the
//! fold reads, and every figure is `madar_till::report`'s. The till vectors
//! (`tests/tills_report_vectors_tests.rs`, generated from the SQL before the
//! move) pin that nothing changed.
//!
//! The loads keep the SQL's own readings of each field, so the fold sees what
//! the aggregates saw: a leg is cash by `COALESCE(is_cash, method = 'cash')`,
//! a missing tip is zero, a sale's refunded tax / service charge is
//! `v_order_refund_totals`', and a correction's bucket is the kind of the
//! movement it corrects.

use madar_till::report::{Leg, Method, Movement, Refund, Sale};
use sqlx::PgConnection;
use uuid::Uuid;

use super::handlers::CashMovementSummaryRow;

/// Everything the fold reads about one till.
pub struct TillRows {
    pub opening: i64,
    /// The drawer figure frozen at close, once the till is closed.
    pub closing_cash_system: Option<i64>,
    pub sales: Vec<Sale>,
    /// The drawer movements as the report lists them (`created_at` order).
    pub movement_rows: Vec<CashMovementSummaryRow>,
    pub refunds: Vec<Refund>,
    /// The org's payment methods (the close preview's names and ids).
    pub methods: Vec<Method>,
}

impl TillRows {
    /// The movements as the fold reads them.
    pub fn moves(&self) -> Vec<Movement> {
        self.movement_rows
            .iter()
            .map(|m| Movement {
                id: m.id.to_string(),
                amount: i64::from(m.amount),
                kind: m.kind.clone(),
                corrects_id: m.corrects_id.map(|c| c.to_string()),
                corrects_kind: m.corrects_kind.clone(),
                note: m.note.clone(),
                moved_by_name: m.moved_by_name.clone(),
                created_at: m.created_at.to_rfc3339(),
            })
            .collect()
    }

    /// The drawer: `madar_till::report::system_cash`.
    pub fn system_cash(&self) -> i64 {
        madar_till::report::system_cash(self.opening, &self.sales, &self.moves(), &self.refunds)
    }

    /// The whole report: `madar_till::report::fold`.
    pub fn fold(&self) -> madar_till::report::Figures {
        madar_till::report::fold(
            self.opening,
            self.closing_cash_system,
            &self.sales,
            &self.moves(),
            &self.refunds,
            &self.methods,
        )
    }
}

/// Load `till_id`'s rows. A till that does not exist loads as an empty one
/// with no float (the aggregates read `NULL`, which callers never reached).
pub async fn load(conn: &mut PgConnection, till_id: Uuid) -> Result<TillRows, sqlx::Error> {
    let till: Option<(i32, Option<i32>, Option<Uuid>)> = sqlx::query_as(
        "SELECT t.opening_cash, t.closing_cash_system, b.org_id \
           FROM tills t LEFT JOIN branches b ON b.id = t.branch_id WHERE t.id = $1",
    )
    .bind(till_id)
    .fetch_optional(&mut *conn)
    .await?;
    let (opening, closing_cash_system, org_id) = till.unwrap_or((0, None, None));

    #[allow(clippy::type_complexity)]
    let orders: Vec<(
        Uuid,
        String,
        String,
        i64,
        i64,
        Option<String>,
        Option<bool>,
        i64,
        i64,
        bool,
        i64,
        i64,
        i64,
    )> = sqlx::query_as(
        "SELECT o.id, o.status::text, o.payment_method, o.total_amount::bigint, \
                COALESCE(o.tip_amount, 0)::bigint, o.tip_payment_method, o.tip_is_cash, \
                o.tax_amount::bigint, o.service_charge_amount::bigint, \
                o.service_charge_waived_by IS NOT NULL, o.service_charge_waived_amount::bigint, \
                COALESCE(rf.refunded_tax, 0)::bigint, COALESCE(rf.refunded_service_charge, 0)::bigint \
           FROM orders o LEFT JOIN v_order_refund_totals rf ON rf.order_id = o.id \
          WHERE o.till_id = $1 \
          ORDER BY o.created_at, o.id",
    )
    .bind(till_id)
    .fetch_all(&mut *conn)
    .await?;
    let legs: Vec<(Uuid, String, i64, bool)> = sqlx::query_as(
        "SELECT op.order_id, op.method::text, op.amount::bigint, \
                COALESCE(op.is_cash, op.method = 'cash') \
           FROM order_payments op JOIN orders o ON o.id = op.order_id \
          WHERE o.till_id = $1",
    )
    .bind(till_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut legs_of: std::collections::HashMap<Uuid, Vec<Leg>> = std::collections::HashMap::new();
    for (order, method, amount, is_cash) in legs {
        legs_of.entry(order).or_default().push(Leg {
            method,
            amount,
            is_cash,
        });
    }
    let sales = orders
        .into_iter()
        .map(
            |(
                id,
                status,
                payment_method,
                total,
                tip,
                tip_method,
                tip_is_cash,
                tax,
                service_charge,
                waived,
                waived_amount,
                refunded_tax,
                refunded_service_charge,
            )| Sale {
                key: id.to_string(),
                status,
                payment_method,
                total,
                tip,
                tip_method,
                tip_is_cash,
                legs: legs_of.remove(&id).unwrap_or_default(),
                tax,
                service_charge,
                waived,
                waived_amount,
                unsent: false,
                refunded_tax,
                refunded_service_charge,
            },
        )
        .collect();

    let movement_rows = sqlx::query_as::<_, CashMovementSummaryRow>(
        r#"SELECT m.id, m.amount, m.kind, m.corrects_id, c.kind AS corrects_kind, m.note,
                  u.name AS moved_by_name, m.created_at
           FROM till_cash_movements m JOIN users u ON u.id = m.moved_by
           LEFT JOIN till_cash_movements c ON c.id = m.corrects_id
           WHERE m.till_id = $1 ORDER BY m.created_at ASC"#,
    )
    .bind(till_id)
    .fetch_all(&mut *conn)
    .await?;

    let refunds = sqlx::query_as::<_, (i64, String, bool, i64, i64)>(
        "SELECT amount::bigint, method, is_cash, tax_amount::bigint, service_charge_amount::bigint \
           FROM order_refunds WHERE till_id = $1",
    )
    .bind(till_id)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|(amount, method, is_cash, tax, service_charge)| Refund {
        amount,
        method,
        is_cash,
        tax,
        service_charge,
    })
    .collect();

    let methods = sqlx::query_as::<_, (Uuid, String, bool, bool, chrono::DateTime<chrono::Utc>)>(
        "SELECT id, name, is_cash, is_active, created_at FROM org_payment_methods WHERE org_id = $1",
    )
    .bind(org_id)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|(id, name, is_cash, is_active, created_at)| Method {
        id: id.to_string(),
        name,
        is_cash,
        is_active,
        created_at: Some(created_at.to_rfc3339()),
    })
    .collect();

    Ok(TillRows {
        opening: i64::from(opening),
        closing_cash_system: closing_cash_system.map(i64::from),
        sales,
        movement_rows,
        refunds,
        methods,
    })
}
