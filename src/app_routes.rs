//! Every API route the server mounts, in one place, so `main.rs` and the
//! route-coverage guard (`route_guard_tests`) mount exactly the same set.
//! Swagger UI, the demo playground and the static file services stay in
//! `main.rs` (they are conditional or not API routes).

use actix_web::web;
use sqlx::PgPool;

use crate::{
    ai, analytics, auth, bookings, branches, bundles, costing, customers, delivery, devices,
    discounts, insights, integrations, inventory, kitchen, loyalty, menu, orders, orgs,
    payment_methods, permissions, purchasing, push, qr_card, realtime, recipes, refunds, reports,
    reservations, staff, stocktakes, sync, tickets, tills, uploads, users,
};

pub fn configure_api(cfg: &mut web::ServiceConfig, read_pool: web::Data<PgPool>) {
    cfg.route(
        "/health",
        web::get().to(|| async { actix_web::HttpResponse::Ok().finish() }),
    )
    .configure(auth::routes::configure)
    .configure(orgs::routes::configure)
    .configure(users::routes::configure)
    .configure(permissions::routes::configure)
    .configure(crate::authz::api::configure)
    .configure(branches::routes::configure)
    .configure(menu::routes::configure)
    .configure(inventory::routes::configure)
    .configure(recipes::routes::configure)
    .configure(tills::legacy_routes::configure)
    .configure(devices::routes::configure)
    .configure(staff::routes::configure)
    .configure(push::routes::configure)
    .configure(tills::routes::configure)
    // One `/floor` scope: `reservations::routes` owns it and pulls the
    // cross-table operations of `floor_ops` in. A second scope on the
    // same prefix would be unreachable — actix never falls through.
    .configure(reservations::routes::configure)
    .configure(bookings::routes::configure)
    .configure(realtime::routes::configure)
    .configure(kitchen::routes::configure)
    .configure(tickets::routes::configure)
    .configure(stocktakes::routes::configure)
    .configure(crate::assets::routes::configure)
    .configure(sync::routes::configure)
    .configure(purchasing::routes::configure)
    .configure(orders::routes::configure)
    .configure(refunds::routes::configure)
    .configure(discounts::routes::configure)
    .configure(customers::routes::configure)
    .configure(|cfg| reports::routes::configure(cfg, read_pool.clone()))
    // Metrics share the read replica with reports: both are read-only
    // and both are dashboard-driven bursts.
    .configure(|cfg| analytics::routes::configure(cfg, read_pool.clone()))
    .configure(uploads::routes::configure)
    .configure(bundles::configure)
    .configure(crate::combos::routes::configure)
    .configure(insights::routes::configure)
    .configure(integrations::routes::configure)
    .configure(payment_methods::routes::configure)
    .configure(costing::routes::configure)
    .configure(delivery::routes::configure)
    .configure(loyalty::routes::configure)
    .configure(crate::staff_pool::routes::configure)
    // Apple's own paths, where a pass's `webServiceURL` points. Not
    // under JwtMiddleware: the caller is a customer's phone, which
    // authenticates with the pass's own token.
    .configure(loyalty::wallet::web_service::configure)
    .configure(qr_card::routes::configure)
    .configure(ai::routes::configure);
}
