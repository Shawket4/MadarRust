use actix_web::web;

use crate::auth::middleware::JwtMiddleware;
use crate::push::handlers::{delete_push_token, set_push_token};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/push")
            .wrap(JwtMiddleware)
            .route("/token", web::put().to(set_push_token))
            .route("/token", web::delete().to(delete_push_token)),
    );
}
