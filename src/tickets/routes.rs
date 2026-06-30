use actix_web::web;

use crate::{auth::middleware::JwtMiddleware, tickets::handlers};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/open-tickets")
            .wrap(JwtMiddleware)
            .route("", web::get().to(handlers::list_open_tickets))
            .route("", web::post().to(handlers::create_open_ticket))
            .route("/{id}", web::get().to(handlers::get_open_ticket))
            .route("/{id}/rounds", web::post().to(handlers::add_round))
            .route("/{id}/void", web::post().to(handlers::void_open_ticket))
            .route("/{id}/table", web::patch().to(handlers::move_ticket_table))
            .route("/{id}/settle", web::post().to(handlers::settle_open_ticket)),
    );
}
