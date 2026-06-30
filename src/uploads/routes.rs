use crate::{auth::middleware::JwtMiddleware, uploads::handlers};
use actix_web::web;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/uploads/menu-items") // more specific, not just /uploads
            .wrap(JwtMiddleware)
            .route(
                "/{menu_item_id}",
                web::post().to(handlers::upload_menu_item_image),
            ),
    );
}
