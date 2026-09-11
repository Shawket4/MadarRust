//! Apple Wallet store cards (`.pkpass`).
//!
//! A `.pkpass` is a zip of `pass.json`, a `manifest.json` of SHA-1 digests, and
//! a PKCS#7 detached signature over that manifest, made with the Pass Type ID
//! certificate. Everything here is built and testable today; the signature is
//! the one step that cannot exist until Madar holds an Apple Developer account,
//! and it is isolated in [`sign_manifest`] so that day is a small change.
//!
//! Configured by `LOYALTY_APPLE_PASS_TYPE_ID`, `LOYALTY_APPLE_TEAM_ID`,
//! `LOYALTY_APPLE_CERT_PEM`, `LOYALTY_APPLE_KEY_PEM` and `LOYALTY_APPLE_WWDR_PEM`.
//! With any of them unset there is no Apple button at all — signup still works
//! and the member still has a token and a QR.

use serde_json::json;
use sqlx::PgPool;

use crate::errors::AppError;
use crate::loyalty::model::MemberRow;
use crate::loyalty::settings::LoyaltySettings;

pub use super::{PassLocation, locations_for_org};

use super::google::progress_line;

/// The images every pass carries, compiled into the binary.
///
/// **`icon.png` is not optional.** A pass without one is rejected by iOS
/// outright, and the only thing the customer sees is Safari saying it "cannot
/// download this file" — no mention of an icon, nothing in any log. The rest
/// are what make the pass look like Madar rather than a grey rectangle.
///
/// Embedded rather than read from disk so a pass can never be half-built by a
/// missing file on one deploy, and so the manifest always covers exactly what
/// ships. Every entry here MUST be hashed into `manifest.json` — an
/// unhashed file in the archive invalidates the signature.
const PASS_IMAGES: &[(&str, &[u8])] = &[
    (
        "icon.png",
        include_bytes!("../../../static/wallet/icon.png"),
    ),
    (
        "icon@2x.png",
        include_bytes!("../../../static/wallet/icon@2x.png"),
    ),
    (
        "icon@3x.png",
        include_bytes!("../../../static/wallet/icon@3x.png"),
    ),
    (
        "logo.png",
        include_bytes!("../../../static/wallet/logo.png"),
    ),
    (
        "logo@2x.png",
        include_bytes!("../../../static/wallet/logo@2x.png"),
    ),
    (
        "logo@3x.png",
        include_bytes!("../../../static/wallet/logo@3x.png"),
    ),
];

/// The organisation's identity, as the PASS must carry it.
///
/// A pass is a file: it cannot reference a logo by URL the way the web card
/// does, so the image has to be resized and packed into the archive. And its
/// colours cannot come from `loyalty_settings` — that is where they used to
/// live, before branding moved to the organisation, which is exactly why a
/// freshly downloaded pass kept coming back in Apple's default grey.
pub struct PassBrand {
    pub org_name: String,
    pub background: String,
    pub foreground: String,
    pub label: String,
    /// The org's logo, already sized for the pass. Madar's is used when the
    /// shop has none.
    pub images: Vec<(String, Vec<u8>)>,
}

/// How near a branch the card starts surfacing on the lock screen, in metres.
///
/// This is the whole proximity feature: neither wallet can send a push when a
/// customer is nearby — the phone does it locally, from the coordinates baked
/// into the pass, and no server is involved or told. How wide the circle is, is
/// the only lever there is.
///
/// It is a setting because the right answer is a property of the SHOP, not of
/// this code: a kiosk on a busy street wants a tighter circle than a unit in a
/// mall you approach across a car park, and finding out means walking around
/// with a phone. `LOYALTY_PROXIMITY_METERS` overrides it without a deploy.
///
/// Wider is not automatically better. A card that surfaces while someone drives
/// past is noise, and a customer who deletes the pass over it is not coming
/// back to it.
const DEFAULT_PROXIMITY_METERS: u32 = 500;

