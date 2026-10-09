use actix_governor::Governor;
use actix_web::{middleware::Condition, web};

use crate::auth::{handlers, middleware::JwtMiddleware};
use crate::rate_limit::{PeerIpOrLocalhost, rate_limiting_enabled};

pub fn configure(cfg: &mut web::ServiceConfig) {
    // Password/PIN login: per IP, burst 120, one more every 500 ms (owner decisions
    // 2026-09-16, doubled 2026-09-25). Fifty tablets behind one router sign in at shift change;
    // PIN guessing is held back by the per-device and per-branch growing delay
    // (`auth::pin_throttle`), not by this per-address ceiling.
    let login_gov = crate::rate_limit::route_governor(PeerIpOrLocalhost, "AUTH", "LOGIN");
    // Activation codes and branch resolution: per IP, burst 20, one every 3 s
    // (an activation code is 8 digits and has no delay of its own). The numbers
    // are rate_limit::ROUTE_LIMITS', each set from the environment.
    let gov = crate::rate_limit::route_governor(PeerIpOrLocalhost, "AUTH", "ACTIVATION");
    // Disabled wholesale by MADAR_DISABLE_RATE_LIMIT for local API fuzzing.
    let limited = rate_limiting_enabled();

    cfg.service(
        web::scope("/auth")
            // Each public endpoint gets its own rate-limited resource so it doesn't
            // shadow the JWT-protected scope below (both scopes having prefix "" would
            // cause the first scope to intercept all /auth/* requests).
            .service(
                web::resource("/login")
                    .wrap(Condition::new(limited, Governor::new(&login_gov)))
                    .route(web::post().to(handlers::login)),
            )
            // Device activation codes (POS_SIGNIN_OVERHAUL §4): unauthenticated,
            // so it shares the login governor.
            .service(
                web::resource("/activate-device")
                    .wrap(Condition::new(limited, Governor::new(&gov)))
                    .route(web::post().to(crate::devices::activation::activate)),
            )
            .service(
                web::resource("/authz-keys")
                    .route(web::get().to(crate::authz::snapshot::authz_keys)),
            )
            // Dawam staff sign-in by phone OTP: unauthenticated, login governor.
            .service(
                web::resource("/staff/otp/request")
                    .wrap(Condition::new(limited, Governor::new(&login_gov)))
                    .route(web::post().to(crate::staff::dawam::signin::otp_request)),
            )
            .service(
                web::resource("/staff/otp/verify")
                    .wrap(Condition::new(limited, Governor::new(&login_gov)))
                    .route(web::post().to(crate::staff::dawam::signin::otp_verify)),
            )
            // The staff app's session refresh: the device token is the
            // credential, so it shares the login governor.
            .service(
                web::resource("/staff/refresh")
                    .wrap(Condition::new(limited, Governor::new(&login_gov)))
                    .route(web::post().to(crate::staff::dawam::signin::refresh)),
            )
            .service(
                web::resource("/resolve-branch")
                    .wrap(Condition::new(limited, Governor::new(&gov)))
                    .route(web::post().to(handlers::resolve_branch)),
            )
            .service(
                web::scope("")
                    .wrap(JwtMiddleware)
                    .route("/me", web::get().to(handlers::me))
                    .route("/permissions", web::get().to(handlers::permissions)),
            ),
    );
}
