//! Where else to find the shop — and what a card is willing to print.
//!
//! Stored as a map so the list of platforms is a list in code rather than a
//! column each: whatever replaces TikTok arrives as one line here instead of a
//! migration. But the vocabulary is CLOSED. These links are rendered on a
//! customer's wallet pass, and a card that prints whatever an org admin typed
//! is a card that can be made to say anything — including something that looks
//! like it came from us.
//!
//! So two rules, both enforced on the way in: the key has to be a platform we
//! know, and the value has to be an `https` URL. Not "looks like a URL" —
//! `javascript:` is a URL, and Apple's back fields render markup.

use std::collections::BTreeMap;

use crate::errors::AppError;

/// The platforms a shop may list, and the order a card lists them in.
///
/// Ordered deliberately: this is the order they appear on the pass, so it is a
/// design decision rather than whatever a hash map felt like. Website last —
/// it is the least likely to be the one a customer taps.
pub const PLATFORMS: &[(&str, &str)] = &[
    ("instagram", "Instagram"),
    ("facebook", "Facebook"),
    ("tiktok", "TikTok"),
    ("x", "X"),
    ("youtube", "YouTube"),
    ("whatsapp", "WhatsApp"),
    ("website", "Website"),
];

/// One link, ready to render.
#[derive(Debug, Clone)]
pub struct SocialLink {
    pub key: &'static str,
    /// What a human calls it.
    pub label: &'static str,
    pub url: String,
}

/// Read the stored map into the order a card prints, dropping anything that
/// does not belong.
///
/// Lenient on read and strict on write, which is the right way round: a value
/// that predates a rule, or a platform we have since removed, must not stop a
/// customer's card from being built.
pub fn links_of(value: &serde_json::Value) -> Vec<SocialLink> {
    let Some(map) = value.as_object() else {
        return Vec::new();
    };
    PLATFORMS
        .iter()
        .filter_map(|(key, label)| {
            let url = map.get(*key)?.as_str()?.trim();
            is_safe(url).then(|| SocialLink {
                key,
                label,
                url: url.to_string(),
            })
        })
        .collect()
}

/// `https` and nothing else.
///
/// `http` is excluded as well as the obvious dangers: these URLs are printed
/// into a pass that lives on a phone for years, and a plaintext link we baked
/// in cannot be upgraded later.
fn is_safe(url: &str) -> bool {
    url.starts_with("https://")
        && url.len() > "https://".len()
        && !url.contains(char::is_whitespace)
}

/// Check what a shop is trying to save.
pub fn validate(value: &serde_json::Value) -> Result<(), AppError> {
    let Some(map) = value.as_object() else {
        return Err(AppError::BadRequest(
            "Social links should be a set of named links".into(),
        ));
    };
    for (key, v) in map {
        if !PLATFORMS.iter().any(|(k, _)| k == key) {
            return Err(AppError::BadRequest(format!(
                "\"{key}\" is not somewhere we can link to"
            )));
        }
        // An empty value is how a link is removed, and is not an error.
        let Some(url) = v.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        if !is_safe(url) {
            return Err(AppError::BadRequest(format!(
                "The {key} link has to be a full https:// address"
            )));
        }
    }
    Ok(())
}

/// Drop the empties, so removing a link removes it rather than storing a blank.
pub fn clean(value: &serde_json::Value) -> serde_json::Value {
    let mut out = BTreeMap::new();
    if let Some(map) = value.as_object() {
        for (k, v) in map {
            if let Some(url) = v.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                out.insert(k.clone(), serde_json::Value::String(url.to_string()));
            }
        }
    }
    serde_json::json!(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_platforms_we_know_and_only_https() {
        assert!(validate(&json!({"instagram": "https://instagram.com/rue"})).is_ok());
        assert!(
            validate(&json!({"myspace": "https://example.com"})).is_err(),
            "an unknown key would print unchecked on a pass"
        );
        assert!(
            validate(&json!({"instagram": "javascript:alert(1)"})).is_err(),
            "Apple's back fields render markup"
        );
        assert!(
            validate(&json!({"instagram": "http://instagram.com/rue"})).is_err(),
            "a pass outlives the decision to allow plaintext"
        );
        assert!(
            validate(&json!({"instagram": ""})).is_ok(),
            "empty is how a link is removed"
        );
    }

    #[test]
    fn reading_is_lenient_because_a_card_must_still_build() {
        let stored = json!({
            "instagram": "https://instagram.com/rue",
            "myspace": "https://example.com",
            "facebook": "not a url",
            "tiktok": ""
        });
        let links = links_of(&stored);
        assert_eq!(links.len(), 1, "one good link survives, nothing throws");
        assert_eq!(links[0].key, "instagram");
    }

    #[test]
    fn the_order_is_the_cards_order_not_the_maps() {
        let stored = json!({
            "website": "https://rue.example",
            "instagram": "https://instagram.com/rue"
        });
        let links = links_of(&stored);
        assert_eq!(links[0].key, "instagram", "website is listed last");
        assert_eq!(links[1].key, "website");
    }

    #[test]
    fn saving_a_blank_removes_the_link() {
        let cleaned = clean(&json!({"instagram": "https://x.example", "facebook": "  "}));
        assert!(cleaned.get("facebook").is_none());
        assert!(cleaned.get("instagram").is_some());
    }
}
