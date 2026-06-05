use actix_web::web;
use crate::{auth::middleware::JwtMiddleware, orgs::handlers};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/orgs")
            .wrap(JwtMiddleware)
            .route("",           web::post().to(handlers::create_org))
            .route("",           web::get().to(handlers::list_orgs))
            .route("/{id}",      web::get().to(handlers::get_org))
            .route("/{id}",      web::patch().to(handlers::update_org))
            .route("/{id}",      web::delete().to(handlers::delete_org))
            .route("/{id}/logo", web::put().to(handlers::upload_org_logo)),
    );

    cfg.service(
        web::scope("/public/orgs")
            .route("", web::get().to(handlers::list_public_orgs)),
    );
}