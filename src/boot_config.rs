//! Required configuration, checked ONCE at boot before anything touches the
//! database. A missing secret used to surface only after `sqlx::migrate!` had
//! already run (the asset worker's `require_secret` panicked on spawn), so a
//! bad deploy migrated production and then crash-looped. Now the process
//! refuses to start, with every problem listed, before it connects.

/// Every configuration problem found, empty when the server may start.
///
/// `lookup` reads one variable (the real server passes `std::env::var`);
/// `release` is whether this is an optimised production build, which is when
/// the development fallbacks are refused.
pub fn problems(lookup: impl Fn(&str) -> Option<String>, release: bool) -> Vec<String> {
    let set = |k: &str| lookup(k).filter(|v| !v.trim().is_empty());
    let mut out = Vec::new();
    for key in ["DATABASE_URL", "JWT_SECRET"] {
        if set(key).is_none() {
            out.push(format!("{key} must be set"));
        }
    }
    // Signed org-scoped asset URLs (src/assets). Debug builds fall back to a
    // key derived from JWT_SECRET; a release build without a real one would
    // mint URLs anyone who has read the source can forge.
    if release && set("ASSET_URL_SECRET").is_none_or(|s| s.len() < 32) {
        out.push(
            "ASSET_URL_SECRET must be set to at least 32 bytes (generate with: openssl rand -hex 32)".into(),
        );
    }
    // TLS: once either file is named, both must be readable — never a silent
    // fall back to plain HTTP.
    if let (Some(cert), Some(key)) = (set("SSL_CERT_FILE"), set("SSL_KEY_FILE")) {
        for (name, path) in [("SSL_CERT_FILE", cert), ("SSL_KEY_FILE", key)] {
            if let Err(e) = std::fs::metadata(&path) {
                out.push(format!("{name} is set but unreadable ({path}): {e}"));
            }
        }
    }
    out
}

/// Check the process environment; on any problem log each one and exit(1).
pub fn validate_or_exit() {
    let found = problems(|k| std::env::var(k).ok(), !cfg!(debug_assertions));
    if found.is_empty() {
        return;
    }
    for p in &found {
        tracing::error!("configuration: {p}");
        eprintln!("madar-rust: configuration error: {p}");
    }
    eprintln!(
        "madar-rust: refusing to start (nothing was migrated); fix the environment and restart"
    );
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::problems;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn boot_config_release_requires_asset_url_secret() {
        let base = [("DATABASE_URL", "postgres://x"), ("JWT_SECRET", "j")];
        let p = problems(env(&base), true);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].contains("ASSET_URL_SECRET"));
        let short = [base[0], base[1], ("ASSET_URL_SECRET", "too-short")];
        assert!(problems(env(&short), true)[0].contains("ASSET_URL_SECRET"));
        let good = [
            base[0],
            base[1],
            ("ASSET_URL_SECRET", "0123456789abcdef0123456789abcdef"),
        ];
        assert!(problems(env(&good), true).is_empty());
        assert!(
            problems(env(&base), false).is_empty(),
            "debug builds use the dev key"
        );
    }

    #[test]
    fn boot_config_lists_every_missing_required_var() {
        let p = problems(env(&[]), true);
        assert_eq!(p.len(), 3, "{p:?}");
        let tls = [
            ("DATABASE_URL", "postgres://x"),
            ("JWT_SECRET", "j"),
            ("SSL_CERT_FILE", "/nonexistent/cert.pem"),
            ("SSL_KEY_FILE", "/nonexistent/key.pem"),
        ];
        assert_eq!(problems(env(&tls), false).len(), 2);
    }
}
