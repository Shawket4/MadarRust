use actix_web::web;
use crate::{auth::middleware::JwtMiddleware, tills::handlers};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/tills")
            .wrap(JwtMiddleware)
            .route("",      web::get().to(handlers::list_tills))
            .route("",      web::post().to(handlers::create_till))
            .route("/{id}", web::patch().to(handlers::update_till))
            .route("/{id}", web::delete().to(handlers::delete_till)),
    );
}
