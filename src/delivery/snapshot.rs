//! The frozen snapshot — single source of truth for a delivery order's money,
//! cost, and inventory.
//!
//! * [`resolve_cart`] runs at **intake**: it server-prices each line using the
//!   org→branch→channel resolution chain, rolls up COGS at the current ingredient
//!   costs, and produces a [`CartSnapshot`] (priced lines) + a flat list of
//!   [`SnapshotDeduction`]s (the inventory plan). Both are stored verbatim on the
//!   `delivery_orders` row.
//! * [`apply_snapshot`] runs at **finalize**: it replays the frozen snapshot to
//!   insert a normal completed `orders` row — order lines, addons, optionals,
//!   payment, inventory movements (`record_movement`), and a minted `order_ref` —
//!   WITHOUT re-pricing or re-resolving anything. Menu/recipe/override edits made
//!   between intake and finalize cannot leak in.
//! * [`record_waste`] runs on **cancel with restore=false**: the food was made
//!   but not delivered, so the frozen plan is deducted from stock and logged as a
//!   `waste` movement.
//!
//! Bundles are intentionally not supported in delivery carts yet (the public
//! page only offers à-la-carte items); intake rejects bundle lines. Money is
//! integer piastres throughout.

use chrono::{DateTime, Utc};
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::errors::AppError;
use crate::orders::component_resolve::{AddonInput, resolve_menu_item_configuration};
use crate::orders::handlers::Order;

// ── Intake input ──────────────────────────────────────────────

/// One line of a public cart. Prices are NOT taken from the client — the server
/// resolves them. `addons` reuses [`AddonInput`] but its `unit_price` is ignored.
#[derive(Deserialize, Serialize, Clone, ToSchema)]
pub struct CartLineInput {
    pub menu_item_id: Uuid,
    #[serde(default)]
    pub size_label: Option<String>,
    pub quantity: i32,
    #[serde(default)]
    pub addons: Vec<AddonInput>,
    #[serde(default)]
    pub optional_field_ids: Vec<Uuid>,
    #[serde(default)]
    pub notes: Option<String>,
}

// ── Frozen snapshot shapes (serialised into delivery_orders.cart) ──

#[derive(Serialize, Deserialize, Clone, ToSchema)]
pub struct SnapshotAddon {
    pub addon_item_id: Uuid,
    pub addon_name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub unit_price: i32,
    pub quantity: i32,
    pub line_cost: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, ToSchema)]
pub struct SnapshotOptional {
    pub optional_field_id: Uuid,
    pub field_name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub price: i32,
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name: Option<String>,
    pub ingredient_unit: Option<String>,
    pub quantity_used: Option<f64>,
    pub cost: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, ToSchema)]
pub struct SnapshotLine {
    pub menu_item_id: Uuid,
    pub item_name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub size_label: Option<String>,
    pub unit_price: i32,
    pub quantity: i32,
    pub line_total: i32,
    pub notes: Option<String>,
    pub addons: Vec<SnapshotAddon>,
    pub optionals: Vec<SnapshotOptional>,
    pub line_cost: Option<i64>,
    pub unit_cost: Option<i64>,
    pub cost_missing: bool,
}

/// The priced line snapshot stored in `delivery_orders.cart`.
#[derive(Serialize, Deserialize, Clone, ToSchema)]
pub struct CartSnapshot {
    pub lines: Vec<SnapshotLine>,
}

/// One frozen inventory deduction. Stored (as a flat list) in
/// `delivery_orders.deductions_snapshot`; `line_index` ties each back to its
/// `cart.lines[i]` so finalize can attach per-line `deductions_snapshot` columns.
#[derive(Serialize, Deserialize, Clone, ToSchema)]
pub struct SnapshotDeduction {
    pub line_index: usize,
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name: String,
    pub unit: String,
    pub quantity: f64,
    pub source: String,
    pub category: String,
    pub addon_item_id: Option<Uuid>,
    pub optional_field_id: Option<Uuid>,
    pub cost_per_unit: Option<f64>,
    pub line_cost: Option<i64>,
}

/// Output of [`resolve_cart`].
pub struct ResolvedCart {
    pub snapshot: CartSnapshot,
    pub deductions: Vec<SnapshotDeduction>,
    /// Sum of line totals (items + addons + optionals), pre-tax, pre-fee.
    pub subtotal: i32,
}

