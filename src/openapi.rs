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
        (name = "stocktakes",   description = "Standalone physical-count sessions that reconcile branch stock and post variance movements."),
        (name = "purchasing",   description = "Suppliers, purchase orders, and receiving (weighted-average cost + purchase_in movements)."),
        (name = "discounts",    description = "Discount definitions and applicability rules."),
        (name = "bundles",      description = "Combo bundles and bundle pricing."),
        (name = "reports",      description = "Sales analytics and reporting."),
        (name = "menu_advisor", description = "Read-only pricing, bundle, and removal suggestions. Never edits menus — the differentiator vs. generic POS."),
        (name = "uploads",      description = "Logo and image uploads."),
        (name = "payment_methods", description = "Dynamic payment methods configuration."),
        (name = "costing",      description = "Canonical recipe/addon cost rollups in piastres. NULL cost = unknown, never zero."),
        (name = "delivery",     description = "Delivery config (settings, zones, org defaults), the staff queue, status transitions, finalize, and cancel/waste."),
        (name = "delivery-public", description = "Unauthenticated, rate-limited customer endpoints: branch selector, channel menu, OSRM quote, WhatsApp OTP, order intake."),
        (name = "whatsapp",     description = "Super-admin relay to the private WhatsApp gateway: QR pairing, link status, logout, and the global send pause switch."),
        (name = "qr",           description = "Branded A6 QR card generator (PNG/SVG) and plain receipt QR. Renders a Shlink short URL into a print-perfect, Sufrix-styled image.")
    ),
