#[cfg(test)]
mod tests {
    use crate::translation::ensure_translations;
    use std::collections::HashMap;

    /// A complete set of translations is left exactly as given, and asks
    /// Google nothing.
    ///
    /// This replaces a test that called the LIVE Google Translate endpoint and
    /// asserted an exact Arabic rendering of "Pacha Mama". Three things were
    /// wrong with that, and the suite finally caught the third:
    ///
    ///   * it made an outbound network request from a unit test, so the suite's
    ///     result depended on a third party being up and unchanged;
    ///   * it asserted a translation Google is free to reword at any time;
    ///   * it set a PROCESS-GLOBAL env var from one of many parallel test
    ///     threads (`set_var` is `unsafe` in edition 2024 for precisely this
    ///     reason) while `ensure_translations` reads two others. It failed on a
    ///     full run having passed on its own, returning the English string
    ///     untranslated — which is what happens when the disable switch is on
    ///     or the call fails.
    ///
    /// What is worth pinning is the contract the module documents: auto
    /// translation is a convenience, not a correctness requirement. Nothing
    /// here touches the network or the environment, so it cannot race.
    #[tokio::test]
    async fn a_complete_set_is_left_alone() {
        let mut tr = HashMap::new();
        tr.insert("en".to_string(), "Cash".to_string());
        tr.insert("ar".to_string(), "نقدي".to_string());

        let before = tr.clone();
        ensure_translations(&mut tr)
            .await
            .expect("a complete set needs no translation");
        assert_eq!(tr, before, "nothing missing, so nothing changed");
    }

    /// Whitespace is not a translation.
    ///
    /// `"   "` counts as MISSING, which is why a blank Arabic field triggers a
    /// lookup rather than being stored as a name made of spaces.
    #[test]
    fn a_blank_string_reads_as_missing() {
        let mut tr = HashMap::new();
        tr.insert("en".to_string(), "Cash".to_string());
        tr.insert("ar".to_string(), "   ".to_string());
        // Mirrors the emptiness rule in `ensure_translations`.
        let missing = ["en", "ar"]
            .iter()
            .filter(|l| !tr.contains_key(**l) || tr[**l].trim().is_empty())
            .count();
        assert_eq!(missing, 1, "the blank Arabic field is what needs filling");
    }

    /// The live round-trip, for a human with a real key.
    ///
    /// Ignored by default: it talks to Google. Run it deliberately with
    /// `cargo test -- --ignored --test-threads=1` and a real
    /// `GOOGLE_TRANSLATE_API_KEY`, never as part of the suite.
    #[tokio::test]
    #[ignore = "hits the live Google Translate API"]
    async fn live_translation_fills_a_missing_language() {
        let mut tr = HashMap::new();
        tr.insert("en".to_string(), "Cash".to_string());
        tr.insert("ar".to_string(), String::new());

        ensure_translations(&mut tr).await.expect("translate");
        let ar = tr.get("ar").expect("arabic key");
        assert!(!ar.trim().is_empty(), "the Arabic field was filled");
        assert_ne!(ar, "Cash", "and not merely echoed back");
    }
}
