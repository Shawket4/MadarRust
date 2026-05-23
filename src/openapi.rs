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
        contact(name = "Sufrix", url = "https://sufrix.app")
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
        (name = "adjustments",  description = "Inventory adjustments and inter-branch transfers."),
        (name = "orders",       description = "Order lifecycle, split payments, voids, aggregator handling."),
        (name = "shifts",       description = "Shift open/close, cash reconciliation, printable reports."),
        (name = "discounts",    description = "Discount definitions and applicability rules."),
        (name = "bundles",      description = "Combo bundles and bundle pricing."),
        (name = "reports",      description = "Sales analytics and reporting."),
        (name = "menu_advisor", description = "Read-only pricing, bundle, and removal suggestions. Never edits menus — the differentiator vs. generic POS."),
        (name = "uploads",      description = "Logo and image uploads.")
    ),
paths(
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