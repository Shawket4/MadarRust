//! The Rewards by Madar card look (the owner's board, 2026-10-19).
//!
//! - **Fallback card** (the org's custom branding is off): the gift mark is the
//!   logo, the shop's name is `organizationName`, the balance rides top right as
//!   a header field, and the strip is the card's own ground with a progress bar
//!   and the static "rewards by madar" footer baked in. The colours are the ones
//!   every fallback card already had (Madar's palette, unchanged).
//! - **Custom card**: everything the shop set stays (logo, colours, photograph
//!   with its scrim, name, balance as the primary field); only the strip gains
//!   the same bar and footer.
//!
//! Nothing here renders TEXT. Wallet fields are drawn by the OS in system
//! fonts and the QR by the OS from the pass data; the only words in an image
//! are the footer, a static file exported by the design system. Apple's
//! `footerImage` exists only on boarding passes, which is why the footer lives
//! in the strip.
//!
//! The brand files are in `static/wallet/rewards/` (copied from the design
//! system export; see REWARDS_DESIGN.md for the names and sizes).

use crate::orgs::branding::{luminance, parse_hex};

/// Bumped whenever what a pass LOOKS like changes, so stored passes and cached
/// strips built under the old look are not served again (store fingerprint,
/// strip cache key).
pub const LOOK_VERSION: u32 = 2;

macro_rules! asset {
    ($name:literal) => {
        include_bytes!(concat!("../../../static/wallet/rewards/", $name))
    };
}

const ICONS: [(&str, &[u8]); 3] = [
    ("icon.png", asset!("icon.png")),
    ("icon@2x.png", asset!("icon@2x.png")),
    ("icon@3x.png", asset!("icon@3x.png")),
];
const LOGO_INK: [(&str, &[u8]); 3] = [
    ("logo.png", asset!("logo-ink.png")),
    ("logo@2x.png", asset!("logo-ink@2x.png")),
    ("logo@3x.png", asset!("logo-ink@3x.png")),
];
const LOGO_PAPER: [(&str, &[u8]); 3] = [
    ("logo.png", asset!("logo-paper.png")),
    ("logo@2x.png", asset!("logo-paper@2x.png")),
    ("logo@3x.png", asset!("logo-paper@3x.png")),
];
const FOOTER_INK_12: &[u8] = asset!("rewards-by-madar-ink-12.png");
const FOOTER_INK_24: &[u8] = asset!("rewards-by-madar-ink-24.png");
const FOOTER_PAPER_12: &[u8] = asset!("rewards-by-madar-paper-12.png");
const FOOTER_PAPER_24: &[u8] = asset!("rewards-by-madar-paper-24.png");

/// Google's `programLogo` (circle-safe) and `heroImage` for a fallback card.
pub const GOOGLE_LOGO_INK: &[u8] = asset!("logo-ink-660.png");
pub const GOOGLE_LOGO_PAPER: &[u8] = asset!("logo-paper-660.png");
pub const GOOGLE_HERO: &[u8] = asset!("hero-1032x336.png");

/// The design system's rule: on a card colour whose relative luminance is
/// below 0.4, the light (Paper) present; otherwise the Ink one.
pub fn paper_on(background: &str) -> bool {
    let (r, g, b) = parse_hex(background).unwrap_or((0x0D, 0x62, 0x73));
    luminance(r, g, b) < 0.4
}

/// The fallback card's icon and logo set: the gift, ink or paper by the ground.
pub fn fallback_images(background: &str) -> Vec<(String, Vec<u8>)> {
    let logo = if paper_on(background) {
        LOGO_PAPER
    } else {
        LOGO_INK
    };
    ICONS
        .iter()
        .chain(logo.iter())
        .map(|(n, b)| ((*n).to_string(), b.to_vec()))
        .collect()
}

