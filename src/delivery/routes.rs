//! Delivery routes. Admin/staff endpoints sit behind JwtMiddleware; the public
//! customer endpoints are unauthenticated and each carries its own per-IP rate
//! limiter (quote + OTP + intake are tightly bounded — they hit OSRM/WhatsApp).

use actix_governor::{Governor, GovernorConfigBuilder};
use actix_web::{middleware::Condition, web};

use crate::auth::middleware::JwtMiddleware;
use crate::delivery::{gateway, public, settings, staff};
use crate::qr_card::handlers as qr_handlers;
use crate::rate_limit::{rate_limiting_enabled, PeerIpOrLocalhost};

pub fn configure(cfg: &mut web::ServiceConfig) {
    // Browsing: ~60 req/min sustained, burst 30 (matches the public menu).
    let browse_gov = GovernorConfigBuilder::default()
        .key_extractor(PeerIpOrLocalhost)
        .seconds_per_request(1)
        .burst_size(30)
        .finish()
        .expect("Invalid delivery browse rate limiter");
    // Quote hits OSRM: ~10/min per IP.
    let quote_gov = GovernorConfigBuilder::default()
        .key_extractor(PeerIpOrLocalhost)
        .seconds_per_request(6)
        .burst_size(10)
        .finish()
        .expect("Invalid delivery quote rate limiter");
    // OTP send: very tight — ~1 per 30s, burst 3.
    let otp_gov = GovernorConfigBuilder::default()
        .key_extractor(PeerIpOrLocalhost)
        .seconds_per_request(30)
        .burst_size(3)
        .finish()
        .expect("Invalid delivery otp rate limiter");
    // Intake: ~10/min per IP, burst 10.
    let intake_gov = GovernorConfigBuilder::default()
        .key_extractor(PeerIpOrLocalhost)
        .seconds_per_request(6)
        .burst_size(10)
        .finish()
        .expect("Invalid delivery intake rate limiter");
    // Disabled wholesale by SUFRIX_DISABLE_RATE_LIMIT for local API fuzzing.
    let limited = rate_limiting_enabled();

    cfg
        // ── Admin config (managers) + POS override (tellers) ─────────────
        .service(
            web::scope("/delivery")
                .wrap(JwtMiddleware)
                .route("/settings", web::get().to(settings::get_branch_settings))
                .route("/settings", web::put().to(settings::put_branch_settings))
                .route("/accepting", web::post().to(settings::set_accepting))
                .route("/zones", web::get().to(settings::list_zones))
                .route("/zones", web::post().to(settings::create_zone))
                .route("/zones/{id}", web::patch().to(settings::update_zone))
                .route("/zones/{id}", web::delete().to(settings::delete_zone))
                .route("/channel-overrides", web::get().to(settings::list_channel_overrides))
                .route("/channel-overrides", web::put().to(settings::upsert_channel_override))
                .route("/channel-overrides", web::delete().to(settings::delete_channel_override))
                .route("/channel-addon-overrides", web::get().to(settings::list_channel_addon_overrides))
                .route("/channel-addon-overrides", web::put().to(settings::upsert_channel_addon_override))
                .route("/channel-addon-overrides", web::delete().to(settings::delete_channel_addon_override)),
        )
        // ── WhatsApp gateway relay (SUPER-ADMIN ONLY; guarded per-handler) ─
        // QR pairing / status / logout / pause for the private Go gateway.
        // JwtMiddleware authenticates; each handler then calls
        // require_super_admin, so no lower role can reach these.
        .service(
            web::scope("/whatsapp")
                .wrap(JwtMiddleware)
                .route("/status", web::get().to(gateway::status))
                .route("/pair", web::post().to(gateway::pair))
                .route("/logout", web::post().to(gateway::logout))
                .route("/pause", web::post().to(gateway::pause)),
        )
        // ── Staff queue ──────────────────────────────────────────────────
        .service(
            web::scope("/delivery-orders")
                .wrap(JwtMiddleware)
                .route("", web::get().to(staff::list_delivery_orders))
                .route("/stream", web::get().to(staff::stream_delivery_orders))
                .route("/{id}", web::get().to(staff::get_delivery_order))
                .route("/{id}/status", web::post().to(staff::set_status))
                .route("/{id}/prep-time", web::post().to(staff::set_prep_time))
                .route("/{id}/cancel", web::post().to(staff::cancel_delivery_order))
                .route("/{id}/finalize", web::post().to(staff::finalize_delivery_order))
                .route("/{id}/qr",       web::get().to(qr_handlers::delivery_order_qr)),
        )
        // ── Public (unauthenticated, rate-limited) ──────────────────────
        .service(
            web::resource("/public/branches")
                .wrap(Condition::new(limited, Governor::new(&browse_gov)))
                .route(web::get().to(public::public_branches)),
        )
        .service(
            web::resource("/public/branches/{id}/menu")
                .wrap(Condition::new(limited, Governor::new(&browse_gov)))
                .route(web::get().to(public::public_menu)),
        )
        .service(
            web::resource("/public/delivery-orders/{id}/track")
                .wrap(Condition::new(limited, Governor::new(&browse_gov)))
                .route(web::get().to(public::track_delivery_order)),
        )
        .service(
            web::resource("/public/branches/{id}/delivery-quote")
                .wrap(Condition::new(limited, Governor::new(&quote_gov)))
                .route(web::get().to(public::delivery_quote)),
        )
        .service(
            web::resource("/public/otp/request")
                .wrap(Condition::new(limited, Governor::new(&otp_gov)))
                .route(web::post().to(public::otp_request)),
        )
        .service(
            web::resource("/public/otp/verify")
                .wrap(Condition::new(limited, Governor::new(&otp_gov)))
                .route(web::post().to(public::otp_verify)),
        )
        .service(
            web::resource("/public/delivery-orders")
                .wrap(Condition::new(limited, Governor::new(&intake_gov)))
                .route(web::post().to(public::create_delivery_order)),
        )
        .service(
            web::resource("/public/delivery-orders/history")
                .wrap(Condition::new(limited, Governor::new(&browse_gov)))
                .route(web::get().to(public::guest_order_history)),
        )
        .service(
            web::resource("/public/delivery-orders/past-locations")
                .wrap(Condition::new(limited, Governor::new(&browse_gov)))
                .route(web::get().to(public::guest_past_locations)),
        );
}
