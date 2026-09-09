//! Brand colours read out of an organisation's logo.
//!
//! A shop uploads one logo and gets a themed loyalty card — no colour pickers,
//! nothing else to configure, and no way to choose two colours nobody can read.
//! The palette is DERIVED, so it cannot be set wrong.
//!
//! The maths here is pure and unit-tested. Deriving a palette means decoding an
//! image, which has no business happening while a customer waits for their card,
//! so the result is cached on the organisation row against the logo it came
//! from — computed once at upload, and healed on first read for a logo that
//! predates the column ([`load`]).

use image::GenericImageView;

/// Madar's own, used when there is no logo or nothing usable in it.
pub const MADAR_TEAL: &str = "#0D6273";
pub const MADAR_TEAL_LIGHT: &str = "#2E94A6";
pub const MADAR_PAPER: &str = "#EFF3F4";
/// Ink for a light ground.
pub const MADAR_INK: &str = "#12222A";

/// The three colours a card is painted with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    /// The card's ground — the logo's dominant colour.
    pub background: String,
    /// Text on that ground. Chosen for contrast, never sampled.
    pub foreground: String,
    /// Filled steps and accents.
    pub accent: String,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            background: MADAR_TEAL.into(),
            foreground: MADAR_PAPER.into(),
            accent: MADAR_TEAL_LIGHT.into(),
        }
    }
}

pub fn hex(r: u8, g: u8, b: u8) -> String {
    format!("#{r:02X}{g:02X}{b:02X}")
}