// ── Intake: resolve + freeze ──────────────────────────────────

/// Server-price and freeze a cart for a branch + channel. Rejects unknown,
/// deleted, or channel/branch-disabled items (the public menu would not have
/// offered them). Bundles are not supported yet.
pub async fn resolve_cart(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    // The delivery channel for public/delivery orders, or `None` for dine-in
    // (POS / waiter tickets). When `None`, the `branch_channel_*_overrides`
    // joins bind `NULL::delivery_channel` and never match, so pricing/availability
    // fall back to the org→branch layer — exactly the dine-in behaviour.
    channel: Option<&str>,
    lines: &[CartLineInput],
    at: DateTime<Utc>,
) -> Result<ResolvedCart, AppError> {
    use crate::delivery::{
        MAX_ADDON_QTY, MAX_CART_LINES, MAX_LINE_NOTES_LEN, MAX_LINE_QTY, MAX_SIZE_LABEL_LEN,
    };

    if lines.is_empty() {
        return Err(AppError::BadRequest("Cart is empty".into()));
    }
    if lines.len() > MAX_CART_LINES {
        return Err(AppError::BadRequest(format!(
            "A cart may contain at most {MAX_CART_LINES} items"
        )));
    }

    let mut snapshot_lines: Vec<SnapshotLine> = Vec::new();
    let mut all_deductions: Vec<SnapshotDeduction> = Vec::new();
    let mut subtotal: i32 = 0;

    for (idx, line) in lines.iter().enumerate() {
        if line.quantity <= 0 || line.quantity > MAX_LINE_QTY {
            return Err(AppError::BadRequest(format!(
                "Item quantity must be between 1 and {MAX_LINE_QTY}"
            )));
        }
        if line
            .addons
            .iter()
            .any(|a| a.quantity <= 0 || a.quantity > MAX_ADDON_QTY)
        {
            return Err(AppError::BadRequest(format!(
                "Addon quantity must be between 1 and {MAX_ADDON_QTY}"
            )));
        }
        if let Some(notes) = &line.notes
            && notes.chars().count() > MAX_LINE_NOTES_LEN
        {
            return Err(AppError::BadRequest(format!(
                "Item notes must be at most {MAX_LINE_NOTES_LEN} characters"
            )));
        }
        if let Some(size) = &line.size_label
            && size.chars().count() > MAX_SIZE_LABEL_LEN
        {
            return Err(AppError::BadRequest("Invalid size".into()));
        }

        // Resolve org → branch → channel price + availability.
        #[allow(clippy::type_complexity)]
        let row: Option<(
            String,
            serde_json::Value,
            i32,
            Option<i32>,
            bool,
            Option<i32>,
            Option<bool>,
        )> = sqlx::query_as(
            r#"SELECT mi.name, mi.name_translations, mi.base_price,
                          bmo.price_override,
                          COALESCE(bmo.is_available, true) AS branch_available,
                          bcmo.price_override AS channel_price,
                          bcmo.is_available  AS channel_available
                   FROM menu_items mi
                   LEFT JOIN branch_menu_overrides bmo
                          ON bmo.menu_item_id = mi.id AND bmo.branch_id = $2
                   LEFT JOIN branch_channel_menu_overrides bcmo
                          ON bcmo.menu_item_id = mi.id AND bcmo.branch_id = $2
                         AND bcmo.channel = $3::delivery_channel
                   WHERE mi.id = $1 AND mi.org_id = $4 AND mi.deleted_at IS NULL"#,
        )
        .bind(line.menu_item_id)
        .bind(branch_id)
        .bind(channel)
        .bind(org_id)
        .fetch_optional(pool)
        .await?;

        let (
            item_name,
            name_translations,
            base_price,
            branch_price,
            branch_available,
            channel_price,
            channel_available,
        ) = row.ok_or_else(|| {
            AppError::NotFound(format!("Menu item {} not found", line.menu_item_id))
        })?;

        // channel availability overrides branch availability; either can disable.
        let available = channel_available.unwrap_or(branch_available);
        if !available {
            return Err(AppError::BadRequest(format!(
                "{} is not available for this channel",
                item_name
            )));
        }

        // Channel price wins over branch over catalog (first non-NULL).
        let effective_base = channel_price.or(branch_price).unwrap_or(base_price);

        let unit_price = match &line.size_label {
            Some(size) => {
                resolve_size_price(pool, branch_id, line.menu_item_id, size, effective_base).await?
            }
            None => effective_base,
        };

        // Reuse the shared config resolver for addons / optionals / recipe deductions
        // (branch-effective addon prices; no channel addon layer yet).
        let config = resolve_menu_item_configuration(
            pool,
            line.menu_item_id,
            line.size_label.clone(),
            line.quantity,
            &line.addons,
            &line.optional_field_ids,
            branch_id,
        )
        .await?;

        // Cost-enrich this line's deductions at the current ingredient costs.
        let mut line_deductions: Vec<SnapshotDeduction> = config
            .deductions
            .into_iter()
            .map(|d| SnapshotDeduction {
                line_index: idx,
                org_ingredient_id: d.org_ingredient_id,
                ingredient_name: d.ingredient_name,
                unit: d.unit,
                quantity: d.quantity,
                source: d.source,
                category: d.category,
                addon_item_id: d.addon_item_id,
                optional_field_id: d.optional_field_id,
                cost_per_unit: None,
                line_cost: None,
            })
            .collect();

        let ids: Vec<Uuid> = line_deductions
            .iter()
            .filter_map(|d| d.org_ingredient_id)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let costs = crate::costing::ingredient_costs_at(pool, branch_id, &ids, at).await?;
        for d in &mut line_deductions {
            let c = d
                .org_ingredient_id
                .and_then(|id| costs.get(&id))
                .and_then(|c| c.to_f64());
            d.cost_per_unit = c;
            d.line_cost = c.map(|c| (d.quantity * c).round() as i64);
        }

        let (line_cost, unit_cost, cost_missing) =
            rollup_line_cost(&line_deductions, line.quantity);

        // Addons: server prices, with per-addon COGS rolled from attributed deductions.
        let mut addons: Vec<SnapshotAddon> = config
            .addons
            .into_iter()
            .map(|a| {
                let line_cost = addon_cost(&line_deductions, a.addon_item_id);
                SnapshotAddon {
                    addon_item_id: a.addon_item_id,
                    addon_name: a.addon_name,
                    name_translations: a.name_translations,
                    unit_price: a.unit_price,
                    quantity: a.quantity,
                    line_cost,
                }
            })
            .collect();

        // Channel addon overrides layer on top of the branch-effective addon price:
        // a price_override replaces the charged price; is_available=false rejects it.
        if !addons.is_empty() {
            let addon_ids: Vec<Uuid> = addons.iter().map(|a| a.addon_item_id).collect();
            let rows: Vec<(Uuid, Option<i32>, Option<bool>)> = sqlx::query_as(
                "SELECT addon_item_id, price_override, is_available \
                 FROM branch_channel_addon_overrides \
                 WHERE branch_id = $1 AND channel = $2::delivery_channel AND addon_item_id = ANY($3)",
            )
            .bind(branch_id)
            .bind(channel)
            .bind(&addon_ids)
            .fetch_all(pool)
            .await?;
            let ovr: std::collections::HashMap<Uuid, (Option<i32>, Option<bool>)> =
                rows.into_iter().map(|(id, p, a)| (id, (p, a))).collect();
            for a in &mut addons {
                if let Some((price, avail)) = ovr.get(&a.addon_item_id) {
                    if *avail == Some(false) {
                        return Err(AppError::BadRequest(format!(
                            "{} is not available for this channel",
                            a.addon_name
                        )));
                    }
                    if let Some(p) = price {
                        a.unit_price = *p;
                    }
                }
            }
        }

        let optionals: Vec<SnapshotOptional> = config
            .optionals
            .into_iter()
            .map(|o| {
                let cost = match (o.quantity_used, o.org_ingredient_id) {
                    (Some(qty), Some(_)) => line_deductions
                        .iter()
                        .find(|d| d.optional_field_id == Some(o.optional_field_id))
                        .and_then(|d| d.cost_per_unit)
                        .map(|c| (qty * c).round() as i64),
                    _ => Some(0),
                };
                SnapshotOptional {
                    optional_field_id: o.optional_field_id,
                    field_name: o.field_name,
                    name_translations: o.name_translations,
                    price: o.price,
                    org_ingredient_id: o.org_ingredient_id,
                    ingredient_name: o.ingredient_name,
                    ingredient_unit: o.ingredient_unit,
                    quantity_used: o.quantity_used,
                    cost,
                }
            })
            .collect();

        // Money math in i64, then range-check back to i32. Even with the per-field
        // caps above this guards against a server-side price/quantity combination
        // overflowing the integer-piastre line total (release builds do not panic
        // on overflow — they would silently wrap and mis-price the order).
        let too_large = || AppError::BadRequest("Order total is too large".into());
        let addon_per_unit: i64 = addons
            .iter()
            .map(|a| a.unit_price as i64 * a.quantity as i64)
            .sum();
        let optional_per_unit: i64 = optionals.iter().map(|o| o.price as i64).sum();
        let line_total_i64 =
            (unit_price as i64 + addon_per_unit + optional_per_unit) * line.quantity as i64;
        let line_total = i32::try_from(line_total_i64).map_err(|_| too_large())?;
        subtotal = subtotal.checked_add(line_total).ok_or_else(too_large)?;

        snapshot_lines.push(SnapshotLine {
            menu_item_id: line.menu_item_id,
            item_name,
            name_translations,
            size_label: line.size_label.clone(),
            unit_price,
            quantity: line.quantity,
            line_total,
            notes: line.notes.clone(),
            addons,
            optionals,
            line_cost,
            unit_cost,
            cost_missing,
        });
        all_deductions.extend(line_deductions);
    }

    Ok(ResolvedCart {
        snapshot: CartSnapshot {
            lines: snapshot_lines,
        },
        deductions: all_deductions,
        subtotal,
    })
}

