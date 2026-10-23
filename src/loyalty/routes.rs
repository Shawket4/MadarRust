//! Loyalty routes.
//!
//! Staff surfaces sit behind `JwtMiddleware`; the customer-facing signup and
//! card endpoints are unauthenticated and each carries its own per-IP limiter,
//! matching how `delivery::routes` treats the public ordering endpoints.

use actix_governor::Governor;
use actix_web::{middleware::Condition, web};

use crate::auth::middleware::JwtMiddleware;
use crate::loyalty::{award, handlers, public, settings};
use crate::rate_limit::{PeerIpOrLocalhost, rate_limiting_enabled};

pub fn configure(cfg: &mut web::ServiceConfig) {
    // Reading a join page or a card: ~120/min sustained, burst 60 — the same
    // budget the public menu gets.
    let browse_gov = crate::rate_limit::route_governor(PeerIpOrLocalhost, "LOYALTY", "BROWSE");
    // Signing up writes a member row and may issue a pass: ~20/min, burst 10.
    // Tighter than browsing because it is the only public write here.
    let join_gov = crate::rate_limit::route_governor(PeerIpOrLocalhost, "LOYALTY", "JOIN");
    let limited = rate_limiting_enabled();

    cfg
        // ── Staff: the teller's scan, and the admin's program ────────────
        .service(
            web::scope("/loyalty")
                .wrap(JwtMiddleware)
                .route("/settings", web::get().to(settings::get_settings))
                .route("/settings", web::put().to(settings::put_settings))
                .route("/settings", web::delete().to(settings::delete_settings))
                .route("/reward-items", web::get().to(settings::get_reward_items))
                .route("/reward-items", web::put().to(settings::put_reward_items))
                // What FILLS a card, beside what it buys.
                .route("/earning-items", web::get().to(settings::get_earning_items))
                .route("/earning-items", web::put().to(settings::put_earning_items))
                // Stamp tracks: one card, up to four rewards.
                .route("/tracks", web::get().to(super::tracks::get_tracks))
                .route("/tracks", web::put().to(super::tracks::put_tracks))
                // The card designer.
                .route("/card-design", web::get().to(super::design::get_design))
                .route("/card-design", web::put().to(super::design::put_design))
                .route("/card-design/publish", web::post().to(super::design::publish))
                .route("/card-design/published", web::delete().to(super::design::unpublish))
                .route("/card-design/preview", web::post().to(super::design::preview))
                .route("/lookup", web::post().to(handlers::lookup))
                .route("/award", web::post().to(award::award))
                .route("/counter/stamp", web::post().to(super::counter::stamp))
                .route("/counter/redeem", web::post().to(super::counter::redeem))
                .route(
                    "/counter/birthday-gift",
                    web::post().to(super::gifts::counter_claim),
                )
                .route("/adjust", web::post().to(handlers::adjust))
                .route(
                    "/birthday-preview",
                    web::post().to(handlers::birthday_preview),
                )
                .route("/analytics", web::get().to(handlers::analytics))
                .route("/behavior", web::get().to(handlers::behavior))
                .route(
                    "/campaign-effectiveness",
                    web::get().to(handlers::campaign_effectiveness),
                )
                .route("/liability-trend", web::get().to(handlers::liability_trend))
                // Makes live calls to Google, so it is asked for, never polled.
                .route("/wallet-status", web::get().to(handlers::wallet_status))
                // The Rewards CRM and figures.
                .route("/members/query", web::post().to(super::crm::query_members))
                .route("/members/{id}/wallet", web::get().to(super::crm::member_wallet))
                .route("/stats", web::get().to(super::crm::stats))
                // Messages to members' cards.
                .route("/broadcasts", web::get().to(super::broadcasts::list))
                .route("/broadcasts", web::post().to(super::broadcasts::create))
                .route("/broadcasts/audience", web::post().to(super::broadcasts::audience_count))
                .route("/broadcasts/{id}", web::get().to(super::broadcasts::get))
                .route("/broadcasts/{id}/cancel", web::post().to(super::broadcasts::cancel))
                .route("/members", web::get().to(handlers::list_members))
                .route("/members/{id}", web::get().to(handlers::get_member))
                // Forgets the person, keeps the books. Admin only.
                .route("/members/{id}", web::delete().to(handlers::delete_member))
                // Also a live call to Google, and also super-admin only.
                .route(
                    "/members/{id}/google-object",
                    web::get().to(handlers::google_object),
                )
                // Writes, so a POST — it provisions for real and reports every
                // word of it. Super-admin only, like its read-only sibling.
                .route(
                    "/members/{id}/google-refresh",
                    web::post().to(handlers::google_refresh),
                ),
        )
        // ── Public: the counter QR's signup form and the customer's card ──
        .service(
            web::resource("/public/loyalty/join-info")
                .wrap(Condition::new(limited, Governor::new(&browse_gov)))
                .route(web::get().to(public::join_info)),
        )
        .service(
            web::resource("/public/loyalty/join")
                .wrap(Condition::new(limited, Governor::new(&join_gov)))
                .route(web::post().to(public::join)),
        )
        .service(
            web::resource("/public/loyalty/card/{token}")
                .wrap(Condition::new(limited, Governor::new(&browse_gov)))
                .route(web::get().to(public::card)),
        )
        .service(
            web::resource("/public/loyalty/card/{token}/orders")
                .wrap(Condition::new(limited, Governor::new(&browse_gov)))
                .route(web::get().to(public::card_orders)),
        )
        .service(
            web::resource("/public/loyalty/card/{token}/preferences")
                .wrap(Condition::new(limited, Governor::new(&browse_gov)))
                .route(web::post().to(public::set_preferences)),
        )
        .service(
            web::resource("/public/loyalty/card/{token}/qr.png")
                .wrap(Condition::new(limited, Governor::new(&browse_gov)))
                .route(web::get().to(public::card_qr)),
        )
        // Madar's mark. Static bytes, no rate limit: Google fetches it from its
        // own servers on a schedule we do not control, and throttling it would
        // show up as a class with a broken logo.
        // Composed for Google's own slots, and fetched by Google. No rate
        // limit: it pulls these on a schedule we do not control, and throttling
        // shows up as a card with a broken image.
        .service(
            web::resource("/public/loyalty/brand/{org_id}/logo/{v}.png")
                .route(web::get().to(public::org_logo_badge)),
        )
        // A designed card's Google hero, per progress (wallet::design). No
        // member data in it; every number is checked against the shop's tracks.
        .service(
            web::resource("/public/loyalty/brand/{org_id}/card/{revision}/{state}")
                .wrap(Condition::new(limited, Governor::new(&browse_gov)))
                .route(web::get().to(super::design::public_hero)),
        )
        .service(
            web::resource("/public/loyalty/brand/{org_id}/banner/{v}.png")
                .route(web::get().to(public::org_card_banner)),
        )
        .service(
            web::resource(super::wallet::google::MADAR_LOGO_PATH)
                .route(web::get().to(public::brand_logo)),
        )
        .service(
            web::resource(super::wallet::google::REWARDS_LOGO_INK_PATH)
                .route(web::get().to(public::rewards_logo_ink)),
        )
        .service(
            web::resource(super::wallet::google::REWARDS_LOGO_PAPER_PATH)
                .route(web::get().to(public::rewards_logo_paper)),
        )
        .service(
            web::resource(super::wallet::google::REWARDS_HERO_PATH)
                .route(web::get().to(public::rewards_hero)),
        )
        .service(
            web::resource("/public/loyalty/pass/{token}/apple.pkpass")
                .wrap(Condition::new(limited, Governor::new(&browse_gov)))
                .route(web::get().to(public::apple_pass)),
        );
}