/// Relative luminance, per WCAG 2.1.
pub fn luminance(r: u8, g: u8, b: u8) -> f64 {
    let f = |c: u8| {
        let c = c as f64 / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
}

/// Contrast ratio between two luminances, per WCAG. 1.0 = identical, 21.0 = max.
pub fn contrast(a: f64, b: f64) -> f64 {
    let (hi, lo) = if a > b { (a, b) } else { (b, a) };
    (hi + 0.05) / (lo + 0.05)
}

/// Text that is actually readable on `(r,g,b)`.
///
/// Picked by measurement, not by taste: whichever of ink or paper has the
/// better contrast wins. A sampled second colour from the logo would look
/// considered and be unreadable about half the time.
pub fn readable_on(r: u8, g: u8, b: u8) -> String {
    let bg = luminance(r, g, b);
    let ink = luminance(0x12, 0x22, 0x2A);
    let paper = luminance(0xEF, 0xF3, 0xF4);
    if contrast(bg, ink) >= contrast(bg, paper) {
        MADAR_INK.into()
    } else {
        MADAR_PAPER.into()
    }
}

/// The AA floor for body text. A brand colour is not worth a card nobody can
/// read off a phone in daylight.
pub const MIN_CONTRAST: f64 = 4.5;

/// Move a ground until its best text colour clears [`MIN_CONTRAST`], keeping the
/// hue.
///
/// Some perfectly ordinary brand colours — a vivid blue like `#0066FF` — clear
/// AA against NEITHER dark ink nor light paper: 4.33:1 either way. Picking the
/// better of the two is not enough, so the ground is darkened or lightened a
/// step at a time, in the direction that already had more headroom, until the
/// text clears. The shop still gets their colour; the customer still gets a
/// card they can read.
fn ensure_readable(r: u8, g: u8, b: u8) -> (u8, u8, u8, String) {
    let ink = luminance(0x12, 0x22, 0x2A);
    let paper = luminance(0xEF, 0xF3, 0xF4);
    let (mut r, mut g, mut b) = (r, g, b);

    // Whichever direction has more room to begin with is the one to commit to;
    // alternating would oscillate without converging.
    let go_darker = {
        let l = luminance(r, g, b);
        contrast(l, paper) >= contrast(l, ink)
    };

    // 24 steps is far more than enough to cross the range; the bound is here so
    // a pathological input cannot spin.
    for _ in 0..24 {
        let l = luminance(r, g, b);
        let best = contrast(l, ink).max(contrast(l, paper));
        if best >= MIN_CONTRAST {
            break;
        }
        let f = |c: u8| {
            if go_darker {
                (c as f64 * 0.90).round() as u8
            } else {
                (c as f64 + (255.0 - c as f64) * 0.10).round() as u8
            }
        };
        let (nr, ng, nb) = (f(r), f(g), f(b));
        if (nr, ng, nb) == (r, g, b) {
            break; // already at an extreme
        }
        (r, g, b) = (nr, ng, nb);
    }
    let fg = readable_on(r, g, b);
    (r, g, b, fg)
}

/// Nudge a colour toward the light or dark end, for the accent.
fn shift(r: u8, g: u8, b: u8, lighter: bool) -> (u8, u8, u8) {
    let f = |c: u8| {
        if lighter {
            (c as f64 + (255.0 - c as f64) * 0.42).round() as u8
        } else {
            (c as f64 * 0.62).round() as u8
        }
    };
    (f(r), f(g), f(b))
}

/// The dominant colour of an already-decoded image, as a palette.
///
/// Deliberately ignores three things, in this order:
///   * **transparent pixels** — most logos are a mark on nothing, and averaging
///     in the empty space returns grey every time;
///   * **near-white and near-black** — the paper a mark sits on and its outline
///     are not the brand;
///   * **near-grey** — a shadow or a border, which would beat a small vivid mark
///     on count alone.
///
/// Returns `None` when nothing survives, so the caller falls back rather than
/// painting a card in whatever grey was left.
pub fn palette_from_image(img: &image::DynamicImage) -> Option<Palette> {
    // Coarse buckets: exact-colour counting loses to anti-aliasing, where every
    // pixel of a flat logo is a slightly different value.
    const BUCKET: u32 = 24;
    /// A bucket's key: the quantised colour.
    type Bucket = (u32, u32, u32);
    /// What we accumulate per bucket: how many pixels, and their channel sums,
    /// so the winner can be the bucket's MEAN rather than its corner.
    type Tally = (u64, u64, u64, u64);
    let mut counts: std::collections::HashMap<Bucket, Tally> = std::collections::HashMap::new();

    // Downscaled with NEAREST, not a smoothing filter: interpolation invents
    // colours that are in no part of the logo, and a dominant-colour search
    // should only ever see pixels the designer actually put there. Also bounds
    // the work regardless of what was uploaded.
    let (w, h) = img.dimensions();
    let small = if w > 96 || h > 96 {
        img.resize_exact(
            w.clamp(1, 96),
            h.clamp(1, 96),
            image::imageops::FilterType::Nearest,
        )
    } else {
        img.clone()
    };
    for (_, _, px) in small.pixels() {
        let [r, g, b, a] = px.0;
        if a < 128 {
            continue;
        }
        let (rf, gf, bf) = (r as f64, g as f64, b as f64);
        let max = rf.max(gf).max(bf);
        let min = rf.min(gf).min(bf);
        // Near-white / near-black.
        if max > 240.0 && min > 240.0 {
            continue;
        }
        if max < 26.0 {
            continue;
        }
        // Near-grey: little separation between channels means no hue to take.
        if max - min < 18.0 {
            continue;
        }
        let key = (r as u32 / BUCKET, g as u32 / BUCKET, b as u32 / BUCKET);
        let e = counts.entry(key).or_insert((0, 0, 0, 0));
        e.0 += r as u64;
        e.1 += g as u64;
        e.2 += b as u64;
        e.3 += 1;
    }

    let (_, (sr, sg, sb, n)) = counts.into_iter().max_by_key(|(_, v)| v.3)?;
    if n == 0 {
        return None;
    }
    // The bucket's mean, not its midpoint — truer to the actual mark. Rounded,
    // not truncated: truncation biases every channel down, which is how a
    // #0D6273 logo produced a #0D6272 card.
    let mean = |sum: u64| (sum as f64 / n as f64).round() as u8;
    let (r, g, b) = (mean(sr), mean(sg), mean(sb));

    // The logo's colour, moved only as far as legibility requires.
    let (r, g, b, foreground) = ensure_readable(r, g, b);
    let dark_ground = luminance(r, g, b) < 0.5;
    let (ar, ag, ab) = shift(r, g, b, dark_ground);
    Some(Palette {
        background: hex(r, g, b),
        foreground,
        accent: hex(ar, ag, ab),
    })
}

/// Is this logo a MARK — a shape on transparency — or an opaque tile?
///
/// It decides how the logo may be drawn, and the two answers are opposites.
///
/// A mark can be recoloured: paint every opaque pixel in the card's foreground
/// and it becomes a silhouette that is legible on the card BY CONSTRUCTION,
/// because the foreground is the one colour already guaranteed to clear AA on
/// that ground. This is what a card wants, since the ground is derived from the
/// logo's own dominant colour — so a logo drawn in its own colours is, almost
/// by definition, the colour it is sitting on. Blue on blue.
///
/// A logo with its background baked in cannot be recoloured: every pixel is
/// opaque, so the silhouette is a solid rectangle. That one gets a plate to sit
/// on instead.
///
/// Two things have to be true, and for a long time this asked only one of them.
///
///  * **A transparent frame.** A mark leaves a lot of the image empty; a photo
///    or a baked tile leaves almost none. Measured over the whole image rather
///    than guessed at from the corners.
///  * **One colour.** This is the half that was missing, and it is the half that
///    matters, because recolouring is destructive: `tint_mark` overwrites every
///    pixel and keeps only the alpha. On a silhouette that changes its colour;
///    on a full-colour logo it DELETES the logo and leaves a flat white shape.
///    Nearly every logo anyone uploads is drawn on transparency, so asking only
///    about the frame classified nearly every logo as repaintable — and that is
///    exactly what shops saw on their passes.
///
/// Colour is judged over strongly-opaque pixels only: a soft edge is a blend
/// with the background and says nothing about the artwork. When the interior
/// spans more than a hair's width of any channel, the logo is drawn as
/// uploaded, and a contrast problem is solved with a plate instead — which
/// costs a plate, where repainting costs the logo. In doubt, don't repaint.
pub fn is_mark(img: &image::DynamicImage) -> bool {
    const CLEAR: u8 = 16;
    /// A mark's frame is mostly empty. A tile's is not remotely.
    const ENOUGH: f64 = 0.10;
    /// Ignore anti-aliased edges: they are blends, not the artist's colour.
    const SOLID: u8 = 250;
    /// How far one colour may wander and still be one colour. Wide enough for
    /// compression noise in a flat fill, far too narrow for a second hue.
    const SPREAD: u8 = 24;

    let rgba = img.to_rgba8();
    let total = (rgba.width() as u64) * (rgba.height() as u64);
    if total == 0 {
        return false;
    }
    let clear = rgba.pixels().filter(|p| p.0[3] < CLEAR).count() as f64;
    if clear / total as f64 <= ENOUGH {
        return false;
    }

    let mut lo = [u8::MAX; 3];
    let mut hi = [u8::MIN; 3];
    let mut seen = false;
    for p in rgba.pixels().filter(|p| p.0[3] >= SOLID) {
        seen = true;
        for c in 0..3 {
            lo[c] = lo[c].min(p.0[c]);
            hi[c] = hi[c].max(p.0[c]);
        }
    }
    // Nothing solid at all: an all-edge wisp of a logo. Not something to repaint
    // on the strength of no evidence.
    seen && (0..3).all(|c| hi[c].saturating_sub(lo[c]) <= SPREAD)
}

/// Repaint a mark in one colour, keeping its alpha.
///
/// Alpha is what carries the shape, so anti-aliased edges survive and the
/// result reads as the same logo rather than a traced one. Only ever called on
/// something [`is_mark`] agreed to.
pub fn tint_mark(img: &image::DynamicImage, hex: &str) -> image::DynamicImage {
    let (r, g, b) = parse_hex(hex).unwrap_or((255, 255, 255));
    let mut rgba = img.to_rgba8();
    for p in rgba.pixels_mut() {
        p.0 = [r, g, b, p.0[3]];
    }
    image::DynamicImage::ImageRgba8(rgba)
}

/// `#RRGGBB` to its channels.
pub fn parse_hex(hex: &str) -> Option<(u8, u8, u8)> {
    if hex.len() != 7 || !hex.starts_with('#') {
        return None;
    }
    let c = |a: usize, b: usize| u8::from_str_radix(&hex[a..b], 16).ok();
    Some((c(1, 3)?, c(3, 5)?, c(5, 7)?))
}

/// Who a shop is, everywhere a customer sees them.
///
/// One row, one loader. The web card, the Apple pass and the Google pass all
/// paint the same organisation, and each having its own copy of this query is
/// how they came to disagree — the pass kept reading colours out of
/// `loyalty_settings` long after the UI for them was removed, so every pass came
/// back Apple's default grey while the web card was correctly themed.
#[derive(Debug, Clone, Default)]
pub struct OrgBrand {
    /// The organisation's name, as it appears in a customer's wallet list.
    /// Empty when the org has somehow gone missing, which callers fall back on.
    pub name: String,
    /// Absolute URL of the logo, for the surfaces that FETCH an image (the web
    /// card, Google Wallet). Apple embeds bytes instead — see
    /// `loyalty::wallet::apple::pass_brand`.
    pub logo_url: Option<String>,
    /// A wide photograph for the card — Apple's strip, Google's hero image.
    ///
    /// Absent is the normal case and is a finished card, not a broken one: the
    /// pass simply has no band, which is what every card looked like until now.
    pub card_image_url: Option<String>,
    /// Derived from the logo when it was uploaded; Madar's own until then, and
    /// Madar's own regardless when the org is not on the branding tier.
    pub palette: Palette,
    /// This organisation may wear its own mark and colours.
    ///
    /// A paid tier, set by a super admin. When it is off, `logo_url` and
    /// `palette` are ALREADY Madar's — the gate is applied in [`load`] rather
    /// than left to each caller, because "remember to check the flag" across a
    /// web card, a signup page and two wallet passes is a rule that gets missed
    /// exactly once and then ships a shop's colours to a tier it did not buy.
    pub custom_branding: bool,
    /// True when [`is_mark`] says the logo can be recoloured for contrast.
    /// False for an opaque tile, and for no logo at all.
    pub logo_is_mark: bool,
    /// Where else the shop can be found — Instagram, a website, and so on.
    ///
    /// NOT gated on the branding tier, and deliberately so. A shop's Instagram
    /// is a fact about the shop in the way its name is, not decoration it is
    /// buying; a customer holding a card is better off able to find them either
    /// way. What the tier sells is looking like yourself, not existing.
    pub social_links: Vec<super::social::SocialLink>,
}

/// Read an organisation's brand.
///
/// A missing organisation, or a row whose colours predate the palette work,
/// yields Madar's own rather than nothing: an unbranded card is a fallback, a
/// blank one is a bug.
pub async fn load(pool: &sqlx::PgPool, org_id: uuid::Uuid) -> Result<OrgBrand, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct Row {
        name: String,
        logo_url: Option<String>,
        brand_background: Option<String>,
        brand_foreground: Option<String>,
        brand_accent: Option<String>,
        brand_logo_is_mark: Option<bool>,
        brand_card_image: Option<String>,
        custom_branding: bool,
        social_links: serde_json::Value,
    }
    let row: Option<Row> = sqlx::query_as(
        "SELECT name, logo_url, brand_background, brand_foreground, brand_accent, \
                brand_logo_is_mark, brand_card_image, custom_branding, social_links \
           FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    let Some(mut row) = row else {
        return Ok(OrgBrand::default());
    };

    // The tier gate, applied once and here. The shop's NAME is always its own —
    // a card that does not say whose it is helps nobody, and the name is not
    // what anyone is paying for.
    let social_links = super::social::links_of(&row.social_links);

    if !row.custom_branding {
        return Ok(OrgBrand {
            name: row.name,
            custom_branding: false,
            social_links,
            ..OrgBrand::default()
        });
    }

    // Same healing for the PALETTE. These columns arrived after some logos did,
    // and a shop whose row predates them was silently wearing Madar's colours
    // on the tier it had paid for — falling back is right for a shop with no
    // logo and wrong for one whose logo we simply never looked at.
    if row.brand_background.is_none()
        && let Some(url) = row.logo_url.as_deref()
        && let Some(p) = read_logo(url).and_then(|img| palette_from_image(&img))
    {
        let _ = sqlx::query(
            "UPDATE organizations \
                SET brand_background = $2, brand_foreground = $3, brand_accent = $4 \
              WHERE id = $1 AND brand_background IS NULL",
        )
        .bind(org_id)
        .bind(&p.background)
        .bind(&p.foreground)
        .bind(&p.accent)
        .execute(pool)
        .await;
        row.brand_background = Some(p.background);
        row.brand_foreground = Some(p.foreground);
        row.brand_accent = Some(p.accent);
    }

    // Healed on first read, not backfilled by an operator. The column arrived
    // after these logos did, and a shop should not have to re-upload its mark
    // to get a card that reads. One decode per organisation, ever.
    let logo_is_mark = match (row.brand_logo_is_mark, row.logo_url.as_deref()) {
        (Some(known), _) => known,
        (None, Some(url)) => {
            let mark = read_logo(url).map(|img| is_mark(&img)).unwrap_or(false);
            let _ = sqlx::query("UPDATE organizations SET brand_logo_is_mark = $2 WHERE id = $1")
                .bind(org_id)
                .bind(mark)
                .execute(pool)
                .await;
            mark
        }
        (None, None) => false,
    };

    let d = Palette::default();
    Ok(OrgBrand {
        name: row.name,
        logo_url: row.logo_url,
        card_image_url: row.brand_card_image,
        palette: Palette {
            background: row.brand_background.unwrap_or(d.background),
            foreground: row.brand_foreground.unwrap_or(d.foreground),
            accent: row.brand_accent.unwrap_or(d.accent),
        },
        logo_is_mark,
        custom_branding: true,
        social_links,
    })
}

