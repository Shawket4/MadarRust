use crate::{
    auth::middleware::JwtMiddleware,
    payment_methods::{availability, handlers::*},
};
use actix_web::web;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/payment-methods")
            .wrap(JwtMiddleware)
            .route("", web::get().to(list_payment_methods))
            .route("", web::post().to(create_payment_method))
            .route(
                "/availability",
                web::get().to(availability::get_availability),
            )
            .route(
                "/availability/branches/{id}",
                web::put().to(availability::put_branch_availability),
            )
            .route(
                "/availability/users/{id}",
                web::put().to(availability::put_user_availability),
            )
            .route(
                "/availability/devices/{id}",
                web::put().to(availability::put_device_availability),
            )
            .route("/effective", web::get().to(availability::get_effective))
            .route("/{id}", web::put().to(update_payment_method))
            .route("/{id}/activate", web::post().to(activate_payment_method))
            .route(
                "/{id}/deactivate",
                web::post().to(deactivate_payment_method),
            ),
    );
}
