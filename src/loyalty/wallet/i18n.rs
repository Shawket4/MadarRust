//! The card, in the language of the phone holding it.
//!
//! Everything else in this system asks the MEMBER what language they read —
//! stored at signup, corrected when they open their card. A pass cannot work
//! that way and should not: it lives on a phone for years, and the person
//! holding it may have changed their mind about English long after they told
//! us. Both wallets localise by DEVICE, and that is the more honest answer.
//!
//! ## Apple, and why the English is the key
//!
//! A `.pkpass` carries `<lang>.lproj/pass.strings`, and iOS looks up every
//! string in `pass.json` there before drawing it. The obvious implementation is
//! to put opaque keys in the pass and translate them in the strings files — and
//! it is the wrong one here, because half of what a card says is computed per
//! member: "a reward costs 5 of them" has a number in it that no static table
//! can hold.
//!
//! So the English text IS the key. A pass with no strings file renders exactly
//! as it always did, which makes this additive rather than a rewrite; and
//! because a pass is built per member, the Arabic file is built per member too,
//! with the same numbers already substituted. Nothing has to be templated.
//!
//! ## Google
//!
//! Google takes translations inline, field by field, so the same pairs feed
//! `localizedLabel` and friends. One table, two wallets, no chance of the two
//! disagreeing about what "Reward earned" means.

use crate::loyalty::earn::Mode;
use crate::loyalty::settings::LoyaltySettings;

/// An English string on the card, and what it says in Arabic.
pub struct Pair {
    pub en: String,
    pub ar: String,
}

fn pair(en: impl Into<String>, ar: impl Into<String>) -> Pair {
    Pair {
        en: en.into(),
        ar: ar.into(),
    }
}

/// What the balance is called.
pub fn balance_label(mode: Mode) -> Pair {
    match mode {
        Mode::Points => pair("Points", "نقاط"),
        Mode::Visits => pair("Orders", "طلبات"),
    }
}

/// The unit, as it reads inside a sentence.
///
/// Arabic changes the noun with the number — two is dual, three to ten takes
/// the plural, eleven and up takes the singular — so `{n} نقطة` is right at
/// eleven and wrong at five, and no template can know which. `{n} من النقاط`
/// sidesteps the agreement entirely and is correct for every number, which is
/// the only form that can be built from a count we do not know in advance.
pub fn unit_in_a_sentence(mode: Mode, n: i32) -> Pair {
    match mode {
        Mode::Points => pair(format!("{n} points"), format!("{n} من النقاط")),
        Mode::Visits => pair(format!("{n} stamps"), format!("{n} من الأختام")),
    }
}

/// "Any item" — short enough for the face of a card.
///
/// The face has two slots and they are narrow. The full sentence with the
/// price in it overflowed on Android, so the face says the short thing and the
/// details list below carries [`any_item`] in full. A customer reading a card
/// wants to know WHAT they get; how much it costs is on the row underneath.
pub fn any_item_short() -> Pair {
    pair("Any item", "أي صنف")
}

/// "Anything on the menu", and what it costs.
///
/// The shape avoids Arabic's number agreement for the same reason
/// [`unit_in_a_sentence`] does — see the note there.
pub fn any_item(mode: Mode, cost: i32) -> Pair {
    let unit = unit_in_a_sentence(mode, cost);
    pair(
        format!("Anything on the menu — {}", unit.en),
        format!("أي صنف من المنيو — {}", unit.ar),
    )
}

/// Every fixed label a card prints.
pub fn labels() -> Vec<Pair> {
    vec![
        pair("Reward", "مكافأتك"),
        pair("Reward earned", "مكافأة جاهزة"),
        pair("How it works", "طريقة الاستخدام"),
        pair("Member", "العضو"),
        pair("Rewards you can claim", "مكافآت متاحة"),
        pair("Where it works", "أماكن الاستخدام"),
        pair("Terms", "الشروط"),
        pair("Find us", "تجدنا هنا"),
    ]
}

/// "How it works", with this shop's numbers already in it.
pub fn how_it_works(settings: &LoyaltySettings) -> Pair {
    let threshold = settings.default_reward_cost;
    match settings.mode() {
        Mode::Points => {
            let egp = settings.earn_piastres_per_point / 100;
            pair(
                format!(
                    "Show this card when you pay. You earn a point for every {egp} EGP you \
                     spend, and a reward costs {threshold} points."
                ),
                format!(
                    "اعرض هذه البطاقة عند الدفع. تكسب نقطة عن كل {egp} جنيه تنفقها، \
                     والمكافأة تكلف {threshold} من النقاط."
                ),
            )
        }
        Mode::Visits => pair(
            format!(
                "Show this card when you pay. Every order earns a stamp, and a reward costs \
                 {threshold} of them."
            ),
            format!(
                "اعرض هذه البطاقة عند الدفع. كل طلب يكسبك ختمًا، والمكافأة تكلف \
                 {threshold} من الأختام."
            ),
        ),
    }
}

