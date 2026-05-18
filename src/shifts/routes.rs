use actix_web::web;
use crate::{auth::middleware::JwtMiddleware, shifts::handlers};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/shifts")
            .wrap(JwtMiddleware)
            .route("/branches/{branch_id}/current",
                web::get().to(handlers::get_current_shift))
            .route("/branches/{branch_id}/open",
                web::post().to(handlers::open_shift))
            .route("/branches/{branch_id}",
                web::get().to(handlers::list_shifts))
            .route("/{shift_id}/report",
                web::get().to(handlers::get_shift_report))
            .route("/{shift_id}/cash-movements",
                web::post().to(handlers::add_cash_movement))
            .route("/{shift_id}/cash-movements",
                web::get().to(handlers::list_cash_movements))
            .route("/{shift_id}/close",
                web::post().to(handlers::close_shift))
            .route("/{shift_id}/force-close",
                web::post().to(handlers::force_close_shift))
            .route("/{shift_id}",
                web::get().to(handlers::get_shift))
            .route("/{shift_id}",
                web::delete().to(handlers::delete_shift)),
    );
}