/// branch-size override > catalog-size override > the branch/channel-effective base.
///
/// The size label arrives from an untrusted client. Comparing on `label::text`
/// (rather than casting the inbound string to ``) means an unknown
/// size is a clean 400 instead of a Postgres enum-cast 500. A size the item does
/// not actually offer is rejected — it must exist as an active catalog size.
async fn resolve_size_price(
    pool: &PgPool,
    branch_id: Uuid,
    menu_item_id: Uuid,
    size: &str,
    effective_base: i32,
) -> Result<i32, AppError> {
    let catalog: Option<Option<i32>> = sqlx::query_scalar(
        "SELECT price_override FROM item_sizes \
         WHERE menu_item_id = $1 AND label::text = $2 AND is_active = true",
    )
    .bind(menu_item_id)
    .bind(size)
    .fetch_optional(pool)
    .await?;
    let Some(catalog_override) = catalog else {
        return Err(AppError::BadRequest(format!("Unknown size '{size}'")));
    };
    let branch_size: Option<i32> = sqlx::query_scalar(
        "SELECT price_override FROM branch_menu_size_overrides \
         WHERE branch_id = $1 AND menu_item_id = $2 AND size_label::text = $3",
    )
    .bind(branch_id)
    .bind(menu_item_id)
    .bind(size)
    .fetch_optional(pool)
    .await?;
    Ok(branch_size.or(catalog_override).unwrap_or(effective_base))
}

