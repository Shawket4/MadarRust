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

/// Hybrid approach: ensures all supported languages exist in `translations`.
/// If `translations` is missing a language, it will automatically translate it
/// from English (or the first available language) using Google Translate API.
pub async fn ensure_translations(
    translations: &mut HashMap<String, String>,
) -> Result<(), String> {
    // Read config
    let api_key = std::env::var("GOOGLE_TRANSLATE_API_KEY").unwrap_or_default();
    let supported_str = std::env::var("SUPPORTED_LANGUAGES").unwrap_or_else(|_| "en,ar".into());
    let supported_langs: Vec<&str> = supported_str.split(',').map(|s| s.trim()).collect();

    // Do we have all supported languages?
    let mut missing_langs = Vec::new();
    for lang in &supported_langs {
        if !translations.contains_key(*lang) || translations[*lang].trim().is_empty() {
            missing_langs.push(*lang);
        }
    }

    if missing_langs.is_empty() {
        return Ok(());
    }

    // We need a source to translate from. Prefer 'en', else pick the first available.
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

    if api_key.is_empty() || api_key == "your_api_key_here" {
        // Fallback for local development if no API key is provided
        tracing::warn!("GOOGLE_TRANSLATE_API_KEY is not set. Using free Google Translate API fallback.");
        
        for target_lang in missing_langs {
            if target_lang == &source_lang {
                continue;
            }

            let encoded_text = urlencoding::encode(&source_text);
            let url = format!(
                "https://translate.googleapis.com/translate_a/single?client=gtx&sl={}&tl={}&dt=t&q={}",
                source_lang, target_lang, encoded_text
            );

            let mut success = false;
            match client.get(&url).send().await {
                Ok(resp) => {
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        if let Some(text) = body.get(0).and_then(|v| v.get(0)).and_then(|v| v.get(0)).and_then(|v| v.as_str()) {
                            translations.insert(target_lang.to_string(), text.to_string());
                            success = true;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to reach free Google Translate API: {}", e);
                }
            }

            if !success {
                translations.insert(target_lang.to_string(), source_text.clone());
            }
        }
        return Ok(());
    }

    let url = format!("https://translation.googleapis.com/language/translate/v2?key={}", api_key);

    for target_lang in missing_langs {
        if target_lang == &source_lang {
            continue;
        }

        let req_body = GoogleTranslateRequest {
            q: vec![&source_text],
            target: target_lang,
            source: &source_lang,
            format: "text",
        };

        let mut success = false;
        match client.post(&url).json(&req_body).send().await {
            Ok(resp) => {
                if let Ok(body) = resp.json::<GoogleTranslateResponse>().await {
                    if let Some(data) = body.data {
                        if let Some(t) = data.translations.first() {
                            translations.insert(target_lang.to_string(), t.translated_text.clone());
                            success = true;
                        }
                    } else {
                        tracing::error!("Google Translate Error: {:?}", body.error);
                    }
                }
            }
            Err(e) => {
                tracing::error!("Failed to reach Google Translate API: {}", e);
            }
        }
        
        if !success {
            // Fallback on error
            translations.insert(target_lang.to_string(), source_text.clone());
        }
    }

    Ok(())
}
