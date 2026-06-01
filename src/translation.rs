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

/// Ensures all supported languages exist in `translations`.
/// If `translations` is missing a language, it will automatically translate it
/// from English (or the first available language) using Google Translate API.
///
/// Tries the paid API first (if `GOOGLE_TRANSLATE_API_KEY` is set),
/// then falls back to the free `translate.googleapis.com` endpoint.
pub async fn ensure_translations(
    translations: &mut HashMap<String, String>,
) -> Result<(), String> {
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
        translations.keys().next().cloned().unwrap_or_else(|| "en".to_string())
    };

    let source_text = match translations.get(&source_lang) {
        Some(t) => t.clone(),
        None => return Err("No source text provided for translation".into()),
    };

    let client = Client::new();

    for target_lang in missing_langs {
        if target_lang == &source_lang {
            continue;
        }

        let mut success = false;

        // Attempt Paid API if key exists
        if !api_key.is_empty() && api_key != "your_api_key_here" {
            let url = format!("https://translation.googleapis.com/language/translate/v2?key={}", api_key);
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
                                translations.insert(target_lang.to_string(), t.translated_text.clone());
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
                        if let Some(text) = body.get(0).and_then(|v| v.get(0)).and_then(|v| v.get(0)).and_then(|v| v.as_str()) {
                            translations.insert(target_lang.to_string(), text.to_string());
                            success = true;
                        } else {
                            tracing::error!("Google Translate Free API Unexpected JSON structure: {}", body);
                        }
                    } else {
                        tracing::error!("Google Translate Free API Failed to parse JSON, status: {}", status);
                    }
                }
                Err(e) => tracing::error!("Google Translate Free API Request Error: {}", e),
            }
        }

        if !success {
            tracing::warn!("All translation attempts failed for {}, falling back to source text", target_lang);
            translations.insert(target_lang.to_string(), source_text.clone());
        }
    }

    Ok(())
}

/// Convenience wrapper for modules that store translations as `serde_json::Value`
/// (e.g. bundles). Converts the JSON value to a `HashMap`, runs `ensure_translations`,
/// and converts back.
///
/// If the input value is `null` or `{}`, it creates an empty map. If the `source_name`
/// is provided, it is used as the English source text when the JSON object has no "en" key.
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

    // If the JSON had no "en" key but we have a source name, seed it
    if let Some(name) = source_name {
        if !map.contains_key("en") || map["en"].trim().is_empty() {
            map.insert("en".to_string(), name.to_string());
        }
    }

    ensure_translations(&mut map).await?;

    *value = serde_json::to_value(&map).unwrap_or_default();
    Ok(())
}
