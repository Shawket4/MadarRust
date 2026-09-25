use actix_governor::Governor;
use actix_web::{middleware::Condition, web};

use crate::rate_limit::{PathToken, PeerIpOrLocalhost, rate_limiting_enabled};
use crate::{
    auth::middleware::JwtMiddleware,
    customers::{handlers, order_now},
};

pub fn configure(cfg: &mut web::ServiceConfig) {
    // Opening the ordering page from a card: the budget the card page gets.
    let browse_gov = crate::rate_limit::route_governor(PeerIpOrLocalhost, "CUSTOMERS", "BROWSE");
    // Changing who a profile belongs to: as tight as a loyalty signup.
    let identity_gov =
        crate::rate_limit::route_governor(PeerIpOrLocalhost, "CUSTOMERS", "IDENTITY");
    // The same two budgets again, per CARD (design §4.2): a card's token is a
    // bearer secret printed on a pass, and guessing at what is behind it from
    // many addresses never touches a per-IP bucket. One person opening their
    // own page is nowhere near forty a burst; an identity change is something
    // a person does a couple of times a month, so its bucket is tighter than
    // the OTP behind it: six tries, then one a minute (doubled 2026-09-25).
    let browse_token_gov =
        crate::rate_limit::route_governor(PathToken, "CUSTOMERS", "BROWSE_TOKEN");
    let identity_token_gov =
        crate::rate_limit::route_governor(PathToken, "CUSTOMERS", "IDENTITY_TOKEN");
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
            .route(
                "/{id}/bookings",
                web::get().to(handlers::list_customer_bookings),
            )
            .route("/{id}/merge", web::post().to(handlers::merge_customer))
            .route("/{id}/erase", web::post().to(handlers::erase_customer)),
    )
    // ── Public: "order now" from the wallet pass (design §4) ─────────────
    .service(
        web::resource("/public/order-now/{token}")
            .wrap(Condition::new(limited, Governor::new(&browse_token_gov)))
            .wrap(Condition::new(limited, Governor::new(&browse_gov)))
            .route(web::get().to(order_now::context)),
    )
    .service(
        web::resource("/public/order-now/{token}/replace-identity")
            // Combine and replace-identity spend the SAME per-card bucket.
            .wrap(Condition::new(limited, Governor::new(&identity_token_gov)))
            .wrap(Condition::new(limited, Governor::new(&identity_gov)))
            .route(web::post().to(order_now::replace_identity)),
    )
    .service(
        web::resource("/public/order-now/{token}/combine")
            // Combine and replace-identity spend the SAME per-card bucket.
            .wrap(Condition::new(limited, Governor::new(&identity_token_gov)))
            .wrap(Condition::new(limited, Governor::new(&identity_gov)))
            .route(web::post().to(order_now::combine)),
    );
}