/// The footer for a strip at `scale` (1, 2 or 3), in the colour that reads on
/// the pass: paper under light text, ink under dark. 3x is the 2x file scaled
/// by 1.5 (there is no 36 px export).
fn footer(scale: u32, foreground_is_light: bool) -> Option<image::RgbaImage> {
    let bytes = match (scale, foreground_is_light) {
        (1, true) => FOOTER_PAPER_12,
        (1, false) => FOOTER_INK_12,
        (_, true) => FOOTER_PAPER_24,
        (_, false) => FOOTER_INK_24,
    };
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    Some(if scale >= 3 {
        image::imageops::resize(
            &img,
            img.width() * 3 / 2,
            img.height() * 3 / 2,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        img
    })
}

fn mix(a: [u8; 3], b: [u8; 3], t: f64) -> [u8; 3] {
    let m = |x: u8, y: u8| (x as f64 * (1.0 - t) + y as f64 * t).round() as u8;
    [m(a[0], b[0]), m(a[1], b[1]), m(a[2], b[2])]
}

/// Fill a strip with the card's own ground (a fallback card has no photograph).
pub fn ground(w: u32, h: u32, background: &str) -> image::RgbaImage {
    let (r, g, b) = parse_hex(background).unwrap_or((0x0D, 0x62, 0x73));
    image::RgbaImage::from_pixel(w, h, image::Rgba([r, g, b, 255]))
}

/// Above this many steps the bar is one continuous track (a points card).
const SEGMENTED_UP_TO: i32 = 12;

/// Bake the progress bar and the footer into the bottom of a strip.
///
/// `progress` of `size` is the CURRENT card (what is left after full cards are
/// set aside: `madar_loyalty::card`), drawn in the pass's foreground on a
/// track of the foreground mixed into what is under it. `scale` is the strip's
/// 1x/2x/3x. On a photograph the bottom is first veiled towards the card's
/// ground so the bar and the footer read on any picture.
pub fn finish(
    img: &mut image::RgbaImage,
    scale: u32,
    progress: i32,
    size: i32,
    foreground: &str,
    background: &str,
    photo: bool,
) {
    let s = scale.max(1);
    let (w, h) = (img.width(), img.height());
    let (fr, fg_, fb) = parse_hex(foreground).unwrap_or((0xEF, 0xF3, 0xF4));
    let fg = [fr, fg_, fb];
    let light_text = luminance(fr, fg_, fb) > 0.5;

    if photo {
        let (br, bg_, bb) = parse_hex(background).unwrap_or((0x0D, 0x62, 0x73));
        let veil = [br, bg_, bb];
        let top = h * 45 / 100;
        for y in top..h {
            let t = 0.8 * (y - top) as f64 / (h - top).max(1) as f64;
            for x in 0..w {
                let p = img.get_pixel_mut(x, y);
                let m = mix([p.0[0], p.0[1], p.0[2]], veil, t);
                p.0 = [m[0], m[1], m[2], 255];
            }
        }
    }

    let margin = 16 * s;
    let foot = footer(s, light_text);
    let foot_h = foot.as_ref().map_or(0, |f| f.height());
    let foot_y = h.saturating_sub(12 * s + foot_h);
    let bar_h = 6 * s;
    let bar_y = foot_y.saturating_sub(10 * s + bar_h);
    let track_w = w.saturating_sub(2 * margin);

    let size = size.max(1);
    let filled = progress.clamp(0, size);
    let paint = |img: &mut image::RgbaImage, x0: u32, x1: u32, on: bool| {
        for y in bar_y..(bar_y + bar_h).min(h) {
            for x in x0..x1.min(w) {
                let p = img.get_pixel_mut(x, y);
                let c = if on {
                    fg
                } else {
                    mix([p.0[0], p.0[1], p.0[2]], fg, 0.3)
                };
                p.0 = [c[0], c[1], c[2], 255];
            }
        }
    };
    if size <= SEGMENTED_UP_TO {
        let gap = 4 * s;
        let seg = (track_w.saturating_sub(gap * (size as u32 - 1))) / size as u32;
        for i in 0..size as u32 {
            let x0 = margin + i * (seg + gap);
            paint(img, x0, x0 + seg, (i as i32) < filled);
        }
    } else {
        let cut = margin + (track_w as f64 * filled as f64 / size as f64).round() as u32;
        paint(img, margin, cut, true);
        paint(img, cut, margin + track_w, false);
    }

    if let Some(f) = foot {
        image::imageops::overlay(img, &f, margin as i64, foot_y as i64);
    }
}
