# Menu Advisor — Codebase Investigation Notes

Reviewer: read this before opening any code file.

---

## Module Layout

Feature modules live in `src/<feature>/`. Each module contains:
- `mod.rs` — re-exports `handlers` and `routes`
- `handlers.rs` — all logic (models, queries, request/response DTOs, business logic)
- `routes.rs` — one `configure(cfg: &mut web::ServiceConfig)` function

No workspace crates. All code lives in a single binary crate (`madar-rust`).

---

## Error Type

`crate::errors::AppError` — a custom enum using `thiserror`. Variants:
```
Unauthorized(String)  → 401
Forbidden(String)     → 403
NotFound(String)      → 404
BadRequest(String)    → 400
Conflict(String)      → 409
Db(#[from] sqlx::Error) → 500
Internal              → 500
```
All handlers return `Result<HttpResponse, AppError>`. `AppError` implements `actix_web::ResponseError`.

Error body: `{ "error": "..." }`.

---

## DB Driver & Query Style

**sqlx 0.7** with `PgPool`. **No compile-time macros** (`query!`, `query_as!`) — the project uses **runtime-checked** `sqlx::query(...)` and `sqlx::query_as::<_, T>(...)`. All SQL is inline strings in handlers.rs. No `.sql` files.

---

## Transactions

```rust
let mut tx = pool.begin().await?;
sqlx::query("...").execute(&mut *tx).await?;
tx.commit().await?;
```

Passed by `&mut *tx` (deref the `Transaction`). No explicit propagation — transactions stay inside a single handler.

---

## Money Type

`i32` for `base_price`, `price`, `unit_price` etc. at the DB level.
`i64` for aggregated sums in reporting.
`rust_decimal::Decimal` for `cost_per_unit` (stored as `numeric(15,2)`).
Conversion: `(cost * Decimal::from(100)).round().to_i32()` for piastres.

> **For the engine:** The prompt specifies `i64` for money minor units — that is the right choice since advisory computations can overflow i32 in aggregate.

---

## Decimal Type

`rust_decimal::Decimal` is the project's decimal type. It's used for `cost_per_unit` in `org_ingredients`. The `rust_decimal::prelude::ToPrimitive` trait is imported to call `.to_f64()`, `.to_i32()`.

---

## Handler Signature

```rust
pub async fn some_handler(
    req:   HttpRequest,
    pool:  web::Data<PgPool>,
    // optionally:
    id:    web::Path<Uuid>,
    query: web::Query<SomeQuery>,
    body:  web::Json<SomeBody>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "resource", "action").await?;
    require_same_org(&claims, Some(org_id))?;
    // ...
    Ok(HttpResponse::Ok().json(result))
}
```

`extract_claims` is a free function defined in each handlers.rs file (copy-paste pattern):
```rust
fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions().get::<Claims>().cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}
```

---

## Route Registration

Each module has `routes.rs`:
```rust
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/scope")
            .wrap(JwtMiddleware)
            .route("", web::get().to(list_handler))
            // ...
    );
}
```

`main.rs` calls `.configure(module::routes::configure)` for each module.

---

## Auth Extractor

`crate::auth::jwt::Claims` struct injected by `JwtMiddleware` into request extensions. Fields:
- `sub: String` — user UUID
- `org_id: Option<String>` — None for super_admin
- `role: UserRole` — enum: SuperAdmin, OrgAdmin, BranchManager, Teller
- `branch_id: Option<String>`

Helper methods: `claims.user_id() -> Uuid`, `claims.org_id() -> Option<Uuid>`.

---

## Multi-Tenancy Enforcement

Query-level filtering by `org_id`. Pattern:
1. Extract `org_id` from query param or claims.
2. `require_same_org(&claims, Some(org_id))?` — checks caller's org matches target.
3. All queries filter by `org_id`.

`require_same_org` in `crate::auth::guards` — super_admin bypasses.

---

## Permission System

`check_permission(pool, &claims, "resource", "action")` — resolves in order:
1. super_admin → always granted
2. per-user `permissions` table override
3. `role_permissions` table default
4. deny

**Known resources** from the `permission_resource` enum in the schema:
`orgs, branches, users, categories, menu_items, addon_groups, shifts, orders, order_items, payments, permissions, addon_items, inventory, inventory_adjustments, inventory_transfers, recipes, soft_serve_batches, shift_counts`

> **Issue**: There is no `menu_advisor` resource in the enum yet. The advisor endpoints will piggyback on `menu_items` + `read`/`update` for now (matching the bundles module convention which also uses `menu_items`). To add a dedicated resource would require an ALTER TYPE migration and schema change.

---

## Migration Tool

No migration tool framework detected (no sqlx-migrate CLI, no refinery, no diesel). Migrations appear to be applied manually via SQL files (`Schemas.sql`, `migrations.sql`, `cost_history_migration.sql`). New schema additions should be written as a SQL file in the repo root.

---

## Tracing / Logging

`tracing` crate with `tracing_subscriber`. Usage in existing code: `tracing::info!(...)`, `tracing::warn!(...)`. No explicit span instrumentation found in module-level handlers (no `#[tracing::instrument]`). Keep it simple: `tracing::info!` / `tracing::warn!`.

---

## Test Conventions

No tests found in the existing codebase. The prompt requires unit tests for engine algorithms. Follow standard Rust: `#[cfg(test)] mod tests { ... }` at the bottom of `engine.rs`.

---

## Response Shape

No envelope. Responses are direct JSON:
- List: `{ data: [...], total, page, per_page, total_pages }` (pagination) or raw `[...]` (for small lists).
- Single: the struct directly.
- Success message: `{ "message": "..." }`.

---

## Size Enum

In the DB: `item_size` PG enum with values: `small, medium, large, extra_large, one_size`.  
In Rust (discovered by usage): passed as strings in most places (`size_label text` in `order_items`). The `item_sizes` table uses `label item_size`. The engine's `SizeLabel` will be a newtype over `String` to avoid coupling to the PG enum — see implementation.

---

## ItemKey Design

The engine prompt specifies `ItemKey { menu_item_id: Uuid, size_label: SizeLabel }`. In Madar, items with sizes are represented by:
- One row in `menu_items` (the base item)
- One or more rows in `item_sizes` (size variants with price overrides)

An `order_items` row has `menu_item_id uuid` and `size_label text`. So a `(menu_item_id, size_label)` pair uniquely identifies a sellable SKU.

The `variant_group_id` required by the engine (to prevent same-parent variants from co-bundling) is the `menu_item_id` itself — all sizes of "Latte" share `menu_item_id`. This is correct.

---

## Key Schema Facts for the Adapter

Sales come from: `order_items JOIN orders` where `orders.status != 'voided'`.

Cost at sale: There is no `unit_cost_at_sale` column in `order_items`. The `deductions_snapshot` JSONB records ingredient quantities deducted. Cost must be reconstructed as: `SUM(ingredient_quantity * ingredient.cost_per_unit)`. Alternatively, compute cost from current recipe as a proxy (same approach as `compute_item_cost` in bundles/handlers.rs).

Item price: `order_items.unit_price` is the actual price paid. For size-specific items: `item_sizes.price_override` is the size price; `menu_items.base_price` is the default.

Baskets: group `order_items` by `order_id` to reconstruct baskets.

---

## Files to Create

```
src/menu_advisor/
    mod.rs
    engine.rs          ← pure; §6 algorithms
    adapter.rs         ← DB → engine types
    handlers.rs        ← HTTP layer
    routes.rs          ← route registration
menu_advisor_migration.sql   ← schema additions
```
