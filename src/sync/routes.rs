use actix_web::web;

use crate::auth::middleware::JwtMiddleware;

/// `/sync/*` — authenticated device-replay routes. Behind the same JWT
/// middleware as every other write route; `replay` does its own org/teller
/// attribution checks on top.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/sync")
            .wrap(JwtMiddleware)
            .route("/replay", web::post().to(super::handlers::replay))
            // Protocol v2: many queued ops per round trip (old tills keep the
            // single-op route above). Its own body limit: a batch of sales.
            .service(
                web::resource("/replay/batch")
                    .app_data(
                        web::JsonConfig::default()
                            .limit(super::batch::MAX_BODY_BYTES)
                            .error_handler(|err, _req| {
                                crate::errors::AppError::BadRequest(err.to_string()).into()
                            }),
                    )
                    .route(web::post().to(super::batch::replay_batch)),
            )
            .route("/pull", web::post().to(super::pull::pull)),
    );
}
