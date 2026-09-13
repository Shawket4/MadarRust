//! Lean per-type projections for `/sync/pull` (§10.2 R-data, TILLS_PAYLOAD_AUDIT).
//!
//! `project` returns the CURRENT POS-facing JSON of each requested entity that
//! still exists; an id missing from the result is sent as a `delete`. Every
//! object carries `id`. Nothing here serializes cost, audit or dashboard-only
//! fields (no `deductions_snapshot`, recipe cost, `org_id`, `created_by`, …), and
//! images travel as the `tile` variant hash, never a URL.
use std::collections::HashMap;

use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::errors::AppError;

/// `tile` hash of an asset group column (`<alias>.<col>`).
fn tile_hash(group_col: &str) -> String {
    format!("(SELECT a.hash FROM assets a WHERE a.group_id = {group_col} AND a.variant = 'tile' ORDER BY a.created_at DESC LIMIT 1)")
}

/// Run `SELECT id, <json>` and key the objects by id. The builders are `json_*`,
/// not `jsonb_*`: the value is only ever re-serialized, and skipping jsonb's
/// binary conversion is a third of the cost of a 25k-order window.
async fn by_sql(conn: &mut PgConnection, sql: &str, ids: &[Uuid]) -> Result<HashMap<Uuid, Value>, AppError> {
    let rows: Vec<(Uuid, Value)> = sqlx::query_as(sql).bind(ids).fetch_all(&mut *conn).await?;
    Ok(rows.into_iter().collect())
}

fn strip(v: &mut Value, keys: &[&str]) {
    if let Value::Object(m) = v {
        for k in keys {
            m.remove(*k);
        }
    }
}

/// Remove `key` at every depth.
fn strip_deep(v: &mut Value, key: &str) {
    match v {
        Value::Object(m) => {
            m.remove(key);
            m.values_mut().for_each(|x| strip_deep(x, key));
        }
        Value::Array(a) => a.iter_mut().for_each(|x| strip_deep(x, key)),
        _ => {}
    }
}

fn keyed<T: serde::Serialize>(items: impl IntoIterator<Item = T>, strip_keys: &[&str]) -> HashMap<Uuid, Value> {
    items
        .into_iter()
        .filter_map(|it| {
            let mut v = serde_json::to_value(it).ok()?;
            strip(&mut v, strip_keys);
            let id = v.get("id")?.as_str().and_then(|s| Uuid::parse_str(s).ok())?;
            Some((id, v))
        })
        .collect()
}

/// The SQL predicate (over an entity id expression `$ID`) that says whether an
/// entity of `ty` is in its type's projected set right now.
///
/// It is the ONE definition of "projects": [`project`] keeps only the ids that
/// pass it (every loader below then returns every id it is given), and the
/// per-type checksums count exactly the feed rows whose entity passes it. So a
/// type's checksum set is its projected set by construction, even where a feed
/// `upsert` row has gone stale (a time-based live rule that has aged out before
/// the sweeper emitted its delete).
pub fn projects_sql(ty: &str) -> Option<&'static str> {
    Some(match ty {
        "category" => "EXISTS (SELECT 1 FROM categories x WHERE x.id = $ID AND sync_live_category(x))",
        "menu_item" => "EXISTS (SELECT 1 FROM menu_items x WHERE x.id = $ID AND sync_live_menu_item(x))",
        "bundle" => "EXISTS (SELECT 1 FROM bundles x WHERE x.id = $ID AND sync_live_bundle(x))",
        "ingredient" => "EXISTS (SELECT 1 FROM org_ingredients x WHERE x.id = $ID AND sync_live_ingredient(x))",
        "payment_method" => "EXISTS (SELECT 1 FROM org_payment_methods x WHERE x.id = $ID)",
        "payment_availability" => {
            "(EXISTS (SELECT 1 FROM branch_payment_methods x WHERE x.branch_id = $ID) \
              OR EXISTS (SELECT 1 FROM user_payment_methods x WHERE x.user_id = $ID) \
              OR EXISTS (SELECT 1 FROM device_payment_methods x WHERE x.device_id = $ID))"
        }
        "discount" => "EXISTS (SELECT 1 FROM discounts x WHERE x.id = $ID AND sync_live_discount(x))",
        "branch_settings" => "EXISTS (SELECT 1 FROM branches x WHERE x.id = $ID AND sync_live_branch_settings(x))",
        "device" => "EXISTS (SELECT 1 FROM devices x WHERE x.id = $ID AND sync_live_device(x))",
        "teller" => "EXISTS (SELECT 1 FROM users x WHERE x.id = $ID AND sync_live_teller(x))",
        "floor_section" => "EXISTS (SELECT 1 FROM floor_sections x WHERE x.id = $ID)",
        "floor_table" => "EXISTS (SELECT 1 FROM branch_tables x WHERE x.id = $ID AND sync_live_floor_table(x))",
        "table_occupancy" => {
            "EXISTS (SELECT 1 FROM table_occupancies x WHERE x.id = $ID AND sync_live_table_occupancy(x))"
        }
        "table_transfer" => {
            "EXISTS (SELECT 1 FROM table_transfer_requests x WHERE x.id = $ID AND sync_live_table_transfer(x))"
        }
        "open_ticket" => "EXISTS (SELECT 1 FROM open_tickets x WHERE x.id = $ID AND sync_live_open_ticket(x))",
        "kitchen_ticket" => {
            "EXISTS (SELECT 1 FROM kitchen_tickets x WHERE x.id = $ID AND sync_live_kitchen_ticket(x))"
        }
        "delivery" => "EXISTS (SELECT 1 FROM delivery_orders x WHERE x.id = $ID AND sync_live_delivery(x))",
        "booking" => "EXISTS (SELECT 1 FROM bookings x WHERE x.id = $ID AND sync_live_booking(x))",
        "till" => "EXISTS (SELECT 1 FROM tills x WHERE x.id = $ID)",
        "cash_movement" => "EXISTS (SELECT 1 FROM till_cash_movements x WHERE x.id = $ID)",
        "order" => "EXISTS (SELECT 1 FROM orders x WHERE x.id = $ID)",
        "refund" => "EXISTS (SELECT 1 FROM order_refunds x WHERE x.id = $ID)",
        _ => return None,
    })
}

