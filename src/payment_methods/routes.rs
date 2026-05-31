use actix_web::web;
use crate::{auth::middleware::JwtMiddleware, payment_methods::handlers::*};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/payment-methods")
            .wrap(JwtMiddleware)
            .route("", web::get().to(list_payment_methods))
            .route("", web::post().to(create_payment_method))
            .route("/{id}", web::put().to(update_payment_method))
            .route("/{id}/activate", web::post().to(activate_payment_method))
            .route("/{id}/deactivate", web::post().to(deactivate_payment_method))
    );
}
