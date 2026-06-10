use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use chrono::Utc;

use crate::{
    auth::jwt::Claims,
    errors::{AppError, AppErrorResponse},
    models::UserRole,
    permissions::checker::check_permission,
};
use utoipa::{IntoParams, ToSchema};

/// Default page size when listing orders for a single shift (POS home stats, shift history).
const DEFAULT_PER_PAGE_SHIFT: i64 = 1000;
/// Default page size when listing orders for a whole branch (dashboard).
const DEFAULT_PER_PAGE_BRANCH: i64 = 100;

// ── Shared SELECT fragment ────────────────────────────────────
const ORDER_SELECT: &str =
    "SELECT o.id, o.branch_id, o.shift_id, o.teller_id, u.name AS teller_name,
     o.order_number, o.status::text, o.payment_method::text,
     o.subtotal, o.discount_type::text, o.discount_value,
     o.discount_amount, o.tax_amount, o.total_amount,
     o.amount_tendered, o.change_given, o.tip_amount, o.tip_payment_method, o.discount_id,
     o.customer_name, o.notes, o.voided_at, o.void_reason::text, o.voided_by, o.created_at
     FROM orders o JOIN users u ON u.id = o.teller_id ";

// ── Models ────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Order {
    pub id:                 Uuid,
    pub branch_id:          Uuid,
    pub shift_id:           Uuid,
    pub teller_id:          Uuid,
    pub teller_name:        String,
    pub order_number:       i32,
    pub status:             String,
    pub payment_method:     String,
    pub subtotal:           i32,
    pub discount_type:      Option<String>,
    pub discount_value:     i32,
    pub discount_amount:    i32,
    pub tax_amount:         i32,
    pub total_amount:       i32,
    pub amount_tendered:    Option<i32>,
    pub change_given:       Option<i32>,
    pub tip_amount:         Option<i32>,
    pub tip_payment_method: Option<String>,
    pub discount_id:        Option<Uuid>,
    pub customer_name:      Option<String>,
    pub notes:              Option<String>,
    pub voided_at:          Option<chrono::DateTime<chrono::Utc>>,
    pub void_reason:        Option<String>,
    pub voided_by:          Option<Uuid>,
    pub created_at:         chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct OrderItem {
    pub id:                  Uuid,
    pub order_id:            Uuid,
    pub menu_item_id:        Option<Uuid>,
    pub item_name:           String,
    #[schema(value_type = Object)] pub name_translations: serde_json::Value,
    pub size_label:          Option<String>,
    pub unit_price:          i32,
    pub quantity:            i32,
    pub line_total:          i32,
    pub notes:               Option<String>,
    pub deductions_snapshot: serde_json::Value,
    pub bundle_id:           Option<Uuid>,
    pub bundle_unit_price:   Option<i32>,
    /// Full line COGS in piastres (recipe + addons + optionals + components).
    /// `null` ⟺ unknown.
    pub line_cost:           Option<i64>,
    /// Recipe-only cost per unit in piastres (incl. swaps). `null` ⟺ unknown
    /// or bundle line.
    pub unit_cost:           Option<i64>,
    /// True when any cost component could not be resolved.
    pub cost_missing:        bool,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct OrderItemAddon {
    pub id:            Uuid,
    pub order_item_id: Uuid,
    pub addon_item_id: Uuid,
    pub addon_name:    String,
    #[schema(value_type = Object)] pub name_translations: serde_json::Value,
    pub unit_price:    i32,
    pub quantity:      i32,
    pub line_total:    i32,
    /// Ingredient cost of this addon line in piastres. `null` ⟺ unknown, or
    /// a swap addon (its cost lives in the item's recipe cost).
    pub line_cost:     Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct OrderItemOptional {
    pub id:               Uuid,
    pub order_item_id:    Uuid,
    pub optional_field_id: Option<Uuid>,
    pub field_name:       String,
    #[schema(value_type = Object)] pub name_translations: serde_json::Value,
    pub price:            i32,
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name:  Option<String>,
    pub ingredient_unit:  Option<String>,
    #[schema(value_type = Option<f64>)]
    pub quantity_deducted: Option<sqlx::types::BigDecimal>,
    /// Ingredient cost per parent-item unit in piastres. `null` ⟺ unknown or
    /// no ingredient linked.
    pub cost:             Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OrderFull {
    #[serde(flatten)]
    pub order: Order,
    pub items: Vec<OrderItemFull>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct OrderBundleComponentAddon {
    pub id:                Uuid,
    pub order_line_id:     Uuid,
    pub component_item_id: Uuid,
    pub addon_item_id:     Uuid,
    pub addon_name:        String,
    #[schema(value_type = Object)] pub name_translations: serde_json::Value,
    pub unit_price:        i32,
    pub quantity:          i32,
    pub line_total:        i32,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct OrderBundleComponentOptional {
    pub id:                Uuid,
    pub order_line_id:     Uuid,
    pub component_item_id: Uuid,
    pub optional_field_id: Option<Uuid>,
    pub field_name:        String,
    #[schema(value_type = Object)] pub name_translations: serde_json::Value,
    pub price:             i32,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OrderBundleComponentFull {
    pub item_id:    Uuid,
    pub item_name:  String,
    #[schema(value_type = Object)] pub name_translations: serde_json::Value,
    pub quantity:   i32,
    pub size_label: Option<String>,
    pub addons:     Vec<OrderBundleComponentAddon>,
    pub optionals:  Vec<OrderBundleComponentOptional>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OrderItemFull {
    #[serde(flatten)]
    pub item:              OrderItem,
    pub addons:            Vec<OrderItemAddon>,
    pub optionals:         Vec<OrderItemOptional>,
    #[serde(default)]
    pub bundle_components: Vec<OrderBundleComponentFull>,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct PaymentSplitInput {
    pub method:    String,
    pub amount:    i32,
    pub reference: Option<String>,
}

pub use crate::orders::component_resolve::AddonInput;

#[derive(Deserialize, Serialize, ToSchema)]
pub struct OrderItemInput {
    pub menu_item_id:      Option<Uuid>,
    pub bundle_id:         Option<Uuid>,
    pub size_label:        Option<String>,
    pub quantity:          i32,
    pub addons:            Vec<AddonInput>,
    pub optional_field_ids: Vec<Uuid>,
    #[serde(default)]
    pub bundle_components: Vec<crate::orders::component_resolve::BundleComponentInput>,
    pub notes:             Option<String>,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct CreateOrderRequest {
    pub branch_id:          Uuid,
    pub shift_id:           Uuid,
    pub payment_method:     String,
    pub customer_name:      Option<String>,
    pub notes:              Option<String>,
    pub discount_type:      Option<String>,
    pub discount_value:     Option<i32>,
    pub discount_id:        Option<Uuid>,
    pub amount_tendered:    Option<i32>,
    pub tip_amount:         Option<i32>,
    pub tip_payment_method: Option<String>,
    pub payment_splits:     Option<Vec<PaymentSplitInput>>,
    pub items:              Vec<OrderItemInput>,
    pub created_at:         Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct VoidOrderRequest {
    pub reason:            String,
    pub voided_at:         Option<chrono::DateTime<chrono::Utc>>,
    pub restore_inventory: Option<bool>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListOrdersQuery {
    pub branch_id:      Option<Uuid>,
    pub shift_id:       Option<Uuid>,
    pub updated_after:  Option<chrono::DateTime<chrono::Utc>>,
    pub page:           Option<i64>,
    pub per_page:       Option<i64>,
    pub teller_name:    Option<String>,
    pub payment_method: Option<String>,
    pub status:         Option<String>,
    pub from:           Option<chrono::DateTime<chrono::Utc>>,
    pub to:             Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct OrderSummary {
    pub revenue:   i64,
    pub completed: i64,
    pub voided:    i64,
    pub discounts: i64,
    pub tips:      i64,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct PaginatedOrders {
    pub data:        Vec<Order>,
    pub total:       i64,
    pub page:        i64,
    pub per_page:    i64,
    pub total_pages: i64,
    pub summary:     OrderSummary,   // ← add this
}

#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct OrderPayment {
    pub id:        Uuid,
    pub order_id:  Uuid,
    pub method:    String,
    pub amount:    i32,
    pub reference: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OrderExport {
    #[serde(flatten)]
    pub order:    Order,
    pub items:    Vec<OrderItemFull>,
    pub payments: Vec<OrderPayment>,
}

#[derive(Serialize, ToSchema)]
pub struct ExportResponse {
    pub data:             Vec<OrderExport>,
    pub total:            i64,
    pub generated_at:     chrono::DateTime<chrono::Utc>,
    pub summary:          OrderSummary,
    pub ingredient_costs: std::collections::HashMap<Uuid, i32>,  // NEW: org_ingredient_id → cost_per_unit (piastres)
}

#[derive(Deserialize, Serialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ExportOrdersQuery {
    pub branch_id:      Option<Uuid>,
    pub shift_id:       Option<Uuid>,
    pub teller_name:    Option<String>,
    pub payment_method: Option<String>,   // same comma-separated semantics
    pub status:         Option<String>,
    pub from:           Option<chrono::DateTime<chrono::Utc>>,
    pub to:             Option<chrono::DateTime<chrono::Utc>>,
}

// ── Deduction helper ──────────────────────────────────────────

#[derive(Serialize, Clone)]
struct InventoryDeduction {
    org_ingredient_id: Option<Uuid>,
    ingredient_name:   String,
    unit:              String,
    quantity:          f64,
    source:            String, // "drink_recipe" | "addon" | "addon_swap:<name>" | "optional" | "bundle_component:<name>"
    category:          String,
    /// Additive-addon attribution (None for recipe/swap/optional entries).
    #[serde(skip_serializing_if = "Option::is_none")]
    addon_item_id:     Option<Uuid>,
    /// Optional-field attribution.
    #[serde(skip_serializing_if = "Option::is_none")]
    optional_field_id: Option<Uuid>,
    /// Bundle-component attribution (which component this entry belongs to).
    #[serde(skip_serializing_if = "Option::is_none")]
    component_item_id: Option<Uuid>,
    /// EGP cost per ingredient unit at sale time. None ⟺ unknown.
    cost_per_unit:     Option<f64>,
    /// quantity × cost_per_unit in piastres, rounded. None ⟺ unknown.
    line_cost:         Option<i64>,
}

struct LineCostSummary {
    line_cost:    Option<i64>,
    unit_cost:    Option<i64>,
    cost_missing: bool,
}

/// Roll a line's enriched deductions up into the stored cost columns.
///
/// * `line_cost` — full COGS in piastres; `None` when anything is unknown.
/// * `unit_cost` — recipe-scope (drink_recipe + addon_swap) cost ÷ quantity;
///   `None` for bundle lines and whenever recipe cost is unknown.
/// * `cost_missing` — any unresolved entry, a menu line with no recipe at
///   all, or an additive addon with no ingredient rows.
fn summarize_line_costs(
    deductions: &[InventoryDeduction],
    quantity: i32,
    is_bundle_line: bool,
    has_uncosted_addon: bool,
) -> LineCostSummary {
    let mut cost_missing = deductions.iter().any(|d| d.line_cost.is_none())
        || deductions.is_empty()
        || has_uncosted_addon;

    let total_egp: f64 = deductions
        .iter()
        .filter_map(|d| d.cost_per_unit.map(|c| c * d.quantity))
        .sum();
    let line_cost = if cost_missing {
        None
    } else {
        Some((total_egp * 100.0).round() as i64)
    };

    let recipe_scope =
        |d: &&InventoryDeduction| d.source == "drink_recipe" || d.source.starts_with("addon_swap:");
    let unit_cost = if is_bundle_line {
        None
    } else {
        let entries: Vec<&InventoryDeduction> = deductions.iter().filter(recipe_scope).collect();
        if entries.is_empty() || entries.iter().any(|d| d.cost_per_unit.is_none()) {
            None
        } else {
            let egp: f64 = entries
                .iter()
                .map(|d| d.cost_per_unit.unwrap() * d.quantity)
                .sum();
            Some((egp * 100.0 / quantity.max(1) as f64).round() as i64)
        }
    };

    if is_bundle_line && deductions.is_empty() {
        cost_missing = true;
    }

    LineCostSummary { line_cost, unit_cost, cost_missing }
}

// ── POST /orders ──────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/orders",
    tag = "orders",
    request_body = CreateOrderRequest,
    responses((status = 201, description = "Order created", body = OrderFull), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_order(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<CreateOrderRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "create").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;

    let idempotency_key = req
        .headers()
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| Uuid::parse_str(s).ok());

    if let Some(key) = idempotency_key
        && let Some(existing) = fetch_order_by_idempotency_key(pool.get_ref(), key).await? {
            let items = fetch_order_items_full(pool.get_ref(), existing.id).await?;
            return Ok(HttpResponse::Ok().json(OrderFull { order: existing, items }));
        }

    if body.items.is_empty() {
        return Err(AppError::BadRequest("Order must have at least one item".into()));
    }

    let shift_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM shifts WHERE id = $1 AND branch_id = $2)"
    )
    .bind(body.shift_id)
    .bind(body.branch_id)
    .fetch_one(pool.get_ref())
    .await?;

    if !shift_exists {
        return Err(AppError::BadRequest(
            "Shift not found or does not belong to this branch".into(),
        ));
    }

    let org_id = claims.org_id().unwrap();

    validate_payment_method(pool.get_ref(), org_id, &body.payment_method).await?;
    if let Some(dt)  = &body.discount_type      { validate_discount_type(dt)?; }
    if let Some(tpm) = &body.tip_payment_method { validate_payment_method(pool.get_ref(), org_id, tpm).await?; }

    let (resolved_discount_type, resolved_discount_value) =
        if let Some(disc_id) = body.discount_id {
            let row: Option<(String, i32)> = sqlx::query_as(
                "SELECT type::text, value FROM discounts WHERE id = $1 AND is_active = true"
            )
            .bind(disc_id)
            .fetch_optional(pool.get_ref())
            .await?;
            match row {
                Some((dtype, dvalue)) => (Some(dtype), dvalue),
                None => return Err(AppError::BadRequest("Discount not found or inactive".into())),
            }
        } else {
            (body.discount_type.clone(), body.discount_value.unwrap_or(0))
        };

    let tax_rate: sqlx::types::BigDecimal = sqlx::query_scalar(
        "SELECT o.tax_rate FROM organizations o JOIN branches b ON b.org_id = o.id WHERE b.id = $1"
    )
    .bind(body.branch_id)
    .fetch_one(pool.get_ref())
    .await?;

    // ── Local types ───────────────────────────────────────────
    struct ResolvedOptional {
        optional_field_id: Uuid,
        field_name:        String,
        name_translations: serde_json::Value,
        price:             i32,
        org_ingredient_id: Option<Uuid>,
        ingredient_name:   Option<String>,
        ingredient_unit:   Option<String>,
        quantity_used:     Option<f64>,
    }

    #[allow(dead_code)]
    struct ResolvedBundleComponent {
        item_id:    Uuid,
        item_name:  String,
        name_translations: serde_json::Value,
        quantity:   i32,
        size_label: Option<String>,
        addons:     Vec<ResolvedAddon>,
        optionals:  Vec<ResolvedOptional>,
    }

    struct ResolvedItem {
        menu_item_id:      Option<Uuid>,
        item_name:         String,
        name_translations: serde_json::Value,
        size_label:        Option<String>,
        unit_price:        i32,
        quantity:          i32,
        notes:             Option<String>,
        addons:            Vec<ResolvedAddon>,
        optionals:         Vec<ResolvedOptional>,
        deductions:        Vec<InventoryDeduction>,
        bundle_id:           Option<Uuid>,
        bundle_unit_price:   Option<i32>,
            bundle_components:   Vec<ResolvedBundleComponent>,
        component_surcharge:   i32,
    }

    struct ResolvedAddon {
        addon_item_id: Uuid,
        addon_name:    String,
        name_translations: serde_json::Value,
        unit_price:    i32,
        quantity:      i32,
        /// False when the addon has no ingredient rows (additive addons only
        /// — swap addons fold into the recipe). No ingredients ⟹ cost-missing.
        has_ingredients: bool,
        /// True when this addon acted as a milk/coffee swap (cost lives in
        /// the recipe-scope deduction it replaced).
        is_swap:       bool,
    }

    let mut resolved_items: Vec<ResolvedItem> = Vec::new();
    let mut subtotal: i32 = 0;

    for item_input in &body.items {
        if item_input.quantity <= 0 {
            return Err(AppError::BadRequest("Item quantity must be > 0".into()));
        }

        let mut deductions: Vec<InventoryDeduction> = Vec::new();
        let mut resolved_addons: Vec<ResolvedAddon> = Vec::new();
        let mut resolved_optionals: Vec<ResolvedOptional> = Vec::new();
        let mut bundle_components = Vec::new();

        let mut component_surcharge: i32 = 0;
        let (resolved_menu_item_id, item_name, name_translations, unit_price, bundle_id, bundle_unit_price) = if let Some(b_id) = item_input.bundle_id {
            // ── 1. Resolve Bundle ─────────────────────────────
            let bundle: (Uuid, String, i32, String) = sqlx::query_as(
                "SELECT id, name, price, status::text FROM bundles WHERE id = $1 AND org_id = $2"
            )
            .bind(b_id)
            .bind(claims.org_id())
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Bundle {} not found", b_id)))?;

            if bundle.3 != "active" {
                return Err(AppError::BadRequest(format!("Bundle {} is not active", bundle.1)));
            }

            // Branch availability
            let available_in_branch: bool = sqlx::query_scalar(
                "SELECT EXISTS(
                    SELECT 1 FROM bundle_branch_availability WHERE bundle_id = $1 AND branch_id = $2
                 ) OR NOT EXISTS(
                    SELECT 1 FROM bundle_branch_availability WHERE bundle_id = $1
                 )"
            )
            .bind(bundle.0)
            .bind(body.branch_id)
            .fetch_one(pool.get_ref())
            .await?;

            if !available_in_branch {
                return Err(AppError::BadRequest(format!("Bundle {} is not available in branch {}", bundle.1, body.branch_id)));
            }

            // Date / Time window validation
            let order_time = body.created_at.unwrap_or_else(Utc::now);
            let branch_tz: String = sqlx::query_scalar(
                "SELECT timezone FROM branches WHERE id = $1"
            )
            .bind(body.branch_id)
            .fetch_one(pool.get_ref())
            .await?;

            let local_dt_rows: Option<(chrono::NaiveDate, chrono::NaiveTime)> = sqlx::query_as(
                "SELECT ($1::timestamptz AT TIME ZONE $2)::date, ($1::timestamptz AT TIME ZONE $2)::time"
            )
            .bind(order_time)
            .bind(&branch_tz)
            .fetch_optional(pool.get_ref())
            .await?;

            if let Some((local_date, local_time)) = local_dt_rows {
                let bundle_limits: (Option<chrono::NaiveDate>, Option<chrono::NaiveDate>, Option<chrono::NaiveTime>, Option<chrono::NaiveTime>) = sqlx::query_as(
                    "SELECT available_from_date, available_until_date, available_from_time, available_until_time \
                     FROM bundles WHERE id = $1"
                )
                .bind(bundle.0)
                .fetch_one(pool.get_ref())
                .await?;

                if let Some(from_d) = bundle_limits.0
                    && local_date < from_d {
                        return Err(AppError::BadRequest(format!("Bundle {} is not yet available", bundle.1)));
                    }
                if let Some(until_d) = bundle_limits.1
                    && local_date > until_d {
                        return Err(AppError::BadRequest(format!("Bundle {} availability has expired", bundle.1)));
                    }
                if let Some(from_t) = bundle_limits.2
                    && local_time < from_t {
                        return Err(AppError::BadRequest(format!("Bundle {} is not available at this hour", bundle.1)));
                    }
                if let Some(until_t) = bundle_limits.3
                    && local_time > until_t {
                        return Err(AppError::BadRequest(format!("Bundle {} is not available at this hour", bundle.1)));
                    }
            }

            // Resolve components (client snapshot or catalog defaults)
            let catalog: Vec<(Uuid, i32, String, serde_json::Value)> = sqlx::query_as(
                "SELECT bc.item_id, bc.quantity, mi.name, mi.name_translations \
                 FROM bundle_components bc \
                 JOIN menu_items mi ON mi.id = bc.item_id \
                 WHERE bc.bundle_id = $1 \
                 ORDER BY bc.position ASC",
            )
            .bind(bundle.0)
            .fetch_all(pool.get_ref())
            .await?;

            if catalog.is_empty() {
                return Err(AppError::BadRequest(format!("Bundle {} has no components", bundle.1)));
            }

            let catalog_map: std::collections::HashMap<Uuid, (i32, String, serde_json::Value)> = catalog
                .iter()
                .map(|(id, qty, name, tr)| (*id, (*qty, name.clone(), tr.clone())))
                .collect();

            let component_inputs: Vec<crate::orders::component_resolve::BundleComponentInput> =
                if item_input.bundle_components.is_empty() {
                    catalog
                        .iter()
                        .map(|(id, qty, _, _)| crate::orders::component_resolve::BundleComponentInput {
                            item_id: *id,
                            quantity: *qty,
                            size_label: None,
                            addons: vec![],
                            optional_field_ids: vec![],
                        })
                        .collect()
                } else {
                    item_input.bundle_components.clone()
                };

            for comp_in in component_inputs {
                let Some((catalog_qty, item_name, name_translations)) = catalog_map.get(&comp_in.item_id) else {
                    return Err(AppError::BadRequest(format!(
                        "Item {} is not a component of bundle {}",
                        comp_in.item_id, bundle.1
                    )));
                };
                if comp_in.quantity != *catalog_qty {
                    return Err(AppError::BadRequest(format!(
                        "Invalid quantity for component {} in bundle {}",
                        item_name, bundle.1
                    )));
                }

                let line_qty = comp_in.quantity * item_input.quantity;
                let config = crate::orders::component_resolve::resolve_menu_item_configuration(
                    pool.get_ref(),
                    comp_in.item_id,
                    comp_in.size_label.clone(),
                    line_qty,
                    &comp_in.addons,
                    &comp_in.optional_field_ids,
                )
                .await?;

                component_surcharge += (config.addon_line + config.optional_line) * comp_in.quantity * item_input.quantity;

                for d in config.deductions {
                    deductions.push(InventoryDeduction {
                        org_ingredient_id: d.org_ingredient_id,
                        ingredient_name:   d.ingredient_name,
                        unit:              d.unit,
                        quantity:          d.quantity,
                        source:            format!("bundle_component:{}", item_name),
                        category:          d.category,
                        addon_item_id:     d.addon_item_id,
                        optional_field_id: d.optional_field_id,
                        component_item_id: Some(comp_in.item_id),
                        cost_per_unit:     None,
                        line_cost:         None,
                    });
                }

                let comp_addons: Vec<ResolvedAddon> = config
                    .addons
                    .into_iter()
                    .map(|a| ResolvedAddon {
                        addon_item_id: a.addon_item_id,
                        addon_name:    a.addon_name,
                        name_translations: a.name_translations,
                        unit_price:    a.unit_price,
                        quantity:      a.quantity,
                        has_ingredients: true, // component-level costing rolls up via deductions
                        is_swap:       false,
                    })
                    .collect();

                let comp_optionals: Vec<ResolvedOptional> = config
                    .optionals
                    .into_iter()
                    .map(|o| ResolvedOptional {
                        optional_field_id: o.optional_field_id,
                        field_name:        o.field_name,
                        name_translations: o.name_translations,
                        price:             o.price,
                        org_ingredient_id: o.org_ingredient_id,
                        ingredient_name:   o.ingredient_name,
                        ingredient_unit:   o.ingredient_unit,
                        quantity_used:     o.quantity_used,
                    })
                    .collect();

                bundle_components.push(ResolvedBundleComponent {
                    item_id:    comp_in.item_id,
                    item_name:  item_name.clone(),
                    name_translations: name_translations.clone(),
                    quantity:   comp_in.quantity,
                    size_label: comp_in.size_label.clone(),
                    addons:     comp_addons,
                    optionals:  comp_optionals,
                });
            }

            (None, bundle.1, serde_json::json!({}), bundle.2, Some(bundle.0), Some(bundle.2))
        } else if let Some(m_item_id) = item_input.menu_item_id {
            // ── 2. Resolve Menu Item ──────────────────────────
            let (item_name, name_translations, base_price): (String, serde_json::Value, i32) = sqlx::query_as(
                "SELECT name, name_translations, base_price FROM menu_items WHERE id = $1 AND deleted_at IS NULL",
            )
            .bind(m_item_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound(
                format!("Menu item {} not found", m_item_id)
            ))?;

            let unit_price: i32 = match &item_input.size_label {
                Some(size) => {
                    let p: Option<i32> = sqlx::query_scalar(
                        "SELECT price_override FROM item_sizes \
                         WHERE menu_item_id = $1 AND label = $2::item_size AND is_active = true"
                    )
                    .bind(m_item_id)
                    .bind(size)
                    .fetch_optional(pool.get_ref())
                    .await?
                    .flatten();
                    p.unwrap_or(base_price)
                }
                None => base_price,
            };

            // Base drink recipe
            let recipe_rows: Vec<(Option<Uuid>, f64, String, String, String)> =
                if let Some(size) = &item_input.size_label {
                    sqlx::query_as(
                        r#"SELECT r.org_ingredient_id, r.quantity_used::float8,
                                  r.ingredient_name, r.ingredient_unit,
                                  COALESCE(i.category, 'general') as category
                           FROM   menu_item_recipes r
                           LEFT JOIN org_ingredients i ON i.id = r.org_ingredient_id
                           WHERE  r.menu_item_id = $1 AND r.size_label = $2::item_size"#,
                    )
                    .bind(m_item_id)
                    .bind(size)
                    .fetch_all(pool.get_ref())
                    .await?
                } else {
                    sqlx::query_as(
                        r#"SELECT r.org_ingredient_id, r.quantity_used::float8,
                                  r.ingredient_name, r.ingredient_unit,
                                  COALESCE(i.category, 'general') as category
                           FROM   menu_item_recipes r
                           LEFT JOIN org_ingredients i ON i.id = r.org_ingredient_id
                           WHERE  r.menu_item_id = $1
                             AND  r.size_label = (
                                 SELECT size_label FROM menu_item_recipes
                                 WHERE  menu_item_id = $1 LIMIT 1
                             )"#,
                    )
                    .bind(m_item_id)
                    .fetch_all(pool.get_ref())
                    .await?
                };

            for (ing_id, qty, name, unit, category) in recipe_rows {
                deductions.push(InventoryDeduction {
                    org_ingredient_id: ing_id,
                    ingredient_name:   name,
                    unit,
                    quantity:          qty * item_input.quantity as f64,
                    source:            "drink_recipe".into(),
                    category,
                    addon_item_id:     None,
                    optional_field_id: None,
                    component_item_id: None,
                    cost_per_unit:     None,
                    line_cost:         None,
                });
            }

            // ── 2. Addon ingredients (flat — no overrides) ────────
            for addon_input in &item_input.addons {
                let addon_qty = addon_input.quantity.max(1) as f64;

                let (addon_name, addon_name_translations, default_price, addon_type): (String, serde_json::Value, i32, String) = sqlx::query_as(
                    "SELECT name, name_translations, default_price, type FROM addon_items WHERE id = $1"
                )
                .bind(addon_input.addon_item_id)
                .fetch_optional(pool.get_ref())
                .await?
                .ok_or_else(|| AppError::NotFound(
                    format!("Addon {} not found", addon_input.addon_item_id)
                ))?;

                resolved_addons.push(ResolvedAddon {
                    addon_item_id: addon_input.addon_item_id,
                    addon_name:    addon_name.clone(),
                    name_translations: addon_name_translations.clone(),
                    unit_price:    default_price,
                    quantity:      addon_input.quantity.max(1),
                    has_ingredients: false, // patched below once addon_rows is known
                    is_swap:       false,
                });

                let addon_rows: Vec<(Option<Uuid>, f64, String, String)> = sqlx::query_as(
                    "SELECT org_ingredient_id, quantity_used::float8,
                            ingredient_name, ingredient_unit
                     FROM   addon_item_ingredients
                     WHERE  addon_item_id = $1",
                )
                .bind(addon_input.addon_item_id)
                .fetch_all(pool.get_ref())
                .await?;

                // Dynamic swap logic for milk and coffee types
                let target_category = match addon_type.as_str() {
                    "milk_type" => Some("milk"),
                    "coffee_type" => Some("coffee_bean"),
                    _ => None,
                };

                if let Some(cat) = target_category {
                    // Find the base recipe's ingredient for this category
                    let base_ing_id = deductions.iter()
                        .find(|d| d.source == "drink_recipe" && d.category == cat)
                        .and_then(|d| d.org_ingredient_id);

                    // Find the addon's ingredient
                    let addon_ing_id = addon_rows.first()
                        .and_then(|(id, _, _, _)| *id);

                    // If both point to the same org_ingredient → this IS the base, not a swap
                    let is_base = base_ing_id.is_some()
                        && addon_ing_id.is_some()
                        && base_ing_id == addon_ing_id;

                    if is_base {
                        // No charge — the drink already uses this ingredient as its base
                        if let Some(last) = resolved_addons.last_mut() {
                            last.unit_price = 0;
                            last.is_swap = true;
                        }
                        // Don't touch deductions — recipe already has the right ingredient
                    } else if let Some((repl_id, _, repl_name, repl_unit)) = addon_rows.first() {
                        // Real swap — calculate price difference
                        let base_addon_price: i32 = if let Some(base_id) = base_ing_id {
                            sqlx::query_scalar(
                                "SELECT COALESCE(MAX(a.default_price), 0)
                                 FROM addon_items a
                                 JOIN addon_item_ingredients i ON i.addon_item_id = a.id
                                 WHERE i.org_ingredient_id = $1 AND a.type = $2"
                            )
                            .bind(base_id)
                            .bind(addon_type.as_str())
                            .fetch_optional(pool.get_ref())
                            .await?
                            .flatten()
                            .unwrap_or(0)
                        } else {
                            0
                        };

                        let new_price = (default_price - base_addon_price).max(0);
                        if let Some(last) = resolved_addons.last_mut() {
                            last.unit_price = new_price;
                            last.is_swap = true;
                        }

                        // Replace base ingredient
                        let mut swapped = false;
                        for ded in deductions.iter_mut() {
                            if ded.source == "drink_recipe" && ded.category == cat {
                                ded.org_ingredient_id = *repl_id;
                                ded.ingredient_name = repl_name.clone();
                                ded.unit = repl_unit.clone();
                                ded.source = format!("addon_swap:{}", addon_name);
                                swapped = true;
                            }
                        }
                        if !swapped {
                            tracing::warn!(addon_name = %addon_name, cat = %cat, "Addon swap failed, no base ingredient found with category");
                        }
                    }
                    continue; // Skip the normal additive deduction for these addon types
                }

                if let Some(last) = resolved_addons.last_mut() {
                    last.has_ingredients = !addon_rows.is_empty();
                }
                for (ing_id, qty, name, unit) in addon_rows {
                    deductions.push(InventoryDeduction {
                        org_ingredient_id: ing_id,
                        ingredient_name:   name,
                        unit,
                        quantity:          qty * item_input.quantity as f64 * addon_qty,
                        source:            "addon".into(),
                        category:          "general".into(),
                        addon_item_id:     Some(addon_input.addon_item_id),
                        optional_field_id: None,
                        component_item_id: None,
                        cost_per_unit:     None,
                        line_cost:         None,
                    });
                }
            }

            for &field_id in &item_input.optional_field_ids {
                let row_result = sqlx::query_as::<_, (String, i32, Option<Uuid>, Option<String>, Option<String>, Option<f64>, Option<String>, serde_json::Value)>(
                    r#"SELECT name, price, org_ingredient_id,
                              ingredient_name, ingredient_unit,
                              quantity_used::float8, size_label::text,
                              name_translations
                       FROM menu_item_optional_fields
                       WHERE id = $1 AND menu_item_id = $2 AND is_active = true"#,
                )
                .bind(field_id)
                .bind(m_item_id)
                .fetch_optional(pool.get_ref())
                .await?;
            
                let Some((fname, fprice, ing_id, ing_name, ing_unit, qty_used, field_size, name_translations)) = row_result else {
                    tracing::warn!(field_id = %field_id, "Optional field not found or inactive — skipping");
                    continue;
                };

                // If field is size-restricted, check it matches
                if let Some(fs) = &field_size
                    && item_input.size_label.as_deref() != Some(fs.as_str()) {
                        tracing::warn!(field_id = %field_id, "Optional field size mismatch — skipping");
                        continue;
                    }

                // Add ingredient deduction if configured
                if let (Some(ref name), Some(ref unit), Some(qty)) =
                    (ing_name.clone(), ing_unit.clone(), qty_used)
                {
                    deductions.push(InventoryDeduction {
                        org_ingredient_id: ing_id,
                        ingredient_name:   name.clone(),
                        unit:              unit.clone(),
                        quantity:          qty * item_input.quantity as f64,
                        source:            "optional".into(),
                        category:          "general".into(),
                        addon_item_id:     None,
                        optional_field_id: Some(field_id),
                        component_item_id: None,
                        cost_per_unit:     None,
                        line_cost:         None,
                    });
                }

                resolved_optionals.push(ResolvedOptional {
                    optional_field_id: field_id,
                    field_name:        fname,
                    name_translations,
                    price:             fprice,
                    org_ingredient_id: ing_id,
                    ingredient_name:   ing_name,
                    ingredient_unit:   ing_unit,
                    quantity_used:     qty_used,
                });
            }

            (Some(m_item_id), item_name, name_translations, unit_price, None, None)
        } else {
            return Err(AppError::BadRequest("Each line item must have either menu_item_id or bundle_id".into()));
        };

        let item_line = unit_price * item_input.quantity;
        let addon_line: i32 = if bundle_id.is_some() {
            0
        } else {
            resolved_addons
                .iter()
                .map(|a| a.unit_price * a.quantity)
                .sum::<i32>()
                * item_input.quantity
        };
        let optional_line: i32 = if bundle_id.is_some() {
            0
        } else {
            resolved_optionals
                .iter()
                .map(|o| o.price)
                .sum::<i32>()
                * item_input.quantity
        };

        subtotal += item_line + addon_line + optional_line + component_surcharge;

        resolved_items.push(ResolvedItem {
            menu_item_id:      resolved_menu_item_id,
            item_name,
            name_translations,
            size_label:        item_input.size_label.clone(),
            unit_price,
            quantity:          item_input.quantity,
            notes:             item_input.notes.clone(),
            addons:            resolved_addons,
            optionals:         resolved_optionals,
            deductions,
            bundle_id,
            bundle_unit_price,
            bundle_components,
            component_surcharge,
        });
    }

    let discount_amount = match resolved_discount_type.as_deref() {
        Some("percentage") => (subtotal as f64 * resolved_discount_value as f64 / 100.0) as i32,
        Some("fixed")      => resolved_discount_value.min(subtotal),
        _                  => 0,
    };
    let taxable      = subtotal - discount_amount;
    let tax_rate_f64: f64 = tax_rate.to_string().parse().unwrap_or(0.14);
    let tax_amount   = (taxable as f64 * tax_rate_f64) as i32;
    let total_amount = taxable + tax_amount;
    let change_given = body.amount_tendered.map(|t| (t - total_amount).max(0));
    let created_at   = body.created_at.unwrap_or_else(chrono::Utc::now);

    // ── Cost snapshot ─────────────────────────────────────────
    // Resolve point-in-time ingredient costs once for the whole order and
    // stamp them onto the deduction entries; per-line / per-addon /
    // per-optional rollups happen at insert time.
    {
        let ingredient_ids: Vec<Uuid> = resolved_items
            .iter()
            .flat_map(|ri| ri.deductions.iter().filter_map(|d| d.org_ingredient_id))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let ingredient_costs =
            crate::costing::ingredient_costs_at(pool.get_ref(), &ingredient_ids, created_at)
                .await?;
        for ri in &mut resolved_items {
            for d in &mut ri.deductions {
                let egp = d
                    .org_ingredient_id
                    .and_then(|id| ingredient_costs.get(&id))
                    .and_then(|c| c.to_f64());
                d.cost_per_unit = egp;
                d.line_cost = egp.map(|c| (d.quantity * c * 100.0).round() as i64);
            }
        }
    }

    let mut tx = pool.get_ref().begin().await?;

    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
        .bind(body.shift_id.to_string())
        .execute(&mut *tx)
        .await?;

    let order_number: i32 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(order_number), 0) + 1 FROM orders WHERE shift_id = $1"
    )
    .bind(body.shift_id)
    .fetch_one(&mut *tx)
    .await?;

    let order = sqlx::query_as::<_, Order>(
        r#"
        INSERT INTO orders
            (branch_id, shift_id, teller_id, order_number,
             payment_method, subtotal, discount_type, discount_value,
             discount_amount, tax_amount, total_amount,
             amount_tendered, change_given, tip_amount, tip_payment_method,
             discount_id, customer_name, notes, status,
             idempotency_key, created_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7::discount_type, $8,
                $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, 'completed', $19, $20)
        RETURNING
            id, branch_id, shift_id, teller_id,
            (SELECT name FROM users WHERE id = $3) AS teller_name,
            order_number, status::text, payment_method::text,
            subtotal, discount_type::text, discount_value,
            discount_amount, tax_amount, total_amount,
            amount_tendered, change_given, tip_amount, tip_payment_method, discount_id,
            customer_name, notes, voided_at, void_reason::text, voided_by, created_at
        "#,
    )
    .bind(body.branch_id)
    .bind(body.shift_id)
    .bind(claims.user_id())
    .bind(order_number)
    .bind(&body.payment_method)
    .bind(subtotal)
    .bind(&resolved_discount_type)
    .bind(resolved_discount_value)
    .bind(discount_amount)
    .bind(tax_amount)
    .bind(total_amount)
    .bind(body.amount_tendered)
    .bind(change_given)
    .bind(body.tip_amount.unwrap_or(0))
    .bind(body.tip_payment_method.as_deref())
    .bind(body.discount_id)
    .bind(&body.customer_name)
    .bind(&body.notes)
    .bind(idempotency_key)
    .bind(created_at)
    .fetch_one(&mut *tx)
    .await?;

    // Payment splits
    if let Some(splits) = &body.payment_splits {
        for split in splits {
            validate_payment_method(pool.get_ref(), org_id, &split.method).await?;
            sqlx::query(
                "INSERT INTO order_payments (order_id, method, amount, reference) \
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(order.id)
            .bind(&split.method)
            .bind(split.amount)
            .bind(&split.reference)
            .execute(&mut *tx)
            .await?;
        }
    } else {
        sqlx::query(
            "INSERT INTO order_payments (order_id, method, amount) \
             VALUES ($1, $2, $3)",
        )
        .bind(order.id)
        .bind(&body.payment_method)
        .bind(total_amount)
        .execute(&mut *tx)
        .await?;
    }

    let mut order_items_full: Vec<OrderItemFull> = Vec::new();

    for resolved in resolved_items {
        let line_total = resolved.unit_price * resolved.quantity
            + resolved.component_surcharge;
        let snapshot   = serde_json::to_value(&resolved.deductions)
            .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()));

        let has_uncosted_addon = resolved
            .addons
            .iter()
            .any(|a| !a.is_swap && !a.has_ingredients);
        let costs = summarize_line_costs(
            &resolved.deductions,
            resolved.quantity,
            resolved.bundle_id.is_some(),
            has_uncosted_addon,
        );

        let order_item = sqlx::query_as::<_, OrderItem>(
            r#"INSERT INTO order_items
                (order_id, menu_item_id, item_name, name_translations, size_label,
                 unit_price, quantity, line_total, notes, deductions_snapshot,
                 bundle_id, bundle_unit_price, line_cost, unit_cost, cost_missing)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
               RETURNING id, order_id, menu_item_id, item_name, name_translations, size_label,
                         unit_price, quantity, line_total, notes, deductions_snapshot,
                         bundle_id, bundle_unit_price, line_cost, unit_cost, cost_missing"#,
        )
        .bind(order.id)
        .bind(resolved.menu_item_id)
        .bind(&resolved.item_name)
        .bind(&resolved.name_translations)
        .bind(&resolved.size_label)
        .bind(resolved.unit_price)
        .bind(resolved.quantity)
        .bind(line_total)
        .bind(&resolved.notes)
        .bind(snapshot)
        .bind(resolved.bundle_id)
        .bind(resolved.bundle_unit_price)
        .bind(costs.line_cost)
        .bind(costs.unit_cost)
        .bind(costs.cost_missing)
        .fetch_one(&mut *tx)
        .await?;

        if let Some(_b_id) = resolved.bundle_id {
            for comp in &resolved.bundle_components {
                // Per-component cost: every enriched deduction attributed to
                // this component. None when any entry is unknown or the
                // component contributed no deductions (no recipe).
                let comp_entries: Vec<&InventoryDeduction> = resolved
                    .deductions
                    .iter()
                    .filter(|d| d.component_item_id == Some(comp.item_id))
                    .collect();
                let comp_cost: Option<i64> = if comp_entries.is_empty()
                    || comp_entries.iter().any(|d| d.cost_per_unit.is_none())
                {
                    None
                } else {
                    let egp: f64 = comp_entries
                        .iter()
                        .map(|d| d.cost_per_unit.unwrap() * d.quantity)
                        .sum();
                    Some((egp * 100.0).round() as i64)
                };

                sqlx::query(
                    "INSERT INTO order_line_bundle_components \
                        (order_line_id, item_id, quantity, size_label, name_translations, line_cost) \
                     VALUES ($1, $2, $3, $4, $5, $6)",
                )
                .bind(order_item.id)
                .bind(comp.item_id)
                .bind(comp.quantity)
                .bind(&comp.size_label)
                .bind(&comp.name_translations)
                .bind(comp_cost)
                .execute(&mut *tx)
                .await?;

                for addon in &comp.addons {
                    let addon_line = addon.unit_price * addon.quantity * comp.quantity * resolved.quantity;
                    sqlx::query(
                        "INSERT INTO order_line_bundle_component_addons \
                            (order_line_id, component_item_id, addon_item_id, addon_name, name_translations, \
                             unit_price, quantity, line_total) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                    )
                    .bind(order_item.id)
                    .bind(comp.item_id)
                    .bind(addon.addon_item_id)
                    .bind(&addon.addon_name)
                    .bind(&addon.name_translations)
                    .bind(addon.unit_price)
                    .bind(addon.quantity)
                    .bind(addon_line)
                    .execute(&mut *tx)
                    .await?;
                }

                for opt in &comp.optionals {
                    sqlx::query(
                        "INSERT INTO order_line_bundle_component_optionals \
                            (order_line_id, component_item_id, optional_field_id, field_name, name_translations, \
                             price, org_ingredient_id, ingredient_name, ingredient_unit, quantity_deducted) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
                    )
                    .bind(order_item.id)
                    .bind(comp.item_id)
                    .bind(opt.optional_field_id)
                    .bind(&opt.field_name)
                    .bind(&opt.name_translations)
                    .bind(opt.price)
                    .bind(opt.org_ingredient_id)
                    .bind(&opt.ingredient_name)
                    .bind(&opt.ingredient_unit)
                    .bind(opt.quantity_used)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }

        // Addons
        let mut addon_rows: Vec<OrderItemAddon> = Vec::new();
        for addon in &resolved.addons {
            let addon_line = addon.unit_price * addon.quantity * resolved.quantity;

            // Additive addons: rollup of their attributed deduction entries.
            // Swap addons keep NULL — their cost lives in the recipe scope.
            let addon_cost: Option<i64> = if addon.is_swap || !addon.has_ingredients {
                None
            } else {
                let entries: Vec<&InventoryDeduction> = resolved
                    .deductions
                    .iter()
                    .filter(|d| {
                        d.source == "addon" && d.addon_item_id == Some(addon.addon_item_id)
                    })
                    .collect();
                if entries.is_empty() || entries.iter().any(|d| d.cost_per_unit.is_none()) {
                    None
                } else {
                    let egp: f64 = entries
                        .iter()
                        .map(|d| d.cost_per_unit.unwrap() * d.quantity)
                        .sum();
                    Some((egp * 100.0).round() as i64)
                }
            };

            let row = sqlx::query_as::<_, OrderItemAddon>(
                r#"INSERT INTO order_item_addons
                    (order_item_id, addon_item_id, addon_name, name_translations, unit_price, quantity, line_total, line_cost)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                   RETURNING id, order_item_id, addon_item_id, addon_name, name_translations,
                             unit_price, quantity, line_total, line_cost"#,
            )
            .bind(order_item.id)
            .bind(addon.addon_item_id)
            .bind(&addon.addon_name)
            .bind(&addon.name_translations)
            .bind(addon.unit_price)
            .bind(addon.quantity)
            .bind(addon_line)
            .bind(addon_cost)
            .fetch_one(&mut *tx)
            .await?;
            addon_rows.push(row);
        }

        // Optionals
        let mut optional_rows: Vec<OrderItemOptional> = Vec::new();
        for opt in &resolved.optionals {
            // Cost per parent-item unit (matches quantity_deducted semantics):
            // unit cost comes from the enriched deduction for this field.
            let opt_cost: Option<i64> = match (opt.quantity_used, opt.org_ingredient_id) {
                (Some(qty), Some(_)) => resolved
                    .deductions
                    .iter()
                    .find(|d| d.optional_field_id == Some(opt.optional_field_id))
                    .and_then(|d| d.cost_per_unit)
                    .map(|egp| (qty * egp * 100.0).round() as i64),
                // No ingredient linked ⟹ genuinely zero marginal cost.
                _ => Some(0),
            };

            let row = sqlx::query_as::<_, OrderItemOptional>(
                r#"INSERT INTO order_item_optionals
                    (order_item_id, optional_field_id, field_name, name_translations, price,
                     org_ingredient_id, ingredient_name, ingredient_unit, quantity_deducted, cost)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                   RETURNING id, order_item_id, optional_field_id, field_name, name_translations, price,
                             org_ingredient_id, ingredient_name, ingredient_unit, quantity_deducted, cost"#,
            )
            .bind(order_item.id)
            .bind(opt.optional_field_id)
            .bind(&opt.field_name)
            .bind(&opt.name_translations)
            .bind(opt.price)
            .bind(opt.org_ingredient_id)
            .bind(&opt.ingredient_name)
            .bind(&opt.ingredient_unit)
            .bind(opt.quantity_used)
            .bind(opt_cost)
            .fetch_one(&mut *tx)
            .await?;
            optional_rows.push(row);
        }

        // Apply inventory deductions (soft-fail — warn if not tracked)
        for deduction in &resolved.deductions {
            let Some(ing_id) = deduction.org_ingredient_id else {
                tracing::warn!(
                    source     = %deduction.source,
                    ingredient = %deduction.ingredient_name,
                    "Deduction skipped — no org_ingredient_id"
                );
                continue;
            };

            let rows_affected = sqlx::query(
                "UPDATE branch_inventory \
                 SET current_stock = current_stock - $1 \
                 WHERE branch_id = $2 AND org_ingredient_id = $3"
            )
            .bind(deduction.quantity)
            .bind(body.branch_id)
            .bind(ing_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();

            if rows_affected == 0 {
                tracing::warn!(
                    branch_id         = %body.branch_id,
                    org_ingredient_id = %ing_id,
                    source            = %deduction.source,
                    "Ingredient not tracked in branch inventory — skipping"
                );
            }
        }

        order_items_full.push(OrderItemFull {
            item:              order_item,
            addons:            addon_rows,
            optionals:         optional_rows,
            bundle_components: vec![],
        });
    }

    tx.commit().await?;
    Ok(HttpResponse::Created().json(OrderFull { order, items: order_items_full }))
}

// ── GET /orders ───────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/orders",
    tag = "orders",
    params(ListOrdersQuery),
    responses((status = 200, description = "List orders", body = PaginatedOrders), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_orders(
    req:   HttpRequest,
    pool:  web::Data<PgPool>,
    query: web::Query<ListOrdersQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;

    let page = query.page.unwrap_or(1).max(1);
    let default_per_page = if query.shift_id.is_some() {
        DEFAULT_PER_PAGE_SHIFT
    } else {
        DEFAULT_PER_PAGE_BRANCH
    };
    let per_page = query.per_page.unwrap_or(default_per_page).clamp(1, 999999);
    let offset   = (page - 1) * per_page;

    let org_id = claims.org_id().unwrap();

    let parsed_payment_methods = match &query.payment_method {
        Some(pm) => {
            let methods = parse_payment_methods(pool.get_ref(), org_id, pm).await?;
            if methods.is_empty() {
                None
            } else {
                Some(methods)
            }
        }
        None => None,
    };

    let branch_id = match (query.shift_id, query.branch_id) {
        (Some(shift_id), _) => {
            let bid: Option<Uuid> = sqlx::query_scalar(
                "SELECT branch_id FROM shifts WHERE id = $1"
            )
            .bind(shift_id)
            .fetch_optional(pool.get_ref())
            .await?
            .flatten();
            bid.ok_or_else(|| AppError::NotFound("Shift not found".into()))?
        }
        (None, Some(bid)) => bid,
        _ => return Err(AppError::BadRequest("Provide either shift_id or branch_id".into())),
    };

    require_branch_access(pool.get_ref(), &claims, branch_id).await?;

    let scope_condition = if query.shift_id.is_some() { "o.shift_id = $1" } else { "o.branch_id = $1" };
    let scope_id = query.shift_id.unwrap_or(branch_id);

    let mut data_filter  = String::new();
    let mut count_filter = String::new();
    let mut data_idx  = 2i32;
    let mut count_idx = 2i32;

    macro_rules! push_filter {
        ($col:expr, $opt:expr) => {
            if $opt.is_some() {
                data_filter.push_str( &format!(" AND {} ${}", $col, data_idx));
                count_filter.push_str(&format!(" AND {} ${}", $col, count_idx));
                data_idx  += 1;
                count_idx += 1;
            }
        };
    }

    push_filter!("u.name ILIKE",             query.teller_name);
    if parsed_payment_methods.is_some() {
        data_filter.push_str( &format!(" AND o.payment_method::text = ANY(${}::text[])", data_idx));
        count_filter.push_str(&format!(" AND o.payment_method::text = ANY(${}::text[])", count_idx));
        data_idx  += 1;
        count_idx += 1;
    }
    push_filter!("o.status::text =",         query.status);
    push_filter!("o.created_at >=",          query.from);
    push_filter!("o.created_at <=",          query.to);
    push_filter!("o.updated_at >",           query.updated_after);

    let data_sql = format!(
        "{} WHERE {} {} ORDER BY o.created_at DESC LIMIT ${} OFFSET ${}",
        ORDER_SELECT, scope_condition, data_filter, data_idx, data_idx + 1
    );
    let count_sql = format!(
        "SELECT COUNT(*) FROM orders o JOIN users u ON u.id = o.teller_id WHERE {} {}",
        scope_condition, count_filter
    );

    tracing::debug!(
        data_params = data_idx + 1,
        count_params = count_idx - 1,
        "List orders filters constructed"
    );

    macro_rules! bind_filters {
        ($q:expr) => {{
            let mut q = $q;
            if let Some(ref v) = query.teller_name    { q = q.bind(format!("%{}%", v)); }
            if let Some(v)     = &parsed_payment_methods { q = q.bind(v); }
            if let Some(ref v) = query.status         { q = q.bind(v.clone()); }
            if let Some(v)     = query.from            { q = q.bind(v); }
            if let Some(v)     = query.to              { q = q.bind(v); }
            if let Some(v)     = query.updated_after   { q = q.bind(v); }
            q
        }};
    }

    let total: i64 = bind_filters!(sqlx::query_scalar(&count_sql).bind(scope_id))
        .fetch_one(pool.get_ref())
        .await?;

    let summary_sql = format!(
        "SELECT
            COALESCE(SUM(CASE WHEN o.status::text = 'completed' THEN o.total_amount ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN o.status::text = 'completed' THEN 1              ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN o.status::text = 'voided'    THEN 1              ELSE 0 END), 0),
            COALESCE(SUM(o.discount_amount), 0),
            COALESCE(SUM(COALESCE(o.tip_amount, 0)), 0)
         FROM orders o JOIN users u ON u.id = o.teller_id
         WHERE {} {}",
        scope_condition, count_filter
    );

    let (revenue, completed, voided, discounts, tips): (i64, i64, i64, i64, i64) =
        bind_filters!(sqlx::query_as(&summary_sql).bind(scope_id))
            .fetch_one(pool.get_ref())
            .await?;

    let summary = OrderSummary { revenue, completed, voided, discounts, tips };

    let data: Vec<Order> = bind_filters!(
        sqlx::query_as::<_, Order>(&data_sql)
            .bind(scope_id)
    )
    .bind(per_page)
    .bind(offset)
    .fetch_all(pool.get_ref())
    .await?;

    let total_pages = (total as f64 / per_page as f64).ceil() as i64;

    Ok(HttpResponse::Ok().json(PaginatedOrders { data, total, page, per_page, total_pages, summary }))
}

// ── GET /orders/:id ───────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/orders/{order_id}",
    tag = "orders",
    params(("order_id" = Uuid, Path, description = "Order ID")),
    responses((status = 200, description = "Get order", body = OrderFull), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_order(
    req:      HttpRequest,
    pool:     web::Data<PgPool>,
    order_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    let order = fetch_order_or_404(pool.get_ref(), *order_id).await?;
    require_branch_access(pool.get_ref(), &claims, order.branch_id).await?;
    let items = fetch_order_items_full(pool.get_ref(), order.id).await?;
    Ok(HttpResponse::Ok().json(OrderFull { order, items }))
}

// ── POST /orders/:id/void ─────────────────────────────────────

#[utoipa::path(
    post,
    path = "/orders/{order_id}/void",
    tag = "orders",
    params(("order_id" = Uuid, Path, description = "Order ID")),
    request_body = VoidOrderRequest,
    responses((status = 200, description = "Void order", body = Order), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn void_order(
    req:      HttpRequest,
    pool:     web::Data<PgPool>,
    order_id: web::Path<Uuid>,
    body:     web::Json<VoidOrderRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "update").await?;
    let order = fetch_order_or_404(pool.get_ref(), *order_id).await?;
    require_branch_access(pool.get_ref(), &claims, order.branch_id).await?;
    if order.status == "voided" { return Ok(HttpResponse::Ok().json(order)); }
    validate_void_reason(&body.reason)?;
    let voided_at = body.voided_at.unwrap_or_else(chrono::Utc::now);

    let items_to_restore = if body.restore_inventory.unwrap_or(false) {
        Some(fetch_order_items_full(pool.get_ref(), *order_id).await?)
    } else {
        None
    };

    let mut tx = pool.begin().await?;

    let updated = sqlx::query_as::<_, Order>(
        r#"UPDATE orders
           SET status      = 'voided',
               voided_at   = $3,
               void_reason = $2::void_reason,
               voided_by   = $4
           WHERE id = $1
           RETURNING
               id, branch_id, shift_id, teller_id,
               (SELECT name FROM users WHERE id = teller_id) AS teller_name,
               order_number, status::text, payment_method::text,
               subtotal, discount_type::text, discount_value,
               discount_amount, tax_amount, total_amount,
               amount_tendered, change_given, tip_amount, tip_payment_method,
               discount_id, customer_name, notes,
               voided_at, void_reason::text, voided_by, created_at"#,
    )
    .bind(*order_id)
    .bind(&body.reason)
    .bind(voided_at)
    .bind(claims.user_id())
    .fetch_one(&mut *tx)
    .await?;

    if let Some(items) = items_to_restore {
        for item in items {
            if let Some(deductions) = item.item.deductions_snapshot.as_array() {
                for d in deductions {
                    if let (Some(qty), Some(ing_id_str)) = (
                        d.get("quantity").and_then(|v| v.as_f64()),
                        d.get("org_ingredient_id").and_then(|v| v.as_str()),
                    )
                        && let Ok(ing_id) = Uuid::parse_str(ing_id_str) {
                            sqlx::query(
                                "UPDATE branch_inventory \
                                 SET current_stock = current_stock + $1 \
                                 WHERE branch_id = $2 AND org_ingredient_id = $3"
                            )
                            .bind(qty)
                            .bind(order.branch_id)
                            .bind(ing_id)
                            .execute(&mut *tx)
                            .await?;
                        }
                }
            }
        }
    }

    tx.commit().await?;
    Ok(HttpResponse::Ok().json(updated))
}

// ── POST /orders/preview-recipe ───────────────────────────────

#[derive(Deserialize, Serialize, ToSchema)]
pub struct PreviewAddonInput {
    pub addon_item_id: Uuid,
    #[serde(default = "crate::orders::component_resolve::default_qty")]
    pub quantity: i32,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct PreviewRecipeRequest {
    pub menu_item_id:      Uuid,
    pub size_label:        Option<String>,
    pub addons:            Vec<PreviewAddonInput>,
    pub optional_field_ids: Vec<Uuid>,
}

#[derive(Serialize, Clone, ToSchema)]
pub struct PreviewIngredient {
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name: String,
    pub unit:            String,
    pub quantity:        f64,
    pub source:          String,
    pub category:        String,
}

#[utoipa::path(
    post,
    path = "/orders/preview-recipe",
    tag = "orders",
    request_body = PreviewRecipeRequest,
    responses((status = 200, description = "Preview recipe", body = Vec<PreviewIngredient>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn preview_recipe(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<PreviewRecipeRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "create").await?;

    let mut result: Vec<PreviewIngredient> = Vec::new();

    // Base recipe
    let recipe_rows: Vec<(Option<Uuid>, f64, String, String, String)> =
        if let Some(size) = &body.size_label {
            sqlx::query_as(
                r#"SELECT r.org_ingredient_id, r.quantity_used::float8,
                          r.ingredient_name, r.ingredient_unit,
                          COALESCE(i.category, 'general') as category
                   FROM   menu_item_recipes r
                   LEFT JOIN org_ingredients i ON i.id = r.org_ingredient_id
                   WHERE  r.menu_item_id = $1 AND r.size_label = $2::item_size"#,
            )
            .bind(body.menu_item_id)
            .bind(size)
            .fetch_all(pool.get_ref())
            .await?
        } else {
            sqlx::query_as(
                r#"SELECT r.org_ingredient_id, r.quantity_used::float8,
                          r.ingredient_name, r.ingredient_unit,
                          COALESCE(i.category, 'general') as category
                   FROM   menu_item_recipes r
                   LEFT JOIN org_ingredients i ON i.id = r.org_ingredient_id
                   WHERE  r.menu_item_id = $1
                     AND  r.size_label = (
                         SELECT size_label FROM menu_item_recipes
                         WHERE  r.menu_item_id = $1 LIMIT 1
                     )"#,
            )
            .bind(body.menu_item_id)
            .fetch_all(pool.get_ref())
            .await?
        };

    for (ing_id, qty, name, unit, category) in recipe_rows {
        result.push(PreviewIngredient { org_ingredient_id: ing_id, ingredient_name: name, unit, quantity: qty, source: "drink_recipe".into(), category });
    }

    // Addons
    for addon in &body.addons {
        let addon_qty = addon.quantity.max(1) as f64;
        
        let (addon_name, addon_type): (String, String) = sqlx::query_as(
            "SELECT name, type FROM addon_items WHERE id = $1"
        )
        .bind(addon.addon_item_id)
        .fetch_optional(pool.get_ref())
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Addon {} not found", addon.addon_item_id)))?;

        let rows: Vec<(Option<Uuid>, f64, String, String)> = sqlx::query_as(
            "SELECT org_ingredient_id, quantity_used::float8, ingredient_name, ingredient_unit
             FROM   addon_item_ingredients WHERE addon_item_id = $1",
        )
        .bind(addon.addon_item_id)
        .fetch_all(pool.get_ref())
        .await?;

        let target_category = match addon_type.as_str() {
            "milk_type" => Some("milk"),
            "coffee_type" => Some("coffee_bean"),
            _ => None,
        };

        if let Some(cat) = target_category {
            // Find the base recipe's ingredient for this category
            let base_ing_id = result.iter()
                .find(|r| r.source == "drink_recipe" && r.category == cat)
                .and_then(|r| r.org_ingredient_id);

            // Find the addon's ingredient
            let addon_ing_id = rows.first()
                .and_then(|(id, _, _, _)| *id);

            // If both match → this IS the base, not a swap — skip
            let is_base = base_ing_id.is_some()
                && addon_ing_id.is_some()
                && base_ing_id == addon_ing_id;

            if !is_base
                && let Some((_, _, repl_name, repl_unit)) = rows.first() {
                    for r in result.iter_mut() {
                        if r.source == "drink_recipe" && r.category == cat {
                            r.ingredient_name = repl_name.clone();
                            r.unit = repl_unit.clone();
                            r.source = format!("addon_swap:{}", addon_name);
                        }
                    }
                }
            continue;
        }

        for (ing_id, qty, name, unit) in rows {
            result.push(PreviewIngredient {
                org_ingredient_id: ing_id,
                ingredient_name: name,
                unit,
                quantity: qty * addon_qty,
                source: "addon".into(),
                category: "general".into(),
            });
        }
    }

    // Optionals
    for &field_id in &body.optional_field_ids {
        // Explicit type annotation fixes the inference issue
        let row_result = sqlx::query_as::<_, (String, Option<f64>, Option<String>, Option<String>)>(
            "SELECT name, quantity_used::float8, ingredient_name, ingredient_unit
             FROM menu_item_optional_fields
             WHERE id = $1 AND menu_item_id = $2 AND is_active = true",
        )
        .bind(field_id)
        .bind(body.menu_item_id)
        .fetch_optional(pool.get_ref())
        .await?;
    
        if let Some((fname, Some(qty), Some(ing_name), Some(ing_unit))) = row_result {
            result.push(PreviewIngredient {
                org_ingredient_id: None,
                ingredient_name: ing_name,
                unit:            ing_unit,
                quantity:        qty,
                source:          format!("optional:{}", fname),
                category:        "general".into(),
            });
        }
    }
    Ok(HttpResponse::Ok().json(result))
}

// ── Helpers ───────────────────────────────────────────────────

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

async fn fetch_order_or_404(pool: &PgPool, order_id: Uuid) -> Result<Order, AppError> {
    let sql = format!("{} WHERE o.id = $1", ORDER_SELECT);
    sqlx::query_as::<_, Order>(&sql)
        .bind(order_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::NotFound("Order not found".into()))
}

async fn fetch_order_by_idempotency_key(
    pool: &PgPool,
    key:  Uuid,
) -> Result<Option<Order>, AppError> {
    let sql = format!("{} WHERE o.idempotency_key = $1", ORDER_SELECT);
    Ok(sqlx::query_as::<_, Order>(&sql)
        .bind(key)
        .fetch_optional(pool)
        .await?)
}

async fn fetch_order_items_full(
    pool:     &PgPool,
    order_id: Uuid,
) -> Result<Vec<OrderItemFull>, AppError> {
    let items = sqlx::query_as::<_, OrderItem>(
        "SELECT id, order_id, menu_item_id, item_name, name_translations, size_label, \
                unit_price, quantity, line_total, notes, deductions_snapshot, \
                bundle_id, bundle_unit_price, line_cost, unit_cost, cost_missing \
         FROM order_items WHERE order_id = $1 ORDER BY id",
    )
    .bind(order_id)
    .fetch_all(pool)
    .await?;

    let mut result = Vec::new();
    for item in items {
        let addons = sqlx::query_as::<_, OrderItemAddon>(
            "SELECT id, order_item_id, addon_item_id, addon_name, name_translations, \
                    unit_price, quantity, line_total, line_cost \
             FROM order_item_addons WHERE order_item_id = $1 ORDER BY id",
        )
        .bind(item.id)
        .fetch_all(pool)
        .await?;

        let optionals = sqlx::query_as::<_, OrderItemOptional>(
            "SELECT id, order_item_id, optional_field_id, field_name, name_translations, price, \
                    org_ingredient_id, ingredient_name, ingredient_unit, quantity_deducted, cost \
             FROM order_item_optionals WHERE order_item_id = $1 ORDER BY id",
        )
        .bind(item.id)
        .fetch_all(pool)
        .await?;

        let bundle_components = if item.bundle_id.is_some() {
            let comps: Vec<(Uuid, i32, Option<String>, serde_json::Value)> = sqlx::query_as(
                "SELECT item_id, quantity, size_label, name_translations \
                 FROM order_line_bundle_components WHERE order_line_id = $1",
            )
            .bind(item.id)
            .fetch_all(pool)
            .await?;

            let mut out = Vec::new();
            for (comp_item_id, qty, size_label, name_translations) in comps {
                let item_name: String = sqlx::query_scalar(
                    "SELECT name FROM menu_items WHERE id = $1",
                )
                .bind(comp_item_id)
                .fetch_one(pool)
                .await?;

                let comp_addons = sqlx::query_as::<_, OrderBundleComponentAddon>(
                    "SELECT id, order_line_id, component_item_id, addon_item_id, addon_name, name_translations, \
                            unit_price, quantity, line_total \
                     FROM order_line_bundle_component_addons \
                     WHERE order_line_id = $1 AND component_item_id = $2 \
                     ORDER BY id",
                )
                .bind(item.id)
                .bind(comp_item_id)
                .fetch_all(pool)
                .await?;

                let comp_optionals = sqlx::query_as::<_, OrderBundleComponentOptional>(
                    "SELECT id, order_line_id, component_item_id, optional_field_id, field_name, name_translations, price \
                     FROM order_line_bundle_component_optionals \
                     WHERE order_line_id = $1 AND component_item_id = $2 \
                     ORDER BY id",
                )
                .bind(item.id)
                .bind(comp_item_id)
                .fetch_all(pool)
                .await?;

                out.push(OrderBundleComponentFull {
                    item_id: comp_item_id,
                    item_name,
                    name_translations,
                    quantity: qty,
                    size_label,
                    addons: comp_addons,
                    optionals: comp_optionals,
                });
            }
            out
        } else {
            vec![]
        };

        result.push(OrderItemFull {
            item,
            addons,
            optionals,
            bundle_components,
        });
    }
    Ok(result)
}

async fn require_branch_access(
    pool:      &PgPool,
    claims:    &Claims,
    branch_id: Uuid,
) -> Result<(), AppError> {
    if claims.role == UserRole::SuperAdmin { return Ok(()); }

    let branch_org: Option<Uuid> = sqlx::query_scalar(
        "SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL"
    )
    .bind(branch_id)
    .fetch_optional(pool)
    .await?
    .flatten();

    let branch_org = branch_org
        .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    if claims.org_id() != Some(branch_org) {
        return Err(AppError::Forbidden("Branch belongs to a different org".into()));
    }
    if claims.role == UserRole::OrgAdmin { return Ok(()); }

    let assigned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM user_branch_assignments \
         WHERE user_id = $1 AND branch_id = $2)"
    )
    .bind(claims.user_id())
    .bind(branch_id)
    .fetch_one(pool)
    .await?;

    if !assigned {
        return Err(AppError::Forbidden("Not assigned to this branch".into()));
    }
    Ok(())
}

async fn validate_payment_method(pool: &PgPool, org_id: Uuid, method: &str) -> Result<(), AppError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM org_payment_methods WHERE org_id = $1 AND name = $2 AND is_active = true)"
    )
    .bind(org_id)
    .bind(method)
    .fetch_one(pool)
    .await?;

    if exists {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!("Invalid or inactive payment_method: {}", method)))
    }
}

async fn parse_payment_methods(pool: &PgPool, org_id: Uuid, raw: &str) -> Result<Vec<String>, AppError> {
    let mut methods = Vec::new();
    for part in raw.split(',') {
        let trimmed = part.trim();
        if !trimmed.is_empty() {
            validate_payment_method(pool, org_id, trimmed).await?;
            methods.push(trimmed.to_string());
        }
    }
    Ok(methods)
}

#[allow(dead_code)]
async fn fetch_order_payments(pool: &PgPool, order_id: Uuid)
    -> Result<Vec<OrderPayment>, AppError>
{
    let rows = sqlx::query_as::<_, OrderPayment>(
        "SELECT id, order_id, method::text AS method, amount, reference \
         FROM order_payments WHERE order_id = $1 ORDER BY id"
    )
    .bind(order_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

fn validate_discount_type(dt: &str) -> Result<(), AppError> {
    match dt {
        "percentage" | "fixed" => Ok(()),
        _ => Err(AppError::BadRequest("discount_type must be 'percentage' or 'fixed'".into())),
    }
}

fn validate_void_reason(reason: &str) -> Result<(), AppError> {
    match reason {
        "customer_request" | "wrong_order" | "quality_issue" | "other" => Ok(()),
        _ => Err(AppError::BadRequest("Invalid void_reason".into())),
    }
}

#[allow(unused_assignments)]
#[utoipa::path(
    get,
    path = "/orders/export",
    tag = "orders",
    params(ExportOrdersQuery),
    responses((status = 200, description = "Export orders", body = ExportResponse), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn export_orders(
    req:   HttpRequest,
    pool:  web::Data<PgPool>,
    query: web::Query<ExportOrdersQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;

    let branch_id = match (query.shift_id, query.branch_id) {
        (Some(shift_id), _) => {
            let bid: Option<Uuid> = sqlx::query_scalar(
                "SELECT branch_id FROM shifts WHERE id = $1"
            )
            .bind(shift_id)
            .fetch_optional(pool.get_ref())
            .await?
            .flatten();
            bid.ok_or_else(|| AppError::NotFound("Shift not found".into()))?
        }
        (None, Some(bid)) => bid,
        _ => return Err(AppError::BadRequest("Provide either shift_id or branch_id".into())),
    };

    require_branch_access(pool.get_ref(), &claims, branch_id).await?;

    let org_id = claims.org_id().unwrap();

    let parsed_payment_methods = match &query.payment_method {
        Some(pm) => {
            let methods = parse_payment_methods(pool.get_ref(), org_id, pm).await?;
            if methods.is_empty() {
                None
            } else {
                Some(methods)
            }
        }
        None => None,
    };

    let scope_condition = if query.shift_id.is_some() { "o.shift_id = $1" } else { "o.branch_id = $1" };
    let scope_id = query.shift_id.unwrap_or(branch_id);

    let mut filter  = String::new();
    #[allow(unused_assignments)]
    let mut idx  = 2i32;

    macro_rules! push_export_filter {
        ($col:expr, $opt:expr) => {
            if $opt.is_some() {
                filter.push_str(&format!(" AND {} ${}", $col, idx));
                idx += 1;
            }
        };
    }

    push_export_filter!("u.name ILIKE",             query.teller_name);
    if parsed_payment_methods.is_some() {
        filter.push_str(&format!(" AND o.payment_method::text = ANY(${}::text[])", idx));
        idx += 1;
    }
    push_export_filter!("o.status::text =",         query.status);
    push_export_filter!("o.created_at >=",          query.from);
    push_export_filter!("o.created_at <=",          query.to);

    macro_rules! bind_export_filters {
        ($q:expr) => {{
            let mut q = $q;
            if let Some(ref v) = query.teller_name    { q = q.bind(format!("%{}%", v)); }
            if let Some(v)     = &parsed_payment_methods { q = q.bind(v); }
            if let Some(ref v) = query.status         { q = q.bind(v.clone()); }
            if let Some(v)     = query.from            { q = q.bind(v); }
            if let Some(v)     = query.to              { q = q.bind(v); }
            q
        }};
    }

    let count_sql = format!(
        "SELECT COUNT(*) FROM orders o JOIN users u ON u.id = o.teller_id WHERE {} {}",
        scope_condition, filter
    );

    let total: i64 = bind_export_filters!(sqlx::query_scalar(&count_sql).bind(scope_id))
        .fetch_one(pool.get_ref())
        .await?;

    if total > 50_000 {
        return Err(AppError::BadRequest(format!(
            "Export too large: {} orders match. Narrow the date range or add filters (limit: 50000).",
            total
        )));
    }

    let summary_sql = format!(
        "SELECT \
            COALESCE(SUM(CASE WHEN o.status::text = 'completed' THEN o.total_amount ELSE 0 END), 0), \
            COALESCE(SUM(CASE WHEN o.status::text = 'completed' THEN 1              ELSE 0 END), 0), \
            COALESCE(SUM(CASE WHEN o.status::text = 'voided'    THEN 1              ELSE 0 END), 0), \
            COALESCE(SUM(o.discount_amount), 0), \
            COALESCE(SUM(COALESCE(o.tip_amount, 0)), 0) \
         FROM orders o JOIN users u ON u.id = o.teller_id \
         WHERE {} {}",
        scope_condition, filter
    );

    let (revenue, completed, voided, discounts, tips): (i64, i64, i64, i64, i64) =
        bind_export_filters!(sqlx::query_as(&summary_sql).bind(scope_id))
            .fetch_one(pool.get_ref())
            .await?;

    let summary = OrderSummary { revenue, completed, voided, discounts, tips };

    let data_sql = format!(
        "{} WHERE {} {} ORDER BY o.created_at DESC",
        ORDER_SELECT, scope_condition, filter
    );

    let orders: Vec<Order> = bind_export_filters!(
        sqlx::query_as::<_, Order>(&data_sql)
            .bind(scope_id)
    )
    .fetch_all(pool.get_ref())
    .await?;

    let order_ids: Vec<Uuid> = orders.iter().map(|o| o.id).collect();
    let payments_rows = if order_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as::<_, OrderPayment>(
            "SELECT id, order_id, method::text AS method, amount, reference \
             FROM order_payments WHERE order_id = ANY($1) ORDER BY id"
        )
        .bind(&order_ids)
        .fetch_all(pool.get_ref())
        .await?
    };

    use std::collections::HashMap;
    let mut payments_by_order: HashMap<Uuid, Vec<OrderPayment>> = HashMap::new();
    for p in payments_rows {
        payments_by_order.entry(p.order_id).or_default().push(p);
    }

    let mut data = Vec::with_capacity(orders.len());
    for order in orders {
        let order_id = order.id;
        let items = fetch_order_items_full(pool.get_ref(), order_id).await?;
        let payments = payments_by_order.remove(&order_id).unwrap_or_default();
        data.push(OrderExport { order, items, payments });
    }

    use std::collections::HashSet;

    // Collect every distinct org_ingredient_id from all deduction snapshots
    let ingredient_ids: Vec<Uuid> = data.iter()
        .flat_map(|o| o.items.iter())
        .flat_map(|i| {
            i.item.deductions_snapshot.as_array()
                .into_iter()
                .flatten()
                .filter_map(|d| d.get("org_ingredient_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok()))
        })
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let ingredient_costs: HashMap<Uuid, i32> = if ingredient_ids.is_empty() {
        HashMap::new()
    } else {
        let decimal_costs: Vec<(Uuid, Decimal)> = sqlx::query_as::<_, (Uuid, Decimal)>(
            "SELECT id, cost_per_unit FROM org_ingredients WHERE id = ANY($1)"
        )
        .bind(&ingredient_ids)
        .fetch_all(pool.get_ref())
        .await?;

        decimal_costs.into_iter()
            .map(|(id, cost)| {
                // Convert from standard currency Decimal (e.g. 5.50 EGP) to i32 piastres (e.g. 550)
                let piastres = (cost * Decimal::from(100))
                    .round()
                    .to_i32()
                    .unwrap_or(0);
                (id, piastres)
            })
            .collect()
    };

    Ok(HttpResponse::Ok().json(ExportResponse {
        data,
        total,
        generated_at: chrono::Utc::now(),
        summary,
        ingredient_costs,
    }))
}
