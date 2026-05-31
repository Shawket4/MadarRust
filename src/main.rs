//! Server entry point. The actual app lives in the library crate so the
//! OpenAPI exporter binary can reach `ApiDoc` without booting the server.

use actix_cors::Cors;
use actix_files::Files;
use actix_web::middleware::Compress;
use actix_web::{web, App, HttpServer};

use dotenvy::dotenv;
use sqlx::postgres::PgPoolOptions;
use std::{env, fs};
use tracing_subscriber::EnvFilter;

use sufrix_rust::openapi::ApiDoc;
use sufrix_rust::{
    auth, branches, bundles, discounts, inventory, menu, menu_advisor,
    orders, orgs, payment_methods, permissions, recipes, reports, shifts, uploads, users,
};

use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let db_url      = env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let jwt_secret  = env::var("JWT_SECRET").expect("JWT_SECRET must be set");
    let uploads_dir = env::var("UPLOADS_DIR").unwrap_or_else(|_| "./uploads".to_string());

    fs::create_dir_all(&uploads_dir).expect("Failed to create uploads directory");

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&db_url)
        .await
        .expect("Failed to connect to PostgreSQL");

    tracing::info!("Seeding default role permissions into database...");
    permissions::seeder::seed_role_permissions(&pool)
        .await
        .expect("Failed to seed default role permissions");

    let pool          = web::Data::new(pool);
    let jwt_secret    = web::Data::new(auth::jwt::JwtSecret(jwt_secret));
    let uploads_clone = uploads_dir.clone();
    let bind_addr     = env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let https_port    = env::var("HTTPS_PORT").unwrap_or_else(|_| "8443".to_string());
    let https_addr    = format!("0.0.0.0:{}", https_port);

    // Swagger UI is dev/staging only. In production leave the env var
    // unset (or set to a falsy value) and front the spec endpoint with
    // nginx basic auth if you need to expose it to a partner.
    let enable_swagger_ui = env::var("SUFRIX_ENABLE_SWAGGER_UI")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);

    let tls_config = build_tls_config();

    tracing::info!("Starting sufrix-rust");
    tracing::info!("Uploads directory: {}", uploads_dir);
    if enable_swagger_ui {
        tracing::info!("Swagger UI enabled at /api-docs/swagger-ui/");
        tracing::info!("OpenAPI JSON at /api-docs/openapi.json");
    } else {
        tracing::info!("Swagger UI disabled (set SUFRIX_ENABLE_SWAGGER_UI=true to enable)");
    }

    let server = HttpServer::new(move || {
        let cors = Cors::default()
            .allow_any_origin()
            .allow_any_method()
            .allow_any_header()
            .max_age(3600);

        // Build the App. All `.wrap()` calls happen first so the App's
        // generic type is stable when we conditionally add Swagger UI.
        let mut app = App::new()
            .wrap(cors)
            .wrap(Compress::default())
            .app_data(pool.clone())
            .app_data(jwt_secret.clone())
            .route("/health", web::get().to(|| async { actix_web::HttpResponse::Ok().finish() }))
            .configure(auth::routes::configure)
            .configure(orgs::routes::configure)
            .configure(users::routes::configure)
            .configure(permissions::routes::configure)
            .configure(branches::routes::configure)
            .configure(menu::routes::configure)
            .configure(inventory::routes::configure)
            .configure(recipes::routes::configure)
            .configure(shifts::routes::configure)
            .configure(orders::routes::configure)
            .configure(discounts::routes::configure)
            .configure(reports::routes::configure)
            .configure(uploads::routes::configure)
            .configure(bundles::routes::configure)
            .configure(menu_advisor::routes::configure)
            .configure(payment_methods::routes::configure);

        if enable_swagger_ui {
            app = app.service(
                SwaggerUi::new("/api-docs/swagger-ui/{_:.*}")
                    .url("/api-docs/openapi.json", ApiDoc::openapi()),
            );
        }

        app.service(Files::new("/uploads", &uploads_clone).use_last_modified(true))
    });

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

fn build_tls_config() -> Option<rustls::ServerConfig> {
    let cert_file = env::var("SSL_CERT_FILE").ok()?;
    let key_file  = env::var("SSL_KEY_FILE").ok()?;
    if cert_file.is_empty() || key_file.is_empty() { return None; }

    let cert_pem = fs::read(&cert_file).ok().or_else(|| {
        tracing::warn!("SSL_CERT_FILE not found: {}", cert_file); None
    })?;
    let key_pem = fs::read(&key_file).ok().or_else(|| {
        tracing::warn!("SSL_KEY_FILE not found: {}", key_file); None
    })?;

    let certs: Vec<rustls::pki_types::CertificateDer> =
        rustls_pemfile::certs(&mut cert_pem.as_slice())
            .filter_map(|c| c.ok()).collect();

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
        tracing::warn!("Could not parse TLS certs/keys — falling back to HTTP");
        return None;
    }

    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, keys.remove(0))
        .map_err(|e| { tracing::warn!("TLS config error: {}", e); e })
        .ok()
}