/// Full line COGS, recipe-scope unit cost, and a cost-missing flag — mirrors the
/// POS `summarize_line_costs` rollup for one line's deductions.
fn rollup_line_cost(
    deductions: &[SnapshotDeduction],
    quantity: i32,
) -> (Option<i64>, Option<i64>, bool) {
    let cost_missing = deductions.is_empty() || deductions.iter().any(|d| d.line_cost.is_none());
    let line_cost = if cost_missing {
        None
    } else {
        Some(
            deductions
                .iter()
                .filter_map(|d| d.cost_per_unit.map(|c| c * d.quantity))
                .sum::<f64>()
                .round() as i64,
        )
    };
    let recipe: Vec<&SnapshotDeduction> = deductions
        .iter()
        .filter(|d| d.source == "drink_recipe" || d.source.starts_with("addon_swap:"))
        .collect();
    let unit_cost = if recipe.is_empty() || recipe.iter().any(|d| d.cost_per_unit.is_none()) {
        None
    } else {
        Some(
            (recipe
                .iter()
                .map(|d| d.cost_per_unit.unwrap() * d.quantity)
                .sum::<f64>()
                / quantity.max(1) as f64)
                .round() as i64,
        )
    };
    (line_cost, unit_cost, cost_missing)
}

/// Additive-addon COGS: sum the deductions attributed to this addon. `None` when
/// the addon has no ingredient rows or any attributed cost is unknown (matches POS).
fn addon_cost(deductions: &[SnapshotDeduction], addon_item_id: Uuid) -> Option<i64> {
    let entries: Vec<&SnapshotDeduction> = deductions
        .iter()
        .filter(|d| d.source == "addon" && d.addon_item_id == Some(addon_item_id))
        .collect();
    if entries.is_empty() || entries.iter().any(|d| d.cost_per_unit.is_none()) {
        None
    } else {
        Some(
            entries
                .iter()
                .map(|d| d.cost_per_unit.unwrap() * d.quantity)
                .sum::<f64>()
                .round() as i64,
        )
    }
}

