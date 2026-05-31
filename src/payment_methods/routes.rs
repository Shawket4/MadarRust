use actix_web::web;
use crate::payment_methods::handlers::*;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.route("/payment-methods", web::get().to(list_payment_methods))
       .route("/payment-methods", web::post().to(create_payment_method))
       .route("/payment-methods/{id}", web::put().to(update_payment_method))
       .route("/payment-methods/{id}/activate", web::post().to(activate_payment_method))
       .route("/payment-methods/{id}/deactivate", web::post().to(deactivate_payment_method));
}
