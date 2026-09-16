use actix_web::web;

use crate::{auth::middleware::JwtMiddleware, customers::handlers};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/customers")
            .wrap(JwtMiddleware)
            .route("", web::get().to(handlers::list_customers))
            .route("", web::post().to(handlers::create_customer))
            .route("/{id}", web::get().to(handlers::get_customer))
            .route("/{id}", web::patch().to(handlers::update_customer))
            .route("/{id}/merge", web::post().to(handlers::merge_customer))
            .route("/{id}/erase", web::post().to(handlers::erase_customer)),
    );
}
