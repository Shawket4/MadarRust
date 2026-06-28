# Madar — Portfolio context for `MadarRust`

---

## 1. Repository identity

- **Repo name:** `MadarRust` (GitHub remote: `https://github.com/shawket4/RueRust.git`)
- **Role in the system:** Sole HTTP backend for the Madar POS platform. Serves a Flutter teller app and a React management dashboard. Acts as the source of truth for the generated `openapi.json` that both clients consume.
- **Primary language(s) with %:** Rust — 100% of production source (82 `.rs` files in `src/`). Utility scripts at repo root are Python/Bash (not counted as production code).
- **LOC (excluding deps and build output):** 27,340 lines across all `.rs` files in `src/`; 30,165 lines total when `.sql` and `Cargo.toml` are included (measured with `wc -l`).
- **Source file count by extension:**
  - `.rs` — 82 files in `src/`
  - `.sql` — 5 migration files
  - `.toml` — 1 (`Cargo.toml`)
- **First commit date:** `2026-03-09 00:50:04 +0200`
- **Last commit date:** `2026-06-02 00:22:05 +0300`
- **Total commits:** 133
- **Active contributors (last 90 days):** 1 — Shawket Ibrahim (133 commits, per `git shortlog -sn --since='90 days ago' --all`)

---

## 2. Tech stack (EXACT versions from `Cargo.toml` / `Cargo.lock`)

### HTTP Framework
| Crate | Version |
|---|---|
| `actix-web` | `4` (with features: brotli, gzip, zstd compression + rustls-0_23) |
| `actix-cors` | `0.7` |
| `actix-multipart` | `0.7` |
| `actix-files` | `0.6` |

### Async Runtime
| Crate | Version |
|---|---|
| `tokio` | `1` (features: full, macros, rt-multi-thread) |
| `futures` | `0.3` |

### Database
| Crate | Version |
|---|---|
| `sqlx` | `0.7` (features: runtime-tokio-rustls, postgres, uuid, chrono, macros, bigdecimal, rust_decimal) |

### Auth / Crypto
| Crate | Version |
|---|---|
| `jsonwebtoken` | `9` |
| `bcrypt` | `0.15` |

### Serialization
| Crate | Version |
|---|---|
| `serde` | `1` (features: derive) |
| `serde_json` | `1` |

### Numeric
| Crate | Version |
|---|---|
| `bigdecimal` | `0.3` (features: serde) |
| `rust_decimal` | `1` (features: serde-float, maths) |
| `rust_decimal_macros` | `1` |

### Date/Time
| Crate | Version |
|---|---|
| `chrono` | `0.4` (features: serde) |

### IDs
| Crate | Version |
|---|---|
| `uuid` | `1` (features: v4, v5, serde) |

### API Documentation
| Crate | Version |
|---|---|
| `utoipa` | `5.5.0` (features: actix_extras, chrono, uuid, decimal) |
| `utoipa-swagger-ui` | `9.0.2` (features: actix-web) |

### TLS
| Crate | Version |
|---|---|
| `rustls` | `0.23` |
| `rustls-pemfile` | `2` |

### Observability
| Crate | Version |
|---|---|
| `tracing` | `0.1` |
| `tracing-actix-web` | `0.7` |
| `tracing-subscriber` | `0.3` (features: env-filter) |

### Images / Uploads
| Crate | Version |
|---|---|
| `image` | `0.25` (features: jpeg, png, gif, webp, bmp) |

### HTTP Client (for Google Translate)
| Crate | Version |
|---|---|
| `reqwest` | `0.13.4` (features: json) |
| `urlencoding` | `2.1.3` |

### Misc
| Crate | Version |
|---|---|
| `dotenvy` | `0.15` |
| `thiserror` | `1` |

### Dev Dependencies
| Crate | Version |
|---|---|
| `tempfile` | `3.27.0` |

### Database (PostgreSQL)
- PostgreSQL **17.10** (confirmed in migration header: `20260531200000_full_schema.sql` line 5)
- Extensions used: `pg_trgm`, `pgcrypto`
- Default currency: **EGP** (Egyptian Pound); default tax rate: **14%** (`organizations.tax_rate DEFAULT 0.14`)
- Default timezone: **`Africa/Cairo`** (`branches.timezone`)

