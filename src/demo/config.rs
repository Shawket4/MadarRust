//! Demo playground configuration, read once from the environment at startup.

use std::sync::OnceLock;

static DEMO_MODE: OnceLock<bool> = OnceLock::new();

/// Process-global `DEMO_MODE`, read once and cached. Lets deep, hard-to-thread
/// code paths (WhatsApp/OTP sends, file uploads) hard-skip external or
/// abuse-prone effects without plumbing config through every call site.
pub fn demo_mode() -> bool {
    *DEMO_MODE.get_or_init(|| {
        std::env::var("DEMO_MODE")
            .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false)
    })
}

/// Runtime knobs for the public demo. Registered as `web::Data` so handlers
/// can read the TTL/cap, and consulted in `main.rs` to decide whether to
/// register the `/demo` scope and spawn the sweeper.
#[derive(Clone, Debug)]
pub struct DemoConfig {
    /// Master switch (`DEMO_MODE`). When false, the `/demo` scope is never
    /// registered and the handler hard-refuses.
    pub enabled: bool,
    /// How long a demo org lives before the sweeper deletes it (`DEMO_TTL_HOURS`).
    pub ttl_hours: i64,
    /// Hard ceiling on concurrent live demo orgs (`DEMO_MAX_ORGS`) — abuse/cost guard.
    pub max_orgs: i64,
    /// Sweeper poll interval in seconds (`DEMO_SWEEP_SECS`).
    pub sweep_secs: u64,
}

fn env_flag(key: &str) -> Option<bool> {
    std::env::var(key)
        .ok()
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}

impl DemoConfig {
    pub fn from_env() -> Self {
        Self {
            enabled: env_flag("DEMO_MODE").unwrap_or(false),
            ttl_hours: env_parse("DEMO_TTL_HOURS").unwrap_or(24),
            max_orgs: env_parse("DEMO_MAX_ORGS").unwrap_or(200),
            sweep_secs: env_parse("DEMO_SWEEP_SECS").unwrap_or(300),
        }
    }
}