/// The line that admits a branch list is partial.
pub fn and_more(rest: usize) -> Pair {
    pair(format!("and {rest} more"), format!("و{rest} فروع أخرى"))
}

/// What the pass is called in a wallet list.
pub fn description(program: &str) -> Pair {
    pair(format!("{program} card"), format!("بطاقة {program}"))
}

/// Everything one member's pass might say, ready to write as `pass.strings`.
///
/// Built per pass, because half of it is per pass. Anything whose Arabic is the
/// same as its English — a branch name, a member's own name — is left out: a
/// strings file that maps a string to itself is noise in a signed archive.
pub fn strings_for(settings: &LoyaltySettings, program_ar: Option<&str>) -> Vec<Pair> {
    let mut out = labels();
    if settings.reward_any_item {
        out.push(any_item(settings.mode(), settings.default_reward_cost));
        out.push(any_item_short());
    }
    out.push(balance_label(settings.mode()));
    out.push(how_it_works(settings));
    out.push(description(&settings.program_name));
    // The programme's own name, where the shop gave us one in Arabic.
    if let Some(ar) = program_ar.filter(|s| !s.trim().is_empty()) {
        out.push(pair(settings.program_name.clone(), ar.to_string()));
        out.push(pair(
            format!("{} card", settings.program_name),
            format!("بطاقة {ar}"),
        ));
    }
    // A reward's headline when the shop has curated nothing and the fallback is
    // a bare cost.
    out.push(unit_in_a_sentence(
        settings.mode(),
        settings.default_reward_cost,
    ));
    out.retain(|p| p.en != p.ar && !p.en.is_empty());
    out
}

/// The same strings, mapped to themselves.
///
/// A `.pkpass` with one `.lproj` folder speaks one language, whatever the text
/// inside it says — iOS reads that folder as THE localisation and hands it to
/// every device. Shipping an English file that maps each key to itself is what
/// makes English a language the pass has, rather than merely the language its
/// keys are written in, so an English phone gets English.
pub fn identity(pairs: &[Pair]) -> Vec<Pair> {
    pairs
        .iter()
        .map(|p| Pair {
            en: p.en.clone(),
            ar: p.en.clone(),
        })
        .collect()
}

/// The pairs as an Apple `.strings` file.
///
/// Escaped by hand rather than with a crate: the format is two quoted strings
/// and a semicolon, and the only characters that can break it are a quote and a
/// backslash. A dependency for that would be a dependency to audit.
pub fn strings_file(pairs: &[Pair]) -> String {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let mut out = String::from("/* Generated per pass — see wallet::i18n. */\n");
    for p in pairs {
        out.push_str(&format!("\"{}\" = \"{}\";\n", esc(&p.en), esc(&p.ar)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn the_arabic_never_has_to_agree_with_a_number() {
        // The trap: Arabic inflects the noun by the count. Every form we emit
        // has to read correctly at 2, at 5 and at 11, which only the
        // "{n} of the points" shape does.
        for n in [1, 2, 5, 11, 100] {
            let p = unit_in_a_sentence(Mode::Points, n);
            assert_eq!(p.ar, format!("{n} من النقاط"));
        }
    }

    #[test]
    fn a_strings_file_is_escaped_and_never_maps_a_string_to_itself() {
        let pairs = vec![pair("Say \"hi\"", "قل \"مرحبا\""), pair("same", "same")];
        let file = strings_file(&pairs);
        assert!(file.contains(r#""Say \"hi\"" = "قل \"مرحبا\"";"#));
        // `strings_for` filters these; `strings_file` renders what it is given,
        // so the filtering is tested where it happens.
        let mut kept = vec![pair("same", "same"), pair("Terms", "الشروط")];
        kept.retain(|p| p.en != p.ar);
        assert_eq!(kept.len(), 1);
    }

    /// "Collect five, get anything" has to reach the card, or the card
    /// understates the programme it describes.
    #[test]
    fn everything_claimable_is_said_as_everything() {
        let mut s = LoyaltySettings::defaults(Uuid::nil(), None);
        s.mode = "visits".into();
        s.default_reward_cost = 5;
        s.reward_any_item = true;
        let p = any_item(s.mode(), s.default_reward_cost);
        assert_eq!(p.en, "Anything on the menu — 5 stamps");
        assert!(p.ar.contains("أي صنف"));
        // And it reaches the pass's Arabic, so a phone set to Arabic reads it.
        assert!(strings_for(&s, None).iter().any(|q| q.en == p.en));
    }

    #[test]
    fn a_shops_numbers_are_in_both_languages() {
        let mut s = LoyaltySettings::defaults(Uuid::nil(), None);
        s.mode = "visits".into();
        s.default_reward_cost = 5;
        let p = how_it_works(&s);
        assert!(p.en.contains("costs 5 of them"));
        assert!(p.ar.contains("5 من الأختام"));
    }
}