---

## 3. Architecture summary

The codebase is structured as a **Rust library crate** (`src/lib.rs`) plus two binaries: the main HTTP server (`src/main.rs`) and an OpenAPI exporter (`src/bin/export_openapi.rs`). The separation keeps `ApiDoc` reachable without booting the HTTP server. Each business domain lives in its own directory under `src/` (e.g. `src/orders/`, `src/menu/`, `src/menu_advisor/`), with a consistent four-file pattern: `mod.rs`, `routes.rs`, `handlers.rs`, and `tests.rs`. All routes are protected by a custom `JwtMiddleware` actix-web transform (`src/auth/middleware.rs`) that extracts Bearer tokens and injects `Claims` into request extensions; per-handler access control is enforced by `src/permissions/checker.rs` which queries `role_permissions` and `permissions` tables in that order. The `menu_advisor` module (`src/menu_advisor/`) is architecturally isolated: `engine.rs` is a zero-import, pure deterministic analytics engine, `adapter.rs` bridges it to Postgres, `persistence.rs` stores run results, and `handlers.rs` dispatches engine work via `tokio::spawn`. The database schema is monolithic PostgreSQL managed by five SQLx migration files in `migrations/`.

---

## 4. Module inventory

| Module | Purpose | Key files | Public surface |
|---|---|---|---|
| `auth` | JWT issuance, Bearer middleware, role guards | `handlers.rs`, `jwt.rs`, `middleware.rs`, `guards.rs` | `POST /auth/login`, `GET /auth/me`, `GET /auth/permissions` |
| `orgs` | Organization CRUD, logo upload (super-admin only) | `handlers.rs`, `routes.rs` | `GET/POST/PATCH/DELETE /orgs`, `POST /orgs/{id}/logo` |
| `branches` | Branch CRUD within an org | `handlers.rs`, `routes.rs` | `GET/POST/PATCH/DELETE /branches` |
| `users` | User accounts, PIN/password, branch assignments | `handlers.rs`, `routes.rs` | `GET/POST/PATCH/DELETE /users`, `/users/{id}/branches` |
| `permissions` | Role defaults seeded at startup, per-user overrides, RBAC checker | `seeder.rs`, `checker.rs`, `handlers.rs` | `GET/PUT /permissions/user/{id}`, `GET/PUT /permissions/roles` |
| `menu` | Categories, menu items, sizes, addon slots, optional fields, public menu | `handlers.rs` (85 KB), `routes.rs` | ~24 routes under `/categories`, `/menu-items`, `/addon-items`, `/menu/public` |
| `inventory` | Org ingredient catalog, branch stock, adjustments, transfers | `handlers.rs`, `routes.rs` | Routes under `/inventory/orgs/{id}/catalog`, `/inventory/branches/{id}/stock`, `/inventory/*/adjustments`, `/inventory/*/transfers` |
| `recipes` | Ingredient-to-menu-item and ingredient-to-addon mappings | `handlers.rs`, `routes.rs` | `POST/DELETE /recipes/drinks/{id}`, `POST/DELETE /recipes/addons/{id}` |
| `orders` | Full order lifecycle: create, list, get, void, export, idempotency, inventory deduction, bundle resolution | `handlers.rs` (80 KB, 2051 lines), `component_resolve.rs`, `routes.rs` | `POST/GET /orders`, `GET /orders/{id}`, `POST /orders/{id}/void`, `POST /orders/preview-recipe`, `GET /orders/export` |
| `shifts` | Shift open/close/force-close, cash movements, shift reports | `handlers.rs`, `routes.rs` | `POST /shifts/branches/{id}/open`, `POST /shifts/{id}/close`, `POST /shifts/{id}/force-close`, `GET /shifts/{id}/report`, cash movement sub-routes |
| `discounts` | Discount definition CRUD | `handlers.rs`, `routes.rs` | `GET/POST/PATCH/DELETE /discounts` |
| `bundles` | Combo bundle CRUD, branch availability, activate/archive, performance analytics | `handlers.rs` (1192 lines), `routes.rs` | `GET/POST /bundles`, `GET/PATCH/DELETE /bundles/{id}`, `POST /bundles/{id}/activate`, `POST /bundles/{id}/archive`, `GET /bundles/available` |
| `reports` | Sales analytics: shift summary, inventory discrepancies, branch comparisons, timeseries, teller stats, addon sales, bundle sales | `handlers.rs` (38 KB), `routes.rs` | 11 routes under `/reports` |
| `menu_advisor` | Pure analytics engine: price suggestions, bundle suggestions, removal scenarios, decision tracking | `engine.rs` (93 KB), `adapter.rs`, `persistence.rs` (55 KB), `handlers.rs` | Routes under `/menu-advisor/branches/{id}` for runs, suggestions, decisions |
| `payment_methods` | Dynamic per-org payment method configuration | `handlers.rs`, `routes.rs` | `GET/POST/PATCH /payment-methods`, activate/deactivate sub-routes |
| `uploads` | Image upload for menu items, org logos | `handlers.rs`, `routes.rs` | `POST /menu-items/{id}/image` |
| `translation` | Auto-translate missing languages via Google Translate API (paid + free fallback) | `translation.rs` | Internal helper; called by menu, bundles, orders modules |
| `openapi` | Aggregates all `#[utoipa::path]` handlers into `ApiDoc`; adds Bearer JWT security scheme | `openapi.rs` | `GET /api-docs/openapi.json` (when Swagger UI enabled) |
| `errors` | Shared `AppError` enum, `ErrorBody` schema, `AppErrorResponse` utoipa helper | `errors.rs` | Used across all handlers |
| `models` | Cross-cutting DB row types: `User`, `UserPublic`, `UserRole`, `Discount` | `models/mod.rs` | Imported by auth, users, orders |
| `e2e_tests` | Full-stack `#[sqlx::test]` integration scenarios (library-internal, gated by `#[cfg(test)]`) | `e2e_tests.rs` | Test-only |