fn proximity_meters() -> u32 {
    std::env::var("LOYALTY_PROXIMITY_METERS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        // Zero would be a circle nobody can be inside — a typo that silently
        // switches the feature off. Anything unparseable or absurd falls back
        // rather than shipping into every pass in the estate.
        .filter(|m| (1..=100_000).contains(m))
        .unwrap_or(DEFAULT_PROXIMITY_METERS)
}

/// Apple's strip sizes for a store card, at 1×/2×/3×.
///
/// Roughly 2.6:1. A photograph is cover-cropped to it rather than letterboxed —
/// a band with bars down the sides looks like a mistake, and the middle of a
/// photograph is where the subject is.
const STRIP_SIZES: [(&str, u32, u32); 3] = [
    ("strip.png", 375, 144),
    ("strip@2x.png", 750, 288),
    ("strip@3x.png", 1125, 432),
];

/// The share of the strip's width the primary field is drawn across.
///
/// Apple lays the primary field out from the leading edge; the value is large
/// and the label sits with it. Two thirds is generous — being wrong here means
/// scrimming slightly more of the photograph than strictly needed, which costs
/// nothing, where being wrong the other way costs legibility.
const TEXT_ZONE: f64 = 0.66;

/// How far the scrim fades past the text before it is gone entirely.
const FADE: f64 = 0.20;

/// The contrast the balance needs against the photograph.
///
/// WCAG's LARGE-text threshold, not the body-text one. The primary field is the
/// biggest thing on the pass — Apple renders it at a size where 3:1 is the
/// published bar — and holding a photograph to 4.5 costs it a great deal of
/// itself for contrast nobody needs. The shop chose that picture.
const STRIP_CONTRAST: f64 = 3.0;

/// Darken (or lighten) a photograph until text can be read on it.
///
/// Apple draws the primary fields ON the strip, in the pass's foreground
/// colour, over whatever the photograph happens to be — so a shop with a bright
/// photo and a white balance gets an unreadable card, and one with a dark photo
/// and dark ink gets the same. Emptying the fields avoids it at the cost of the
/// number a customer opens the card to see.
///
/// Since we render the strip, the photograph can simply be made safe to write
/// on. The strength is MEASURED rather than guessed: the scrim deepens until
/// the WORST pixel under the text clears AA against the foreground. A picture
/// that is already dark enough is left alone.
fn scrim(img: &mut image::RgbaImage, foreground: &str) {
    let (fr, fg_, fb) = crate::orgs::branding::parse_hex(foreground).unwrap_or((255, 255, 255));
    let fg_lum = crate::orgs::branding::luminance(fr, fg_, fb);
    // Toward the opposite end from the text, which is the direction that buys
    // contrast: a dark veil under white text, a light one under dark ink.
    let veil: f64 = if fg_lum > 0.5 { 0.0 } else { 255.0 };

    let (w, h) = (img.width(), img.height());
    let zone_w = (w as f64 * TEXT_ZONE).ceil() as u32;
    if zone_w == 0 {
        return;
    }

    // The hardest pixel to write on, per channel.
    //
    // Sampling a grid and hoping would miss exactly the pixel that matters — a
    // highlight one column wide is still a hole in the text. Instead each
    // channel's extreme across the whole zone is taken, and the scrim is solved
    // for the pixel made of those extremes. That pixel may not exist in the
    // photograph, and it is at least as hard to write on as any that does,
    // because luminance rises with every channel independently. Slightly
    // stronger than strictly needed, never weaker.
    let dark_veil = veil == 0.0;
    let mut extreme = if dark_veil { [0u8; 3] } else { [255u8; 3] };
    for y in 0..h {
        for x in 0..zone_w {
            let p = img.get_pixel(x, y).0;
            for c in 0..3 {
                extreme[c] = if dark_veil {
                    extreme[c].max(p[c])
                } else {
                    extreme[c].min(p[c])
                };
            }
        }
    }

    let reads_at = |alpha: f64| {
        let blend = |c: u8| (c as f64 * (1.0 - alpha) + veil * alpha).round() as u8;
        let l = crate::orgs::branding::luminance(
            blend(extreme[0]),
            blend(extreme[1]),
            blend(extreme[2]),
        );
        crate::orgs::branding::contrast(l, fg_lum) >= STRIP_CONTRAST
    };

    // Already safe: leave the photograph alone. Dimming one that needed no
    // dimming is a worse photograph for no gain — the shop chose it.
    if reads_at(0.0) {
        return;
    }
    let mut alpha = 0.95;
    let mut a = 0.05;
    while a <= 0.95 {
        if reads_at(a) {
            alpha = a;
            break;
        }
        a += 0.05;
    }

    // Full strength across the text, then faded out, so the photograph is only
    // dimmed where something is written on it.
    let fade_end = ((TEXT_ZONE + FADE) * w as f64).min(w as f64);
    for (x, _y, px) in img.enumerate_pixels_mut() {
        let x = x as f64;
        let a = if x <= zone_w as f64 {
            alpha
        } else if x >= fade_end {
            0.0
        } else {
            alpha * (1.0 - (x - zone_w as f64) / (fade_end - zone_w as f64))
        };
        if a <= 0.0 {
            continue;
        }
        for c in 0..3 {
            px.0[c] = (px.0[c] as f64 * (1.0 - a) + veil * a).round() as u8;
        }
    }
}

/// The shop's photograph, sized for the pass's strip.
///
/// Empty when there is no picture, which is the ordinary case and a finished
/// card — every pass looked like that until now.
pub fn strip_images(
    brand: &crate::orgs::branding::OrgBrand,
    foreground: &str,
) -> Vec<(String, Vec<u8>)> {
    let Some(img) = brand
        .card_image_url
        .as_deref()
        .and_then(crate::orgs::branding::read_upload)
    else {
        return Vec::new();
    };
    STRIP_SIZES
        .iter()
        .filter_map(|(name, w, h)| {
            let mut scaled = img
                .resize_to_fill(*w, *h, image::imageops::FilterType::Lanczos3)
                .to_rgba8();
            // Apple writes the balance across this. Make it safe to write on.
            scrim(&mut scaled, foreground);
            let mut buf = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(scaled)
                .write_to(&mut buf, image::ImageFormat::Png)
                .ok()
                .map(|_| ((*name).to_string(), buf.into_inner()))
        })
        .collect()
}

impl Default for PassBrand {
    fn default() -> Self {
        let p = crate::orgs::branding::Palette::default();
        Self {
            org_name: String::new(),
            background: p.background,
            foreground: p.foreground,
            label: p.accent,
            images: PASS_IMAGES
                .iter()
                .map(|(n, b)| ((*n).to_string(), b.to_vec()))
                .collect(),
        }
    }
}

/// Resize the org's logo into the image set a pass needs.
///
/// Apple's sizes, and both are required: `icon` (29pt) and `logo` (up to
/// 160x50pt in the header). They are NOT the same picture, and treating them as
/// one is what made a blue logo arrive white.
///
/// `logo` is drawn ON THE PASS, over the pass's own ground. A mark is repainted
/// in the foreground so it reads there, and it keeps its transparency —
/// giving it an opaque rectangle would look like a sticker stuck on the card.
///
/// `icon` is not drawn on the pass at all. iOS uses it in NOTIFICATIONS, on the
/// lock screen and in Wallet's list, where the system supplies the background
/// and chooses it — light or dark — without asking. A mark repainted white to
/// read on a blue pass is then white on a white notification, which is nothing
/// at all. So the icon carries its own ground: the shop's background colour,
/// opaque, with the mark on top. The same thing Google's badge has always done.
///
/// ASPECT RATIO is preserved for both. A wordmark is wide, a monogram is
/// square; `resize` fits and keeps the shape, where `resize_to_fill` — which
/// the icon used to use — cover-crops, quietly eating the ends off a wordmark.
fn images_from_logo(
    img: &image::DynamicImage,
    background: &str,
    tint: Option<&str>,
) -> Option<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::with_capacity(6);
    let encode = |im: image::DynamicImage| -> Option<Vec<u8>> {
        let mut buf = std::io::Cursor::new(Vec::new());
        im.write_to(&mut buf, image::ImageFormat::Png).ok()?;
        Some(buf.into_inner())
    };

    // 0.82 rather than Google's 0.70: Apple rounds the corners of this slot but
    // does not mask it to a circle, so there is more of the square to use.
    for (name, px) in [
        ("icon.png", 29u32),
        ("icon@2x.png", 58),
        ("icon@3x.png", 87),
    ] {
        out.push((
            name.to_string(),
            encode(crate::orgs::branding::on_ground(
                img, background, tint, px, 0.82,
            ))?,
        ));
    }

    let owned;
    let header = match tint {
        Some(hex) => {
            owned = crate::orgs::branding::tint_mark(img, hex);
            &owned
        }
        None => img,
    };
    for (name, w, h) in [
        ("logo.png", 160u32, 50u32),
        ("logo@2x.png", 320, 100),
        ("logo@3x.png", 480, 150),
    ] {
        out.push((
            name.to_string(),
            encode(header.resize(w, h, image::imageops::FilterType::Lanczos3))?,
        ));
    }
    Some(out)
}

/// Dress a pass in an organisation's brand.
///
/// The logo is read from DISK, not fetched: uploads are written locally, so the
/// file is already there — no network call while a customer waits, and no
/// server-side request to an address someone else supplied.
pub fn pass_brand(brand: &crate::orgs::branding::OrgBrand) -> PassBrand {
    let d = PassBrand::default();
    let images = brand
        .logo_url
        .as_deref()
        .and_then(crate::orgs::branding::read_logo)
        .and_then(|img| {
            // A mark is repainted in the pass's own foreground; a baked tile is
            // left alone, because a silhouette of it is just a rectangle.
            let tint = brand
                .logo_is_mark
                .then_some(brand.palette.foreground.as_str());
            images_from_logo(&img, &brand.palette.background, tint)
        })
        .unwrap_or(d.images);

    PassBrand {
        org_name: brand.name.clone(),
        background: brand.palette.background.clone(),
        foreground: brand.palette.foreground.clone(),
        label: brand.palette.accent.clone(),
        images,
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.trim().is_empty())
}

/// Shared with Google — see [`super::key_material`].
use super::key_material as pem_material;

pub fn pass_type_id() -> Option<String> {
    env_nonempty("LOYALTY_APPLE_PASS_TYPE_ID")
}

pub fn team_id() -> Option<String> {
    env_nonempty("LOYALTY_APPLE_TEAM_ID")
}

pub fn is_configured() -> bool {
    missing_env().is_empty()
}

/// Which settings Apple still needs, by name. See [`super::google::missing_env`]
/// for why this is reported rather than merely counted.
pub fn missing_env() -> Vec<String> {
    let mut out = Vec::new();
    if pass_type_id().is_none() {
        out.push("LOYALTY_APPLE_PASS_TYPE_ID".into());
    }
    if team_id().is_none() {
        out.push("LOYALTY_APPLE_TEAM_ID".into());
    }
    for key in [
        "LOYALTY_APPLE_CERT_PEM",
        "LOYALTY_APPLE_KEY_PEM",
        "LOYALTY_APPLE_WWDR_PEM",
    ] {
        if pem_material(key).is_none() {
            out.push(format!("{key} (or {key}_FILE)"));
        }
    }
    out
}

