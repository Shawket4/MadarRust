use actix_governor::{Governor, GovernorConfigBuilder};
use actix_web::{middleware::Condition, web};

use crate::rate_limit::{PeerIpOrLocalhost, rate_limiting_enabled};
use crate::{auth::middleware::JwtMiddleware, tickets::handlers, tickets::public};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/open-tickets")
            .wrap(JwtMiddleware)
            .route("", web::get().to(handlers::list_open_tickets))
            .route("", web::post().to(handlers::create_open_ticket))
            .route("/{id}", web::get().to(handlers::get_open_ticket))
            .route("/{id}/rounds", web::post().to(handlers::add_round))
            .route("/{id}/void", web::post().to(handlers::void_open_ticket))
            // Two fixed segments, so it cannot be shadowed by `/{id}/…`.
            .route(
                "/{id}/items/{item_id}/void",
                web::post().to(handlers::void_ticket_line),
            )
            .route("/{id}/table", web::patch().to(handlers::move_ticket_table))
            .route("/{id}/settle", web::post().to(handlers::settle_open_ticket)),
    );
    // Ordering from the code on the table. NOT under the scope above: these are
    // unauthenticated by necessity — the customer is a stranger with a phone —
    // and mounting them inside a `JwtMiddleware` scope would 401 every scan.
    //
    // Both are rate-limited per IP, and the intake one tightly. It is the only
    // public endpoint that WRITES a bill onto a shop's floor, and without a
    // bound one script could fill a restaurant with orders faster than anyone
    // could void them. Ten a minute is far more than a table of six sending
    // rounds and far less than an attack.
    let table_browse = GovernorConfigBuilder::default()
        .key_extractor(PeerIpOrLocalhost)
        .seconds_per_request(1)
        .burst_size(30)
        .finish()
        .expect("Invalid table browse rate limiter");
    let table_intake = GovernorConfigBuilder::default()
        .key_extractor(PeerIpOrLocalhost)
        .seconds_per_request(6)
        .burst_size(10)
        .finish()
        .expect("Invalid table intake rate limiter");
    let limited = rate_limiting_enabled();
    cfg.service(
        web::resource("/public/tables/{id}")
            .wrap(Condition::new(limited, Governor::new(&table_browse)))
            .route(web::get().to(public::table)),
    )
    .service(
        web::resource("/public/tables/{id}/menu")
            .wrap(Condition::new(limited, Governor::new(&table_browse)))
            .route(web::get().to(public::table_menu)),
    )
    .service(
        web::resource("/public/table-orders")
            .wrap(Condition::new(limited, Governor::new(&table_intake)))
            .route(web::post().to(public::create_table_order)),
    );
}
