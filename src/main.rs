//! Server entry point. The actual app lives in the library crate so the
//! OpenAPI exporter binary can reach `ApiDoc` without booting the server.

use actix_cors::Cors;
use actix_files::Files;
use actix_web::middleware::Compress;
use actix_web::{App, HttpServer, web};

use dotenvy::dotenv;
use sqlx::postgres::PgPoolOptions;
use std::{env, fs};
use tracing_subscriber::EnvFilter;

use madar_rust::openapi::ApiDoc;
use madar_rust::{
    ai, auth, branches, bundles, costing, delivery, demo, discounts, insights, inventory, kitchen,
    menu, orders, orgs, payment_methods, permissions, purchasing, qr_card, realtime, recipes,
    reports, reservations, shifts, stocktakes, sync, tickets, tills, uploads, users,
};

use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let jwt_secret = env::var("JWT_SECRET").expect("JWT_SECRET must be set");
    let uploads_dir = env::var("UPLOADS_DIR").unwrap_or_else(|_| "./uploads".to_string());

    fs::create_dir_all(&uploads_dir).expect("Failed to create uploads directory");

    // Connection pool sizing is env-driven so a SaaS deployment can tune it to its
    // box/core count (and to PgBouncer) without a rebuild. Defaults preserve the
    // historical behavior (10 conns, sqlx's default prepared-statement cache).
    let max_conns: u32 = env::var("DB_MAX_CONNECTIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let pool = build_pool(&db_url, max_conns).await;

    // Reports/analytics read pool. When READ_DATABASE_URL points at a read replica
    // the heavy aggregation queries run there, off the primary's order path. Unset
    // → reuse the primary pool, so there is NO behavior change by default.
    let read_pool = match env::var("READ_DATABASE_URL") {
        Ok(u) if !u.trim().is_empty() => {
            tracing::info!("Reports read pool → replica ({} max conns)", max_conns);
            build_pool(&u, max_conns).await
        }
        _ => pool.clone(),
    };

    // Apply pending migrations on boot so the running binary and its schema can
    // never drift (a query referencing a not-yet-applied column would otherwise
    // 500 at runtime — invisible to tests, which always provision a fresh DB with
    // the full migration set). Migrations are embedded at compile time; this is a
    // no-op when the DB is already current and fails fast on a checksum mismatch.
    tracing::info!("Applying database migrations...");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("Failed to apply database migrations");

    tracing::info!("Seeding default role permissions into database...");
    permissions::seeder::seed_role_permissions(&pool)
        .await
        .expect("Failed to seed default role permissions");

    let pool = web::Data::new(pool);
    // Read pool handed to the /reports scope only (see configure call below).
    let read_pool = web::Data::new(read_pool);
    // Optional per-org menu cache; a no-op unless MENU_CACHE_TTL_SECS>0.
    let menu_cache = web::Data::new(menu::cache::MenuCache::from_env());
    let jwt_secret = web::Data::new(auth::jwt::JwtSecret(jwt_secret));
    // Per-process org-suspension cache, consulted by JwtMiddleware on every
    // authenticated request. Registering it is what arms the kill-switch.
    let org_status = web::Data::new(auth::org_status::OrgStatusCache::new());
    // The unified per-branch realtime bus — delivery, kitchen, waiter tickets, and
    // order events all ride one connection. Shared across all workers.
    let realtime_bus = web::Data::new(realtime::hub::BranchEventHub::new());

    // AI analytics chat state (Gemini provider + response cache). Wires the
    // provider only when GEMINI_API_KEY is set; otherwise the /ai endpoints
    // report the feature as unavailable and the rest of the server is unaffected.
    let ai_state = web::Data::new(ai::AiState::from_env());

    // The reservations nudge scheduler — flat departure nudges, no-show warns,
    // table holds, and the OSRM waitlist head-out. Spawned ONCE here (not in the
    // per-worker closure below) so there's a single instance; idempotent via
    // booking_nudges. No-op when RESERVATION_NUDGES_ENABLED is falsy.
    reservations::nudge::spawn(pool.get_ref().clone(), realtime_bus.get_ref().clone());

    // Public demo playground (DEMO_MODE). Throwaway per-visitor orgs with a TTL;
    // the sweeper GCs expired ones. Spawned ONCE (single instance). Off by
    // default — and meant to run on a SEPARATE backend + DB from production.
    let demo_settings = demo::config::DemoConfig::from_env();
    if demo_settings.enabled {
        tracing::warn!(
            "⚠️  DEMO_MODE ENABLED — public throwaway orgs (TTL {}h, max {}). Run on a non-prod DB.",
            demo_settings.ttl_hours,
            demo_settings.max_orgs,
        );
        demo::sweeper::spawn(pool.get_ref().clone(), demo_settings.sweep_secs);
    }
    let demo_enabled = demo_settings.enabled;
    let demo_cfg = web::Data::new(demo_settings);
    // Shlink short-URL provider (reads env vars on each call; degrade-safe).
    let qr_provider = qr_card::routes::make_provider();
    let uploads_clone = uploads_dir.clone();
    let bind_addr = env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let https_port = env::var("HTTPS_PORT").unwrap_or_else(|_| "8443".to_string());
    let https_addr = format!("0.0.0.0:{}", https_port);

    // Swagger UI is dev/staging only. In production leave the env var
    // unset (or set to a falsy value) and front the spec endpoint with
    // nginx basic auth if you need to expose it to a partner.
    let enable_swagger_ui = env::var("MADAR_ENABLE_SWAGGER_UI")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);

    let tls_config = build_tls_config();

    tracing::info!("Starting madar-rust");
    tracing::info!("Uploads directory: {}", uploads_dir);
    if enable_swagger_ui {
        tracing::warn!("⚠️  Swagger UI ENABLED — exposes full API surface unauthenticated.");
        tracing::warn!("    Do NOT run with MADAR_ENABLE_SWAGGER_UI=true in production");
        tracing::warn!("    without nginx basic-auth in front of /api-docs/.");
        tracing::info!(
            "Swagger UI at /api-docs/swagger-ui/  |  OpenAPI JSON at /api-docs/openapi.json"
        );
    } else {
        tracing::info!("Swagger UI disabled (set MADAR_ENABLE_SWAGGER_UI=true to enable in dev)");
    }

    let server = HttpServer::new(move || {
        // Restrict CORS to first-party frontends via CORS_ALLOWED_ORIGINS
        // (comma-separated). Unset/empty → allow any origin (local dev fallback).
        let cors = {
            let base = Cors::default()
                .allow_any_method()
                .allow_any_header()
                .max_age(3600);
            match std::env::var("CORS_ALLOWED_ORIGINS") {
                Ok(list) if !list.trim().is_empty() => list
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .fold(base, |c, origin| c.allowed_origin(origin)),
                _ => base.allow_any_origin(),
            }
        };

        // Build the App. All `.wrap()` calls happen first so the App's
        // generic type is stable when we conditionally add Swagger UI.
        let mut app = App::new()
            .wrap(cors)
            .wrap(Compress::default())
            .app_data(pool.clone())
            .app_data(menu_cache.clone())
            .app_data(jwt_secret.clone())
            .app_data(org_status.clone())
            .app_data(realtime_bus.clone())
            .app_data(qr_provider.clone())
            .app_data(demo_cfg.clone())
            .app_data(ai_state.clone())
            // Render actix's built-in extractor parse errors (bad path UUID, bad
            // query param, malformed JSON body) as our JSON ErrorBody with a 400,
            // instead of the default text/plain — so the wire contract is uniform.
            // (API fuzzing flagged these as undocumented text/plain responses.)
            .app_data(web::PathConfig::default().error_handler(|err, _req| {
                madar_rust::errors::AppError::BadRequest(err.to_string()).into()
            }))
            .app_data(web::QueryConfig::default().error_handler(|err, _req| {
                madar_rust::errors::AppError::BadRequest(err.to_string()).into()
            }))
            .app_data(web::JsonConfig::default().error_handler(|err, _req| {
                madar_rust::errors::AppError::BadRequest(err.to_string()).into()
            }))
            .route(
                "/health",
                web::get().to(|| async { actix_web::HttpResponse::Ok().finish() }),
            )
            .configure(auth::routes::configure)
            .configure(orgs::routes::configure)
            .configure(users::routes::configure)
            .configure(permissions::routes::configure)
            .configure(branches::routes::configure)
            .configure(menu::routes::configure)
            .configure(inventory::routes::configure)
            .configure(recipes::routes::configure)
            .configure(shifts::routes::configure)
            .configure(tills::routes::configure)
            .configure(reservations::routes::configure)
            .configure(realtime::routes::configure)
            .configure(kitchen::routes::configure)
            .configure(tickets::routes::configure)
            .configure(stocktakes::routes::configure)
            .configure(sync::routes::configure)
            .configure(purchasing::routes::configure)
            .configure(orders::routes::configure)
            .configure(discounts::routes::configure)
            .configure(|cfg| reports::routes::configure(cfg, read_pool.clone()))
            .configure(uploads::routes::configure)
            .configure(bundles::routes::configure)
            .configure(insights::routes::configure)
            .configure(payment_methods::routes::configure)
            .configure(costing::routes::configure)
            .configure(delivery::routes::configure)
            .configure(qr_card::routes::configure)
            .configure(ai::routes::configure);

        // Public demo endpoints only when DEMO_MODE is on.
        if demo_enabled {
            app = app.configure(demo::routes::configure);
        }

        if enable_swagger_ui {
            app = app.service(
                SwaggerUi::new("/api-docs/swagger-ui/{_:.*}")
                    .url("/api-docs/openapi.json", ApiDoc::openapi()),
            );
        }

        app.service(Files::new("/uploads", &uploads_clone).use_last_modified(true))
    })
    // Drop slow/stalled clients so a resource-tight box can't be tied up.
    .client_request_timeout(std::time::Duration::from_secs(30))
    .client_disconnect_timeout(std::time::Duration::from_secs(5));

    if let Some(tls) = tls_config {
        tracing::info!("HTTPS on {} and HTTP on {}", https_addr, bind_addr);
        server
            .bind(&bind_addr)?
            .bind_rustls_0_23(&https_addr, tls)?
            .run()
            .await
    } else {
        tracing::info!("HTTP on {} (no TLS certs found)", bind_addr);
        server.bind(&bind_addr)?.run().await
    }
}