---

## 5. Data model

- **Total tables/migrations count:** 5 migration files. The main schema file (`20260531200000_full_schema.sql`, 2665 lines) defines all core tables. Subsequent migrations add: `org_payment_methods` (2026-05-31), `name_translations` JSONB columns to categories/menu_items/addon_items (2026-06-01), `name_translations` to order items/addons/optionals (2026-06-01), and nested translation fields (2026-06-01).

- **Top entity tables (from `20260531200000_full_schema.sql`):**
  | Table | Description |
  |---|---|
  | `organizations` | Root tenant entity; holds currency, tax rate, logo |
  | `branches` | Physical locations within an org; holds printer config, timezone |
  | `users` | Accounts with dual auth: `password_hash` (email login) or `pin_hash` (teller PIN) |
  | `user_branch_assignments` | Many-to-many user↔branch |
  | `role_permissions` | Default RBAC grants per role |
  | `permissions` | Per-user permission overrides |
  | `categories` | Menu categories with display order and soft-delete |
  | `menu_items` | Products with `base_price`, category FK, soft-delete |
  | `item_sizes` | Size variants (small/medium/large/extra_large/one_size) with price override |
  | `menu_item_addon_slots` | Typed addon slots attached to items |
  | `addon_items` | Addons with type, default price, `name_translations` |
  | `menu_item_optional_fields` | Optional "extras" with optional ingredient deduction link |
  | `menu_item_recipes` | Ingredient quantities per menu item per size |
  | `addon_item_ingredients` | Ingredient quantities per addon |
  | `org_ingredients` | Ingredient catalog at org level with cost history |
  | `branch_inventory` | Per-branch stock levels |
  | `branch_inventory_adjustments` | Inventory add/remove/transfer events |
  | `branch_inventory_transfers` | Cross-branch ingredient transfers |
  | `shifts` | Teller sessions with open/close/force-close state and cash reconciliation |
  | `shift_cash_movements` | Cash deposits/withdrawals within a shift |
  | `shift_inventory_counts` | End-of-shift actual vs expected stock with generated `discrepancy` column |
  | `orders` | Orders with full payment, discount, tip, void fields + `idempotency_key` |
  | `order_items` | Line items with `deductions_snapshot` JSONB |
  | `order_item_addons` | Addon lines per order item |
  | `order_item_optionals` | Optional field selections per order item |
  | `order_payments` | Split-payment records per order |
  | `order_line_bundle_components` | Bundle component snapshot per order line |
  | `order_line_bundle_component_addons` | Addons on bundle component lines |
  | `order_line_bundle_component_optionals` | Optional fields on bundle component lines |
  | `bundles` | Combo bundles with status (draft/active/archived), date/time availability windows |
  | `bundle_components` | Items within a bundle with quantity and position |
  | `bundle_branch_availability` | Restricts bundles to specific branches (empty = all branches) |
  | `bundle_price_epochs` | Historical bundle price changes |
  | `discounts` | Discount rules (percentage or fixed) |
  | `org_payment_methods` | Dynamic payment methods per org with translations, icon, color |
  | `menu_item_price_epochs` | Historical menu item price changes |
  | `ingredient_cost_history` | Historical ingredient cost per unit |
  | `menu_advisor_runs` | Analytics run metadata |
  | `menu_advisor_price_suggestions` | Stored price suggestions per run |
  | `menu_advisor_bundle_suggestions` | Stored bundle suggestions per run |
  | `menu_advisor_removal_scenarios` | Stored removal scenarios per run |
  | `menu_advisor_decisions` | User decisions on suggestions |
  | `branch_menu_overrides` | Per-branch item price or availability overrides |