// ── Finalize: replay snapshot into a real orders row ──────────

pub struct FinalizeCtx<'a> {
    pub branch_id: Uuid,
    pub shift_id: Uuid,
    pub teller_id: Uuid,
    pub payment_method: &'a str,
    pub is_cash: bool,
    pub created_at: DateTime<Utc>,
    pub subtotal: i32,
    pub tax_amount: i32,
    pub delivery_fee: i32,
    pub total_amount: i32,
    /// Frozen channel discount (item subtotal only). `discount_amount` is 0
    /// when none; `total_amount == subtotal - discount_amount + delivery_fee`.
    pub discount_id: Option<Uuid>,
    pub discount_type: Option<&'a str>,
    pub discount_value: i32,
    pub discount_amount: i32,
    pub customer_name: Option<&'a str>,
    pub notes: Option<&'a str>,
    /// `'delivery'` for a finalized delivery order, `'dine_in'` for a settled
    /// waiter open ticket (and any other POS-style materialization).
    pub order_type: &'a str,
    /// The originating delivery order, when materializing a delivery. `None` for
    /// dine-in tickets (which have no `delivery_orders` row) — the delivery-only
    /// RETURNING subselects resolve to NULL in that case.
    pub delivery_order_id: Option<Uuid>,
}

/// Replay the frozen snapshot into a normal completed `orders` row inside an
/// existing transaction. Returns the created order plus any oversold warnings.
pub async fn apply_snapshot(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ctx: &FinalizeCtx<'_>,
    lines: &[SnapshotLine],
    deductions: &[SnapshotDeduction],
) -> Result<(Order, Vec<String>), AppError> {
    let order_number: i32 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(order_number), 0) + 1 FROM orders WHERE shift_id = $1",
    )
    .bind(ctx.shift_id)
    .fetch_one(&mut **tx)
    .await?;

    // Mint a normal order_ref off the per-(branch, business_date) counter — same
    // infra POS orders use (the delivery_ref was minted separately at intake).
    let (branch_code, biz_date): (String, chrono::NaiveDate) = sqlx::query_as(
        "SELECT b.code, ($1::timestamptz AT TIME ZONE COALESCE(b.timezone, o.timezone)::text)::date
         FROM branches b JOIN organizations o ON o.id = b.org_id WHERE b.id = $2",
    )
    .bind(ctx.created_at)
    .bind(ctx.branch_id)
    .fetch_one(&mut **tx)
    .await?;
    let ref_seq: i32 = sqlx::query_scalar(
        "INSERT INTO order_ref_counters (branch_id, business_date, last_seq)
         VALUES ($1, $2, 1)
         ON CONFLICT (branch_id, business_date)
         DO UPDATE SET last_seq = order_ref_counters.last_seq + 1
         RETURNING last_seq",
    )
    .bind(ctx.branch_id)
    .bind(biz_date)
    .fetch_one(&mut **tx)
    .await?;
    let order_ref = format!(
        "{}-{}-{:04}",
        branch_code,
        biz_date.format("%y%m%d"),
        ref_seq
    );

    let order = sqlx::query_as::<_, Order>(
        r#"
        INSERT INTO orders
            (branch_id, shift_id, teller_id, order_number,
             payment_method, subtotal,
             discount_type, discount_value, discount_amount, discount_id,
             tax_amount, total_amount, tip_amount, status,
             customer_name, notes, created_at, order_ref,
             price_flagged, price_expected_total, tip_is_cash,
             order_type, delivery_fee, delivery_order_id)
        VALUES ($1, $2, $3, $4, $5, $6,
                $15::discount_type, $16, $17, $18,
                $7, $8, 0, 'completed',
                $9, $10, $11, $12,
                false, $8, NULL,
                $19, $13, $14)
        RETURNING
            id, branch_id, shift_id, teller_id,
            (SELECT name FROM users WHERE id = $3) AS teller_name,
            order_number, order_ref, status::text, payment_method::text,
            subtotal, discount_type::text, discount_value,
            discount_amount, tax_amount, total_amount,
            amount_tendered, change_given, tip_amount, tip_payment_method, discount_id,
            customer_name, notes, order_type, delivery_fee, delivery_order_id,
            (SELECT channel::text FROM delivery_orders WHERE id = orders.delivery_order_id) AS delivery_channel,
            (SELECT customer_lat FROM delivery_orders WHERE id = orders.delivery_order_id) AS delivery_lat,
            (SELECT customer_lng FROM delivery_orders WHERE id = orders.delivery_order_id) AS delivery_lng,
            voided_at, void_reason::text, void_note, voided_by, created_at
        "#,
    )
    .bind(ctx.branch_id)
    .bind(ctx.shift_id)
    .bind(ctx.teller_id)
    .bind(order_number)
    .bind(ctx.payment_method)
    .bind(ctx.subtotal)
    .bind(ctx.tax_amount)
    .bind(ctx.total_amount)
    .bind(ctx.customer_name)
    .bind(ctx.notes)
    .bind(ctx.created_at)
    .bind(&order_ref)
    .bind(ctx.delivery_fee)
    .bind(ctx.delivery_order_id)
    .bind(ctx.discount_type)   // $15
    .bind(ctx.discount_value)  // $16
    .bind(ctx.discount_amount) // $17
    .bind(ctx.discount_id)     // $18
    .bind(ctx.order_type)      // $19
    .fetch_one(&mut **tx)
    .await?;

    sqlx::query(
        "INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ($1, $2, $3, $4)",
    )
    .bind(order.id)
    .bind(ctx.payment_method)
    .bind(ctx.total_amount)
    .bind(ctx.is_cash)
    .execute(&mut **tx)
    .await?;

    let mut warnings: Vec<String> = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        let line_deductions: Vec<&SnapshotDeduction> =
            deductions.iter().filter(|d| d.line_index == idx).collect();
        let snapshot_json = serde_json::to_value(&line_deductions)
            .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()));

        let item_id: Uuid = sqlx::query_scalar(
            r#"INSERT INTO order_items
                (order_id, menu_item_id, item_name, name_translations, size_label,
                 unit_price, quantity, line_total, notes, deductions_snapshot,
                 bundle_id, bundle_unit_price, line_cost, unit_cost, cost_missing, price_flagged)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, NULL, NULL, $11, $12, $13, false)
               RETURNING id"#,
        )
        .bind(order.id)
        .bind(line.menu_item_id)
        .bind(&line.item_name)
        .bind(&line.name_translations)
        .bind(&line.size_label)
        .bind(line.unit_price)
        .bind(line.quantity)
        .bind(line.line_total)
        .bind(&line.notes)
        .bind(snapshot_json)
        .bind(line.line_cost)
        .bind(line.unit_cost)
        .bind(line.cost_missing)
        .fetch_one(&mut **tx)
        .await?;

        for addon in &line.addons {
            let addon_line = addon.unit_price * addon.quantity * line.quantity;
            sqlx::query(
                "INSERT INTO order_item_addons \
                    (order_item_id, addon_item_id, addon_name, name_translations, unit_price, quantity, line_total, line_cost) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(item_id)
            .bind(addon.addon_item_id)
            .bind(&addon.addon_name)
            .bind(&addon.name_translations)
            .bind(addon.unit_price)
            .bind(addon.quantity)
            .bind(addon_line)
            .bind(addon.line_cost)
            .execute(&mut **tx)
            .await?;
        }

        for opt in &line.optionals {
            sqlx::query(
                "INSERT INTO order_item_optionals \
                    (order_item_id, optional_field_id, field_name, name_translations, price, \
                     org_ingredient_id, ingredient_name, ingredient_unit, quantity_deducted, cost) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            )
            .bind(item_id)
            .bind(opt.optional_field_id)
            .bind(&opt.field_name)
            .bind(&opt.name_translations)
            .bind(opt.price)
            .bind(opt.org_ingredient_id)
            .bind(&opt.ingredient_name)
            .bind(&opt.ingredient_unit)
            .bind(opt.quantity_used)
            .bind(opt.cost)
            .execute(&mut **tx)
            .await?;
        }

        apply_one_deductions(
            tx,
            ctx.branch_id,
            order.id,
            ctx.teller_id,
            &line_deductions,
            &mut warnings,
        )
        .await?;
    }

    Ok((order, warnings))
}

