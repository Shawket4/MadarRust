//! Offline-PIN verifier (argon2id) for the offline-auth bundle (POS rebuild,
//! Layer 3). Distinct from the bcrypt login `pin_hash`: this is the ONLY thing
//! shipped to devices (via `GET /orgs/{id}/offline-auth-bundle`), so it is
//! memory-hard and a leak is never the login credential. The device verifies a
//! typed PIN against it OFFLINE; the server only ever DERIVES it.
use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHasher};

/// Derive an argon2id PHC string for a teller's offline PIN.
pub fn hash_offline_pin(pin: &str) -> Result<String, argon2::password_hash::Error> {
    // rand_core 0.6.4 gates OsRng behind a feature the lock no longer
    // enables — mint the 16 random salt bytes from the OS RNG via a v4
    // uuid instead (uuid pulls getrandom itself; no new dependency).
    let salt =
        SaltString::encode_b64(uuid::Uuid::new_v4().as_bytes()).expect("16 bytes fit a b64 salt");
    Ok(Argon2::default()
        .hash_password(pin.as_bytes(), &salt)?
        .to_string())
}

/// Verify a typed PIN against a stored argon2id PHC string. The shipping
/// verification runs in the rust-core on the device; both run madar-shared's
/// `madar_authz::pin::verify_offline_pin`.
pub use madar_authz::pin::verify_offline_pin;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_then_verify_roundtrip() {
        let phc = hash_offline_pin("1234").unwrap();
        assert!(
            phc.starts_with("$argon2id$"),
            "should be argon2id PHC, got {phc}"
        );
        assert!(verify_offline_pin("1234", &phc));
        assert!(!verify_offline_pin("9999", &phc));
    }

    /// The one PHC string both sides verify: madar-shared's `TEST_PHC` is what
    /// THIS hasher derives under its fixed salt.
    #[test]
    fn the_shared_phc_string_is_this_hashers_output() {
        use madar_authz::pin::{TEST_PHC, TEST_PIN};
        let salt = SaltString::encode_b64(b"madar-shared-pin").unwrap();
        let phc = Argon2::default()
            .hash_password(TEST_PIN.as_bytes(), &salt)
            .unwrap()
            .to_string();
        assert_eq!(phc, TEST_PHC);
        assert!(verify_offline_pin(TEST_PIN, TEST_PHC));
    }

    #[test]
    fn distinct_salts_per_hash() {
        assert_ne!(
            hash_offline_pin("1234").unwrap(),
            hash_offline_pin("1234").unwrap()
        );
    }
}
