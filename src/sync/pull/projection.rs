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
    format!(
        "(SELECT a.hash FROM assets a WHERE a.group_id = {group_col} AND a.variant = 'tile' ORDER BY a.created_at DESC LIMIT 1)"
    )
}

/// Run `SELECT id, <json>` and key the objects by id. The builders are `json_*`,
/// not `jsonb_*`: the value is only ever re-serialized, and skipping jsonb's
/// binary conversion is a third of the cost of a 25k-order window.
async fn by_sql(
    conn: &mut PgConnection,
    sql: &str,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, Value>, AppError> {
    let rows: Vec<(Uuid, Value)> = sqlx::query_as(sql).bind(ids).fetch_all(&mut *conn).await?;
    Ok(rows.into_iter().collect())
}

/// A teller row's grants from the new model: `permissions` (legacy cells, for
/// older tablets), `capabilities`, `limits`, `ask_manager`, `is_owner`,
/// `authz_epoch`. Additive fields; old tablets ignore them.
async fn add_capabilities(
    conn: &mut PgConnection,
    user_id: Uuid,
    branch_id: Uuid,
    v: &mut Value,
) -> Result<(), AppError> {
    let eff = crate::authz::require::effective_on(conn, user_id, Some(branch_id)).await?;
    let epoch = crate::authz::load::epoch_of(conn, user_id).await?;
    let mine = crate::authz::api::my_authz(user_id, Some(branch_id), epoch, &eff, false);
    let mut cells: Vec<String> = crate::permissions::permission_cells()
        .filter(|(r, a)| crate::authz::legacy::granted(&eff, r, a))
        .map(|(r, a)| format!("{r}:{a}"))
        .collect();
    cells.sort();
    if let Value::Object(m) = v {
        m.insert("permissions".into(), serde_json::json!(cells));
        m.insert("capabilities".into(), serde_json::json!(mine.capabilities));
        m.insert("limits".into(), serde_json::json!(mine.limits));
        m.insert("ask_manager".into(), serde_json::json!(mine.ask_manager));
        m.insert("is_owner".into(), serde_json::json!(mine.owner));
        m.insert("role_kinds".into(), serde_json::json!(mine.role_kinds));
        m.insert("authz_epoch".into(), serde_json::json!(epoch));
    }
    Ok(())
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

fn keyed<T: serde::Serialize>(
    items: impl IntoIterator<Item = T>,
    strip_keys: &[&str],
) -> HashMap<Uuid, Value> {
    items
        .into_iter()
        .filter_map(|it| {
            let mut v = serde_json::to_value(it).ok()?;
            strip(&mut v, strip_keys);
            let id = v
                .get("id")?
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok())?;
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
        "category" => {
            "EXISTS (SELECT 1 FROM categories x WHERE x.id = $ID AND sync_live_category(x))"
        }
        "menu_item" => {
            "EXISTS (SELECT 1 FROM menu_items x WHERE x.id = $ID AND sync_live_menu_item(x))"
        }
        "bundle" => "EXISTS (SELECT 1 FROM bundles x WHERE x.id = $ID AND sync_live_bundle(x))",
        "ingredient" => {
            "EXISTS (SELECT 1 FROM org_ingredients x WHERE x.id = $ID AND sync_live_ingredient(x))"
        }
        "payment_method" => "EXISTS (SELECT 1 FROM org_payment_methods x WHERE x.id = $ID)",
        "payment_availability" => {
            "(EXISTS (SELECT 1 FROM branch_payment_methods x WHERE x.branch_id = $ID) \
              OR EXISTS (SELECT 1 FROM user_payment_methods x WHERE x.user_id = $ID) \
              OR EXISTS (SELECT 1 FROM device_payment_methods x WHERE x.device_id = $ID))"
        }
        "discount" => {
            "EXISTS (SELECT 1 FROM discounts x WHERE x.id = $ID AND sync_live_discount(x))"
        }
        "branch_settings" => {
            "EXISTS (SELECT 1 FROM branches x WHERE x.id = $ID AND sync_live_branch_settings(x))"
        }
        "device" => "EXISTS (SELECT 1 FROM devices x WHERE x.id = $ID AND sync_live_device(x))",
        "teller" => "EXISTS (SELECT 1 FROM users x WHERE x.id = $ID AND sync_live_teller(x))",
        "floor_section" => "EXISTS (SELECT 1 FROM floor_sections x WHERE x.id = $ID)",
        "floor_table" => {
            "EXISTS (SELECT 1 FROM branch_tables x WHERE x.id = $ID AND sync_live_floor_table(x))"
        }
        "table_occupancy" => {
            "EXISTS (SELECT 1 FROM table_occupancies x WHERE x.id = $ID AND sync_live_table_occupancy(x))"
        }
        "table_transfer" => {
            "EXISTS (SELECT 1 FROM table_transfer_requests x WHERE x.id = $ID AND sync_live_table_transfer(x))"
        }
        "open_ticket" => {
            "EXISTS (SELECT 1 FROM open_tickets x WHERE x.id = $ID AND sync_live_open_ticket(x))"
        }
        "kitchen_ticket" => {
            "EXISTS (SELECT 1 FROM kitchen_tickets x WHERE x.id = $ID AND sync_live_kitchen_ticket(x))"
        }
        "delivery" => {
            "EXISTS (SELECT 1 FROM delivery_orders x WHERE x.id = $ID AND sync_live_delivery(x))"
        }
        "booking" => "EXISTS (SELECT 1 FROM bookings x WHERE x.id = $ID AND sync_live_booking(x))",
        "till" => "EXISTS (SELECT 1 FROM tills x WHERE x.id = $ID)",
        "cash_movement" => "EXISTS (SELECT 1 FROM till_cash_movements x WHERE x.id = $ID)",
        "order" => "EXISTS (SELECT 1 FROM orders x WHERE x.id = $ID)",
        "refund" => "EXISTS (SELECT 1 FROM order_refunds x WHERE x.id = $ID)",
        "addon_item" => {
            "EXISTS (SELECT 1 FROM addon_items x WHERE x.id = $ID AND sync_live_addon_item(x.id))"
        }
        "customer" => {
            "EXISTS (SELECT 1 FROM customers x WHERE x.id = $ID AND sync_live_customer(x))"
        }
        // A recorded staff drink is an immutable audit row: once it exists it
        // is live, and it is never edited or withdrawn. Nothing to age out.
        "staff_drink" => "EXISTS (SELECT 1 FROM staff_drinks x WHERE x.id = $ID)",
        _ => return None,
    })
}

/// The ids of `ids` that are in `ty`'s projected set (see [`projects_sql`]).
async fn projectable(
    conn: &mut PgConnection,
    ty: &str,
    ids: &[Uuid],
) -> Result<Vec<Uuid>, AppError> {
    let pred = projects_sql(ty)
        .ok_or_else(|| AppError::BadRequest(format!("Unknown sync type `{ty}`")))?;
    let sql = format!(
        "SELECT i FROM unnest($1::uuid[]) AS u(i) WHERE {}",
        pred.replace("$ID", "u.i")
    );
    Ok(sqlx::query_scalar(&sql)
        .bind(ids)
        .fetch_all(&mut *conn)
        .await?)
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
                        'is_active', c.is_active, 'display_order', c.display_order, 'image_hash', {}) \
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
            // Per-channel prices: only where a delivery channel resolves a size or an
            // option differently from the in-store price above (TILLS_VERIFICATION gap).
            let mut channel_prices = crate::menu::catalog_sync::sync_channel_prices_by_ids(&mut *conn, org_id, branch_id, ids).await?;
            for (id, v) in out.iter_mut() {
                if let Value::Object(m) = v {
                    m.insert("channel_prices".into(), channel_prices.remove(id).unwrap_or_else(|| json!({})));
                }
            }
            out
        }
        "addon_item" => crate::menu::handlers::addon_items_by_ids(&mut *conn, org_id, branch_id, ids).await?,
        // A till searches by name or phone offline. Notes stay in the
        // dashboard; the phone is shown only to `customers.view` holders.
        "customer" => {
            by_sql(
                conn,
                "SELECT c.id, json_build_object('id', c.id, 'name', c.name, 'phone', c.phone, \
                        'phone_key', c.phone_key, 'loyalty_customer_id', c.loyalty_customer_id, \
                        'updated_at', c.updated_at) \
                   FROM customers c WHERE c.id = ANY($1)",
                ids,
            )
            .await?
        }
        // The till counts its branch's pool from these rows, so every device of
        // a branch converges on the same day's count over the cloud exactly as
        // it does over the LAN. `business_date` is the branch's business day,
        // already resolved server-side — the till must never re-derive it from
        // `recorded_at` and a timezone it might not have.
        "staff_drink" => {
            by_sql(
                conn,
                "SELECT s.id, json_build_object('id', s.id, 'branch_id', s.branch_id, \
                        'business_date', s.business_date, 'quantity', s.quantity, \
                        'menu_item_id', s.menu_item_id, 'item_name', s.item_name, \
                        'size_label', s.size_label, 'note', s.note, \
                        'overspent', s.overspent, 'overspent_on_replay', s.overspent_on_replay, \
                        'cost_minor', s.cost_minor, 'order_id', s.order_id, \
                        'recorded_at', s.recorded_at, 'updated_at', s.updated_at) \
                   FROM staff_drinks s WHERE s.id = ANY($1)",
                ids,
            )
            .await?
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
                "SELECT i.id, json_build_object('id', i.id, 'name', i.name, 'unit', i.unit::text, 'is_active', i.is_active, \
                        'cost_per_unit', i.cost_per_unit::float8) \
                   FROM org_ingredients i WHERE i.id = ANY($1) AND i.deleted_at IS NULL",
                ids,
            )
            .await?
        }
        "payment_method" => {
            by_sql(
                conn,
                "SELECT p.id, json_build_object('id', p.id, 'name', p.name, 'label_translations', p.label_translations, \
                        'color', p.color, 'icon', p.icon, 'is_cash', p.is_cash, 'is_active', p.is_active, \
                        'created_at', p.created_at) \
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
                        'old_bill_hours', b.old_bill_hours, 'standard_float', b.standard_float, 'logo_hash', {}, \
                        'delivery_prep_minutes', COALESCE((SELECT d.prep_time_minutes FROM branch_delivery_settings d WHERE d.branch_id = b.id), 20), \
                        'kitchen_routing_effective', COALESCE(b.kitchen_routing_mode::text, \
                            CASE WHEN EXISTS (SELECT 1 FROM kitchen_stations ks WHERE ks.branch_id = b.id AND ks.is_active AND ks.deleted_at IS NULL) \
                                 THEN 'kds' ELSE 'till' END), \
                        'kitchen_stations', COALESCE((SELECT json_agg(json_build_object('id', ks.id, 'org_id', ks.org_id, 'branch_id', ks.branch_id, \
                                'name', ks.name, 'name_translations', ks.name_translations, 'sort_order', ks.sort_order, \
                                'printer_brand', ks.printer_brand, 'printer_ip', ks.printer_ip, 'printer_port', ks.printer_port, \
                                'is_default', ks.is_default, 'is_active', ks.is_active, 'created_at', ks.created_at, 'updated_at', ks.updated_at) \
                                ORDER BY ks.sort_order, lower(ks.name), ks.id) \
                              FROM kitchen_stations ks WHERE ks.branch_id = b.id AND ks.deleted_at IS NULL), '[]'::json), \
                        'delivery', (SELECT json_build_object('branch_id', d.branch_id, 'in_mall_enabled', d.in_mall_enabled, \
                                'outside_enabled', d.outside_enabled, 'in_mall_override', d.in_mall_override, 'outside_override', d.outside_override, \
                                'in_mall_fee', d.in_mall_fee, 'prep_time_minutes', d.prep_time_minutes, \
                                'umbrella_enabled', d.umbrella_enabled, 'pickup_enabled', d.pickup_enabled, \
                                'umbrella_override', d.umbrella_override, 'pickup_override', d.pickup_override) \
                              FROM branch_delivery_settings d WHERE d.branch_id = b.id), \
                        'tax_policy', json_build_object('tax_rate', COALESCE(b.tax_rate, o.tax_rate), \
                                'tax_inclusive', COALESCE(b.tax_inclusive, o.tax_inclusive), \
                                'service_charge_rate', COALESCE(b.service_charge_rate, o.service_charge_rate), \
                                'service_charge_taxable', COALESCE(b.service_charge_taxable, o.service_charge_taxable)), \
                        'org_require_table_for_orders', o.require_table_for_orders, \
                        'loyalty', (SELECT json_build_object('enabled', l.enabled, 'mode', l.mode, \
                                'program_name', l.program_name, 'program_name_ar', l.program_name_ar) \
                              FROM loyalty_settings l WHERE l.org_id = b.org_id AND (l.branch_id = b.id OR l.branch_id IS NULL) \
                             ORDER BY l.branch_id NULLS LAST LIMIT 1), \
                        'staff_pool', (SELECT json_build_object('enabled', sp.enabled, \
                                'daily_allowance', sp.daily_allowance, \
                                'eligible_item_ids', sp.eligible_item_ids) \
                              FROM staff_pool_settings sp WHERE sp.org_id = b.org_id AND (sp.branch_id = b.id OR sp.branch_id IS NULL) \
                             ORDER BY sp.branch_id NULLS LAST LIMIT 1)) \
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
            let mut rows =
            // `permissions`: the person's EFFECTIVE granted `resource:action` pairs
            // (user override → role default), granted only, the same resolution as
            // `GET /auth/permissions` — so a grant or a revocation reaches a till
            // through the feed instead of at the next online sign-in.
            by_sql(
                conn,
                "SELECT u.id, json_build_object('id', u.id, 'user_id', u.id, 'name', u.name, 'role', u.role::text, \
                        'is_active', u.is_active, \
                        'permissions', COALESCE((SELECT json_agg(g.p ORDER BY g.p) FROM ( \
                            SELECT rp.resource::text || ':' || rp.action::text AS p \
                              FROM role_permissions rp \
                             WHERE rp.role = u.role \
                               AND COALESCE((SELECT pm.granted FROM permissions pm WHERE pm.user_id = u.id \
                                              AND pm.resource = rp.resource AND pm.action = rp.action), rp.granted) \
                            UNION \
                            SELECT pm.resource::text || ':' || pm.action::text \
                              FROM permissions pm WHERE pm.user_id = u.id AND pm.granted) g), '[]'::json)) \
                   FROM users u WHERE u.id = ANY($1) AND u.deleted_at IS NULL",
                ids,
            )
            .await?;
            // Architecture E: overwrite with the person's effective grants AT THIS
            // BRANCH, and add the capability view newer tills gate on.
            for (id, v) in rows.iter_mut() {
                add_capabilities(conn, *id, branch_id, v).await?;
            }
            rows
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
            let mut out = keyed(
                rows,
                // The opening-cash edit and the discrepancy stay: the till's Z report is
                // computed on the device from these rows (offline plan B).
                &["branch_name", "closed_by", "force_closed_by", "force_close_reason", "notes", "flagged_at"],
            );
            // The per-method close reconciliation, for the Z report the device prints.
            let mut lines = crate::tills::reconcile::stored_lines_by_till(&mut *conn, ids).await?;
            // Who viewed / printed the spot report, for the Z report and the ledger rows.
            let mut checks = crate::tills::spot_views::spot_views_by_till(&mut *conn, ids).await?;
            for (id, v) in out.iter_mut() {
                if let Value::Object(m) = v {
                    m.insert(
                        "spot_views".into(),
                        serde_json::to_value(checks.remove(id).unwrap_or_default()).unwrap_or_else(|_| json!([])),
                    );
                    m.insert(
                        "reconciliation".into(),
                        serde_json::to_value(lines.remove(id).unwrap_or_default()).unwrap_or_else(|_| json!([])),
                    );
                }
            }
            out
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
            // Everything a till reads off a sale — the history row, the drawer
            // computation (payment legs with `is_cash`, `tip_is_cash`), and the
            // receipt reprint (`items` with their modifiers) — in the `OrderFull`
            // shape. Never cost/COGS: no deductions snapshot, no line or unit cost.
            let mut out = by_sql(
                conn,
                "SELECT o.id, json_build_object('id', o.id, 'branch_id', o.branch_id, 'till_id', o.till_id, \
                        'shift_id', o.till_id, \
                        'teller_id', o.teller_id, 'teller_name', u.name, 'waiter_id', o.waiter_id, 'waiter_name', w.name, \
                        'order_number', o.order_number, 'device_code', o.device_code, 'device_id', o.device_id, \
                        'display_number', CASE WHEN o.device_code IS NOT NULL THEN o.device_code || '-' || o.order_number \
                                               ELSE o.order_number::text END, \
                        'verification', o.verification, \
                        'order_ref', o.order_ref, 'idempotency_key', o.idempotency_key, 'status', o.status::text, \
                        'order_type', o.order_type, 'open_ticket_id', o.open_ticket_id, 'table_id', o.table_id, \
                        'subtotal', o.subtotal, 'discount_type', o.discount_type::text, \
                        'discount_value', (CASE WHEN o.discount_value > 0 AND o.discount_value <= 1 \
                                                THEN round(o.discount_value * 100) ELSE round(o.discount_value) END)::bigint, \
                        'discount_rate', o.discount_value, 'discount_id', o.discount_id, \
                        'discount_amount', o.discount_amount, 'tax_amount', o.tax_amount, \
                        'service_charge_amount', o.service_charge_amount, 'delivery_fee', o.delivery_fee, \
                        'total_amount', o.total_amount, 'amount_tendered', o.amount_tendered, 'change_given', o.change_given, \
                        'tip_amount', o.tip_amount, 'tip_payment_method', o.tip_payment_method, 'tip_is_cash', o.tip_is_cash, \
                        'payment_method', o.payment_method::text, \
                        'payment_legs', COALESCE((SELECT json_agg(json_build_object('method', p.method, 'amount', p.amount, \
                                                   'is_cash', p.is_cash) ORDER BY p.id) FROM order_payments p WHERE p.order_id = o.id), '[]'::json), \
                        'customer_name', o.customer_name, 'notes', o.notes, \
                        'delivery_order_id', o.delivery_order_id, \
                        'voided_at', o.voided_at, 'void_reason', o.void_reason::text, 'void_note', o.void_note, 'voided_by', o.voided_by, \
                        'price_flagged', o.price_flagged, \
                        'loyalty_customer_id', o.loyalty_customer_id, 'loyalty_member_name', lc.name, \
                        'timezone', effective_timezone(o.branch_id), \
                        'created_at', o.created_at) \
                   FROM orders o LEFT JOIN users u ON u.id = o.teller_id LEFT JOIN users w ON w.id = o.waiter_id \
                   LEFT JOIN loyalty_customers lc ON lc.id = o.loyalty_customer_id \
                  WHERE o.id = ANY($1)",
                ids,
            )
            .await?;
            // The policy the bill was priced under and its service-charge
            // waiver. A second object because `json_build_object` takes at most
            // 100 arguments and the one above is at its limit.
            let pricing: Vec<(Uuid, Value)> = sqlx::query_as(
                "SELECT o.id, json_build_object('tax_inclusive', o.tax_inclusive, \
                        'tax_rate_applied', o.tax_rate_applied, \
                        'service_charge_rate_applied', o.service_charge_rate_applied, \
                        'service_charge_taxable_applied', o.service_charge_taxable_applied, \
                        'service_charge_waived_by', o.service_charge_waived_by, \
                        'service_charge_waived_by_name', sw.name, \
                        'service_charge_waived_at', o.service_charge_waived_at, \
                        'service_charge_waived_amount', o.service_charge_waived_amount, \
                        'discount_kind', o.discount_kind, 'discount_percent_bps', o.discount_percent_bps, \
                        'discount_applied_by', o.discount_applied_by, 'discount_approval_id', o.discount_approval_id) \
                   FROM orders o LEFT JOIN users sw ON sw.id = o.service_charge_waived_by \
                  WHERE o.id = ANY($1)",
            )
            .bind(ids)
            .fetch_all(&mut *conn)
            .await?;
            let mut pricing: HashMap<Uuid, Value> = pricing.into_iter().collect();
            for (id, v) in out.iter_mut() {
                if let (Value::Object(m), Some(Value::Object(extra))) = (v, pricing.remove(id)) {
                    m.extend(extra);
                }
            }
            let mut items = crate::orders::handlers::fetch_orders_items_full_batch_on(&mut *conn, ids).await?;
            for (id, v) in out.iter_mut() {
                let mut lines = serde_json::to_value(items.remove(id).unwrap_or_default()).unwrap_or_else(|_| json!([]));
                for key in ["deductions_snapshot", "line_cost", "unit_cost", "cost_missing", "cost", "quantity_deducted",
                            "org_ingredient_id", "ingredient_name", "ingredient_unit"] {
                    strip_deep(&mut lines, key);
                }
                if let Value::Object(m) = v {
                    m.insert("items".into(), lines);
                }
            }
            out
        }
        "refund" => {
            let mut out = by_sql(
                conn,
                "SELECT r.id, json_build_object('id', r.id, 'branch_id', r.branch_id, 'order_id', r.order_id, \
                        'till_id', r.till_id, 'shift_id', r.till_id, 'amount', r.amount, \
                        'method', r.method, 'is_cash', r.is_cash, 'reason', r.reason, 'note', r.note, \
                        'issued_by', r.issued_by, 'issued_by_name', (SELECT name FROM users WHERE id = r.issued_by), \
                        'issued_at', r.issued_at, 'client_ref', r.client_ref, 'created_at', r.created_at, \
                        'tax_amount', r.tax_amount, 'service_charge_amount', r.service_charge_amount) \
                   FROM order_refunds r WHERE r.id = ANY($1)",
                ids,
            )
            .await?;
            let lines: Vec<(Uuid, Value)> = sqlx::query_as(
                "SELECT l.refund_id, json_agg(json_build_object('id', l.id, 'order_item_id', l.order_item_id, \
                        'item_name', i.item_name, 'quantity', l.quantity, 'amount', l.amount, 'restock', l.restock) ORDER BY l.id) \
                   FROM order_refund_lines l JOIN order_items i ON i.id = l.order_item_id \
                  WHERE l.refund_id = ANY($1) GROUP BY l.refund_id",
            )
            .bind(ids)
            .fetch_all(&mut *conn)
            .await?;
            let mut by_refund: HashMap<Uuid, Value> = lines.into_iter().collect();
            for (id, v) in out.iter_mut() {
                if let Value::Object(m) = v {
                    m.insert("lines".into(), by_refund.remove(id).unwrap_or_else(|| json!([])));
                }
            }
            out
        }
        other => return Err(AppError::BadRequest(format!("Unknown sync type `{other}`"))),
    })
}

#[cfg(test)]
mod projection_gate_tests {
    /// Every wire type the POS may ask for must have a projection gate.
    ///
    /// `project()` refuses an unknown type with `Unknown sync type`, so a type
    /// added to `ALL_TYPES` (and to `sync_source_tables()`, and given a feed
    /// trigger) but NOT given an arm here fails only at runtime, on the pull —
    /// the rows are emitted and then never delivered. `staff_drink` shipped
    /// exactly that way and was caught by a feature test rather than here.
    ///
    /// The existing `.expect("state type has a projection gate")` in
    /// `sync::pull` covers state types only, which is why a LEDGER type slipped
    /// through; this covers every type either way.
    #[test]
    fn every_wire_type_has_a_projection_gate() {
        for ty in crate::sync::pull::ALL_TYPES {
            assert!(
                super::projects_sql(ty).is_some(),
                "`{ty}` is in ALL_TYPES but has no arm in projects_sql, so a pull for it 400s"
            );
        }
    }
}
