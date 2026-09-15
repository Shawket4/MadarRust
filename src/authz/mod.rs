//! Architecture E permissions on the server (PERMISSIONS_ARCHITECTURE.md).
//!
//! The decision logic is the shared `madar_authz` crate (the POS core links a
//! byte-identical copy). This module loads grants from Postgres and adapts the
//! crate to actix handlers.

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
