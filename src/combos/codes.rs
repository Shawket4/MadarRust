//! The combos and deals refusals (COMBOS_CONTRACT.md §2.7), ONE table: the
//! code, its HTTP status and its English and Arabic words. The server answers
//! with the English sentence in `reason` (vars filled in) and the figures in
//! `vars`; every client words the code itself (the POS core's `i18n.rs`, the
//! dashboard's locales, the public pages), from the same words as here.
//!
//! `{name}` placeholders are filled from `vars`; a missing var is left as is.

use serde_json::Value;

use crate::errors::AppError;

/// (code, status, English, Arabic).
pub const REFUSALS: &[(&str, u16, &str, &str)] = &[
    (
        "COMBO_UNAVAILABLE",
        409,
        "This combo isn't available right now.",
        "هذا الكومبو غير متاح الآن.",
    ),
    (
        "COMBO_PICKS_REQUIRED",
        400,
        "Choose the items for this combo.",
        "اختر أصناف الكومبو.",
    ),
    (
        "COMBO_SLOT_TOO_FEW",
        400,
        "Choose at least {min} for {slot}.",
        "اختر {min} على الأقل من {slot}.",
    ),
    (
        "COMBO_SLOT_TOO_MANY",
        400,
        "Choose at most {max} for {slot}.",
        "اختر {max} كحد أقصى من {slot}.",
    ),
    (
        "COMBO_CHOICE_NOT_ALLOWED",
        400,
        "That item can't be chosen here.",
        "لا يمكن اختيار هذا الصنف هنا.",
    ),
    (
        "COMBO_ITEM_UNAVAILABLE",
        409,
        "{item} isn't available right now.",
        "{item} غير متاح الآن.",
    ),
    (
        "COMBO_WHOLE_ONLY",
        409,
        "A combo is refunded or voided as a whole.",
        "يُسترد الكومبو أو يُلغى بالكامل فقط.",
    ),
    (
        "STAFF_DRINK_IN_COMBO",
        400,
        "A staff drink can't be part of a combo.",
        "لا يمكن أن يكون مشروب الموظف ضمن كومبو.",
    ),
    (
        "REWARD_IN_COMBO",
        409,
        "Rewards can't be used inside a combo.",
        "لا يمكن استخدام المكافآت داخل الكومبو.",
    ),
    (
        "COMBO_NESTED",
        400,
        "A combo can't contain another combo.",
        "لا يمكن أن يحتوي الكومبو على كومبو آخر.",
    ),
    (
        "COMBO_SLOT_INVALID",
        400,
        "Check the slot \"{slot}\".",
        "راجع الخانة «{slot}».",
    ),
    (
        "COMBO_SLOTS_REQUIRED",
        400,
        "Add at least one slot.",
        "أضف خانة واحدة على الأقل.",
    ),
    (
        "COMBO_NO_RECIPE",
        409,
        "A combo has no recipe of its own; each item uses its own.",
        "ليس للكومبو وصفة خاصة؛ كل صنف يستخدم وصفته.",
    ),
    (
        "COMBO_KIND_LOCKED",
        409,
        "This item has sales; its type can't change.",
        "لهذا الصنف مبيعات؛ لا يمكن تغيير نوعه.",
    ),
    (
        "MEAL_TARGET_INVALID",
        400,
        "That combo has no slot for this item.",
        "لا توجد خانة لهذا الصنف في الكومبو.",
    ),
    (
        "DEAL_NOT_ELIGIBLE",
        409,
        "This deal no longer applies to the cart.",
        "هذا العرض لم يعد ينطبق على السلة.",
    ),
    (
        "DEAL_INVALID",
        400,
        "Check the deal's \"{field}\".",
        "راجع «{field}» في العرض.",
    ),
    (
        "DEAL_UNITS_OVERLAP",
        400,
        "An item can only count toward one deal.",
        "يُحتسب الصنف في عرض واحد فقط.",
    ),
];

/// The economics warnings (never refusals): (code, English, Arabic).
pub const WARNINGS: &[(&str, &str, &str)] = &[
    (
        "MARGIN_BELOW_MIN",
        "Margin {margin} is below your minimum {min}.",
        "الهامش {margin} أقل من الحد الأدنى {min}.",
    ),
    (
        "NO_SAVING",
        "Customers save nothing versus ordering separately.",
        "لا يوفر العميل شيئًا مقارنة بالطلب المنفصل.",
    ),
    (
        "COST_UNKNOWN",
        "The cost of an item in this combo is unknown.",
        "تكلفة أحد أصناف هذا الكومبو غير معروفة.",
    ),
    (
        "SLOT_EMPTY_NOW",
        "A slot has no item available right now.",
        "لا يوجد صنف متاح الآن في إحدى الخانات.",
    ),
    (
        "CHOICE_INACTIVE",
        "A choice in this combo is switched off.",
        "أحد اختيارات هذا الكومبو متوقف.",
    ),
];

/// Fill `{name}` placeholders from `vars` (strings as is, numbers printed).
pub fn fill(template: &str, vars: &Value) -> String {
    let mut out = template.to_string();
    if let Some(map) = vars.as_object() {
        for (k, v) in map {
            let needle = format!("{{{k}}}");
            if out.contains(&needle) {
                let s = match v {
                    Value::String(s) => s.clone(),
                    Value::Null => continue,
                    other => other.to_string(),
                };
                out = out.replace(&needle, &s);
            }
        }
    }
    out
}

/// The words of `code`: (status, English, Arabic).
pub fn words(code: &str) -> Option<(u16, &'static str, &'static str)> {
    REFUSALS
        .iter()
        .find(|r| r.0 == code)
        .map(|r| (r.1, r.2, r.3))
}

/// The coded refusal `code`, its English sentence filled from `vars`.
pub fn refuse(code: &'static str, vars: Value) -> AppError {
    let (status, en, _) = words(code).unwrap_or((400, "Refused.", ""));
    AppError::CodedVars {
        status,
        code,
        reason: fill(en, &vars),
        vars,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_has_both_words_and_a_client_status() {
        for (code, status, en, ar) in REFUSALS {
            assert!(!en.trim().is_empty() && !ar.trim().is_empty(), "{code}");
            assert!(matches!(status, 400 | 409), "{code}");
            // The same placeholders in both languages.
            let ph = |s: &str| {
                let mut v: Vec<String> = s
                    .split('{')
                    .skip(1)
                    .filter_map(|p| p.split('}').next().map(str::to_string))
                    .collect();
                v.sort();
                v
            };
            assert_eq!(ph(en), ph(ar), "{code}");
        }
    }

    #[test]
    fn fill_names_the_figures() {
        assert_eq!(
            fill(
                "Choose at least {min} for {slot}.",
                &serde_json::json!({"min": 2, "slot": "Drink"})
            ),
            "Choose at least 2 for Drink."
        );
    }
}
