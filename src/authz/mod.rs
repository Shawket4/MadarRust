//! Architecture E permissions on the server (PERMISSIONS_ARCHITECTURE.md).
//!
//! The decision logic is the shared `madar_authz` crate (the POS core links a
//! byte-identical copy). This module loads grants from Postgres and adapts the
//! crate to actix handlers.

pub mod api;
pub mod load;
pub mod require;
pub mod scope;
pub mod shadow;

#[cfg(test)]
mod api_tests;
#[cfg(test)]
mod phase2_tests;

pub use madar_authz::*;

#[cfg(test)]
mod registry_tests {
    use super::*;

    /// The Rust registry was generated from the spec as it stands. Edit the spec,
    /// then `cd authz/gen && cargo run -- --dashboard ../../../MadarDashboard --pos ../../../madar`.
    #[test]
    fn generated_registry_matches_the_spec() {
        let raw = include_bytes!("../../authz/spec/capabilities.toml");
        let mut h: u64 = 0xcbf29ce484222325;
        for b in raw {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x100000001b3);
        assert_eq!(
            format!("{h:016x}"),
            SPEC_HASH,
            "authz/crate/src/generated.rs is stale"
        );
    }

    /// Every cell GET /auth/permissions reports is answered by exactly one capability,
    /// so a pre-0.8 tablet reads the same grid from the new model.
    #[test]
    fn every_permission_cell_has_a_capability() {
        for (r, a) in crate::permissions::permission_cells() {
            assert!(
                Cap::from_legacy(r, a).is_some(),
                "{r}:{a} has no capability"
            );
        }
    }
}

/// Keep the `capabilities` table equal to the compiled registry. Additive: a
/// new capability is inserted with the spec's metadata; an existing id whose key
/// changed means the registry was edited unsafely, and boot stops.
pub async fn sync_catalogue(pool: &sqlx::PgPool) -> Result<(), String> {
    let existing: Vec<(i16, String)> = sqlx::query_as("SELECT id, key FROM capabilities")
        .fetch_all(pool)
        .await
        .map_err(|e| e.to_string())?;
    for (id, key) in &existing {
        match Cap::from_id(*id as u16) {
            Some(c) if c.key() == key => {}
            Some(c) => {
                return Err(format!(
                    "capability id {id} is '{key}' in the database but '{}' in the registry; ids are never reused",
                    c.key()
                ));
            }
            None => {}
        }
    }
    let letters = |k: Kinds| -> String {
        RoleKind::ALL
            .iter()
            .zip(['o', 'm', 't', 'w', 'k'])
            .filter(|(r, _)| k.contains(**r))
            .map(|(_, l)| l)
            .collect()
    };
    for m in CAPS {
        let tier = match m.tier {
            Tier::Core => "core",
            Tier::Configurable => "configurable",
            Tier::Advanced => "advanced",
            Tier::Legacy => "legacy",
        };
        sqlx::query(
            "INSERT INTO capabilities (id, key, legacy_resource, legacy_action, tier, defaults, core, approval, protected, spec_version)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (id) DO UPDATE SET tier = EXCLUDED.tier, defaults = EXCLUDED.defaults,
                 core = EXCLUDED.core, approval = EXCLUDED.approval, protected = EXCLUDED.protected,
                 spec_version = EXCLUDED.spec_version",
        )
        .bind(m.cap.id() as i16)
        .bind(m.key)
        .bind(m.legacy.map(|l| l.0))
        .bind(m.legacy.map(|l| l.1))
        .bind(tier)
        .bind(letters(m.defaults))
        .bind(letters(m.core))
        .bind(m.approval)
        .bind(m.protected)
        .bind(SPEC_VERSION as i32)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod catalogue_tests {
    #[sqlx::test]
    async fn the_database_catalogue_equals_the_registry(pool: sqlx::PgPool) {
        super::sync_catalogue(&pool).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM capabilities")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n as usize, super::CAPS.len());
        sqlx::query("UPDATE capabilities SET key = 'renamed.badly' WHERE id = 64")
            .execute(&pool)
            .await
            .unwrap();
        assert!(super::sync_catalogue(&pool).await.is_err());
    }
}
