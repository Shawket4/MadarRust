use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize)]
struct GoogleTranslateRequest<'a> {
    q: Vec<&'a str>,
    target: &'a str,
    source: &'a str,
    format: &'a str,
}

#[derive(Deserialize, Debug)]
struct GoogleTranslateResponse {
    data: Option<GoogleTranslateData>,
    error: Option<serde_json::Value>,
}

#[derive(Deserialize, Debug)]
struct GoogleTranslateData {
    translations: Vec<GoogleTranslation>,
}

#[derive(Deserialize, Debug)]
struct GoogleTranslation {
    #[serde(rename = "translatedText")]
    translated_text: String,
}

/// True when `MADAR_DISABLE_AUTO_TRANSLATION` is set to a truthy value — the
/// offline switch for auto-translation (fuzzing / CI / air-gapped environments).
pub fn auto_translation_disabled() -> bool {
    std::env::var("MADAR_DISABLE_AUTO_TRANSLATION")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// Ensures all supported languages exist in `translations`.
/// If `translations` is missing a language, it will automatically translate it
/// from English (or the first available language) using Google Translate API.
///
/// Tries the paid API first (if `GOOGLE_TRANSLATE_API_KEY` is set),
/// then falls back to the free `translate.googleapis.com` endpoint.
pub async fn ensure_translations(translations: &mut HashMap<String, String>) -> Result<(), String> {
    // Offline switch: when set, skip ALL outbound Google Translate calls and
    // leave translations as provided. Auto-translation is a convenience, not a
    // correctness requirement (entities are creatable without it). Used by the
    // fuzz harness / CI so fuzzing never makes uncontrolled outbound requests.
    if auto_translation_disabled() {
        return Ok(());
    }

    let api_key = std::env::var("GOOGLE_TRANSLATE_API_KEY").unwrap_or_default();
    let supported_str = std::env::var("SUPPORTED_LANGUAGES").unwrap_or_else(|_| "en,ar".into());
    let supported_langs: Vec<&str> = supported_str.split(',').map(|s| s.trim()).collect();

    let mut missing_langs = Vec::new();
    for lang in &supported_langs {
        if !translations.contains_key(*lang) || translations[*lang].trim().is_empty() {
            missing_langs.push(*lang);
        }
    }

    if missing_langs.is_empty() {
        return Ok(());
    }

    let source_lang = if translations.contains_key("en") && !translations["en"].trim().is_empty() {
        "en".to_string()
    } else {
        translations
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| "en".to_string())
    };

    let source_text = match translations.get(&source_lang) {
        Some(t) if !t.trim().is_empty() => t.clone(),
        // No usable source text → nothing to translate. Leave the translations as
        // provided (an empty/partial map is a valid state) rather than failing the
        // whole request. Callers previously mapped this Err to a 500 on empty input
        // (found via API fuzzing on POST/PUT /payment-methods with empty fields).
        _ => return Ok(()),
    };

    let client = Client::new();

    for target_lang in missing_langs {
        if target_lang == &source_lang {
            continue;
        }

        let mut success = false;

        // Attempt Paid API if key exists
        if !api_key.is_empty() && api_key != "your_api_key_here" {
            let url = format!(
                "https://translation.googleapis.com/language/translate/v2?key={}",
                api_key
            );
            let req_body = GoogleTranslateRequest {
                q: vec![&source_text],
                target: target_lang,
                source: &source_lang,
                format: "text",
            };

            match client.post(&url).json(&req_body).send().await {
                Ok(resp) => {
                    if let Ok(body) = resp.json::<GoogleTranslateResponse>().await {
                        if let Some(data) = body.data {
                            if let Some(t) = data.translations.first() {
                                translations
                                    .insert(target_lang.to_string(), t.translated_text.clone());
                                success = true;
                            }
                        } else {
                            tracing::error!("Google Translate Paid Error: {:?}", body.error);
                        }
                    } else {
                        tracing::error!("Google Translate Paid Failed to parse JSON response");
                    }
                }
                Err(e) => tracing::error!("Google Translate Paid Request Error: {}", e),
            }
        }

        // Attempt Free API fallback if paid API failed or was not configured
        if !success {
            let encoded_text = urlencoding::encode(&source_text);
            let url = format!(
                "https://translate.googleapis.com/translate_a/single?client=gtx&sl={}&tl={}&dt=t&q={}",
                source_lang, target_lang, encoded_text
            );

            match client.get(&url).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        if let Some(text) = body
                            .get(0)
                            .and_then(|v| v.get(0))
                            .and_then(|v| v.get(0))
                            .and_then(|v| v.as_str())
                        {
                            translations.insert(target_lang.to_string(), text.to_string());
                            success = true;
                        } else {
                            tracing::error!(
                                "Google Translate Free API Unexpected JSON structure: {}",
                                body
                            );
                        }
                    } else {
                        tracing::error!(
                            "Google Translate Free API Failed to parse JSON, status: {}",
                            status
                        );
                    }
                }
                Err(e) => tracing::error!("Google Translate Free API Request Error: {}", e),
            }
        }

        if !success {
            tracing::warn!(
                "All translation attempts failed for {}, falling back to source text",
                target_lang
            );
            translations.insert(target_lang.to_string(), source_text.clone());
        }
    }

    Ok(())
}

