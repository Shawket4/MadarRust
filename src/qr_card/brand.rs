//! The shop's identity, prepared for the card.
//!
//! [`crate::orgs::branding::load`] is the one brand loader and it has already
//! applied the tier gate, so nothing here asks whether the organisation is
//! allowed its own colours — it asks only whether the colours it was handed can
//! be printed on a card that still scans, and whether the logo it was handed is
//! big enough to print at all.
//!
//! The split matters: everything the layout needs is resolved HERE, once, into
//! plain strings and PNG bytes. `layout.rs` stays a pure SVG emitter that does
//! no colour arithmetic and touches no files, which is what keeps the composed
//! card unit-testable without a database or an uploads directory.

use std::io::Cursor;

use image::GenericImageView;

use super::{PAPER, TEAL};
use crate::orgs::branding::{self, OrgBrand};

/// The AA floor between the QR's dark modules and its quiet zone.
///
/// A card that came back in a shop's colours but would not scan is a worse
/// outcome than one that came back in Madar's, because the failure only shows
/// up after several hundred of them have been printed.
pub const MIN_QR_CONTRAST: f64 = 4.5;

/// WCAG's floor for a non-text graphic, which is all the frame hairline is.
/// Holding a decorative rule to the body-text bar would reject most accents
/// for no legibility anyone would notice.
pub const MIN_FRAME_CONTRAST: f64 = 3.0;

/// The smallest uploaded logo worth printing in the mark slot, in pixels on its
/// long edge.
///
/// The slot is `layout::MARK_SIZE` — 15 mm square — and the logo's long
/// edge is fitted to it, so at the card's default 600 DPI the slot is
/// `15 / 25.4 * 600 = 354` device pixels. Demanding 354 px would reject a great
/// many perfectly good uploads, so the bar is the accepted commercial-print
/// floor of 300 DPI instead: `15 / 25.4 * 300 = 177` px. Below that the logo is
/// being upscaled more than twofold to reach the printed slot and goes visibly
/// soft — and these cards are ordered by the hundred, so nobody finds out until
/// the box arrives. Madar's vector mark is the better card at that point.
pub const MIN_LOGO_PX: u32 = 177;

/// The largest logo worth carrying, in pixels on its long edge.
///
/// `15 / 25.4 * 2400 = 1417` px is the slot at `MAX_DPI`, the highest
/// raster this module will ever produce, so no pixel above it can be seen on
/// any output. Left uncapped, a 4000 px upload becomes several megabytes of
/// base64 inside every card SVG — and the SVG is returned inline in a JSON
/// response.
pub const MAX_LOGO_PX: u32 = 1417;

/// A shop's logo, decoded, tinted if that was allowed, and re-encoded as PNG.
///
/// PNG bytes rather than an `image::DynamicImage` because both consumers want
/// bytes: the SVG path base64s them into an `<image>` href, and the raster path
/// hands them back to a decoder anyway. Carrying the dimensions alongside saves
/// the layout from parsing a PNG header to work out its aspect fit.
#[derive(Clone)]
pub struct CardLogo {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Byte arrays make for useless debug output, so only the shape is printed.
impl std::fmt::Debug for CardLogo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "CardLogo {{ {}x{}, {} bytes }}",
            self.width,
            self.height,
            self.png.len()
        )
    }
}

/// Everything the card needs to wear a shop's brand instead of Madar's.
///
/// The colours here are already RESOLVED — ordered dark-on-light, checked for
/// contrast, and replaced with Madar's own if they could not clear the bar. A
/// `CardBrand` is therefore always safe to paint, and the layout never has to
/// decide whether to trust it.
#[derive(Clone, Debug)]
pub struct CardBrand {
    /// The shop's name, typeset where Madar's wordmark sits on an unbranded card.
    pub name: String,
    /// The card's ground and the QR's quiet zone. The lighter of the pair.
    pub ground: String,
    /// QR modules, the shop name, the caption, crop marks. The darker of the pair.
    pub ink: String,
    /// The frame hairline, when it reads against the ground; `ink` otherwise.
    pub accent: String,
    /// The shop's mark. `None` means Madar's vector mark is drawn instead —
    /// either the shop has no logo, or the one it has is too small to print.
    pub logo: Option<CardLogo>,
}