/// The ids of `ids` that are in `ty`'s projected set (see [`projects_sql`]).
async fn projectable(conn: &mut PgConnection, ty: &str, ids: &[Uuid]) -> Result<Vec<Uuid>, AppError> {
    let pred = projects_sql(ty).ok_or_else(|| AppError::BadRequest(format!("Unknown sync type `{ty}`")))?;
    let sql = format!("SELECT i FROM unnest($1::uuid[]) AS u(i) WHERE {}", pred.replace("$ID", "u.i"));
    Ok(sqlx::query_scalar(&sql).bind(ids).fetch_all(&mut *conn).await?)
}

/// The CURRENT projection of every id of `ids` that is in `ty`'s projected set,
/// all on `conn` (one connection per pull, never a second one from the pool) and
/// in a fixed number of queries per type.
pub async fn project(
    conn: &mut PgConnection,
    org_id: Uuid,
    branch_id: Uuid,
    ty: &str,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, Value>, AppError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let ids = projectable(conn, ty, ids).await?;
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let ids = ids.as_slice();
    Ok(match ty {
        "category" => {
            let sql = format!(
                "SELECT c.id, json_build_object('id', c.id, 'name', c.name, 'name_translations', c.name_translations, \
                        'is_active', c.is_active, 'image_hash', {}) \
                   FROM categories c WHERE c.id = ANY($1) AND c.deleted_at IS NULL AND c.is_active",
                tile_hash("c.image_group_id")
            );
            by_sql(conn, &sql, ids).await?
        }
        "menu_item" => {
            let items = crate::menu::catalog_sync::sync_items_by_ids(&mut *conn, org_id, branch_id, ids).await?;
            let mut out = keyed(items, &[]);
            // Option recipes are the preview's business, not the till's (35% of the
            // old catalog body).
            for v in out.values_mut() {
                if let Some(groups) = v.get_mut("modifier_groups").and_then(Value::as_array_mut) {
                    for g in groups {
                        if let Some(opts) = g.get_mut("options").and_then(Value::as_array_mut) {
                            opts.iter_mut().for_each(|o| strip(o, &["recipe"]));
                        }
                    }
                }
            }
            let hashes: Vec<(Uuid, Option<String>)> = sqlx::query_as(&format!(
                "SELECT m.id, {} FROM menu_items m WHERE m.id = ANY($1)",
                tile_hash("m.image_group_id")
            ))
            .bind(ids)
            .fetch_all(&mut *conn)
            .await?;
            for (id, h) in hashes {
                if let Some(Value::Object(m)) = out.get_mut(&id) {
                    m.insert("image_hash".into(), json!(h));
                }
            }
            out
        }
        "bundle" => {
            let mut out = keyed(
                crate::bundles::handlers::fetch_bundles_full(&mut *conn, ids).await?,
                &["org_id", "created_at", "updated_at", "created_by", "image_url"],
            );
            let hashes: Vec<(Uuid, Option<String>)> = sqlx::query_as(&format!(
                "SELECT b.id, {} FROM bundles b WHERE b.id = ANY($1)",
                tile_hash("b.image_group_id")
            ))
            .bind(ids)
            .fetch_all(&mut *conn)
            .await?;
            for (id, h) in hashes {
                if let Some(Value::Object(m)) = out.get_mut(&id) {
                    m.insert("image_hash".into(), json!(h));
                }
            }
            out
        }
        "ingredient" => {
            by_sql(
                conn,
                "SELECT i.id, json_build_object('id', i.id, 'name', i.name, 'unit', i.unit::text, 'is_active', i.is_active) \
                   FROM org_ingredients i WHERE i.id = ANY($1) AND i.deleted_at IS NULL",
                ids,
            )
            .await?
        }
        "payment_method" => {
            by_sql(
                conn,
                "SELECT p.id, json_build_object('id', p.id, 'name', p.name, 'label_translations', p.label_translations, \
                        'color', p.color, 'icon', p.icon, 'is_cash', p.is_cash, 'is_active', p.is_active) \
                   FROM org_payment_methods p WHERE p.id = ANY($1)",
                ids,
            )
            .await?
        }
        "payment_availability" => {
            let rows: Vec<(Uuid, String, Vec<Uuid>)> = sqlx::query_as(
                "SELECT owner, scope, array_agg(pm ORDER BY pm) FROM ( \
                    SELECT branch_id AS owner, 'branch' AS scope, payment_method_id AS pm FROM branch_payment_methods WHERE branch_id = ANY($1) \
                    UNION ALL SELECT user_id, 'user', payment_method_id FROM user_payment_methods WHERE user_id = ANY($1) \
                    UNION ALL SELECT device_id, 'device', payment_method_id FROM device_payment_methods WHERE device_id = ANY($1) \
                 ) x GROUP BY owner, scope",
            )
            .bind(ids)
            .fetch_all(&mut *conn)
            .await?;
            rows.into_iter()
                .map(|(id, scope, pms)| (id, json!({ "id": id, "scope": scope, "payment_method_ids": pms })))
                .collect()
        }
        "discount" => {
            by_sql(
                conn,
                "SELECT d.id, json_build_object('id', d.id, 'name', d.name, 'name_translations', d.name_translations, \
                        'type', d.type::text, 'value', d.value, 'is_active', d.is_active) \
                   FROM discounts d WHERE d.id = ANY($1)",
                ids,
            )
            .await?
        }
        "branch_settings" => {
            let sql = format!(
                "SELECT b.id, json_build_object('id', b.id, 'name', b.name, 'code', b.code, \
                        'timezone', effective_timezone(b.id), 'tax_rate', b.tax_rate, 'tax_inclusive', b.tax_inclusive, \
                        'service_charge_rate', b.service_charge_rate, 'service_charge_taxable', b.service_charge_taxable, \
                        'require_table_for_orders', b.require_table_for_orders, 'kitchen_routing_mode', b.kitchen_routing_mode, \
                        'old_bill_hours', b.old_bill_hours, 'standard_float', b.standard_float, 'logo_hash', {}) \
                   FROM branches b JOIN organizations o ON o.id = b.org_id \
                  WHERE b.id = ANY($1) AND b.deleted_at IS NULL",
                tile_hash("o.logo_group_id")
            );
            by_sql(conn, &sql, ids).await?
        }
        "device" => {
            by_sql(
                conn,
                "SELECT d.id, json_build_object('id', d.id, 'code', d.code, 'label', d.label, 'kind', d.kind) \
                   FROM devices d WHERE d.id = ANY($1) AND d.retired_at IS NULL",
                ids,
            )
            .await?
        }
        "teller" => {
            by_sql(
                conn,
                "SELECT u.id, json_build_object('id', u.id, 'user_id', u.id, 'name', u.name, 'role', u.role::text, \
                        'is_active', u.is_active, 'offline_pin_hash', u.offline_pin_hash) \
                   FROM users u WHERE u.id = ANY($1) AND u.deleted_at IS NULL",
                ids,
            )
            .await?
        }
        "floor_section" => {
            by_sql(
                conn,
                "SELECT s.id, json_build_object('id', s.id, 'branch_id', s.branch_id, 'name', s.name, 'ordering', s.ordering, \
                        'canvas_w', s.canvas_w, 'canvas_h', s.canvas_h) \
                   FROM floor_sections s WHERE s.id = ANY($1)",
                ids,
            )
            .await?
        }
        "floor_table" => keyed(
            crate::reservations::floor::tables_by_ids(&mut *conn, ids).await?,
            &["org_id", "created_at", "updated_at"],
        ),
        "table_occupancy" => {
            by_sql(
                conn,
                "SELECT o.id, json_build_object('id', o.id, 'table_id', o.table_id, 'held_by', o.held_by, \
                        'open_ticket_id', o.open_ticket_id, 'booking_id', o.booking_id, 'party_size', o.party_size, \
                        'started_at', o.started_at, 'started_by', o.started_by, 'started_till_id', o.started_till_id, \
                        'seated_at', o.seated_at, 'ended_at', o.ended_at, 'end_reason', o.end_reason, \
                        'needs_bussing', o.needs_bussing, 'cleared_at', o.cleared_at) \
                   FROM table_occupancies o WHERE o.id = ANY($1)",
                ids,
            )
            .await?
        }
        "table_transfer" => {
keyed(crate::floor_ops::transfer_views(&mut *conn, ids).await?, &["org_id"])
        }
        "open_ticket" => {
            let mut out = keyed(crate::tickets::open_ticket_views_on(&mut *conn, ids).await?, &["org_id"]);
            // The line's `input` echo is a third of the old body and never read.
            out.values_mut().for_each(|v| strip_deep(v, "input"));
            out
        }
        "kitchen_ticket" => {
keyed(crate::kitchen::kitchen_ticket_views(&mut *conn, ids).await?, &["org_id"])
        }
        "delivery" => {
            let mut out = keyed(
                crate::delivery::staff::fetch_delivery_orders(&mut *conn, ids).await?,
                &["org_id", "updated_at"],
            );
            out.values_mut().for_each(|v| strip_deep(v, "name_translations"));
            out
        }
        "booking" => keyed(crate::bookings::model::views_by_ids(&mut *conn, ids).await?, &["manage_token"]),
        "till" => {
            let rows: Vec<crate::tills::handlers::Till> = sqlx::query_as(&format!(
                "SELECT {} {} WHERE s.id = ANY($1)",
                crate::tills::handlers::TILL_COLUMNS,
                crate::tills::handlers::TILL_FROM
            ))
            .bind(ids)
            .fetch_all(&mut *conn)
            .await?;
            keyed(
                rows,
                &[
                    "branch_name", "opening_cash_original", "opening_cash_was_edited", "opening_cash_edit_reason",
                    "cash_discrepancy", "closed_by", "force_closed_by", "force_close_reason", "notes", "timezone",
                    "flagged_at",
                ],
            )
        }
        "cash_movement" => {
            by_sql(
                conn,
                "SELECT m.id, json_build_object('id', m.id, 'till_id', m.till_id, 'amount', m.amount, 'kind', m.kind, \
                        'corrects_id', m.corrects_id, 'note', m.note, 'moved_by', m.moved_by, \
                        'moved_by_name', (SELECT name FROM users WHERE id = m.moved_by), 'created_at', m.created_at, \
                        'client_ref', m.client_ref, 'device_id', m.device_id) \
                   FROM till_cash_movements m WHERE m.id = ANY($1)",
                ids,
            )
            .await?
        }
        "order" => {
            by_sql(
                conn,
                "SELECT o.id, json_build_object('id', o.id, 'branch_id', o.branch_id, 'till_id', o.till_id, \
                        'teller_id', o.teller_id, 'teller_name', u.name, 'waiter_id', o.waiter_id, \
                        'order_number', o.order_number, 'device_code', o.device_code, \
                        'display_number', CASE WHEN o.device_code IS NOT NULL THEN o.device_code || '-' || o.order_number \
                                               ELSE o.order_number::text END, \
                        'order_ref', o.order_ref, 'status', o.status::text, 'order_type', o.order_type, \
                        'subtotal', o.subtotal, 'discount_amount', o.discount_amount, 'tax_amount', o.tax_amount, \
                        'service_charge_amount', o.service_charge_amount, 'delivery_fee', o.delivery_fee, \
                        'total_amount', o.total_amount, 'tip_amount', o.tip_amount, 'tip_payment_method', o.tip_payment_method, \
                        'payment_method', o.payment_method::text, \
                        'payment_legs', COALESCE((SELECT json_agg(json_build_object('method', p.method, 'amount', p.amount, \
                                                   'is_cash', p.is_cash) ORDER BY p.id) FROM order_payments p WHERE p.order_id = o.id), '[]'::json), \
                        'open_ticket_id', o.open_ticket_id, 'table_id', o.table_id, 'customer_name', o.customer_name, \
                        'created_at', o.created_at, 'voided_at', o.voided_at, 'idempotency_key', o.idempotency_key, \
                        'device_id', o.device_id) \
                   FROM orders o LEFT JOIN users u ON u.id = o.teller_id WHERE o.id = ANY($1)",
                ids,
            )
            .await?
        }
        "refund" => {
            by_sql(
                conn,
                "SELECT r.id, json_build_object('id', r.id, 'order_id', r.order_id, 'till_id', r.till_id, 'amount', r.amount, \
                        'method', r.method, 'is_cash', r.is_cash, 'reason', r.reason, 'note', r.note, \
                        'issued_by', r.issued_by, 'issued_at', r.issued_at, 'client_ref', r.client_ref) \
                   FROM order_refunds r WHERE r.id = ANY($1)",
                ids,
            )
            .await?
        }
        other => return Err(AppError::BadRequest(format!("Unknown sync type `{other}`"))),
    })
}
