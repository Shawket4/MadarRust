//! What a shop may call itself, once its name is a hostname.
//!
//! A slug used to be a URL path segment, where the only thing that could go
//! wrong was a collision. On the branding tier it becomes a SUBDOMAIN —
//! `rue.madar-pos.cloud` — and two new things can go wrong, both of them ours
//! rather than the shop's.
//!
//! The first is that a shop can take a name we are already using. `api` is the
//! obvious one and the least likely; the ones that will actually happen are
//! `order`, `menu`, `book`, `support` — words a café would pick without a
//! thought, each of which is a host we run or will want to.
//!
//! The second is subtler. A slug that changes takes every printed QR code with
//! it. The counter card on the till, the sticker on the window, the poster in
//! the window of the branch that has not been visited this month: all of them
//! encode a URL, and none of them can be recalled. So once a shop's name is
//! load-bearing, it stops being editable.

use crate::errors::AppError;

/// Names a shop may not take.
///
/// Deliberately a list here rather than a table: it is a property of OUR
/// deployment, not of any tenant's data, and a value that has to be right on
/// the first request after a fresh boot has no business being a query. Adding
/// to it is a one-line change and a deploy, which is the right friction for
/// something that can break production.
const RESERVED: &[&str] = &[
    // Live today. A shop taking one of these takes down that service.
    "api",
    "demo",
    "demo-api",
    "get",
    "legal",
    "loyalty",
    "order",
    "reservations",
    "sentry",
    "www",
    "autoconfig",
    "autodiscover",
    // Mail and the names the internet expects to find.
    "mail",
    "smtp",
    "imap",
    "pop",
    "mx",
    "ns",
    "ns1",
    "ns2",
    "ftp",
    "postmaster",
    "hostmaster",
    "webmaster",
    "abuse",
    "noreply",
    "no-reply",
    // Ours, and ours to keep.
    "madar",
    "madarpos",
    "admin",
    "app",
    "dashboard",
    "portal",
    "staff",
    "pos",
    "kds",
    "auth",
    "login",
    "sso",
    "id",
    "account",
    "accounts",
    "my",
    "billing",
    "pay",
    "payments",
    "invoice",
    "status",
    "docs",
    "help",
    "support",
    "blog",
    "security",
    "official",
    "root",
    "internal",
    "ops",
    "metrics",
    "grafana",
    "prometheus",
    "logs",
    "webhook",
    "webhooks",
    "git",
    "ci",
    "vpn",
    // Products we have not built yet. Cheap to reserve now, expensive to
    // reclaim from a shop that has printed it on a thousand receipts.
    "orders",
    "booking",
    "bookings",
    "book",
    "menu",
    "track",
    "tracking",
    "qr",
    "link",
    "links",
    "go",
    "s",
    "shop",
    "store",
    "cdn",
    "static",
    "assets",
    "img",
    "images",
    "files",
    "uploads",
    // Environments.
    "test",
    "dev",
    "stage",
    "staging",
    "prod",
    "beta",
    "preview",
];

/// Longest a hostname label may be. Not our rule — the DNS's.
const MAX_LABEL: usize = 63;

/// Check a slug on its way in.
///
/// Runs for every organisation, not only the branded ones: a shop can be put on
/// the tier at any time by a super admin, and discovering then that its name has
/// been unusable all along is a worse conversation than refusing it at creation.
pub fn validate(slug: &str) -> Result<(), AppError> {
    let s = slug.trim();
    let bad = |why: &str| Err(AppError::BadRequest(format!("That short name {why}.")));

    if s.is_empty() {
        return bad("cannot be empty");
    }
    // One and two characters are reserved wholesale. They are what short links
    // will want, and they are the first thing anyone squats.
    if s.chars().count() < 3 {
        return bad("has to be at least three characters");
    }
    if s.len() > MAX_LABEL {
        return bad("is too long for a web address");
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return bad("may only use lowercase letters, numbers and hyphens");
    }
    if s.starts_with('-') || s.ends_with('-') {
        return bad("cannot start or end with a hyphen");
    }
    // Punycode. A homograph of a real shop's name, served from our own domain,
    // is a phishing page we hosted.
    if s.starts_with("xn--") {
        return bad("cannot start with \"xn--\"");
    }
    // All digits reads as an id, and will collide with anything path-shaped we
    // add later.
    if s.chars().all(|c| c.is_ascii_digit()) {
        return bad("cannot be only numbers");
    }
    if RESERVED.contains(&s) {
        return bad("is reserved");
    }
    Ok(())
}

/// Whether a shop's slug is load-bearing and therefore frozen.
///
/// The tier is the line because it is the tier that puts the slug in a hostname
/// and on printed cards. A shop that has not been given custom branding can
/// still rename itself freely, and should be able to.
///
/// A shop with NO slug yet is not frozen either, whatever its tier. Nothing is
/// encoded in a name that does not exist: there is no hostname, no printed
/// card, nothing outside our control that names it. Freezing one was a deadlock
/// rather than a safeguard — an org from before slugs existed, carrying no slug
/// and the branding tier, could not be given one, and because the dashboard
/// requires one to submit the form, could not be edited AT ALL. Not its name,
/// not its tax rate, not its receipt footer.
///
/// `None` is the whole of "no slug" now; the column was NOT NULL and such a
/// shop carried `''`, which a CHECK constraint no longer permits. The blank
/// arm survives here anyway, because a value arriving from a request is not a
/// value that came from the column.
pub fn is_frozen(custom_branding: bool, current_slug: Option<&str>) -> bool {
    custom_branding && current_slug.is_some_and(|s| !s.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The freeze protects something that EXISTS. An org from before slugs
    /// were a thing carries an empty one, and there is a live example: branded,
    /// active, `slug = \'\'`. Freezing that made the whole organisation
    /// uneditable, because the dashboard cannot submit without a slug and the
    /// server refused every slug it sent.
    #[test]
    fn a_shop_with_no_slug_yet_can_still_be_given_one() {
        assert!(
            !is_frozen(true, None),
            "nothing is printed on a name that does not exist"
        );
        assert!(!is_frozen(true, Some("")), "nor on an empty one");
        assert!(!is_frozen(true, Some("   ")), "nor on a blank one");
        // And once it has one, it is load-bearing and stops moving.
        assert!(is_frozen(true, Some("drops")));
        // Off the tier, a shop renames itself freely either way.
        assert!(!is_frozen(false, Some("drops")));
        assert!(!is_frozen(false, None));
    }

    #[test]
    fn the_names_we_are_already_using_are_refused() {
        for taken in ["api", "loyalty", "order", "www", "legal", "demo-api"] {
            assert!(validate(taken).is_err(), "{taken} is a live host");
        }
    }

    #[test]
    fn the_shapes_that_would_bite_later_are_refused() {
        assert!(validate("s").is_err(), "one character");
        assert!(validate("ab").is_err(), "two characters");
        assert!(validate("12345").is_err(), "all numbers reads as an id");
        assert!(validate("xn--80ak6aa92e").is_err(), "punycode homograph");
        assert!(validate("-rue").is_err());
        assert!(validate("rue-").is_err());
        assert!(validate("Rue").is_err(), "hostnames are lowercase");
        assert!(validate("rue coffee").is_err());
        assert!(validate("rue_coffee").is_err(), "underscore is not a label");
    }

    #[test]
    fn an_ordinary_shop_name_is_fine() {
        for ok in ["rue", "rue-coffee", "cafe27", "the-daily-grind"] {
            assert!(validate(ok).is_ok(), "{ok} should be allowed");
        }
    }
}