- **Notable schema decisions:**
  - Monetary values stored as **integers (piastres/cents)** — no floating-point money columns. `tax_rate` and ingredient costs are `NUMERIC`.
  - **Soft deletes** via `deleted_at` on `organizations`, `branches`, `users`, `menu_items`, `org_ingredients`.
  - `shift_inventory_counts.discrepancy` and `shifts.cash_discrepancy` are **generated stored columns** (computed as `expected - actual` and `closing_cash_declared - closing_cash_system`).
  - `orders.idempotency_key` UUID column enables idempotent order creation for offline-sync scenarios.
  - `JSONB` used for multi-language translations (`name_translations`, `label_translations`, `deductions_snapshot`, `components_json`, etc.) rather than a normalized translations table.
  - `users` table enforces a DB-level `chk_login_method` CHECK constraint (`password_hash IS NOT NULL OR pin_hash IS NOT NULL`) and `chk_super_admin_no_org` CHECK constraint.
  - `branch_inventory_transfers` has a `chk_transfer_branches` CHECK constraint preventing source = destination.
  - PostgreSQL `pg_trgm` extension is included (present in schema dump) but no trigram indexes are visible in the main migration.

- **Foreign key topology:** `branches.org_id → organizations.id`; `users.org_id → organizations.id`; `menu_items.org_id → organizations.id`, `menu_items.category_id → categories.id`; `orders.branch_id → branches.id`, `orders.shift_id → shifts.id`, `orders.teller_id → users.id`; `order_items.order_id → orders.id`, `order_items.menu_item_id → menu_items.id`; `bundle_components.bundle_id → bundles.id`, `bundle_components.item_id → menu_items.id`; `org_payment_methods.org_id → organizations.id` (with CASCADE DELETE).

---

## 6. API surface

- **Total route count:** 138 `.route(` call-sites across all `routes.rs` files (measured with `grep -r '\.route(' src/ --include='*.rs' | wc -l`). 22 additional `.service(` entries wrap scoped route groups.

- **Route groups with counts (from `src/openapi.rs` `paths(...)` block):**
  | Group | Handler count |
  |---|---|
  | `auth` | 3 |
  | `orgs` | 6 |
  | `branches` | 5 |
  | `users` | 8 |
  | `permissions` | 6 |
  | `menu` | 24 |
  | `uploads` | 1 |
  | `inventory` | 14 |
  | `recipes` | 6 |
  | `discounts` | 4 |
  | `bundles` | 9 |
  | `shifts` | 9 |
  | `orders` | 6 |
  | `reports` | 11 |
  | `payment_methods` | 5 |
  | `menu_advisor` | not in OpenAPI `paths()` list — served but not documented in `openapi.rs` |
  | `/health` | 1 (anonymous, no auth) |
  | `/api-docs/openapi.json` + Swagger UI | conditional on `MADAR_ENABLE_SWAGGER_UI` env var |