/// Convenience wrapper for modules that store translations as `serde_json::Value`
/// (e.g. bundles). Converts the JSON value to a `HashMap`, runs `ensure_translations`,
/// and converts back.
///
/// If the input value is `null` or `{}`, it creates an empty map. When `source_name`
/// is provided it is treated as the authoritative English ("en") value and always
/// written into the map, keeping the plain `name` column and `name_translations["en"]`
/// in sync on renames.
pub async fn ensure_translations_json(
    value: &mut serde_json::Value,
    source_name: Option<&str>,
) -> Result<(), String> {
    let mut map: HashMap<String, String> = match value.as_object() {
        Some(obj) => obj
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect(),
        None => HashMap::new(),
    };

    // The base `name` column is the source of truth for the English ("en")
    // display name, so always mirror it into the translations map. Previously
    // this only seeded "en" when it was absent, so a rename updated the plain
    // `name` column but left `name_translations["en"]` stale — and bilingual
    // clients (e.g. the POS) that resolve the display name from the translations
    // map first kept showing the old name.
    if let Some(name) = source_name {
        map.insert("en".to_string(), name.to_string());
    }

    ensure_translations(&mut map).await?;

    *value = serde_json::to_value(&map).unwrap_or_default();
    Ok(())
}

/// Overlay the non-empty string entries of `src` onto `dst` (both are
/// `*_translations` JSON objects). Used on update paths so a client-supplied
/// translation (e.g. `{"ar": "..."}`) sent alongside a rename is applied on top
/// of the existing map instead of being dropped or replacing it wholesale.
/// A non-object `src` replaces `dst` outright; non-string / empty entries are ignored.
pub fn merge_translations_json(dst: &mut serde_json::Value, src: &serde_json::Value) {
    let Some(src_obj) = src.as_object() else {
        *dst = src.clone();
        return;
    };
    if !dst.is_object() {
        *dst = serde_json::json!({});
    }
    let dst_obj = dst.as_object_mut().expect("dst is an object");
    for (key, val) in src_obj {
        if let Some(text) = val.as_str() {
            if !text.trim().is_empty() {
                dst_obj.insert(key.clone(), serde_json::Value::String(text.to_string()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Regression: a rename must OVERWRITE the existing English translation, not
    // just seed it when absent. Otherwise the plain `name` column moves forward
    // while `name_translations["en"]` stays stale, and clients that resolve the
    // display name from the translations map first (e.g. the POS) keep showing
    // the old name. The map already carries en+ar, so `ensure_translations` finds
    // nothing missing and makes no network call — deterministic.
    #[tokio::test]
    async fn ensure_translations_json_overwrites_existing_en_on_rename() {
        let mut value = json!({ "en": "Old name", "ar": "الاسم القديم" });
        ensure_translations_json(&mut value, Some("New name"))
            .await
            .unwrap();
        assert_eq!(value["en"], "New name");
        assert_eq!(value["ar"], "الاسم القديم"); // untouched language preserved
    }

    #[tokio::test]
    async fn ensure_translations_json_seeds_en_when_absent() {
        let mut value = json!({ "ar": "قهوة" });
        ensure_translations_json(&mut value, Some("Coffee"))
            .await
            .unwrap();
        assert_eq!(value["en"], "Coffee");
        assert_eq!(value["ar"], "قهوة");
    }

    #[test]
    fn merge_translations_json_overlays_without_dropping_existing() {
        let mut dst = json!({ "en": "Latte", "ar": "لاتيه" });
        // Client sends only a new Arabic name alongside a rename.
        merge_translations_json(&mut dst, &json!({ "ar": "لاتيه كبير" }));
        assert_eq!(dst["en"], "Latte"); // untouched language preserved
        assert_eq!(dst["ar"], "لاتيه كبير"); // overlaid
    }

    #[test]
    fn merge_translations_json_ignores_empty_and_non_string() {
        let mut dst = json!({ "en": "Tea", "ar": "شاي" });
        merge_translations_json(&mut dst, &json!({ "ar": "  ", "en": 42 }));
        assert_eq!(dst["en"], "Tea");
        assert_eq!(dst["ar"], "شاي"); // blank string skipped, not cleared
    }

    #[test]
    fn merge_translations_json_replaces_when_src_not_object() {
        let mut dst = json!({ "en": "X" });
        merge_translations_json(&mut dst, &json!("not-an-object"));
        assert_eq!(dst, json!("not-an-object"));
    }
}
