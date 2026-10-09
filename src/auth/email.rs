//! A sign-in email is one address however it was typed. Phones capitalise the
//! first letter and keyboards leave a trailing space, so " Ahmed@Cafe.com "
//! and "ahmed@cafe.com" must find the same account. Every lookup and every
//! write goes through `normalize`; the `users_normalize_email` trigger applies
//! the same rule to any write that does not (seeds, the demo, raw SQL).

/// Trimmed and lowercased: the form stored in `users.email` and looked up at
/// sign-in. Keep in step with the trigger in
/// `migrations/20261008090000_users_email_normalized.sql`.
pub fn normalize(email: &str) -> String {
    email.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::normalize;

    #[test]
    fn case_and_surrounding_space_do_not_matter() {
        assert_eq!(normalize("  Ahmed@Cafe.COM \t"), "ahmed@cafe.com");
        assert_eq!(normalize("ahmed@cafe.com"), "ahmed@cafe.com");
    }
}