/// Deduct a line's frozen ingredient plan from branch stock and record a `sale`
/// movement per ingredient (negative stock allowed but flagged), mirroring the
/// POS path exactly.
async fn apply_one_deductions(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    branch_id: Uuid,
    order_id: Uuid,
    teller_id: Uuid,
    deductions: &[&SnapshotDeduction],
    warnings: &mut Vec<String>,
) -> Result<(), AppError> {
    for d in deductions {
        let Some(ing_id) = d.org_ingredient_id else {
            continue;
        };
        let updated: Option<(Uuid, f64)> = sqlx::query_as(
            "UPDATE branch_inventory SET current_stock = current_stock - $1 \
             WHERE branch_id = $2 AND org_ingredient_id = $3 RETURNING id, current_stock::float8",
        )
        .bind(d.quantity)
        .bind(branch_id)
        .bind(ing_id)
        .fetch_optional(&mut **tx)
        .await?;
        let Some((bi_id, balance)) = updated else {
            continue;
        };
        let below_zero = balance < 0.0;
        if below_zero {
            warnings.push(format!(
                "{} is oversold — stock is now {:.3} {}",
                d.ingredient_name, balance, d.unit
            ));
        }
        crate::inventory::movements::record_movement(
            &mut **tx,
            crate::inventory::movements::MovementParams {
                branch_id,
                org_ingredient_id: ing_id,
                branch_inventory_id: Some(bi_id),
                movement_type: "sale",
                quantity: -d.quantity,
                balance_after: Some(balance),
                unit_cost: d.cost_per_unit.map(|c| c.round() as i64),
                reason: None,
                below_zero,
                source_type: Some("order"),
                source_id: Some(order_id),
                note: None,
                created_by: Some(teller_id),
            },
        )
        .await?;
    }
    Ok(())
}