/// Decode an organisation's logo from the uploads directory.
///
/// From DISK, never fetched: the file is already local, so there is no network
/// call while a customer waits and no server-side request to an address someone
/// else supplied. Returns `None` for anything unreadable, which every caller
/// treats as "no logo" rather than as an error.
pub fn read_logo(url: &str) -> Option<image::DynamicImage> {
    read_upload(url)
}

/// A cache key for an uploaded file, from its name.
///
/// Google caches an image by its URL and will not re-fetch one it has seen. A
/// shop that swaps its logo would keep the old one on every card forever, so
/// the URL has to change when the file does — and the upload already mints a
/// fresh uuid per file, which is exactly that.
pub fn asset_key(url: &str) -> String {
    url.rsplit('/')
        .next()
        .and_then(|f| f.split('.').next())
        .filter(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .unwrap_or("v1")
        .to_string()
}

/// The shop's mark as a square, opaque badge.
///
/// Google masks a programme logo to a CIRCLE and we cannot change that — but we
/// can decide what is inside it. Handed a raw upload it produces a pale sticker
/// on a coloured card: a wide wordmark gets its ends cut off, and a transparent
/// mark gets whatever backing Google chooses to put behind it.
///
/// So the badge is composed here instead: the shop's own ground, the mark
/// tinted to read on it, centred inside the circle's safe area. Fully opaque,
/// which sidesteps the question of what Google does with transparency.
pub fn logo_badge(brand: &OrgBrand, size: u32) -> Option<image::DynamicImage> {
    let logo = brand.logo_url.as_deref().and_then(read_upload)?;
    let (br, bg, bb) = parse_hex(&brand.palette.background).unwrap_or((13, 98, 115));
    let mut canvas = image::RgbaImage::from_pixel(size, size, image::Rgba([br, bg, bb, 255]));

    // A circle's inscribed square is about 0.707 of its diameter, so anything
    // inside 70% of the canvas survives the mask whatever shape it is.
    let inner = (size as f64 * 0.70) as u32;
    let logo = if brand.logo_is_mark {
        tint_mark(&logo, &brand.palette.foreground)
    } else {
        logo
    };
    let fitted = logo.resize(inner, inner, image::imageops::FilterType::Lanczos3);
    let x = ((size - fitted.width()) / 2) as i64;
    let y = ((size - fitted.height()) / 2) as i64;
    image::imageops::overlay(&mut canvas, &fitted.to_rgba8(), x, y);
    Some(image::DynamicImage::ImageRgba8(canvas))
}

/// The shop's photograph, cropped to a banner.
///
/// Cover-cropped, because a band with bars down the sides looks like a mistake
/// and the middle of a photograph is where the subject is. Handed the raw
/// upload instead, Google renders a portrait photograph at full width and it
/// swallows half the card.
pub fn card_banner(brand: &OrgBrand, w: u32, h: u32) -> Option<image::DynamicImage> {
    let img = brand.card_image_url.as_deref().and_then(read_upload)?;
    Some(img.resize_to_fill(w, h, image::imageops::FilterType::Lanczos3))
}

/// Decode an uploaded image from the uploads directory.
///
/// From DISK, never fetched: the file is already local, so there is no network
/// call while a customer waits and no server-side request to an address someone
/// else supplied. Returns `None` for anything unreadable, which every caller
/// treats as "no image" rather than as an error.
///
/// Only the last two segments of the URL are used — the subdirectory and the
/// file — and neither may climb out of the uploads directory.
pub fn read_upload(url: &str) -> Option<image::DynamicImage> {
    let (rest, file) = url.rsplit_once('/')?;
    let sub = rest.rsplit_once('/').map(|(_, s)| s).unwrap_or(rest);
    let unsafe_part = |s: &str| s.is_empty() || s.contains("..") || s.contains('\\');
    if unsafe_part(sub) || unsafe_part(file) {
        return None;
    }
    let dir = std::env::var("UPLOADS_DIR").unwrap_or_else(|_| "./uploads".into());
    let bytes = std::fs::read(format!("{dir}/{sub}/{file}")).ok()?;
    image::load_from_memory(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, Rgba, RgbaImage};

    fn solid(w: u32, h: u32, px: [u8; 4]) -> DynamicImage {
        let mut img = RgbaImage::new(w, h);
        for p in img.pixels_mut() {
            *p = Rgba(px);
        }
        DynamicImage::ImageRgba8(img)
    }

    /// Draw `shapes` (x range, y range, colour) onto a transparent field.
    fn on_transparency(size: u32, shapes: &[((u32, u32), (u32, u32), [u8; 4])]) -> DynamicImage {
        let mut img = RgbaImage::new(size, size);
        for p in img.pixels_mut() {
            *p = Rgba([0, 0, 0, 0]);
        }
        for ((x0, x1), (y0, y1), px) in shapes {
            for y in *y0..*y1 {
                for x in *x0..*x1 {
                    img.put_pixel(x, y, Rgba(*px));
                }
            }
        }
        DynamicImage::ImageRgba8(img)
    }

    /// The bug shops actually saw: their logo came back a flat white shape.
    ///
    /// Being drawn on transparency was taken as permission to repaint, and
    /// almost every uploaded logo is drawn on transparency. Repainting is
    /// destructive — it keeps the alpha and throws the colours away — so the
    /// question is not "does this have a transparent frame" but "is there only
    /// one colour here to lose".
    #[test]
    fn a_two_colour_logo_is_never_repainted() {
        let img = on_transparency(
            64,
            &[
                ((16, 32), (16, 48), [200, 32, 40, 255]),
                ((32, 48), (16, 48), [20, 40, 190, 255]),
            ],
        );
        assert!(
            !is_mark(&img),
            "red beside blue is not a silhouette; repainting it would erase the logo"
        );
    }

    #[test]
    fn a_single_colour_silhouette_may_be_repainted() {
        let img = on_transparency(64, &[((16, 48), (16, 48), [17, 17, 17, 255])]);
        assert!(is_mark(&img), "one colour on transparency is a mark");
        // And repainting it is what a mark is for.
        let white = tint_mark(&img, "#FFFFFF").to_rgba8();
        assert_eq!(white.get_pixel(24, 24).0, [255, 255, 255, 255]);
        assert_eq!(white.get_pixel(0, 0).0[3], 0, "the empty frame stays empty");
    }

    #[test]
    fn a_baked_tile_is_not_a_mark() {
        assert!(
            !is_mark(&solid(64, 64, [13, 98, 115, 255])),
            "an opaque tile has no silhouette to draw"
        );
    }

    #[test]
    fn a_flat_fill_survives_its_own_compression_noise() {
        // A PNG round-trip leaves a flat fill a shade off in places. That is
        // still one colour, and a mark it must remain.
        let mut img = on_transparency(64, &[((16, 48), (16, 48), [17, 17, 17, 255])]).to_rgba8();
        img.put_pixel(20, 20, Rgba([21, 15, 19, 255]));
        img.put_pixel(30, 30, Rgba([14, 20, 16, 255]));
        assert!(is_mark(&DynamicImage::ImageRgba8(img)));
    }

    #[test]
    fn a_solid_mark_gives_its_own_colour() {
        let p = palette_from_image(&solid(64, 64, [13, 98, 115, 255])).unwrap();
        assert_eq!(p.background, "#0D6273");
        // Dark ground → light text.
        assert_eq!(p.foreground, MADAR_PAPER);
    }

    #[test]
    fn text_is_chosen_for_contrast_not_taste() {
        // A pale yellow logo must NOT get white text.
        let p = palette_from_image(&solid(32, 32, [250, 224, 96, 255])).unwrap();
        assert_eq!(p.foreground, MADAR_INK, "light ground needs dark ink");
        let p = palette_from_image(&solid(32, 32, [20, 30, 90, 255])).unwrap();
        assert_eq!(p.foreground, MADAR_PAPER, "dark ground needs light text");
    }

    #[test]
    fn transparency_is_ignored_so_a_mark_on_nothing_still_reads() {
        // A small red mark on a fully transparent field: the mark is the brand,
        // and averaging in the empty space would return grey.
        let mut img = RgbaImage::new(64, 64);
        for p in img.pixels_mut() {
            *p = Rgba([0, 0, 0, 0]);
        }
        for y in 20..44 {
            for x in 20..44 {
                img.put_pixel(x, y, Rgba([200, 32, 40, 255]));
            }
        }
        let p = palette_from_image(&DynamicImage::ImageRgba8(img)).unwrap();
        let r = u8::from_str_radix(&p.background[1..3], 16).unwrap();
        assert!(r > 150, "the mark's red should win, got {}", p.background);
    }

    #[test]
    fn a_logo_with_no_colour_falls_back_rather_than_painting_it_grey() {
        // Pure black-and-white marks are common, and a grey card is worse than
        // the Madar one.
        assert!(palette_from_image(&solid(32, 32, [255, 255, 255, 255])).is_none());
        assert!(palette_from_image(&solid(32, 32, [10, 10, 10, 255])).is_none());
        assert!(palette_from_image(&solid(32, 32, [128, 128, 128, 255])).is_none());
        // And the default is Madar's, never something derived from nothing.
        assert_eq!(Palette::default().background, MADAR_TEAL);
    }

    #[test]
    fn an_empty_image_does_not_panic() {
        assert!(palette_from_image(&solid(1, 1, [0, 0, 0, 0])).is_none());
    }

    #[test]
    fn every_derived_palette_is_actually_readable() {
        // The point of deriving rather than letting someone pick: the text must
        // clear the AA floor on whatever ground the logo produced. Swept across
        // the colour cube so this holds for logos nobody has uploaded yet.
        for r in (0u16..=255).step_by(51) {
            for g in (0u16..=255).step_by(51) {
                for b in (0u16..=255).step_by(51) {
                    let (r, g, b) = (r as u8, g as u8, b as u8);
                    let Some(p) = palette_from_image(&solid(8, 8, [r, g, b, 255])) else {
                        continue; // greys and extremes fall back by design
                    };
                    // Measured against the palette's OWN background, which is
                    // the contract: `palette_from_image` may move the ground to
                    // reach legibility, and what ships is what must be read.
                    let parse = |h: &str, i: usize| {
                        u8::from_str_radix(&h[1 + i * 2..3 + i * 2], 16).unwrap()
                    };
                    let (br, bg_, bb) = (
                        parse(&p.background, 0),
                        parse(&p.background, 1),
                        parse(&p.background, 2),
                    );
                    let fg = if p.foreground == MADAR_INK {
                        (0x12u8, 0x22u8, 0x2Au8)
                    } else {
                        (0xEFu8, 0xF3u8, 0xF4u8)
                    };
                    let ratio = {
                        let a = luminance(br, bg_, bb);
                        let c = luminance(fg.0, fg.1, fg.2);
                        let (hi, lo) = if a > c { (a, c) } else { (c, a) };
                        (hi + 0.05) / (lo + 0.05)
                    };
                    assert!(
                        ratio >= 4.5,
                        "logo #{r:02X}{g:02X}{b:02X} -> card {} on {} is only {ratio:.2}:1",
                        p.foreground,
                        p.background
                    );
                }
            }
        }
    }
}
