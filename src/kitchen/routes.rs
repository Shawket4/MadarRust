use actix_web::web;

use crate::{auth::middleware::JwtMiddleware, kitchen::kds, kitchen::stations};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/kitchen")
            .wrap(JwtMiddleware)
            // Stations CRUD
            .route("/stations", web::get().to(stations::list_stations))
            .route("/stations", web::post().to(stations::create_station))
            .route("/stations/{id}", web::patch().to(stations::update_station))
            .route("/stations/{id}", web::delete().to(stations::delete_station))
            // Routing config
            .route("/routes", web::get().to(stations::list_routes))
            .route("/routes/category", web::put().to(stations::put_category_route))
            .route("/routes/category", web::delete().to(stations::delete_category_route))
            .route("/routes/item", web::put().to(stations::put_item_route))
            .route("/routes/item", web::delete().to(stations::delete_item_route))
            // Routing mode
            .route("/routing-mode", web::get().to(stations::get_routing_mode))
            .route("/routing-mode", web::put().to(stations::set_routing_mode))
            // KDS feed + bump
            .route("/orders", web::get().to(kds::feed))
            .route("/items/{item_id}/bump", web::post().to(kds::bump))
            .route("/items/{item_id}/unbump", web::post().to(kds::unbump)),
    );
}
