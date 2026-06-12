# Claude Code Conventions

## Project Overview
This is a Rust backend project (`sufrix-rust`) built for the Sufrix ecosystem. It uses Actix-Web for the HTTP framework and SQLx (PostgreSQL) for database interactions.

## Technology Stack
- **Language**: Rust (Edition 2024)
- **Web Framework**: Actix-Web 4
- **Database**: PostgreSQL (via `sqlx` 0.7)
- **Async Runtime**: Tokio
- **API Documentation**: OpenAPI / Swagger (via `utoipa` 5.5)
- **Serialization**: Serde, JSON

## Development Workflow
- **Build**: `cargo build`
- **Run**: `cargo run`
- **Test**: `cargo test`
- **Check**: `cargo check`
- **OpenAPI Export**: `cargo run --bin export-openapi`
- **Reprice order cost snapshots at current recipes & ingredient costs** (operator-only, never exposed over HTTP):
  `cargo run --bin backfill-cost-snapshots -- (--org <uuid> | --branch <uuid>) [--dry-run]`
  Rewrites `order_items.unit_cost/line_cost` + addon/optional/bundle-component costs as if each
  line were ordered today (current recipe/addon rollups × quantities — mirrors the menu-engineering
  `cost_basis=current` view). Always `--dry-run` first.

## Coding Guidelines
1. **Idiomatic Rust**: Follow standard Rust formatting (`cargo fmt`) and linting (`cargo clippy`).
2. **Error Handling**: Use `thiserror` for defining custom domain errors. Avoid unwrapping unless absolutely necessary (e.g., in tests).
3. **Database**: Use `sqlx` macros for compile-time checked queries. Ensure migrations are placed in the `migrations/` directory.
4. **API Documentation**: All new endpoints must be annotated with `#[utoipa::path(...)]` and included in the OpenAPI documentation. Make sure to define response schemas using `ToSchema` for structures.
5. **Types**: Use `uuid::Uuid` for primary keys and references. Use `rust_decimal::Decimal` or `bigdecimal::BigDecimal` for currency and financial calculations to avoid floating-point errors.
6. **Async**: Rely on `tokio` for async operations. Use `actix_web::web::Data` for shared application state (e.g., database connection pools).

## File Structure
- `src/`: Main application source code.
  - `main.rs`: Entry point for the server.
  - `lib.rs`: Library module exports.
  - `bin/`: Additional binaries (e.g., `export_openapi.rs`).
- `tests/`: Integration tests.
- `migrations/`: SQL files for `sqlx` migrations.
- `api_dumps/`: Stored OpenAPI and API dump data.

## Related Projects (Ecosystem)
When working on API changes or feature implementations, be aware that this backend interacts with other projects in the Sufrix ecosystem. You may need to search or modify code in these directories:
- **SufrixDashboard**: `/Users/shawket/Desktop/SufrixDashboard` (Frontend Dashboard application)
- **Sufrix POS**: `/Users/shawket/Desktop/sufrix_pos` (Point of Sale frontend application)
You can use file read commands or `cd` into these directories to analyze frontend consumption of this backend API.
