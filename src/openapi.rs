//! Root OpenAPI document for the Sufrix backend.
//!
//! - [`ApiDoc`] is the single `#[derive(OpenApi)]` aggregator.
//! - Each handler annotated with `#[utoipa::path]` is registered here via
//!   the `paths(...)` list. Schemas referenced from those handlers are
//!   pulled in automatically; `components(schemas(...))` is for types
//!   that need explicit registration (rare).
//! - Bearer-JWT security scheme is added by `SecurityAddon`. Auth-gated
//!   handlers opt in with `security(("bearer_jwt" = []))` in their path
//!   attribute.
//!
//! Adding a new module's handlers means appending entries to `paths(...)`.

use utoipa::{
    openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme},
    Modify, OpenApi,
};

#[derive(OpenApi)]
#[openapi(
    info(
    title = "Sufrix API",
    version = env!("CARGO_PKG_VERSION"),
    description = "Sufrix POS — multi-tenant cafe and restaurant management. \
                   The Rust backend is the source of truth for this spec; \
                   the Flutter teller app and React management dashboard \
                   consume the generated openapi.json.",
    contact(name = "Sufrix", url = "https://sufrix.app"),
    license(name = "Proprietary", identifier = "LicenseRef-Sufrix-Proprietary")
),
    servers(
        (url = "http://localhost:8080", description = "Local development"),
        (url = "https://api.sufrix.app", description = "Production")
    ),
    modifiers(&SecurityAddon),
    tags(
        (name = "auth",         description = "Email/password and PIN login, JWT issuance."),
        (name = "orgs",         description = "Organization CRUD. Super-admin only."),
        (name = "branches",     description = "Branch CRUD within an organization."),
        (name = "users",        description = "User accounts, roles, and branch assignments."),
        (name = "permissions",  description = "Role permissions and per-user permission overrides."),
        (name = "menu",         description = "Categories, menu items, sizes, and addon groups."),
        (name = "recipes",      description = "Org-level recipes referencing the ingredient catalog."),
        (name = "inventory",    description = "Org-level ingredient catalog and branch-level stock."),
        (name = "orders",       description = "Order lifecycle, split payments, voids, aggregator handling."),
        (name = "shifts",       description = "Shift open/close, cash reconciliation, printable reports."),
        (name = "discounts",    description = "Discount definitions and applicability rules."),
        (name = "bundles",      description = "Combo bundles and bundle pricing."),
        (name = "reports",      description = "Sales analytics and reporting."),
        (name = "menu_advisor", description = "Read-only pricing, bundle, and removal suggestions. Never edits menus — the differentiator vs. generic POS."),
        (name = "uploads",      description = "Logo and image uploads."),
        (name = "payment_methods", description = "Dynamic payment methods configuration."),
        (name = "costing",      description = "Canonical recipe/addon cost rollups in piastres. NULL cost = unknown, never zero.")
    ),
