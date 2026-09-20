use actix_governor::{Governor, GovernorConfigBuilder};
use actix_web::{middleware::Condition, web};

use crate::rate_limit::{PeerIpOrLocalhost, rate_limiting_enabled};
use crate::{
    auth::middleware::JwtMiddleware,
    customers::{handlers, order_now},
};

pub fn configure(cfg: &mut web::ServiceConfig) {
    // Opening the ordering page from a card: the budget the card page gets.
    let browse_gov = GovernorConfigBuilder::default()
        .key_extractor(PeerIpOrLocalhost)
        .seconds_per_request(1)
        .burst_size(30)
        .finish()
        .expect("Invalid order-now browse rate limiter");
    // Changing who a profile belongs to: as tight as a loyalty signup.
    let identity_gov = GovernorConfigBuilder::default()
        .key_extractor(PeerIpOrLocalhost)
        .seconds_per_request(6)
        .burst_size(5)
        .finish()
        .expect("Invalid order-now identity rate limiter");
    let limited = rate_limiting_enabled();

    cfg.service(
        web::scope("/customers")
            .wrap(JwtMiddleware)
            .route("", web::get().to(handlers::list_customers))
            .route("", web::post().to(handlers::create_customer))
            .route("/{id}", web::get().to(handlers::get_customer))
            .route("/{id}", web::patch().to(handlers::update_customer))
            .route(
                "/{id}/addresses",
                web::get().to(handlers::list_customer_addresses),
            )
            .route("/{id}/merge", web::post().to(handlers::merge_customer))
            .route("/{id}/erase", web::post().to(handlers::erase_customer)),
    )
    // ── Public: "order now" from the wallet pass (design §4) ─────────────
    .service(
        web::resource("/public/order-now/{token}")
            .wrap(Condition::new(limited, Governor::new(&browse_gov)))
            .route(web::get().to(order_now::context)),
    )
    .service(
        web::resource("/public/order-now/{token}/replace-identity")
            .wrap(Condition::new(limited, Governor::new(&identity_gov)))
            .route(web::post().to(order_now::replace_identity)),
    )
    .service(
        web::resource("/public/order-now/{token}/combine")
            .wrap(Condition::new(limited, Governor::new(&identity_gov)))
            .route(web::post().to(order_now::combine)),
    );
}
