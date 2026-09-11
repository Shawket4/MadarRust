use crate::{auth::middleware::JwtMiddleware, refunds::handlers};
use actix_web::web;

/// Mounted at `/refunds` rather than under `/orders/{id}` because actix routes
/// a request into the first scope whose prefix matches and does not fall
/// through to a later one — a second `/orders` scope here would be shadowed by
/// the orders module's. The order is named in the body, the way a sale names
/// its branch and shift.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/refunds")
            .wrap(JwtMiddleware)
            .route("", web::post().to(handlers::create_refund))
            // Two-segment paths cannot collide with `/{id}`, but keep the
            // fixed words first all the same, as the orders scope does.
            .route(
                "/order/{order_id}",
                web::get().to(handlers::list_order_refunds),
            )
            .route(
                "/shift/{shift_id}",
                web::get().to(handlers::list_shift_refunds),
            )
            .route("/{id}", web::get().to(handlers::get_refund)),
    );
}