- **Auth model:**
  - Dual-mode login (`POST /auth/login`): email+password for admins/managers, PIN+name for tellers.
  - JWT issued with `jsonwebtoken` v9 using HS256 (`Header::default()`). Claims carry: `sub` (user UUID), `org_id`, `role`, `branch_id` (tellers only), `exp`, `iat`.
  - Token TTL: 12 hours for tellers, 24 hours for all other roles (hardcoded in `src/auth/handlers.rs` line 171).
  - All protected routes wrap their `web::scope` with `JwtMiddleware` (`src/auth/middleware.rs`), which verifies the token and injects `Claims` into request extensions.
  - RBAC: `src/permissions/checker.rs` resolves: super_admin → always granted → per-user `permissions` table override → role default from `role_permissions` table → deny. Role hierarchy: `super_admin > org_admin > branch_manager > teller`.
  - `src/auth/guards.rs` provides `require_super_admin`, `require_org_admin`, `require_manager`, and `require_same_org` helper fns called directly in handlers.
  - Role permissions are **seeded at startup** on every boot (`src/permissions/seeder.rs` called from `main.rs` line 44).

- **Response format:** JSON throughout (`actix-web::HttpResponse::*.json()`). Error responses always follow `{ "error": "<message>" }` shape (enforced via `ErrorBody` in `src/errors.rs`). Pagination uses `{ data: [...], total, page, per_page, total_pages, summary }` shape in orders.

- **Real-time channels:** None present in this repo. No WebSocket, SSE, or push endpoints.

---

## 7. Frontend specifics

Not applicable — backend repo. The Flutter teller app and React dashboard are separate codebases. This repo generates `openapi.json` (committed at root, 469 KB) that both clients consume. The `.env` references `UPLOADS_BASE_URL=https://madar-pos.ddns.net/api/uploads`.

---

## 8. Offline / sync

**Partial support, client-side driven:**
- `orders.idempotency_key` UUID column + server-side check in `POST /orders` (`src/orders/handlers.rs` lines 309–319): if an `Idempotency-Key` header UUID matches an existing order, the existing order is returned immediately (no duplicate insertion).
- `GET /orders` accepts an `updated_after` query parameter for delta sync.
- No SQLite, local cache, conflict-resolution protocol, or sync engine exists in this repo. The Flutter client is presumed to implement offline queuing.

---

## 9. Hardware integrations

- **Thermal receipt printer (Star / Epson):** The `branches` table has `printer_ip inet`, `printer_port integer DEFAULT 9100`, and `printer_brand public.printer_brand` (enum: `'star'`, `'epson'`). The brand/IP/port fields are returned in branch CRUD responses. **No Rust code in this repo implements actual ESC/POS or StarPRNT print commands** — printing is implemented in the Flutter client using the data from the branch record.

---

## 10. Third-party integrations

| Name | Status | Entry point |
|---|---|---|
| **Google Translate API** (paid v2) | Implemented, active; falls back to free unofficial endpoint if `GOOGLE_TRANSLATE_API_KEY` is unset or placeholder. Supported languages configured via `SUPPORTED_LANGUAGES` env var (default: `en,ar`). | `src/translation.rs` — `ensure_translations()` and `ensure_translations_json()` |
| **Talabat** (food aggregator) | Referenced only in default payment method seed data: `talabat_online` and `talabat_cash` payment method names (`.env`-seeded labels). No API integration in this repo. | `migrations/20260531130918_dynamic_payment_methods.sql` lines 39–40 |
| **Let's Encrypt / DDNS** | TLS cert paths referenced in `.env` (commented out): `SSL_CERT_FILE=/etc/letsencrypt/live/madar-pos.ddns.net/fullchain.pem`. Server builds TLS config from env vars via `build_tls_config()` in `main.rs`. | `src/main.rs` lines 128–165 |

---

## 11. CI/CD and deployment