/// Prepare an organisation's brand for the card, or `None` to print Madar's.
///
/// `None` for an org that is not on the branding tier is not a special case
/// bolted on here — it is the whole point of returning an `Option`. The
/// unbranded card must come out byte-for-byte as it did before this feature
/// existed, and the only way to guarantee that is for the unbranded path to
/// remain literally the code that was already there.
pub fn card_brand(org: &OrgBrand) -> Option<CardBrand> {
    if !org.custom_branding {
        return None;
    }

    let (ground, ink) = qr_safe_pair(&org.palette.background, &org.palette.foreground);

    // The accent is the only colour nothing else has vetted: the palette
    // guarantees the background/foreground pair reads, but says nothing about
    // the accent against whichever of them ended up as the ground.
    let accent = if contrast_between(&ground, &org.palette.accent) >= MIN_FRAME_CONTRAST {
        org.palette.accent.clone()
    } else {
        ink.clone()
    };

    let logo = org
        .logo_url
        .as_deref()
        .and_then(branding::read_upload)
        .and_then(|img| prepare_logo(&img, org.logo_is_mark, &ink));

    Some(CardBrand {
        name: org.name.clone(),
        ground,
        ink,
        accent,
        logo,
    })
}

/// Order a brand's two colours into `(ground, modules)` such that the QR scans,
/// falling back to Madar's paper and teal when they cannot be made to.
///
/// Two separate things have to be true. Contrast is the obvious one. The other
/// is polarity: the QR specification wants dark modules on a light field, and
/// while a few readers cope with an inverted code, plenty of phone cameras and
/// every cheap handheld scanner do not — so the darker of the pair becomes the
/// modules whichever way round the palette named them. That reordering is not a
/// compromise on the brand, it is usually the shop's colour arriving intact: a
/// navy logo yields a navy-on-paper card, a gold one a near-black-on-gold card.
///
/// Only when the two are genuinely too close does the card give up its colour,
/// and that is a defensive path — the palette derivation already moves a ground
/// until it clears AA — for rows written before that existed or set by hand.
fn qr_safe_pair(background: &str, foreground: &str) -> (String, String) {
    let (Some(bg), Some(fg)) = (
        branding::parse_hex(background),
        branding::parse_hex(foreground),
    ) else {
        return (PAPER.to_string(), TEAL.to_string());
    };
    let (bg_l, fg_l) = (
        branding::luminance(bg.0, bg.1, bg.2),
        branding::luminance(fg.0, fg.1, fg.2),
    );
    if branding::contrast(bg_l, fg_l) < MIN_QR_CONTRAST {
        return (PAPER.to_string(), TEAL.to_string());
    }
    if bg_l >= fg_l {
        (background.to_string(), foreground.to_string())
    } else {
        (foreground.to_string(), background.to_string())
    }
}

/// Contrast between two `#RRGGBB` strings; `1.0` (no contrast at all) for
/// anything unparseable, so a malformed column fails closed onto the fallback.
fn contrast_between(a: &str, b: &str) -> f64 {
    let (Some(a), Some(b)) = (branding::parse_hex(a), branding::parse_hex(b)) else {
        return 1.0;
    };
    branding::contrast(
        branding::luminance(a.0, a.1, a.2),
        branding::luminance(b.0, b.1, b.2),
    )
}

/// Fit an uploaded logo to the mark slot, or refuse it.
///
/// Refusing is the interesting half. Everything else here is arithmetic; the
/// [`MIN_LOGO_PX`] gate is the decision, and it is made once at preparation
/// time so the layout only ever sees a logo it is safe to print.
///
/// The tint is conditional on `is_mark` for a reason that is easy to get wrong:
/// repainting every pixel one colour turns a two-colour wordmark into a
/// silhouette. It erases a logo rather than recolouring it, so only a shape
/// that [`crate::orgs::branding::is_mark`] already agreed is a single-colour
/// mark is ever touched.
pub fn prepare_logo(img: &image::DynamicImage, is_mark: bool, ink: &str) -> Option<CardLogo> {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 || w.max(h) < MIN_LOGO_PX {
        return None;
    }

    let tinted = if is_mark {
        branding::tint_mark(img, ink)
    } else {
        img.clone()
    };
    let fitted = if w.max(h) > MAX_LOGO_PX {
        tinted.resize(
            MAX_LOGO_PX,
            MAX_LOGO_PX,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        tinted
    };

    let (fw, fh) = fitted.dimensions();
    let mut buf = Cursor::new(Vec::new());
    fitted.write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some(CardLogo {
        png: buf.into_inner(),
        width: fw,
        height: fh,
    })
}
