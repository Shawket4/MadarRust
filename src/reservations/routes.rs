//! Floor routes.
//!
//! `/floor/*` — section + table geometry authoring, and reading live table
//! status. Geometry is dashboard-authored (`floor_plan` permission); the POS
//! renders it.
//!
//! There is deliberately no route that SETS a table's status. Status is derived
//! from the order on the table, so a table cannot be declared free while a
//! ticket is open on it. The previous `PATCH /floor/tables/{id}/status` wrote
//! `branch_tables.status` directly, with no lock and no occupancy check, which
//! could do exactly that.

use actix_web::web;

use crate::auth::middleware::JwtMiddleware;
use crate::reservations::floor;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/floor")
            .wrap(JwtMiddleware)
            .route("/sections", web::get().to(floor::list_sections))
            .route("/sections", web::post().to(floor::create_section))
            .route("/sections/{id}", web::patch().to(floor::update_section))
            .route("/sections/{id}", web::delete().to(floor::delete_section))
            .route("/tables", web::get().to(floor::list_tables))
            .route("/tables", web::post().to(floor::create_table))
            .route("/tables/{id}", web::patch().to(floor::update_table))
            .route("/tables/{id}", web::delete().to(floor::delete_table))
            .route("/layout", web::put().to(floor::save_layout)),
    );
}
