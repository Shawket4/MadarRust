use actix_web::web;

use crate::auth::middleware::JwtMiddleware;

use super::handlers;

/// `/ai/*` — merchant analytics chat. Behind `JwtMiddleware`; the handler
/// further requires an org-scoped account and `reports:read`.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/ai")
            .wrap(JwtMiddleware)
            .route("/chat", web::post().to(handlers::chat))
            // The same turn, streamed. Progress frames then one terminal frame
            // whose payload is identical to /chat's body.
            .route("/chat/stream", web::post().to(super::stream::chat_stream))
            // Stored conversations. Private to the user who created them —
            // see `ai::store` for the double fence (RLS + user_id).
            .route(
                "/conversations",
                web::get().to(handlers::list_conversations),
            )
            .route(
                "/conversations/{id}",
                web::get().to(handlers::get_conversation),
            )
            .route(
                "/conversations/{id}",
                web::patch().to(handlers::rename_conversation),
            )
            .route(
                "/conversations/{id}",
                web::delete().to(handlers::delete_conversation),
            ),
    );
}
