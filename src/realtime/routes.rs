use actix_web::web;

use crate::{auth::middleware::JwtMiddleware, realtime::stream};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/realtime")
            .wrap(JwtMiddleware)
            .route("/stream", web::get().to(stream::stream)),
    );
}
