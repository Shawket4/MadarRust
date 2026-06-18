//! Mint a long-lived JWT for the local API-fuzz harness (scripts/api-fuzz.sh).
//!
//! NOT for production. It signs a token with whatever `JWT_SECRET` is in the
//! environment for one of the fixed seed users created by scripts/seed_fuzz.sql,
//! so Schemathesis can authenticate against a throwaway `sufrix_fuzz` database.
//!
//! Usage:
//!   JWT_SECRET=... cargo run --bin fuzz-token -- super-admin   # org_id=NULL, needs X-Org-Id
//!   JWT_SECRET=... cargo run --bin fuzz-token -- org-admin     # org-scoped

use sufrix_rust::auth::jwt::{create_token, JwtSecret};
use sufrix_rust::models::UserRole;
use uuid::Uuid;

// Must match the fixed UUIDs in scripts/seed_fuzz.sql.
const ORG_ID: &str = "00000000-0000-0000-0000-000000000001";
const SUPER_ADMIN_ID: &str = "00000000-0000-0000-0000-000000000003";
const ORG_ADMIN_ID: &str = "00000000-0000-0000-0000-000000000004";

fn main() {
    let secret = std::env::var("JWT_SECRET").expect("JWT_SECRET must be set");
    let role_arg = std::env::args().nth(1).unwrap_or_else(|| "org-admin".to_string());

    let org_id = Uuid::parse_str(ORG_ID).unwrap();
    let (user_id, org, role) = match role_arg.as_str() {
        "super-admin" => (Uuid::parse_str(SUPER_ADMIN_ID).unwrap(), None, UserRole::SuperAdmin),
        "org-admin" => (Uuid::parse_str(ORG_ADMIN_ID).unwrap(), Some(org_id), UserRole::OrgAdmin),
        other => {
            eprintln!("unknown role '{other}' (expected: super-admin | org-admin)");
            std::process::exit(2);
        }
    };

    let token = create_token(&JwtSecret(secret), user_id, org, role, None, 48)
        .expect("failed to mint token");
    println!("{token}");
}