- **Provider:** GitHub Actions (`.github/workflows/deploy.yml`)
- **Trigger:** Push to `main` or `master` branch, or manual `workflow_dispatch`
- **Runner:** `ubuntu-latest`
- **Steps:**
  1. Checkout (`actions/checkout@v4`)
  2. Install Rust stable toolchain for `x86_64-unknown-linux-gnu` (`dtolnay/rust-toolchain@stable`)
  3. Three-layer cargo cache: registry, git sources, `target/` directory (keyed on `Cargo.lock` hash)
  4. Install system deps: `pkg-config libssl-dev`
  5. `cargo build --release --target x86_64-unknown-linux-gnu`, then `strip` the binary
  6. Package binary into `deployment.tar.gz`
  7. SSH keyscan + key setup from `secrets.SSH_PRIVATE_KEY`, `secrets.SSH_KNOWN_HOSTS`
  8. `scp` tarball to `$SSH_HOST:/tmp/`
  9. Remote SSH: stop `madar-rust` systemd service → extract binary to `/opt/madar-rust/` → backup previous binary to `/opt/madar-rust/backups/` (keeps last 5) → `systemctl start madar-rust` → print status
- **Target:** A single VPS (SSH host configured via GitHub secret `SSH_HOST`)
- **Mechanism:** Binary replacement via SCP + systemd service restart
- **Environments:** One production environment (no staging environment visible in workflow)
- **Required secrets:** `SSH_PRIVATE_KEY`, `SSH_KNOWN_HOSTS`, `SSH_PORT`, `SSH_USER`, `SSH_HOST`
- **No Docker, no Kubernetes, no container build step present**

---

## 12. Performance signals

- PostgreSQL connection pool: `max_connections(10)` (hardcoded in `src/main.rs` line 38)
- HTTP response compression: Brotli, gzip, zstd enabled via `actix-web` feature flags and `Compress::default()` middleware
- CORS: `max_age(3600)` cache for preflight responses
- No load test results, benchmark files, or performance metrics documented in this repo

---

## 13. Test coverage