paths(
        // ── costing ─────────────────────────────────────────────────
        crate::costing::handlers::list_sku_costs,
        crate::costing::handlers::list_addon_costs,
        crate::reports::handlers::branch_menu_engineering,
        // ── auth ────────────────────────────────────────────────────
        crate::auth::handlers::login,
        crate::auth::handlers::me,
        crate::auth::handlers::permissions,
        // ── branches ────────────────────────────────────────────────
        crate::branches::handlers::list_branches,
        crate::branches::handlers::get_branch,
        crate::branches::handlers::create_branch,
        crate::branches::handlers::update_branch,
        crate::branches::handlers::delete_branch,
        // ── orgs ────────────────────────────────────────────────────
        crate::orgs::handlers::list_orgs,
        crate::orgs::handlers::get_org,
        crate::orgs::handlers::create_org,
        crate::orgs::handlers::update_org,
        crate::orgs::handlers::upload_org_logo,
        crate::orgs::handlers::delete_org,
        crate::orgs::handlers::list_public_orgs,
        // ── users ───────────────────────────────────────────────────
        crate::users::handlers::list_users,
        crate::users::handlers::get_user,
        crate::users::handlers::create_user,
        crate::users::handlers::update_user,
        crate::users::handlers::delete_user,
        crate::users::handlers::assign_branch,
        crate::users::handlers::unassign_branch,
        crate::users::handlers::list_user_branches,
        // ── permissions ─────────────────────────────────────────────
        crate::permissions::handlers::get_user_permissions,
        crate::permissions::handlers::get_permission_matrix,
        crate::permissions::handlers::upsert_user_permission,
        crate::permissions::handlers::delete_user_permission,
        crate::permissions::handlers::get_role_permissions,
        crate::permissions::handlers::upsert_role_permission,
        // ── menu ────────────────────────────────────────────────────
        crate::menu::handlers::list_categories,
        crate::menu::handlers::create_category,
        crate::menu::handlers::update_category,
        crate::menu::handlers::delete_category,
        crate::menu::handlers::list_menu_items,
        crate::menu::handlers::create_menu_item,
        crate::menu::handlers::get_menu_item,
        crate::menu::handlers::update_menu_item,
        crate::menu::handlers::delete_menu_item,
        crate::menu::handlers::upsert_size,
        crate::menu::handlers::delete_size,
        crate::menu::handlers::list_addon_items,
        crate::menu::handlers::create_addon_item,
        crate::menu::handlers::update_addon_item,
        crate::menu::handlers::delete_addon_item,
        crate::menu::handlers::list_addon_slots,
        crate::menu::handlers::create_addon_slot,
        crate::menu::handlers::update_addon_slot,
        crate::menu::handlers::delete_addon_slot,
        crate::menu::handlers::list_addon_overrides,
        crate::menu::handlers::upsert_addon_override,
        crate::menu::handlers::delete_addon_override,
        crate::menu::handlers::list_optional_fields,
        crate::menu::handlers::create_optional_field,
        crate::menu::handlers::update_optional_field,
        crate::menu::handlers::delete_optional_field,
        crate::menu::handlers::get_public_menu,
        // ── uploads ───────────────────────────────────────────────────
        crate::uploads::handlers::upload_menu_item_image,
        // ── inventory ─────────────────────────────────────────────────
        crate::inventory::handlers::list_catalog,
        crate::inventory::handlers::create_catalog_item,
        crate::inventory::handlers::update_catalog_item,
        crate::inventory::handlers::delete_catalog_item,
        crate::inventory::handlers::list_branch_stock,
        crate::inventory::handlers::add_to_branch_stock,
        crate::inventory::handlers::update_branch_stock,
        crate::inventory::handlers::remove_from_branch_stock,
        crate::inventory::handlers::create_adjustment,
        crate::inventory::handlers::list_adjustments,
        crate::inventory::handlers::create_transfer,
        crate::inventory::handlers::list_transfers,
        crate::inventory::handlers::update_transfer,
        crate::inventory::handlers::delete_transfer,
        // ── recipes ───────────────────────────────────────────────────
        crate::recipes::handlers::list_drink_recipes,
        crate::recipes::handlers::upsert_drink_recipe,
        crate::recipes::handlers::delete_drink_recipe,
        crate::recipes::handlers::list_addon_ingredients,
        crate::recipes::handlers::upsert_addon_ingredient,
        crate::recipes::handlers::delete_addon_ingredient,
        // ── discounts ─────────────────────────────────────────────────
        crate::discounts::handlers::list_discounts,
        crate::discounts::handlers::create_discount,
        crate::discounts::handlers::update_discount,
        crate::discounts::handlers::delete_discount,
        // ── bundles ───────────────────────────────────────────────────
        crate::bundles::handlers::list_bundles,
        crate::bundles::handlers::create_bundle,
        crate::bundles::handlers::available_bundles,
        crate::bundles::handlers::get_bundle,
        crate::bundles::handlers::update_bundle,
        crate::bundles::handlers::delete_bundle,
        crate::bundles::handlers::activate_bundle,
        crate::bundles::handlers::archive_bundle,
        crate::bundles::handlers::bundle_performance,
        // ── shifts ────────────────────────────────────────────────────
        crate::shifts::handlers::get_current_shift,
        crate::shifts::handlers::open_shift,
        crate::shifts::handlers::list_shifts,
        crate::shifts::handlers::get_shift,
        crate::shifts::handlers::get_shift_report,
        crate::shifts::handlers::add_cash_movement,
        crate::shifts::handlers::list_cash_movements,
        crate::shifts::handlers::close_shift,
        crate::shifts::handlers::force_close_shift,
        crate::shifts::handlers::delete_shift,
        // ── orders ────────────────────────────────────────────────────
        crate::orders::handlers::create_order,
        crate::orders::handlers::list_orders,
        crate::orders::handlers::get_order,
        crate::orders::handlers::void_order,
        crate::orders::handlers::preview_recipe,
        crate::orders::handlers::export_orders,
        // ── reports ───────────────────────────────────────────────────
        crate::reports::handlers::shift_summary,
        crate::reports::handlers::shift_inventory_discrepancies,
        crate::reports::handlers::shift_deductions,
        crate::reports::handlers::branch_sales,
        crate::reports::handlers::branch_stock,
        crate::reports::handlers::branch_sales_timeseries,
        crate::reports::handlers::branch_teller_stats,
        crate::reports::handlers::branch_addon_sales,
        crate::reports::handlers::org_branch_comparison,
        crate::reports::handlers::branch_bundle_sales,
        crate::reports::handlers::branch_combined_item_sales,
        // ── payment_methods ───────────────────────────────────────────
        crate::payment_methods::handlers::list_payment_methods,
        crate::payment_methods::handlers::create_payment_method,
        crate::payment_methods::handlers::update_payment_method,
        crate::payment_methods::handlers::activate_payment_method,
        crate::payment_methods::handlers::deactivate_payment_method,
    ),
    components(schemas(
        // Most schemas are pulled in transitively via path responses, but
        // listing the shared error body explicitly makes it discoverable.
        
        crate::errors::ErrorBody,
    ))
)]
pub struct ApiDoc;

struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi
            .components
            .as_mut()
            .expect("OpenAPI components should exist after derive");

        components.add_security_scheme(
            "bearer_jwt",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("JWT")
                    .description(Some(
                        "JWT obtained from `POST /auth/login` (email/password) or \
                         `POST /auth/pin-login` (teller PIN). Send as \
                         `Authorization: Bearer <token>`.",
                    ))
                    .build(),
            ),
        );
    }
}