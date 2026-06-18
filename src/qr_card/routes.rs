use std::sync::Arc;

use actix_web::web;
use crate::auth::middleware::JwtMiddleware;
use crate::qr_card::shlink::{ShortLinkProvider, ShlinkClient};

use super::handlers;

/// Routes owned exclusively by the QR module (no prefix conflict with other modules).
/// Branch/org/delivery-order QR sub-routes are registered in their respective
/// module's configure() alongside the rest of that resource's routes.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/qr")
            .wrap(JwtMiddleware)
            .route("/links",  web::post().to(handlers::create_marketing_link))
            .route("/links",  web::get().to(handlers::list_marketing_links)),
    );
}

/// Register the live Shlink provider into the app's `Data` so handlers can
/// extract it.  Call once during server construction:
/// `.app_data(qr_card::routes::make_provider())`.
pub fn make_provider() -> web::Data<Arc<dyn ShortLinkProvider>> {
    web::Data::new(Arc::new(ShlinkClient) as Arc<dyn ShortLinkProvider>)
}