- **Framework:** `#[sqlx::test]` (integration tests against a real PostgreSQL instance via SQLx's test harness, which provisions isolated databases per test)
- **`#[sqlx::test]` count:** 142 test functions (measured with `grep -r '#\[sqlx::test\]' src/ --include='*.rs' | wc -l`)
- **`#[test]` count:** 14 additional unit-style tests (measured with `grep -r '#\[test\]' src/ --include='*.rs' | wc -l`)
- **Test location:** Co-located in each module directory (e.g. `src/auth/tests.rs`, `src/orders/tests.rs`, `src/menu/tests.rs`) plus the large end-to-end scenarios in `src/e2e_tests.rs` (1144 lines, 52 KB, gated by `#[cfg(test)]` in `src/lib.rs`)
- **E2E scenario count:** 4 named scenarios in `src/e2e_tests.rs`:
  1. `test_e2e_merchant_setup_and_operation_happy_path` — full org/branch/user/menu/permission flow
  2. `test_e2e_tenant_and_role_isolation_security_violation_path` — cross-org attack attempts (expects 403)
  3. `test_e2e_kitchen_inventory_order_lifecycle` — recipes, inventory deduction, negative stock, void+rollback, shift close/report
  4. `test_e2e_menu_advisor_bundle_promotion_workflow` — advisor run trigger, promotion loop
- **External `tests/` directory:** Present (`tests/openapi_test.rs`, 246 bytes; `tests/common/` is empty)
- **Coverage %:** Not measured — no `cargo-tarpaulin` or similar config in repo

---

## 14. Notable patterns

### 1. Typed `AppError` → HTTP response mapping (`src/errors.rs`)
Every handler returns `Result<HttpResponse, AppError>`. The `ResponseError` impl maps variants to HTTP status codes. `AppErrorResponse` produces a single shared `$ref` to `ErrorBody` in the OpenAPI spec (not inlined per-handler), keeping the generated spec compact.

```rust
impl actix_web::ResponseError for AppError {
    fn error_response(&self) -> HttpResponse {
        let body = ErrorBody { error: self.to_string() };
        match self {
            AppError::Unauthorized(_) => HttpResponse::Unauthorized().json(body),
            AppError::Forbidden(_)    => HttpResponse::Forbidden().json(body),
            AppError::NotFound(_)     => HttpResponse::NotFound().json(body),
            // ...
        }
    }
}
```

### 2. Three-level RBAC resolution (`src/permissions/checker.rs`)
```rust
// 1. super_admin bypasses all
if claims.role == UserRole::SuperAdmin { return Ok(()); }
// 2. per-user override in `permissions` table
if let Some(granted) = user_override { return if granted { Ok(()) } else { Err(Forbidden) }; }
// 3. role default in `role_permissions` table
match role_default { Some(true) => Ok(()), _ => Err(Forbidden) }
```
Role permissions are re-seeded idempotently on every server boot (`src/permissions/seeder.rs`), ensuring defaults are always current without requiring a dedicated migration for each RBAC change.

### 3. Idempotent order creation via header (`src/orders/handlers.rs`)
```rust
let idempotency_key = req.headers()
    .get("Idempotency-Key")
    .and_then(|v| v.to_str().ok())
    .and_then(|s| Uuid::parse_str(s).ok());
if let Some(key) = idempotency_key
    && let Some(existing) = fetch_order_by_idempotency_key(pool, key).await? {
        return Ok(HttpResponse::Ok().json(OrderFull { order: existing, items }));
    }
```
Client sends `Idempotency-Key: <uuid>` header; duplicate network retries return the original order rather than creating a duplicate.

### 4. Milk/coffee "smart swap" deduction (`src/orders/handlers.rs`, lines ~724–792)
When a customer changes milk type or coffee type via an addon, the engine replaces the base recipe ingredient in the deduction list rather than doubling it. The swap logic uses ingredient `category` (`milk`, `coffee_bean`) to identify base vs. replacement, and recalculates the addon's price delta vs. the base addon's price.

### 5. Library crate / binary separation (`src/lib.rs`, `src/main.rs`, `src/bin/export_openapi.rs`)
The entire app lives in a library crate (`madar_rust`). Both `src/main.rs` (HTTP server) and `src/bin/export_openapi.rs` (offline OpenAPI exporter) depend on the library. This means `cargo run --bin export-openapi` regenerates `openapi.json` without starting the server — the committed `openapi.json` (469 KB) at root is the artifact from this exporter.

---

## 15. What this repo is genuinely strong at

- **Deep inventory coupling:** Orders automatically deduct ingredient stock based on recipes (`src/orders/handlers.rs`), with a "smart swap" that correctly handles milk/coffee type substitutions without double-counting. Void orders optionally restore inventory (`void_order` with `restore_inventory: true`). The E2E tests in `src/e2e_tests.rs` (scenario C) verify stock goes negative and rolls back correctly.

- **Analytics engine quality:** `src/menu_advisor/engine.rs` (2308 lines, zero imports from the rest of the crate) implements a full Kasavana-Smith menu engineering algorithm with: recency-weighted KPIs, Wilson 95% confidence intervals for popularity, two parallel classification taxonomies (CM-tracked vs revenue-only, never mixing), hysteresis to prevent quadrant flapping, market basket association mining with lift scoring, bundle profit forecasting with incremental CM triplets, removal scenario simulation with complementary loss modeling, and Egyptian café price rounding rules. All pure Rust with no external ML libs.

- **Well-typed error handling:** `AppError` with `thiserror`, `From<sqlx::Error>` derivation, uniform `{ "error": "..." }` JSON shape, and the OpenAPI `AppErrorResponse` pattern that keeps the spec DRY. Every handler returns `Result<HttpResponse, AppError>`.

- **Comprehensive integration tests:** 142 `#[sqlx::test]` functions covering auth, multi-tenancy boundaries, inventory lifecycle, shift reconciliation, and menu advisor promotion workflow. Tests hit real PostgreSQL with isolated per-test databases.

- **OpenAPI-first documentation:** Every production handler is annotated with `#[utoipa::path]` and registered in `src/openapi.rs`. The `export-openapi` binary generates a self-consistent spec. Swagger UI is feature-flagged for dev/staging via `MADAR_ENABLE_SWAGGER_UI=true` and explicitly disabled in production.

---

## 16. Honest weaknesses / rough edges

- **No staging environment in CI/CD:** The GitHub Actions deploy workflow (`deploy.yml`) targets a single VPS with no intermediate staging step. A broken build goes straight to production after the binary swap.

- **Committed `.env` with real credentials:** The `.env` file is present in the repository with a live database URL (`postgres://rue:TheRue%40123%23%25@100.101.100.57:5432/rue`) and JWT secret (`Sh@d0wW1zard`). The Google Translate API key is a placeholder, but the DB and JWT secrets are not. The `.gitignore` file (8 bytes) does not appear to exclude `.env`.

- **`menu_advisor` routes not in OpenAPI `paths()`:** The menu advisor handlers are registered in `main.rs` (`.configure(menu_advisor::routes::configure)`) but none appear in the `paths(...)` block of `src/openapi.rs`. The Flutter/React clients cannot rely on generated types for this module.

- **Single-contributor, single-VPS production target:** All 133 commits are from one author. There is no documented DR strategy, database backup automation, horizontal scaling config, or load balancer in any config file in this repo.

- **N+1 query risk in order creation:** `src/orders/handlers.rs` `create_order` executes multiple individual `sqlx::query` calls inside a `for item_input in &body.items` loop (addon resolution, recipe lookup, optional field resolution per item). No batch query or explicit transaction wrapping the full loop is visible in the first 800 lines shown; for large orders or menu advisor runs with many SKUs this could be slow.

---

## 17. Production deployments

- **Deployment target:** A VPS at `madar-pos.ddns.net` (from `.env`: `UPLOADS_BASE_URL=https://madar-pos.ddns.net/api/uploads`)
- **Production server URL declared in OpenAPI spec:** `https://api.madar.app` (`src/openapi.rs` line 33)
- **Service name on VPS:** `madar-rust` (systemd unit, from deploy script)
- **Install path on VPS:** `/opt/madar-rust/madar-rust`
- **HTTP port:** `0.0.0.0:8081` (from `.env` `BIND_ADDR`)
- **HTTPS port:** TLS cert paths commented out in `.env` — HTTPS may not be active locally, served via nginx reverse proxy based on DDNS domain
- **No git tags marking release versions are visible** (not checked; `git rev-list --all --count` = 133 with no tag references shown)
- **Database:** PostgreSQL on `100.101.100.57:5432`, database `rue`

---

## 18. Anything else worth knowing

- **Dual-language UX:** The system is built for Arabic-first markets (Egypt). Default currency is EGP, default timezone is `Africa/Cairo`, and all translatable strings (categories, menu items, addon items, order item snapshots, payment method labels, bundle names) carry a `name_translations JSONB` column with `en`/`ar` keys. The `translation.rs` module auto-fills missing language slots on create/update using Google Translate.

- **"Smart swap" milk/coffee pricing logic** is a domain-specific differentiator for specialty coffee shops: when a customer orders an espresso and selects "oat milk" instead of the default whole milk, the system calculates the price delta (oat milk addon price minus base milk addon price), deducts the correct ingredient from inventory (oat milk, not both milks), and charges only the upcharge. This logic is in `src/orders/handlers.rs` lines ~724–792 and is specifically tested in the e2e suite.

- **`menu_advisor` is explicitly read-only:** The OpenAPI description for the `menu_advisor` tag reads: *"Read-only pricing, bundle, and removal suggestions. Never edits menus — the differentiator vs. generic POS."* (`src/openapi.rs` line 50). The engine writes to its own result tables but does not touch `menu_items`, `bundles`, or `discounts`.

- **Repo root contains significant non-production artifacts:** `openapi.json` (469 KB), `menu.json` (37 KB), `menu_fixed.json` (41 KB), `backend_advisor_code.txt` (90 KB), many Python patch scripts (`patch_rust.py`, `patch_bundles.py`, etc.), shell backfill scripts, and a `agent_prompts/` directory. These appear to be development tooling and AI-assisted editing artifacts, not part of the deployed binary.

- **Flutter client references in git history:** `git log --pretty=format: --name-only --since=3.months` shows files like `rue_pos/lib/features/order/order_screen.dart` and `rue_pos/lib/core/api/shift_api.dart`, indicating the Flutter client was previously in the same git history before being moved to a separate location.

- **The `BUNDLES_AUDIT.md` and `MENU_ADVISOR_NOTES.md` files** at root (7 KB and 7 KB respectively) contain in-progress design notes and audit findings for those features — useful for onboarding context.

---

*Generated: 2026-06-04. All facts cited from actual file contents, git commands, and shell output. No values estimated.*
