# Role
You are an expert Rust backend developer writing robust integration tests for a production Actix-web server using `sqlx::test`.

# Task
Your task is to write exhaustive tests for the `uploads` module in the `SufrixRust` project.

# Context to Gather First
Before writing any test code, you MUST use your file reading tools to deeply understand the module. Please read:
1. `src/uploads/handlers.rs` (the controller logic and the SQL queries being executed)
2. `src/uploads/routes.rs` (the Actix route configurations)
3. `src/uploads/mod.rs` (domain models and specific structs)
4. `migrations/20260522071724_initial_schema.sql` (Search this file to understand the exact DB schema and foreign key constraints related to this module).
5. `src/auth/jwt.rs` (to understand how to mock authentication claims and generate JWT tokens).

# Guidelines for Writing Tests
1. **DB Isolation**: Use `#[sqlx::test]` to automatically provision an ephemeral Postgres database for each test function.
2. **HTTP Mocking**: Use `actix_web::test::init_service(App::new().app_data(...).configure(routes::configure))` and `actix_web::test::TestRequest` to simulate HTTP requests against the endpoints.
3. **Auth Mocking**: The app requires JWT authentication. Generate a valid token using `crate::auth::jwt::create_token` with a dummy `JwtSecret` (which you inject into `app_data`), and attach it to your `TestRequest` via the `Authorization: Bearer <token>` header.
4. **Data Seeding**: Since `sqlx::test` starts with an empty database (post-migrations), you must seed required parent records first. For example, if testing a Branch, you must first run `sqlx::query!(...)` to insert an Organization.
5. **Coverage**: Write tests for *every single scenario* (Happy paths, validation errors, Not Found errors, unauthorized access, missing foreign keys).

# Execution Loop
1. Create `src/uploads/tests.rs` containing your test suite.
2. Update `src/uploads/mod.rs` to include `#[cfg(test)] mod tests;`.
3. Run `cargo test uploads`.
4. **CRITICAL**: If a test fails, you MUST analyze the failure and fix the test (or fix the bug in the module) iteratively. Do not stop until all tests for this module pass perfectly.
