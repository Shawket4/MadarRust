//! The boot guard for `-- no-transaction` index migrations.
//!
//! `CREATE INDEX CONCURRENTLY` cannot run in a transaction, so each such index
//! is its own `-- no-transaction` migration (sqlx 0.8 runs those outside a
//! transaction and records them only after they succeed). A build that fails
//! half way — a lock timeout, a duplicate key, the container killed — leaves an
//! INVALID index behind, and the retry's `CREATE INDEX CONCURRENTLY IF NOT
//! EXISTS` would then skip it and record the migration as done, with an index
//! that is never used and never enforced.
//!
//! Before the migrator runs, [`drop_invalid_pending_indexes`] finds every index
//! named by a `-- no-transaction` migration that is NOT yet applied, and drops
//! it when Postgres marks it invalid (`pg_index.indisvalid = false`). It never
//! touches an index of an applied migration, nor a valid one, nor any index
//! named by no migration. Runs on every boot; with nothing pending it is one
//! catalog query.

use sqlx::PgPool;
use sqlx::migrate::Migrator;

/// The index a `-- no-transaction` migration creates CONCURRENTLY, if any.
pub fn concurrent_index_name(sql: &str) -> Option<String> {
    if !sql.starts_with("-- no-transaction") {
        return None;
    }
    let upper = sql.to_ascii_uppercase();
    let at = upper.find("CONCURRENTLY")?;
    let rest = &sql[at + "CONCURRENTLY".len()..];
    let mut words = rest.split_whitespace();
    let mut name = words.next()?;
    if name.eq_ignore_ascii_case("IF") {
        // CREATE … IF NOT EXISTS <name>  |  DROP … IF EXISTS <name>
        if words.next()?.eq_ignore_ascii_case("NOT") {
            words.next()?;
        }
        name = words.next()?;
    }
    let name = name.trim_end_matches(';').trim_matches('"');
    (!name.is_empty() && !name.eq_ignore_ascii_case("ON")).then(|| name.to_string())
}

/// Drop the INVALID leftovers of failed CONCURRENTLY builds of not-yet-applied
/// migrations. Returns the names dropped.
pub async fn drop_invalid_pending_indexes(
    pool: &PgPool,
    migrator: &Migrator,
) -> Result<Vec<String>, sqlx::Error> {
    let applied: Vec<i64> = match sqlx::query_scalar::<_, i64>(
        "SELECT version FROM _sqlx_migrations WHERE success",
    )
    .fetch_all(pool)
    .await
    {
        Ok(v) => v,
        // A brand-new database: nothing applied, nothing can be left over.
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("42P01") => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let pending: Vec<String> = migrator
        .iter()
        .filter(|m| m.no_tx && !applied.contains(&m.version))
        .filter_map(|m| concurrent_index_name(&m.sql))
        .collect();
    if pending.is_empty() {
        return Ok(vec![]);
    }
    let invalid: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_index i \
           JOIN pg_class c ON c.oid = i.indexrelid \
          WHERE c.relnamespace = 'public'::regnamespace \
            AND NOT i.indisvalid AND c.relname = ANY($1)",
    )
    .bind(&pending)
    .fetch_all(pool)
    .await?;
    for name in &invalid {
        tracing::warn!(index = %name, "dropping the INVALID index a failed CONCURRENTLY build left; its migration will build it again");
        // The name comes from the catalog (it matched one of our migrations).
        sqlx::raw_sql(&format!(
            "DROP INDEX CONCURRENTLY IF EXISTS public.\"{}\"",
            name.replace('"', "\"\"")
        ))
        .execute(pool)
        .await?;
    }
    Ok(invalid)
}