/// Cancel-with-waste: the food was made but not delivered. Deduct the frozen plan
/// from stock (it was never deducted — no orders row existed) and log each as a
/// `waste` movement against the delivery order. Best-effort per ingredient.
pub async fn record_waste(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    branch_id: Uuid,
    delivery_order_id: Uuid,
    deductions: &[SnapshotDeduction],
    created_by: Uuid,
) -> Result<(), AppError> {
    for d in deductions {
        let Some(ing_id) = d.org_ingredient_id else {
            continue;
        };
        let updated: Option<(Uuid, f64)> = sqlx::query_as(
            "UPDATE branch_inventory SET current_stock = current_stock - $1 \
             WHERE branch_id = $2 AND org_ingredient_id = $3 RETURNING id, current_stock::float8",
        )
        .bind(d.quantity)
        .bind(branch_id)
        .bind(ing_id)
        .fetch_optional(&mut **tx)
        .await?;
        let Some((bi_id, balance)) = updated else {
            continue;
        };
        crate::inventory::movements::record_movement(
            &mut **tx,
            crate::inventory::movements::MovementParams {
                branch_id,
                org_ingredient_id: ing_id,
                branch_inventory_id: Some(bi_id),
                movement_type: "waste",
                quantity: -d.quantity,
                balance_after: Some(balance),
                unit_cost: d.cost_per_unit.map(|c| c.round() as i64),
                reason: Some("order_cancelled"),
                below_zero: balance < 0.0,
                source_type: Some("delivery_order"),
                source_id: Some(delivery_order_id),
                note: Some("Delivery order cancelled — made, not restocked"),
                created_by: Some(created_by),
            },
        )
        .await?;
    }
    Ok(())
}