/// Build a Postgres pool with env-tunable sizing and PgBouncer compatibility.
/// `DB_STATEMENT_CACHE_CAPACITY=0` disables sqlx's prepared-statement cache —
/// REQUIRED behind PgBouncer in transaction-pooling mode, where pooled server
/// connections are shared per-transaction and server-side prepared statements
/// would otherwise collide. Unset → sqlx's default cache (direct-Postgres path).
async fn build_pool(url: &str, max_conns: u32) -> sqlx::Pool<sqlx::Postgres> {
    use std::str::FromStr;
    let mut opts = sqlx::postgres::PgConnectOptions::from_str(url).expect("invalid database URL");
    if let Some(cap) = env::var("DB_STATEMENT_CACHE_CAPACITY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        opts = opts.statement_cache_capacity(cap);
    }
    PgPoolOptions::new()
        .max_connections(max_conns)
        .connect_with(opts)
        .await
        .expect("Failed to connect to PostgreSQL")
}

fn build_tls_config() -> Option<rustls::ServerConfig> {
    let cert_file = env::var("SSL_CERT_FILE").ok()?;
    let key_file = env::var("SSL_KEY_FILE").ok()?;
    if cert_file.is_empty() || key_file.is_empty() {
        return None;
    }

    // Once env vars are set, failure to load them is a hard error — do not
    // silently fall back to HTTP, which would expose production traffic unencrypted.
    let cert_pem = fs::read(&cert_file)
        .unwrap_or_else(|e| panic!("SSL_CERT_FILE set but unreadable ({}): {}", cert_file, e));
    let key_pem = fs::read(&key_file)
        .unwrap_or_else(|e| panic!("SSL_KEY_FILE set but unreadable ({}): {}", key_file, e));

    let certs: Vec<rustls::pki_types::CertificateDer> =
        rustls_pemfile::certs(&mut cert_pem.as_slice())
            .filter_map(|c| c.ok())
            .collect();

    let mut keys: Vec<rustls::pki_types::PrivateKeyDer> =
        rustls_pemfile::pkcs8_private_keys(&mut key_pem.as_slice())
            .filter_map(|k| k.ok().map(rustls::pki_types::PrivateKeyDer::from))
            .collect();

    if keys.is_empty() {
        keys = rustls_pemfile::rsa_private_keys(&mut key_pem.as_slice())
            .filter_map(|k| k.ok().map(rustls::pki_types::PrivateKeyDer::from))
            .collect();
    }

    if certs.is_empty() || keys.is_empty() {
        panic!(
            "SSL_CERT_FILE/SSL_KEY_FILE are set but contain no parseable certs/keys — refusing to start without TLS"
        );
    }

    Some(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, keys.remove(0))
            .unwrap_or_else(|e| panic!("TLS configuration error: {}", e)),
    )
}