paths(
        // ── costing ─────────────────────────────────────────────────
        crate::costing::handlers::list_sku_costs,
        crate::orgs::onboarding::get_onboarding,
        crate::orgs::onboarding::complete_onboarding,
        crate::costing::handlers::list_addon_costs,
        crate::reports::handlers::branch_menu_engineering,
        // ── auth ────────────────────────────────────────────────────
        crate::auth::handlers::login,
        crate::auth::handlers::resolve_branch,
        crate::auth::handlers::me,
        crate::auth::handlers::permissions,
        // ── branches ────────────────────────────────────────────────
        crate::branches::handlers::list_branches,
        crate::branches::handlers::get_branch,
        crate::branches::handlers::create_branch,
        crate::branches::handlers::update_branch,
        crate::branches::handlers::delete_branch,
        crate::branches::handlers::list_timezones,
        // ── orgs ────────────────────────────────────────────────────
        crate::orgs::handlers::list_orgs,
        crate::orgs::handlers::get_org,
        crate::orgs::handlers::offline_auth_bundle,
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
        crate::menu::handlers::list_menu_catalog,
        crate::menu::handlers::create_menu_item,
        crate::menu::handlers::get_menu_item,
        crate::menu::handlers::update_menu_item,
        crate::menu::handlers::delete_menu_item,
        crate::menu::handlers::upsert_size,
        crate::menu::handlers::delete_size,
        crate::menu::handlers::list_addon_items,
        crate::menu::handlers::list_addon_catalog,
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
        crate::menu::handlers::put_allowed_addons,
        crate::menu::handlers::list_optional_fields,
        crate::menu::handlers::create_optional_field,
        crate::menu::handlers::update_optional_field,
        crate::menu::handlers::delete_optional_field,
        crate::menu::handlers::list_branch_menu_overrides,
        crate::menu::handlers::upsert_branch_menu_override,
        crate::menu::handlers::delete_branch_menu_override,
        crate::menu::handlers::list_branch_addon_overrides,
        crate::menu::handlers::upsert_branch_addon_override,
        crate::menu::handlers::delete_branch_addon_override,
        // ── uploads ───────────────────────────────────────────────────
        crate::uploads::handlers::upload_menu_item_image,
        // ── inventory ─────────────────────────────────────────────────
        crate::inventory::handlers::list_catalog,
        crate::inventory::handlers::create_catalog_item,
        crate::inventory::handlers::update_catalog_item,
        crate::inventory::handlers::delete_catalog_item,
        crate::inventory::handlers::get_inventory_settings,
        crate::inventory::handlers::update_inventory_settings,
        crate::inventory::handlers::list_branch_stock,
        crate::inventory::handlers::add_to_branch_stock,
        crate::inventory::handlers::update_branch_stock,
        crate::inventory::handlers::remove_from_branch_stock,
        crate::inventory::handlers::list_movements,
        crate::inventory::handlers::create_waste,
        crate::inventory::handlers::list_waste,
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
        // ── stocktakes ────────────────────────────────────────────────
        crate::stocktakes::handlers::create_stocktake,
        crate::stocktakes::handlers::list_stocktakes,
        crate::stocktakes::handlers::get_stocktake,
        crate::stocktakes::handlers::upsert_items,
        crate::stocktakes::handlers::finalize_stocktake,
        crate::stocktakes::handlers::cancel_stocktake,
        crate::stocktakes::handlers::variance_report,
        // ── purchasing ────────────────────────────────────────────────
        crate::purchasing::handlers::create_supplier,
        crate::purchasing::handlers::list_suppliers,
        crate::purchasing::handlers::update_supplier,
        crate::purchasing::handlers::delete_supplier,
        crate::purchasing::handlers::create_order,
        crate::purchasing::handlers::list_orders,
        crate::purchasing::handlers::list_org_orders,
        crate::purchasing::handlers::reorder_suggestions,
        crate::purchasing::handlers::create_return,
        crate::purchasing::handlers::list_po_receipts,
        crate::purchasing::handlers::get_order,
        crate::purchasing::handlers::submit_order,
        crate::purchasing::handlers::receive_order,
        crate::purchasing::handlers::cancel_order,
        // ── orders ────────────────────────────────────────────────────
        crate::orders::handlers::create_order,
        crate::orders::handlers::list_orders,
        crate::orders::handlers::get_order,
        crate::orders::handlers::void_order,
        crate::orders::handlers::preview_recipe,
        crate::orders::handlers::export_orders,
        // ── reports ───────────────────────────────────────────────────
        crate::reports::handlers::shift_summary,
        crate::reports::handlers::shift_deductions,
        crate::reports::handlers::branch_sales,
        crate::reports::handlers::branch_stock,
        crate::reports::handlers::branch_sales_timeseries,
        crate::reports::handlers::branch_sales_peak_hours,
        crate::reports::handlers::branch_teller_stats,
        crate::reports::handlers::branch_addon_sales,
        crate::reports::handlers::org_branch_comparison,
        crate::reports::handlers::branch_delivery_sales,
        crate::reports::handlers::branch_bundle_sales,
        crate::reports::handlers::branch_combined_item_sales,
        crate::reports::handlers::branch_inventory_valuation,
        crate::reports::handlers::org_inventory_valuation,
        crate::reports::handlers::org_low_stock,
        crate::reports::handlers::branch_low_stock,
        crate::reports::handlers::branch_consumption,
        crate::reports::handlers::branch_waste_report,
        crate::reports::handlers::branch_shrinkage,
        crate::reports::handlers::org_consumption,
        crate::reports::handlers::org_waste_report,
        crate::reports::handlers::org_shrinkage,
        // ── payment_methods ───────────────────────────────────────────
        crate::payment_methods::handlers::list_payment_methods,
        crate::payment_methods::handlers::create_payment_method,
        crate::payment_methods::handlers::update_payment_method,
        crate::payment_methods::handlers::activate_payment_method,
        crate::payment_methods::handlers::deactivate_payment_method,
        // ── menu_advisor ──────────────────────────────────────────────
        crate::menu_advisor::handlers::create_run_handler,
        crate::menu_advisor::handlers::list_runs_handler,
        crate::menu_advisor::handlers::get_latest_run_handler,
        crate::menu_advisor::handlers::get_active_run_handler,
        crate::menu_advisor::handlers::get_run_handler,
        crate::menu_advisor::handlers::list_price_suggestions_handler,
        crate::menu_advisor::handlers::list_bundle_suggestions_handler,
        crate::menu_advisor::handlers::list_removal_scenarios_handler,
        crate::menu_advisor::handlers::get_price_suggestion_handler,
        crate::menu_advisor::handlers::get_bundle_suggestion_handler,
        crate::menu_advisor::handlers::get_removal_scenario_handler,
        crate::menu_advisor::handlers::record_decision_handler,
        crate::menu_advisor::handlers::list_decisions_handler,
        crate::menu_advisor::handlers::get_calibration_handler,
        crate::menu_advisor::handlers::set_bundle_promoted_handler,
        crate::menu_advisor::handlers::get_latest_item_kpi_handler,
        // ── delivery (admin + staff) ──────────────────────────────────
        crate::delivery::settings::get_branch_settings,
        crate::delivery::settings::put_branch_settings,
        crate::delivery::settings::set_accepting,
        crate::delivery::settings::list_zones,
        crate::delivery::settings::create_zone,
        crate::delivery::settings::update_zone,
        crate::delivery::settings::delete_zone,
        crate::delivery::settings::list_channel_overrides,
        crate::delivery::settings::upsert_channel_override,
        crate::delivery::settings::delete_channel_override,
        crate::delivery::settings::list_channel_addon_overrides,
        crate::delivery::settings::upsert_channel_addon_override,
        crate::delivery::settings::delete_channel_addon_override,
        crate::delivery::staff::list_delivery_orders,
        crate::delivery::staff::get_delivery_order,
        crate::delivery::staff::stream_delivery_orders,
        crate::delivery::staff::set_status,
        crate::delivery::staff::set_prep_time,
        crate::delivery::staff::cancel_delivery_order,
        crate::delivery::staff::finalize_delivery_order,
        // ── delivery (public) ─────────────────────────────────────────
        crate::delivery::public::public_branches,
        crate::delivery::public::public_menu,
        crate::delivery::public::delivery_quote,
        crate::delivery::public::otp_request,
        crate::delivery::public::otp_verify,
        crate::delivery::public::create_delivery_order,
        crate::delivery::public::track_delivery_order,
        crate::delivery::public::guest_order_history,
        crate::delivery::public::guest_past_locations,
        // ── whatsapp gateway relay (super-admin) ──────────────────────
        crate::delivery::gateway::status,
        crate::delivery::gateway::pair,
        crate::delivery::gateway::logout,
        crate::delivery::gateway::pause,
        // ── qr (dynamic short-URL + branded card) ────────────────────
        crate::qr_card::handlers::org_qr,
        crate::qr_card::handlers::branch_qr,
        crate::qr_card::handlers::create_table,
        crate::qr_card::handlers::list_tables,
        crate::qr_card::handlers::delete_table,
        crate::qr_card::handlers::table_qr,
        crate::qr_card::handlers::delivery_order_qr,
        crate::qr_card::handlers::create_marketing_link,
        crate::qr_card::handlers::list_marketing_links,
    ),
    components(schemas(
        // Most schemas are pulled in transitively via path responses, but
        // listing the shared error body explicitly makes it discoverable.
        crate::errors::ErrorBody,
        crate::auth::handlers::ResolveBranchRequest,
        crate::auth::handlers::ResolveBranchResponse,
        // GET /orders?include_items=true response shape (the annotation's
        // `body` documents the default PaginatedOrders variant).
        crate::orders::handlers::PaginatedOrdersFull,
        // Delivery context nested on the single-order detail (GET /orders/{id}).
        crate::orders::handlers::OrderDeliveryInfo,
        crate::shifts::handlers::PaginatedShifts,
        crate::menu::handlers::PaginatedMenuItems,
        crate::menu::handlers::MenuItemWithCosts,
        crate::menu::handlers::PaginatedAddonItems,
        crate::menu::handlers::BranchMenuOverride,
        crate::menu::handlers::BranchMenuOverrideInput,
        crate::menu::handlers::BranchSizeOverride,
        crate::menu::handlers::BranchSizeOverrideInput,
        crate::menu::handlers::BranchAddonOverride,
        crate::menu::handlers::BranchAddonOverrideInput,
        // ── delivery ──────────────────────────────────────────────────
        crate::delivery::settings::BranchDeliverySettings,
        crate::delivery::settings::DeliveryZone,
        crate::delivery::settings::ChannelMenuOverride,
        crate::delivery::settings::ChannelAddonOverride,
        crate::delivery::staff::DeliveryOrder,
        crate::delivery::staff::FinalizeResponse,
        crate::delivery::hub::DeliveryEvent,
        crate::delivery::public::PublicBranch,
        crate::delivery::public::DeliveryMenu,
        crate::delivery::public::DeliveryMenuDiscount,
        crate::delivery::public::DeliveryMenuItem,
        crate::delivery::public::DeliveryMenuSize,
        crate::delivery::public::DeliveryMenuCategory,
        crate::delivery::public::DeliveryAddonOption,
        crate::delivery::public::DeliveryOptionalField,
        crate::delivery::public::QuoteResponse,
        crate::delivery::public::OtpRequestResponse,
        crate::delivery::public::OtpVerifyResponse,
        crate::delivery::public::DeliveryTracking,
        crate::delivery::snapshot::CartLineInput,
        crate::delivery::gateway::WhatsappStatus,
        crate::delivery::gateway::PauseInput,
        // ── qr ────────────────────────────────────────────────────────
        crate::qr_card::handlers::QrResponse,
        crate::qr_card::handlers::MarketingLink,
        crate::qr_card::handlers::CreateMarketingLinkRequest,
        crate::qr_card::db::BranchTable,
        crate::qr_card::db::CreateTableRequest,
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