/// The pass as Apple models it.
///
/// Field layout, as decided: the **balance** is the primary field (it is what
/// staff and customer reconcile against), the **progress** line is secondary,
/// the member's **name** is auxiliary, and the how-it-works, reward list and
/// terms go on the back. The barcode is the member token, with the member's name
/// as `altText` so a teller can eyeball that they scanned the right card.
pub fn pass_json(
    member: &MemberRow,
    settings: &LoyaltySettings,
    locations: &[PassLocation],
    copy: &super::CardCopy,
    // What they are working towards, in the shop's own words.
    headline: &str,
    brand: &PassBrand,
) -> Result<serde_json::Value, AppError> {
    let (Some(pass_type), Some(team)) = (pass_type_id(), team_id()) else {
        return Err(AppError::ServiceUnavailable(
            "Apple Wallet is not configured".into(),
        ));
    };
    let program = &settings.program_name;
    let mode = settings.mode();
    let threshold = settings.default_reward_cost;
    let balance = member.balance_in(mode);

    // Empty unless something is outstanding, and an empty array is no row.
    let notice: Vec<serde_json::Value> = member
        .pass_notice
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .map(|text| {
            vec![json!({
                "key": "notice",
                "label": "",
                "value": text,
                "changeMessage": "%@"
            })]
        })
        .unwrap_or_default();

    // Where they are, and — only when there is something to say — what it is
    // for.
    //
    // The reward slot used to print the COST when a shop had curated nothing:
    // "Reward / 5 orders", which reads as though the reward IS five orders. It
    // is the price of one, and the stepper beside it already says that. An
    // empty headline now means no row rather than a false one.
    let mut secondary = vec![json!({
        "key": "progress",
        "label": program,
        "value": progress_line(balance, threshold)
    })];
    if !headline.trim().is_empty() {
        secondary.push(json!({
            "key": "reward",
            "label": "Reward",
            "value": headline
        }));
    }

    let mut back: Vec<serde_json::Value> = super::back_of_card(member, settings, copy)
        .into_iter()
        .map(|l| json!({ "key": l.key, "label": l.label, "value": l.value }))
        .collect();

    // Where else to find them, as links a finger can hit.
    //
    // `attributedValue` is the only field property Apple renders markup in, and
    // it takes a narrow subset — an anchor and little else. `value` is the
    // fallback for anywhere the attributed one is not used, so both are set and
    // they must say the same thing.
    if !copy.social.is_empty() {
        let anchors = copy
            .social
            .iter()
            .map(|l| format!(r#"<a href="{}">{}</a>"#, l.url, l.label))
            .collect::<Vec<_>>()
            .join("   ");
        let plain = copy
            .social
            .iter()
            .map(|l| format!("{}: {}", l.label, l.url))
            .collect::<Vec<_>>()
            .join("\n");
        back.push(json!({
            "key": "social",
            "label": "Find us",
            "value": plain,
            "attributedValue": anchors
        }));
    }

    let mut pass = json!({
        "formatVersion": 1,
        "passTypeIdentifier": pass_type,
        "teamIdentifier": team,
        // Stable per member: re-issuing must update the card already in the
        // customer's wallet, never add a second one.
        "serialNumber": member.id.to_string(),
        // The SHOP's name, not the programme's. This is the line a customer
        // sees in their wallet list and on the lock screen; "Rewards" there
        // tells them nothing about whose card it is.
        "organizationName": if brand.org_name.trim().is_empty() { program.as_str() } else { brand.org_name.as_str() },
        "description": format!("{program} card"),
        // The web service that serves updates. Apple only calls it when the
        // pass carries an auth token, which is minted at signup.
        "authenticationToken": member.apple_auth_token,
        "storeCard": {
            // No header fields. They sit beside the logo at the top and were
            // showing the balance a second time, in small type, directly above
            // the same number in large type — the card said "2" twice and
            // looked cluttered for it.
            //
            // The cost is real and worth stating: header fields are the only
            // part of a pass visible in Wallet's stacked view, so the sliver
            // now shows the shop and nothing else. The card has to be opened to
            // read the balance.
            //
            // Apple renders these OVER the strip. That used to mean a card
            // with a photograph had to give up its balance — but the strip is
            // scrimmed until the worst pixel under the text clears AA, so
            // there is nothing left to give up.
            "primaryFields": [{
                "key": "balance",
                "label": super::google::balance_label(mode),
                "value": balance
            }],
            // Progress and reward SHARE a row. Apple lays several fields in
            // one row side by side, and having them as separate secondary and
            // auxiliary rows cost the card a whole band of height for two
            // short strings — which is most of why it read as tall and empty.
            "secondaryFields": [
                {
                    "key": "progress",
                    "label": program,
                    "value": progress_line(balance, threshold)
                },
                {
                    "key": "reward",
                    "label": "Reward",
                    "value": headline
                }
            ],
            // A message riding on the card — see `wallet::notices`.
            //
            // Apple has no way to push text. iOS notifies when a FIELD's value
            // changes and that field's definition carries a `changeMessage`, so
            // a message has to BE a field: it appears on the card with the
            // notification and is dropped on the next update, which notifies
            // nobody. `%@` is the new value, so the notification reads as
            // whatever we wrote.
            //
            // Deliberately its own field and never the balance: a card that
            // announced every point earned would be a card people mute.
            "backFields": back
        },
        "barcodes": [{
            "format": "PKBarcodeFormatQR",
            "message": member.member_token,
            "messageEncoding": "iso-8859-1",
            "altText": member.name
        }]
    });

    // Apple appends `/v1/...` itself, so this is the scope's parent. Without it
    // the device never registers and the pass can never update — which is
    // permanent for every pass already issued, so it is worth falling back to
    // the API's own origin rather than depending on one variable.
    if let Some(url) = super::web_service_url() {
        pass["webServiceURL"] = json!(url);
    }
    // From the ORGANISATION's derived palette. These used to read
    // `loyalty_settings`, which stopped being written when branding moved to
    // the org — so every pass came back in Apple's default grey however the
    // shop's card looked on the web.
    // A card does not stop at full. Six orders against a reward every five is
    // one reward waiting AND one step towards the next, so the earned row goes
    // above and the live stepper below it — which is Apple's own order, since
    // auxiliary fields are drawn under secondary ones.
    if let Some(earned) = super::google::earned_line(balance, threshold) {
        pass["storeCard"]["secondaryFields"] = json!([json!({
            "key": "earned",
            "label": super::google::earned_label(balance, threshold),
            "value": earned
        })]);
        pass["storeCard"]["auxiliaryFields"] = json!(secondary);
    }

    // Added only when there is something to say, and left OUT entirely
    // otherwise — not as an empty array.
    //
    // Apple lays a store card out from the field groups that are PRESENT.
    // `"auxiliaryFields": []` is present, so it reserved a row and drew
    // nothing in it: a band of empty colour between the balance and the row
    // below, on every card that had no message. Which is every card, almost
    // always.
    //
    // Appended rather than assigned: the overflow row may already be there, and
    // a message must not cost the customer the sight of their own progress.
    if !notice.is_empty() {
        match pass["storeCard"]["auxiliaryFields"].as_array_mut() {
            Some(rows) => rows.extend(notice),
            None => pass["storeCard"]["auxiliaryFields"] = json!(notice),
        }
    }

    pass["backgroundColor"] = json!(hex_to_rgb_css(&brand.background));
    pass["foregroundColor"] = json!(hex_to_rgb_css(&brand.foreground));
    pass["labelColor"] = json!(hex_to_rgb_css(&brand.label));
    if !locations.is_empty() {
        pass["locations"] = json!(
            locations
                .iter()
                .map(|l| json!({
                    "latitude": l.latitude,
                    "longitude": l.longitude,
                    // How far away the card starts surfacing. Apple's own
                    // default is tight enough that the card appears at the
                    // door, by which point the customer is already deciding —
                    // most of the practical difference between noticing your
                    // card and not is showing it while they are still down the
                    // street. Not larger: a card that surfaces while someone
                    // drives past is noise, and noise gets passes deleted.
                    "maxDistance": proximity_meters(),
                    "relevantText": format!("{program} — you're near {}", l.name)
                }))
                .collect::<Vec<_>>()
        );
    }
    Ok(pass)
}

/// Apple wants `rgb(r, g, b)`, not `#rrggbb`. An unparseable colour is dropped
/// rather than shipped — a malformed value makes iOS reject the whole pass.
fn hex_to_rgb_css(hex: &str) -> String {
    let parse = |s: &str| u8::from_str_radix(s, 16).ok();
    let ok = hex.len() == 7
        && hex.starts_with('#')
        && parse(&hex[1..3]).is_some()
        && parse(&hex[3..5]).is_some()
        && parse(&hex[5..7]).is_some();
    if !ok {
        return "rgb(255, 255, 255)".to_string();
    }
    format!(
        "rgb({}, {}, {})",
        parse(&hex[1..3]).unwrap(),
        parse(&hex[3..5]).unwrap(),
        parse(&hex[5..7]).unwrap()
    )
}

/// PKCS#7 detached signature over `manifest.json`, made with the Pass Type ID
/// certificate and chained through Apple's WWDR intermediate.
///
/// Detached and binary, in DER: iOS verifies this blob against `manifest.json`
/// and refuses the pass — with no message the customer or the operator can see —
/// if anything about it is off. That silent failure is why this uses OpenSSL's
/// `PKCS7_sign` rather than a hand-rolled CMS structure.
///
/// The WWDR intermediate goes in as a chain certificate, not a signer: leaving it
/// out produces a signature that verifies on a Mac (which has WWDR installed) and
/// fails on a customer's phone, which is the worst way to find a bug.
fn sign_manifest(manifest: &[u8]) -> Result<Vec<u8>, AppError> {
    use openssl::pkcs7::{Pkcs7, Pkcs7Flags};
    use openssl::pkey::PKey;
    use openssl::stack::Stack;
    use openssl::x509::X509;

    let missing = |what: &str| {
        AppError::ServiceUnavailable(format!("Apple Wallet is not configured: {what} is missing"))
    };
    let cert_pem =
        pem_material("LOYALTY_APPLE_CERT_PEM").ok_or_else(|| missing("the certificate"))?;
    let key_pem =
        pem_material("LOYALTY_APPLE_KEY_PEM").ok_or_else(|| missing("the private key"))?;
    let wwdr_pem = pem_material("LOYALTY_APPLE_WWDR_PEM")
        .ok_or_else(|| missing("the Apple WWDR intermediate"))?;

    let bad = |what: &str, e: openssl::error::ErrorStack| {
        tracing::error!(error = %e, "Apple Wallet: {what} is not usable");
        AppError::ServiceUnavailable(format!("Apple Wallet {what} is not usable"))
    };
    let cert = X509::from_pem(&cert_pem).map_err(|e| bad("certificate", e))?;
    let key = PKey::private_key_from_pem(&key_pem).map_err(|e| bad("private key", e))?;
    let wwdr = X509::from_pem(&wwdr_pem).map_err(|e| bad("WWDR certificate", e))?;

    let mut chain = Stack::new().map_err(|e| bad("certificate chain", e))?;
    chain.push(wwdr).map_err(|e| bad("certificate chain", e))?;

    // DETACHED: the signature covers the manifest without embedding it.
    // BINARY: no MIME canonicalisation, so the bytes signed are the bytes
    // shipped — a CRLF rewrite here would break verification on the device.
    let signed = Pkcs7::sign(
        &cert,
        &key,
        &chain,
        manifest,
        Pkcs7Flags::DETACHED | Pkcs7Flags::BINARY,
    )
    .map_err(|e| bad("signature", e))?;
    signed.to_der().map_err(|e| bad("signature", e))
}

/// Assemble the `.pkpass` archive: `pass.json`, its manifest of SHA-1 digests,
/// and the detached signature over that manifest.
pub fn build_pkpass(
    pass: &serde_json::Value,
    images: &[(String, Vec<u8>)],
) -> Result<Vec<u8>, AppError> {
    use sha1::{Digest, Sha1};
    use std::io::Write;

    let pass_bytes = serde_json::to_vec(pass).map_err(|_| AppError::Internal)?;
    let digest = |b: &[u8]| {
        let mut h = Sha1::new();
        h.update(b);
        hex(&h.finalize())
    };

    // Every file in the archive except the manifest and the signature itself.
    // Built once and used for BOTH the manifest and the zip, so the two can
    // never disagree — a hash for a file that is not there, or a file with no
    // hash, invalidates the signature and iOS refuses the pass without saying
    // why.
    let mut payload: Vec<(&str, &[u8])> = vec![("pass.json", pass_bytes.as_slice())];
    payload.extend(images.iter().map(|(n, b)| (n.as_str(), b.as_slice())));

    let mut manifest_map = serde_json::Map::new();
    for (name, bytes) in &payload {
        manifest_map.insert((*name).to_string(), json!(digest(bytes)));
    }
    let manifest = serde_json::to_vec(&serde_json::Value::Object(manifest_map))
        .map_err(|_| AppError::Internal)?;
    let signature = sign_manifest(&manifest)?;

    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        // Stored, not deflated: a pass is three small files and iOS does not
        // care, so the simpler archive is the better one to debug.
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in payload.iter().copied().chain([
            ("manifest.json", manifest.as_slice()),
            ("signature", signature.as_slice()),
        ]) {
            zip.start_file(name, opts).map_err(|_| AppError::Internal)?;
            zip.write_all(bytes).map_err(|_| AppError::Internal)?;
        }
        zip.finish().map_err(|_| AppError::Internal)?;
    }
    Ok(buf.into_inner())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Build this member's `.pkpass`, ready to serve.
///
/// One place, so the pass a customer downloads and the pass their phone fetches
/// after an update are byte-identical in structure — a device that got a
/// different shape from the two paths would show a pass that never settles.
pub async fn build_pass_for(pool: &PgPool, member: &MemberRow) -> Result<Vec<u8>, AppError> {
    let settings = crate::loyalty::settings::load_scope(pool, member.org_id, None)
        .await?
        .unwrap_or_else(|| LoyaltySettings::defaults(member.org_id, None));
    let locations = super::locations_for_member(pool, member).await?;
    let copy = super::card_copy(pool, member.org_id, &settings).await;
    let org = crate::orgs::branding::load(pool, member.org_id).await?;
    let brand = pass_brand(&org);
    let strip = strip_images(&org, &brand.foreground);
    let headline = super::reward_headline(pool, member.org_id, &settings).await;
    let pass = pass_json(member, &settings, &locations, &copy, &headline, &brand)?;
    // One list for the archive AND the manifest, so an image cannot end up in
    // the zip unhashed — which invalidates the signature and makes iOS refuse
    // the pass with no explanation at all.
    let mut images = brand.images.clone();
    images.extend(strip);
    // BOTH languages, as files inside the pass — and the English one is not
    // redundant.
    //
    // A `.pkpass` carrying a single `.lproj` is a pass that speaks one language,
    // and iOS gives it to everyone: an English phone found only `ar.lproj`,
    // took it as the pass's localisation, and drew an Arabic card for someone
    // who reads English. The English file maps every key to itself, which looks
    // like a no-op and is the thing that makes English a language the pass HAS
    // rather than the text it happens to contain.
    //
    // See `wallet::i18n` for why the English text is the key.
    let pairs = super::i18n::strings_for(&settings, settings.program_name_ar.as_deref());
    images.push((
        "en.lproj/pass.strings".to_string(),
        super::i18n::strings_file(&super::i18n::identity(&pairs)).into_bytes(),
    ));
    images.push((
        "ar.lproj/pass.strings".to_string(),
        super::i18n::strings_file(&pairs).into_bytes(),
    ));
    build_pkpass(&pass, &images)
}

/// Tell every device holding this member's pass to come back for a new copy.
///
/// Apple's update model is a silent APNs push carrying no payload; the device
/// then calls the pass web service for the changed pass. Skipped when Apple is
/// not configured, so an org on Google only costs nothing here.
pub async fn notify_devices(pool: &PgPool, member: &MemberRow) -> Result<(), AppError> {
    if !is_configured() {
        return Ok(());
    }
    let tokens: Vec<(String, String)> = sqlx::query_as(
        "SELECT device_library_id, push_token FROM loyalty_pass_devices WHERE customer_id = $1",
    )
    .bind(member.id)
    .fetch_all(pool)
    .await?;
    if tokens.is_empty() {
        return Ok(());
    }
    if !super::apns::is_configured() {
        tracing::info!(
            customer_id = %member.id,
            devices = tokens.len(),
            "loyalty: pass changed but APNs is not configured — \
             the customer sees the new balance next time they open the pass"
        );
        return Ok(());
    }
    let Some(topic) = pass_type_id() else {
        return Ok(());
    };

    for (device_library_id, push_token) in tokens {
        match super::apns::push(&push_token, &topic).await {
            super::apns::PushOutcome::Delivered => {}
            // The pass is gone from that device. Dropping the registration is
            // the point of distinguishing this: otherwise we push into the void
            // on every balance change, forever.
            super::apns::PushOutcome::Unregistered => {
                let _ = sqlx::query(
                    "DELETE FROM loyalty_pass_devices \
                      WHERE device_library_id = $1 AND customer_id = $2",
                )
                .bind(&device_library_id)
                .bind(member.id)
                .execute(pool)
                .await;
            }
            super::apns::PushOutcome::Failed(why) => {
                tracing::warn!(customer_id = %member.id, error = %why, "APNs push failed");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn member() -> MemberRow {
        MemberRow {
            id: uuid::Uuid::nil(),
            org_id: uuid::Uuid::nil(),
            name: "Ali Hassan".into(),
            phone: "+201000000000".into(),
            member_token: "Mabcdefghijklmnopqrstuv".into(),
            points_balance: 30,
            visits_balance: 3,
            lifetime_points: 130,
            lifetime_visits: 3,
            locale: "en".into(),
            apple_serial: None,
            apple_auth_token: Some("tok".into()),
            google_object_id: None,
            pass_updated_at: None,
            joined_branch_id: None,
            enrolled_at: chrono::Utc::now(),
            marketing_opt_out: false,
            pass_notice: None,
        }
    }

    /// Take the wallet env lock for the duration of a test. Held by every test
    /// here, because these variables are process-global.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        super::super::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn configured() {
        // SAFETY: callers hold `env_guard`, so this is the only thread touching
        // the wallet environment.
        unsafe {
            std::env::set_var("LOYALTY_APPLE_PASS_TYPE_ID", "pass.cloud.madar-pos.loyalty");
            std::env::set_var("LOYALTY_APPLE_TEAM_ID", "TEAM123456");
        }
    }

    #[test]
    fn balance_is_primary_and_progress_is_secondary() {
        let _guard = env_guard();
        configured();
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let p = pass_json(
            &member(),
            &s,
            &[],
            &crate::loyalty::wallet::CardCopy::default(),
            "Free espresso",
            &PassBrand::default(),
        )
        .unwrap();
        assert_eq!(p["storeCard"]["primaryFields"][0]["value"], 30);
        // A hundred is past the point where dots are worth counting, so the
        // field carries the bare ratio — the only case where figures appear.
        assert_eq!(p["storeCard"]["secondaryFields"][0]["value"], "30 / 100");
        // No header fields: they sat beside the logo showing the same balance
        // in small type directly above the same number in large type.
        assert!(
            p["storeCard"]["headerFields"]
                .as_array()
                .is_none_or(|a| a.is_empty()),
            "the balance belongs on the card once"
        );
        // Progress and reward share ONE row: two short strings did not need a
        // band of card height each, which is most of why it read as tall.
        assert_eq!(p["storeCard"]["secondaryFields"][1]["label"], "Reward");
        assert_eq!(
            p["storeCard"]["secondaryFields"][1]["value"],
            "Free espresso"
        );
        assert!(
            p["storeCard"]["auxiliaryFields"]
                .as_array()
                .is_none_or(|a| a.is_empty()),
            "nothing left below it: {p}"
        );
        // And the name is still on the card, once, where a teller reads it.
        assert_eq!(p["barcodes"][0]["altText"], "Ali Hassan");
    }

    #[test]
    fn a_stamp_card_is_explained_in_stamps_not_pounds() {
        let _guard = env_guard();
        configured();
        let mut s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        s.mode = "visits".into();
        s.default_reward_cost = 5;
        let p = pass_json(
            &member(),
            &s,
            &[],
            &crate::loyalty::wallet::CardCopy::default(),
            "Free espresso",
            &PassBrand::default(),
        )
        .unwrap();
        // The stamps balance leads, not the points one.
        assert_eq!(p["storeCard"]["primaryFields"][0]["value"], 3);
        assert_eq!(p["storeCard"]["primaryFields"][0]["label"], "Orders");
        // A stamp card reads as a STEPPER, not as arithmetic: five orders is
        // few enough to count at a glance, and joining the steps shows the
        // direction of travel the way loose dots do not.
        assert_eq!(p["storeCard"]["secondaryFields"][0]["value"], "●─●─●─○─○");
        let how = p["storeCard"]["backFields"][0]["value"].as_str().unwrap();
        assert!(how.contains("stamp"), "{how}");
        assert!(
            !how.contains("EGP"),
            "a stamp card must not talk in EGP: {how}"
        );
    }

    #[test]
    fn the_pass_wears_the_shop_and_not_apple_default_grey() {
        configured();
        let _guard = env_guard();
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let brand = PassBrand {
            org_name: "RUE Coffee".into(),
            background: "#7B1E3A".into(),
            foreground: "#EFF3F4".into(),
            label: "#C8607F".into(),
            ..PassBrand::default()
        };
        let p = pass_json(
            &member(),
            &s,
            &[],
            &crate::loyalty::wallet::CardCopy::default(),
            "Free espresso",
            &brand,
        )
        .unwrap();

        // The colours reach the pass. They used to be read from
        // `loyalty_settings`, which stopped being written when branding moved
        // to the organisation — so every pass came back grey however the card
        // looked on the web, and a fresh download looked identical to an old one.
        assert_eq!(p["backgroundColor"], "rgb(123, 30, 58)");
        assert_eq!(p["foregroundColor"], "rgb(239, 243, 244)");
        assert_eq!(p["labelColor"], "rgb(200, 96, 127)");

        // And the SHOP's name is what a customer sees in their wallet list,
        // not the programme's.
        assert_eq!(p["organizationName"], "RUE Coffee");
    }

    #[test]
    fn a_nameless_org_falls_back_to_the_programme_rather_than_blank() {
        configured();
        let _guard = env_guard();
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let p = pass_json(
            &member(),
            &s,
            &[],
            &crate::loyalty::wallet::CardCopy::default(),
            "Free espresso",
            &PassBrand::default(),
        )
        .unwrap();
        assert_eq!(p["organizationName"], s.program_name);
        // Madar's palette, not an absent one — a pass with no colours is grey.
        assert_eq!(p["backgroundColor"], "rgb(13, 98, 115)");
    }

    #[test]
    fn a_shop_logo_becomes_the_full_apple_image_set() {
        // Apple needs icon and logo at three scales each, inside the archive —
        // a pass cannot reference an image by URL the way the web card does.
        let mut img = image::RgbaImage::new(400, 120);
        for px in img.pixels_mut() {
            *px = image::Rgba([123, 30, 58, 255]);
        }
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();

        let decoded = image::load_from_memory(&png.into_inner()).unwrap();
        let set = images_from_logo(&decoded, "#0D6273", None).expect("a real PNG resizes");
        let names: Vec<&str> = set.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "icon.png",
                "icon@2x.png",
                "icon@3x.png",
                "logo.png",
                "logo@2x.png",
                "logo@3x.png"
            ]
        );
        assert!(set.iter().all(|(_, b)| !b.is_empty()));

        // Junk falls back rather than shipping a pass with no icon, which iOS
        // refuses outright.

        // A MARK is repainted so it reads on the card. The pass's ground comes
        // from the logo's own dominant colour, so a logo left alone is very
        // nearly the colour it sits on — the shop that reported this had a blue
        // mark on a blue card.
        let mut mark = image::RgbaImage::new(40, 40);
        for (x, y, px) in mark.enumerate_pixels_mut() {
            // A shape on transparency: opaque in the middle, clear around it.
            let solid = (10..30).contains(&x) && (10..30).contains(&y);
            *px = image::Rgba([0x1E, 0x3A, 0x8A, if solid { 255 } else { 0 }]);
        }
        assert!(
            crate::orgs::branding::is_mark(&image::DynamicImage::ImageRgba8(mark.clone())),
            "a shape on transparency is a mark"
        );
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(mark)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let decoded = image::load_from_memory(&buf.into_inner()).unwrap();
        let tinted = images_from_logo(&decoded, "#0D6273", Some("#EFF3F4")).unwrap();
        // The HEADER LOGO is the one drawn on the pass's own ground, so it is
        // where "repainted, not left blue" is visible: transparent around a
        // mark in the foreground colour.
        let (_, logo_png) = tinted.iter().find(|(n, _)| n == "logo.png").unwrap();
        let logo = image::load_from_memory(logo_png).unwrap().to_rgba8();
        let opaque = logo.pixels().find(|p| p.0[3] > 200).expect("a shape");
        assert_eq!(
            [opaque.0[0], opaque.0[1], opaque.0[2]],
            [0xEF, 0xF3, 0xF4],
            "the mark is repainted in the pass's foreground, not left blue"
        );
        // The ICON carries its own ground instead, so the same mark sits on the
        // shop's background rather than on whatever a notification supplies.
        let icon = image::load_from_memory(&tinted[0].1).unwrap().to_rgba8();
        assert_eq!(icon.get_pixel(0, 0).0, [0x0D, 0x62, 0x73, 255]);
        assert!(
            icon.pixels()
                .any(|p| [p.0[0], p.0[1], p.0[2]] == [0xEF, 0xF3, 0xF4]),
            "the repainted mark is still on it"
        );

        // A logo with its background BAKED IN is not a mark: every pixel is
        // opaque, so repainting it would give a solid rectangle. It keeps its
        // own colours and gets a plate to sit on instead.
        assert!(!crate::orgs::branding::is_mark(
            &image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                40,
                40,
                image::Rgba([0x1E, 0x3A, 0x8A, 255])
            ))
        ));
    }

    /// The claim the whole scrim exists to make: whatever photograph a shop
    /// uploads, the balance Apple writes across it can be read.
    #[test]
    fn a_photograph_is_made_safe_to_write_the_balance_on() {
        use crate::orgs::branding::{contrast, luminance, parse_hex};

        // The worst pixel in the text zone, against the pass's foreground.
        let worst = |img: &image::RgbaImage, fg: &str| {
            let (r, g, b) = parse_hex(fg).unwrap();
            let fl = luminance(r, g, b);
            let zone = (img.width() as f64 * TEXT_ZONE) as u32;
            let mut worst = f64::MAX;
            for y in 0..img.height() {
                for x in 0..zone {
                    let p = img.get_pixel(x, y).0;
                    worst = worst.min(contrast(luminance(p[0], p[1], p[2]), fl));
                }
            }
            worst
        };

        // A blinding white photograph under white text — the case that made
        // emptying the primary fields look like the only option.
        let mut white = image::RgbaImage::from_pixel(300, 120, image::Rgba([255, 255, 255, 255]));
        assert!(
            worst(&white, "#EFF3F4") < STRIP_CONTRAST,
            "starts unreadable"
        );
        scrim(&mut white, "#EFF3F4");
        assert!(
            worst(&white, "#EFF3F4") >= STRIP_CONTRAST,
            "white text must read on it after the scrim"
        );

        // And the mirror image: a near-black photograph under dark ink, where
        // the veil has to go the other way.
        let mut black = image::RgbaImage::from_pixel(300, 120, image::Rgba([8, 8, 10, 255]));
        assert!(worst(&black, "#12222A") < STRIP_CONTRAST);
        scrim(&mut black, "#12222A");
        assert!(worst(&black, "#12222A") >= STRIP_CONTRAST);

        // A busy photograph: every pixel different, including the worst ones.
        let mut busy = image::RgbaImage::new(300, 120);
        for (x, y, p) in busy.enumerate_pixels_mut() {
            *p = image::Rgba([
                (x % 256) as u8,
                (y * 2 % 256) as u8,
                ((x + y) % 256) as u8,
                255,
            ]);
        }
        scrim(&mut busy, "#EFF3F4");
        assert!(
            worst(&busy, "#EFF3F4") >= STRIP_CONTRAST,
            "no pixel is exempt"
        );
    }

    /// Renders the real strip from a real photograph, to be LOOKED at. No
    /// assertion tells you whether a scrim is heavy-handed.
    ///
    ///     MADAR_STRIP_PREVIEW=photo.jpg \
    ///       cargo test --lib apple::tests::preview_strip -- --ignored --nocapture
    #[test]
    #[ignore = "writes a preview to look at"]
    fn preview_strip() {
        let Some(path) = std::env::var("MADAR_STRIP_PREVIEW").ok() else {
            println!("set MADAR_STRIP_PREVIEW=/path/to/photo.jpg to render one");
            return;
        };
        let src = image::open(&path).expect("a readable photo");
        for (name, fg) in [("light-text", "#EFF3F4"), ("dark-text", "#12222A")] {
            let mut band = src
                .resize_to_fill(750, 288, image::imageops::FilterType::Lanczos3)
                .to_rgba8();
            scrim(&mut band, fg);
            let at = std::env::temp_dir().join(format!("madar-strip-{name}.png"));
            band.save(&at).unwrap();
            println!("wrote {}", at.display());
        }
    }

    #[test]
    fn a_photograph_that_already_reads_is_left_alone() {
        // Dimming a picture that needed no dimming is a worse photograph for
        // no gain — the shop chose it.
        let dark = image::RgbaImage::from_pixel(200, 80, image::Rgba([12, 14, 18, 255]));
        let mut copy = dark.clone();
        scrim(&mut copy, "#EFF3F4");
        assert_eq!(dark, copy);
    }

    #[test]
    fn the_scrim_fades_out_rather_than_ending_in_a_line() {
        // A hard edge down the middle of a photograph reads as damage.
        let mut img = image::RgbaImage::from_pixel(400, 100, image::Rgba([255, 255, 255, 255]));
        scrim(&mut img, "#EFF3F4");
        let at = |x: u32| img.get_pixel(x, 50).0[0];
        let text_end = (400.0 * TEXT_ZONE) as u32;
        let fade_end = (400.0 * (TEXT_ZONE + FADE)) as u32;
        assert!(at(10) < at(text_end + 20), "it lightens across the fade");
        assert!(at(text_end + 20) < at(fade_end + 5), "and keeps lightening");
        assert_eq!(at(399), 255, "the far side is the photograph, untouched");
    }

    /// A pass with ONE `.lproj` speaks one language to everybody.
    ///
    /// iOS reads a single localisation folder as THE localisation, so an
    /// English phone that found only `ar.lproj` drew an Arabic card for someone
    /// who reads English. The English file maps each key to itself, which looks
    /// like a no-op and is exactly what makes English a language the pass has.
    #[test]
    fn a_pass_carries_both_languages_or_it_carries_one() {
        let mut s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        s.default_reward_cost = 5;
        let pairs = crate::loyalty::wallet::i18n::strings_for(&s, None);
        assert!(!pairs.is_empty());

        let english = crate::loyalty::wallet::i18n::identity(&pairs);
        assert_eq!(english.len(), pairs.len(), "every key, in both files");
        assert!(
            english.iter().all(|p| p.en == p.ar),
            "the English file says what the key says"
        );
        let file = crate::loyalty::wallet::i18n::strings_file(&english);
        assert!(file.contains(r#""How it works" = "How it works";"#));

        // And the Arabic file genuinely differs, or there was no point.
        let arabic = crate::loyalty::wallet::i18n::strings_file(&pairs);
        assert!(arabic.contains("طريقة الاستخدام"));
    }

    /// Apple cannot be sent a message, so a message has to be a field.
    ///
    /// iOS notifies when a field's value changes AND its definition carries a
    /// `changeMessage`. The notice field exists only while there is something
    /// to say — an empty row on every card would be a permanent blank line, and
    /// removing a field notifies nobody, which is what makes it disposable.
    /// A card does not stop at full, and it does not lie about the reward.
    #[test]
    fn a_full_card_starts_another_underneath_it() {
        let _guard = super::super::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // SAFETY: the lock makes this the only thread touching the environment.
        unsafe {
            std::env::set_var("LOYALTY_APPLE_PASS_TYPE_ID", "pass.example");
            std::env::set_var("LOYALTY_APPLE_TEAM_ID", "TEAM123456");
        }
        let mut s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        s.mode = "visits".into();
        s.default_reward_cost = 5;
        let copy = crate::loyalty::wallet::CardCopy::default();

        // Six orders against a reward every five: one waiting, one step made.
        let mut m = member();
        m.visits_balance = 6;
        let p = pass_json(&m, &s, &[], &copy, "", &PassBrand::default()).unwrap();
        assert_eq!(
            p["storeCard"]["secondaryFields"][0]["label"],
            "Reward ready"
        );
        assert_eq!(
            p["storeCard"]["secondaryFields"][0]["value"], "●─●─●─●─●",
            "the one they finished"
        );
        assert_eq!(
            p["storeCard"]["auxiliaryFields"][0]["value"], "●─○─○─○─○",
            "and the one they have started, below it"
        );

        // No curated reward means NO reward row. It used to print the cost
        // under a heading reading "Reward", which says the reward is five
        // orders; it is the price of one.
        let rows = p["storeCard"]["auxiliaryFields"].as_array().unwrap();
        assert!(
            !rows.iter().any(|f| f["key"] == "reward"),
            "an empty headline is not a reward"
        );

        // And with one named, it is there and says what it is.
        let told = pass_json(&m, &s, &[], &copy, "Free espresso", &PassBrand::default()).unwrap();
        assert_eq!(
            told["storeCard"]["auxiliaryFields"][1]["value"],
            "Free espresso"
        );
    }

    #[test]
    fn a_notice_rides_on_the_card_as_a_field_that_can_be_notified_on() {
        let _guard = super::super::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // SAFETY: the lock makes this the only thread touching the environment.
        unsafe {
            std::env::set_var("LOYALTY_APPLE_PASS_TYPE_ID", "pass.example");
            std::env::set_var("LOYALTY_APPLE_TEAM_ID", "TEAM123456");
        }
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let copy = crate::loyalty::wallet::CardCopy::default();

        let quiet = pass_json(
            &member(),
            &s,
            &[],
            &copy,
            "Free espresso",
            &PassBrand::default(),
        )
        .unwrap();
        assert!(
            quiet["storeCard"]["auxiliaryFields"]
                .as_array()
                .is_none_or(|a| a.is_empty()),
            "no message, no row"
        );

        let mut m = member();
        m.pass_notice = Some("Happy birthday, Ali 🎂".into());
        let told = pass_json(&m, &s, &[], &copy, "Free espresso", &PassBrand::default()).unwrap();
        let field = &told["storeCard"]["auxiliaryFields"][0];
        assert_eq!(field["value"], "Happy birthday, Ali 🎂");
        assert_eq!(
            field["changeMessage"], "%@",
            "without this iOS shows nothing at all"
        );
        // Never the balance: a card that announced every point earned is a card
        // people mute, and then the messages that matter go with it.
        assert!(told["storeCard"]["primaryFields"][0]["changeMessage"].is_null());
    }

    #[test]
    fn the_barcode_carries_the_token_not_the_id() {
        let _guard = env_guard();
        configured();
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let p = pass_json(
            &member(),
            &s,
            &[],
            &crate::loyalty::wallet::CardCopy::default(),
            "Free espresso",
            &PassBrand::default(),
        )
        .unwrap();
        assert_eq!(p["barcodes"][0]["message"], "Mabcdefghijklmnopqrstuv");
        assert_eq!(p["barcodes"][0]["format"], "PKBarcodeFormatQR");
        // The member id must never be the scannable value — it is guessable
        // from any other API response that carries one.
        assert_ne!(p["barcodes"][0]["message"], uuid::Uuid::nil().to_string());
    }

    #[test]
    fn the_proximity_radius_is_a_setting_with_a_sane_default() {
        let _guard = env_guard();
        // SAFETY: the guard makes this the only thread touching the environment.
        unsafe {
            std::env::remove_var("LOYALTY_PROXIMITY_METERS");
        }
        assert_eq!(proximity_meters(), 500, "the default a shop gets untouched");

        unsafe {
            std::env::set_var("LOYALTY_PROXIMITY_METERS", "150");
        }
        assert_eq!(proximity_meters(), 150);

        // A typo must not ship into every pass in the estate. Zero especially:
        // a circle nobody can stand inside switches the feature off silently.
        for bad in ["0", "-40", "abc", "", "  ", "99999999"] {
            unsafe {
                std::env::set_var("LOYALTY_PROXIMITY_METERS", bad);
            }
            assert_eq!(proximity_meters(), 500, "{bad:?} should fall back");
        }

        // Whitespace around a real number is an operator, not a mistake.
        unsafe {
            std::env::set_var("LOYALTY_PROXIMITY_METERS", " 250 ");
        }
        assert_eq!(proximity_meters(), 250);
        unsafe {
            std::env::remove_var("LOYALTY_PROXIMITY_METERS");
        }
    }

    #[test]
    fn branch_coordinates_become_lock_screen_locations() {
        let _guard = env_guard();
        configured();
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let locs = vec![PassLocation {
            latitude: 30.0444,
            longitude: 31.2357,
            name: "Zamalek".into(),
        }];
        let p = pass_json(
            &member(),
            &s,
            &locs,
            &crate::loyalty::wallet::CardCopy::default(),
            "Free espresso",
            &PassBrand::default(),
        )
        .unwrap();
        assert_eq!(p["locations"][0]["latitude"], 30.0444);
        assert!(
            p["locations"][0]["relevantText"]
                .as_str()
                .unwrap()
                .contains("Zamalek")
        );
    }

    #[test]
    fn colours_are_converted_and_bad_ones_do_not_reach_the_pass() {
        assert_eq!(hex_to_rgb_css("#0D6273"), "rgb(13, 98, 115)");
        // iOS rejects a pass outright on a malformed colour, so anything
        // unparseable falls back rather than shipping.
        assert_eq!(hex_to_rgb_css("teal"), "rgb(255, 255, 255)");
    }

    #[test]
    fn a_pass_is_refused_rather_than_served_unsigned() {
        let _guard = env_guard();
        configured();
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let p = pass_json(
            &member(),
            &s,
            &[],
            &crate::loyalty::wallet::CardCopy::default(),
            "Free espresso",
            &PassBrand::default(),
        )
        .unwrap();
        // With no certificate configured, building an archive must fail loudly.
        // An unsigned .pkpass is rejected by iOS with no explanation at all, so
        // serving one would look to the customer like a broken link.
        let err = build_pkpass(&p, &PassBrand::default().images).unwrap_err();
        assert!(
            matches!(err, AppError::ServiceUnavailable(_)),
            "an unconfigured signer is a 503 the operator can read, not a 500"
        );
    }

    #[test]
    fn a_signed_pass_is_a_zip_of_exactly_the_three_files_ios_expects() {
        let _guard = env_guard();
        configured();
        // A throwaway self-signed cert stands in for the Pass Type ID one: it
        // exercises the real PKCS#7 path (which is where the bugs are), and iOS
        // would reject the result — as it should, since this is not Apple's.
        let rsa = openssl::rsa::Rsa::generate(2048).unwrap();
        let key = openssl::pkey::PKey::from_rsa(rsa).unwrap();
        let mut name = openssl::x509::X509NameBuilder::new().unwrap();
        name.append_entry_by_text("CN", "madar-test").unwrap();
        let name = name.build();
        let mut b = openssl::x509::X509::builder().unwrap();
        b.set_version(2).unwrap();
        b.set_subject_name(&name).unwrap();
        b.set_issuer_name(&name).unwrap();
        b.set_pubkey(&key).unwrap();
        b.set_not_before(&openssl::asn1::Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        b.set_not_after(&openssl::asn1::Asn1Time::days_from_now(1).unwrap())
            .unwrap();
        b.sign(&key, openssl::hash::MessageDigest::sha256())
            .unwrap();
        let cert = b.build();

        // SAFETY: single-threaded test process.
        unsafe {
            std::env::set_var(
                "LOYALTY_APPLE_CERT_PEM",
                String::from_utf8(cert.to_pem().unwrap()).unwrap(),
            );
            std::env::set_var(
                "LOYALTY_APPLE_KEY_PEM",
                String::from_utf8(key.private_key_to_pem_pkcs8().unwrap()).unwrap(),
            );
            std::env::set_var(
                "LOYALTY_APPLE_WWDR_PEM",
                String::from_utf8(cert.to_pem().unwrap()).unwrap(),
            );
        }

        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let pass = pass_json(
            &member(),
            &s,
            &[],
            &crate::loyalty::wallet::CardCopy::default(),
            "Free espresso",
            &PassBrand::default(),
        )
        .unwrap();
        let bytes = build_pkpass(&pass, &PassBrand::default().images).unwrap();

        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();
        // `icon.png` is MANDATORY. A pass without one is rejected by iOS with
        // nothing but "cannot download this file" — no mention of an icon, and
        // nothing in any log. This assertion previously listed only the three
        // metadata files and so pinned that exact bug as correct.
        assert!(
            names.contains(&"icon.png".to_string()),
            "a pass without icon.png is refused by iOS: {names:?}"
        );
        assert_eq!(
            names,
            [
                "icon.png",
                "icon@2x.png",
                "icon@3x.png",
                "logo.png",
                "logo@2x.png",
                "logo@3x.png",
                "manifest.json",
                "pass.json",
                "signature",
            ]
        );

        // The manifest must carry the SHA-1 of the pass bytes actually shipped;
        // a digest of anything else is a pass iOS silently discards.
        use std::io::Read;
        let mut manifest = String::new();
        zip.by_name("manifest.json")
            .unwrap()
            .read_to_string(&mut manifest)
            .unwrap();
        let mut pass_bytes = Vec::new();
        zip.by_name("pass.json")
            .unwrap()
            .read_to_end(&mut pass_bytes)
            .unwrap();
        let digest = {
            use sha1::{Digest, Sha1};
            let mut h = Sha1::new();
            h.update(&pass_bytes);
            hex(&h.finalize())
        };
        let parsed: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(parsed["pass.json"], digest);

        // The manifest must cover EVERY file in the archive bar itself and the
        // signature, and hash nothing that is absent. Either mismatch
        // invalidates the signature, and iOS reports neither.
        let hashed: std::collections::BTreeSet<String> =
            parsed.as_object().unwrap().keys().cloned().collect();
        let shipped: std::collections::BTreeSet<String> = names
            .iter()
            .filter(|n| *n != "manifest.json" && *n != "signature")
            .cloned()
            .collect();
        assert_eq!(hashed, shipped, "manifest and archive must agree exactly");

        // And each hash must be of the bytes actually shipped, not of an
        // earlier version of the file.
        for name in &shipped {
            use sha1::{Digest, Sha1};
            let mut bytes = Vec::new();
            zip.by_name(name).unwrap().read_to_end(&mut bytes).unwrap();
            let mut h = Sha1::new();
            h.update(&bytes);
            assert_eq!(
                parsed[name].as_str().unwrap(),
                hex(&h.finalize()),
                "{name}: manifest hash does not match the shipped bytes"
            );
        }

        unsafe {
            std::env::remove_var("LOYALTY_APPLE_CERT_PEM");
            std::env::remove_var("LOYALTY_APPLE_KEY_PEM");
            std::env::remove_var("LOYALTY_APPLE_WWDR_PEM");
        }
    }

    // ── The icon carries its own ground ──────────────────────────────────────

    /// A solid block of one colour, `w` by `h`, on full transparency around a
    /// centred shape — a stand-in for a logo at whatever aspect ratio.
    fn fake_logo(w: u32, h: u32) -> image::DynamicImage {
        let mut img = image::RgbaImage::from_pixel(w, h, image::Rgba([0, 0, 0, 0]));
        // A mark: mostly clear, with an opaque shape in the middle.
        for y in (h / 4)..(h * 3 / 4) {
            for x in (w / 4)..(w * 3 / 4) {
                img.put_pixel(x, y, image::Rgba([0, 40, 200, 255]));
            }
        }
        image::DynamicImage::ImageRgba8(img)
    }

    fn decode(images: &[(String, Vec<u8>)], name: &str) -> image::DynamicImage {
        let (_, bytes) = images.iter().find(|(n, _)| n == name).expect(name);
        image::load_from_memory(bytes).expect("valid png")
    }

    /// THE BUG THIS PINS: the icon used to be a mark repainted in the pass's
    /// FOREGROUND and saved on transparency. That reads on the pass, whose
    /// ground is the shop's background — but iOS draws `icon.png` in
    /// notifications and on the lock screen, where IT picks the background. A
    /// white mark on a light notification is nothing at all, which is how a
    /// shop with a blue logo got no icon.
    #[test]
    fn every_icon_is_opaque_because_the_system_picks_what_is_behind_it() {
        let imgs = images_from_logo(&fake_logo(512, 512), "#0D6273", Some("#EFF3F4")).unwrap();
        for name in ["icon.png", "icon@2x.png", "icon@3x.png"] {
            let icon = decode(&imgs, name).to_rgba8();
            assert!(
                icon.pixels().all(|p| p.0[3] == 255),
                "{name} has transparent pixels; a notification would show the system's background through it"
            );
            // And it is the SHOP's ground, not black or white.
            assert_eq!(
                icon.get_pixel(0, 0).0,
                [0x0D, 0x62, 0x73, 255],
                "{name} corner"
            );
        }
    }

    /// The header logo is the opposite case: it IS drawn on the pass's own
    /// ground, so an opaque rectangle there would look like a sticker.
    #[test]
    fn the_header_logo_keeps_its_transparency() {
        let imgs = images_from_logo(&fake_logo(512, 512), "#0D6273", Some("#EFF3F4")).unwrap();
        let logo = decode(&imgs, "logo.png").to_rgba8();
        assert!(
            logo.pixels().any(|p| p.0[3] == 0),
            "the pass's own ground must show through around the mark"
        );
    }

    /// LOGOS ARE NOT SQUARE. A wordmark is wide, a monogram is square, some are
    /// tall — and the icon slot is square whatever arrives. Fitted and centred,
    /// never cover-cropped: `resize_to_fill`, which this used to use, would eat
    /// the ends off a wordmark, which is usually the shop's name.
    #[test]
    fn a_logo_of_any_shape_is_fitted_whole_and_centred() {
        for (w, h, shape) in [
            (1200, 300, "wide"),
            (300, 1200, "tall"),
            (512, 512, "square"),
        ] {
            let imgs = images_from_logo(&fake_logo(w, h), "#0D6273", None).unwrap();
            let icon = decode(&imgs, "icon@3x.png").to_rgba8();
            assert_eq!(
                (icon.width(), icon.height()),
                (87, 87),
                "{shape} icon is square"
            );

            // The artwork is centred, so the ground is symmetric around it: the
            // margins on opposite sides match. A cover-crop would fill one axis
            // edge to edge and leave nothing.
            let ground = image::Rgba([0x0D, 0x62, 0x73, 255]);
            let row = 87 / 2;
            let left = (0..87)
                .take_while(|x| *icon.get_pixel(*x, row) == ground)
                .count();
            let right = (0..87)
                .rev()
                .take_while(|x| *icon.get_pixel(*x, row) == ground)
                .count();
            let col = 87 / 2;
            let top = (0..87)
                .take_while(|y| *icon.get_pixel(col, *y) == ground)
                .count();
            let bottom = (0..87)
                .rev()
                .take_while(|y| *icon.get_pixel(col, *y) == ground)
                .count();
            assert!(
                left.abs_diff(right) <= 1,
                "{shape} not centred horizontally"
            );
            assert!(top.abs_diff(bottom) <= 1, "{shape} not centred vertically");

            // A wide logo keeps its width and gains margin above and below;
            // a tall one the reverse. Either way nothing is cropped away.
            if shape == "wide" {
                assert!(
                    top > left,
                    "a wide logo should sit as a band, not fill the square"
                );
            }
            if shape == "tall" {
                assert!(left > top, "a tall logo should sit as a column");
            }
        }
    }

    /// Madar's own fallback icons ship in the binary, and had the same defect —
    /// so an unbranded shop's pass was invisible in notifications too.
    #[test]
    fn the_built_in_fallback_icons_are_opaque_too() {
        for (name, bytes) in PASS_IMAGES.iter().filter(|(n, _)| n.starts_with("icon")) {
            let img = image::load_from_memory(bytes).expect(name).to_rgba8();
            assert!(
                img.pixels().all(|p| p.0[3] == 255),
                "{name} ships with transparency"
            );
        }
    }
}
