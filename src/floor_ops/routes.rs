use actix_web::web;

use crate::{auth::middleware::JwtMiddleware, floor_ops::handlers};

/// Cross-table floor operations.
///
/// A parked order is NOT here: it is a client-local draft, so parking, naming,
/// resuming and discarding one never touch the network. What remains is what is
/// genuinely shared across tills -- swapping two tables' occupants atomically,
/// and the transfer waitlist a host works.
///
/// There is deliberately no route that SETS a table's status. Status is derived
/// from the ticket on the table -- except `clear`, which performs the single
/// transition no server can observe: a bussed table becoming ready.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/floor")
            .wrap(JwtMiddleware)
            .route("/tables/swap", web::post().to(handlers::swap_tables))
            // The one human act status cannot derive: "the plates are gone".
            .route("/tables/{id}/clear", web::post().to(handlers::clear_table))
            .route("/transfers", web::get().to(handlers::list_floor_transfers))
            .route(
                "/transfers",
                web::post().to(handlers::create_floor_transfer),
            )
            .route(
                "/transfers/{id}/cancel",
                web::post().to(handlers::cancel_transfer),
            )
            .route(
                "/transfers/{id}/fulfill",
                web::post().to(handlers::fulfill_transfer),
            ),
    );
}
