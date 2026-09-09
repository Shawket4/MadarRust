//! Vector composition of the branded A6 QR card.
//!
//! The whole card is built as a single SVG document in **millimetres** (the
//! viewBox unit is 1 mm), then handed to [`super::render::rasterize`]. Building
//! in real units gives exact A6 output and lets the same document be returned
//! as SVG for unlimited-scale print.
//!
//! The Madar **mark** and **label** (wordmark) are supplied as brand assets and
//! embedded verbatim — never reconstructed in code. Each asset is parsed for its
//! own `viewBox`, then scaled-to-fit and centred on its target point, so dropping
//! in a redrawn asset of any dimensions just works.
//!
//! A shop on the branding tier gets its own colours and its own mark in those
//! same slots, supplied as a resolved [`super::brand::CardBrand`]. Nothing here
//! decides whether that is allowed or whether those colours are safe — by the
//! time a `CardBrand` exists both questions have been answered — so the whole
//! difference between the two cards is which strings get written and whether the
//! mark slot holds a vector asset or a raster one. With no `CardBrand` the
//! functions below emit precisely the bytes they emitted before shops could be
//! branded at all, which is a property the tests assert rather than hope for.

use std::fmt::Write as _;

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};

use super::brand::CardLogo;
use super::render::Matrix;
use super::{PAPER, QrCardError, QrCardOptions, TEAL, TEAL_LIGHT};

// ── A6 geometry (trim-relative mm) ──────────────────────────────────────────
const TRIM_W: f32 = 105.0;
const TRIM_H: f32 = 148.0;

const FRAME_INSET: f32 = 6.0;
const FRAME_RADIUS: f32 = 3.0; // gentler corners (was 5.0)
const FRAME_STROKE: f32 = 0.6;

const QR_SIZE: f32 = 70.0;
const QR_TOP: f32 = 24.0;
const QR_X: f32 = (TRIM_W - QR_SIZE) / 2.0; // 17.5
const QR_CX: f32 = TRIM_W / 2.0; // 52.5
const QR_CY: f32 = QR_TOP + QR_SIZE / 2.0; // 59.0
const QUIET: u32 = 4; // modules of quiet zone, drawn in cream

const PLAQUE_SIDE: f32 = 21.0;
const PLAQUE_RADIUS: f32 = 4.5;
/// The mark slot, square. `≤ 22%` of QR width (15.4 mm); `≥3` mm cream clear
/// space each side. `brand::MIN_LOGO_PX` is derived from this, so a change here
/// changes which uploads are printable.
pub(super) const MARK_SIZE: f32 = 15.0;

// "madar" wordmark asset, centred in the space below the QR.
const LABEL_CENTER_Y: f32 = 112.0;
const LABEL_MAX_W: f32 = 46.0;
const LABEL_MAX_H: f32 = 15.0;

// Optional caption sits just under the wordmark.
const CAPTION_BASELINE: f32 = 128.0;
const CAPTION_SIZE: f32 = 4.0;
const CAPTION_OPACITY: f32 = 0.72;

// ── branded footer ──────────────────────────────────────────────────────────
// A shop's name is typeset where Madar's wordmark sits, because there is no
// asset to embed for it. SVG text does not wrap, so the size is chosen to make
// the name fit rather than letting a long one run off the card: Manrope's
// average advance is close enough to 0.58 em for that, and the estimate only
// has to be good enough to pick between a handful of sizes. Past the point
// where even the smallest size fits, the name is cut — an elided name is a
// card, an overflowing one is a reprint.

// Madar's attribution on a branded card. It stays on every card either way —
// what changes is that it stops being the lockup and becomes a line of type,
// low in the frame and under the caption, where it credits without competing.
const POWERED_TEXT: &str = "Powered by Madar";
const POWERED_BASELINE: f32 = 137.0;
const POWERED_SIZE: f32 = 3.2;
const POWERED_OPACITY: f32 = 0.6;
/// The mark above the words, and the air between them.
const POWERED_MARK_H: f32 = 4.0;
const POWERED_MARK_GAP: f32 = 1.6;
/// As the font's own `name` table spells it — fontsource folds the weight into
/// the family on its subset builds, and the renderer matches on that string.
const POWERED_FAMILY: &str = "IBM Plex Sans Arabic Medium";

/// The Madar mark — embedded verbatim, the single source of truth.
const MARK_SVG: &str = include_str!("../../assets/madar-mark.svg");
/// The Madar "madar" wordmark (includes the terracotta tittle) — embedded verbatim.
const LABEL_SVG: &str = include_str!("../../assets/madar-label.svg");

/// Compose the full card SVG for a given QR matrix + options.
pub fn build_card_svg(m: &Matrix, opts: &QrCardOptions) -> Result<String, QrCardError> {
    let b = opts.bleed_mm.clamp(0.0, 20.0);
    let canvas_w = TRIM_W + 2.0 * b;
    let canvas_h = TRIM_H + 2.0 * b;

    // Madar's tokens are not a default that a brand overrides piecemeal — they
    // are what these three names resolve to when there is no brand at all, and
    // every write below goes through them so the two paths cannot diverge by
    // one forgotten literal.
    let brand = opts.brand.as_ref();
    let ground = brand.map_or(PAPER, |br| br.ground.as_str());
    let ink = brand.map_or(TEAL, |br| br.ink.as_str());
    let frame = brand.map_or(TEAL, |br| br.accent.as_str());
    // The Madar mark ships hardcoded in the two teals, and it is still the
    // fallback for a shop whose logo is missing or unprintable — so on a
    // branded card those two literals have to become the card's own pair.
    // Unbranded they map to themselves, which is what keeps the asset verbatim.
    let (mark_primary, mark_secondary) = match brand {
        Some(br) => (br.ink.as_str(), br.accent.as_str()),
        None => (TEAL, TEAL_LIGHT),
    };

    let mut s = String::with_capacity(16 * 1024);
    let _ = write!(
        s,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{cw}mm" height="{ch}mm" viewBox="0 0 {cw} {ch}">"#,
        cw = f(canvas_w),
        ch = f(canvas_h),
    );

    // Cream bleed background across the whole canvas (also the QR quiet zone).
    // The quiet zone being the card ground is why a brand's two colours have to
    // be ordered dark-on-light before they get here: there is no separate field
    // behind the QR that could be made safe on its own.
    let _ = write!(
        s,
        r#"<rect x="0" y="0" width="{cw}" height="{ch}" fill="{ground}"/>"#,
        cw = f(canvas_w),
        ch = f(canvas_h),
    );

    // Trim-relative content, shifted into the bleed.
    let _ = write!(s, r#"<g transform="translate({b},{b})">"#, b = f(b));

    push_frame(&mut s, frame);
    push_qr_modules(&mut s, m, ink);
    push_centre(
        &mut s,
        ground,
        mark_primary,
        mark_secondary,
        brand.and_then(|br| br.logo.as_ref()),
    )?;
    // Madar's wordmark on Madar's card, and nothing at all on a shop's. The
    // shop's name used to be typeset here, and it earned its place on neither
    // count: the shop's own mark is already at the top of the card, and anyone
    // holding one is standing in the shop.
    if brand.is_none() {
        push_label(&mut s)?;
    }
    push_caption(&mut s, opts.caption.as_deref(), ink);
    if brand.is_some() {
        push_powered_by(&mut s, ink)?;
    }

    s.push_str("</g>");

    if opts.crop_marks && b > 0.0 {
        push_crop_marks(&mut s, b, ink);
    }

    s.push_str("</svg>");
    Ok(s)
}

fn push_frame(s: &mut String, stroke: &str) {
    let _ = write!(
        s,
        r#"<rect x="{x}" y="{x}" width="{w}" height="{h}" rx="{r}" ry="{r}" fill="none" stroke="{stroke}" stroke-width="{sw}"/>"#,
        x = f(FRAME_INSET),
        w = f(TRIM_W - 2.0 * FRAME_INSET),
        h = f(TRIM_H - 2.0 * FRAME_INSET),
        r = f(FRAME_RADIUS),
        sw = f(FRAME_STROKE),
    );
}

/// Dark modules as grid-snapped navy rects. One module =
/// `70 mm / (matrix + 2*quiet)`; the quiet zone is left as cream background.
fn push_qr_modules(s: &mut String, m: &Matrix, ink: &str) {
    let n = m.size as u32;
    let module = QR_SIZE / (n + 2 * QUIET) as f32;
    s.push_str(r#"<g fill=""#);
    s.push_str(ink);
    s.push_str(r#"" shape-rendering="crispEdges">"#);
    for row in 0..n {
        for col in 0..n {
            if m.is_dark(row as usize, col as usize) {
                let x = QR_X + (col + QUIET) as f32 * module;
                let y = QR_TOP + (row + QUIET) as f32 * module;
                let _ = write!(
                    s,
                    r#"<rect x="{x}" y="{y}" width="{w}" height="{w}"/>"#,
                    x = f(x),
                    y = f(y),
                    w = f(module),
                );
            }
        }
    }
    s.push_str("</g>");
}

/// Centre plaque in the card ground + the mark (drawn over the QR centre; ECC
/// High recovers the obscured ~9% of module area).
///
/// The plaque is the ground colour rather than a fixed cream so that a scanner
/// still reads it as a light region on a branded card — it sits inside the
/// matrix, and a dark patch there is a hole in the code rather than a plaque.
fn push_centre(
    s: &mut String,
    ground: &str,
    mark_primary: &str,
    mark_secondary: &str,
    logo: Option<&CardLogo>,
) -> Result<(), QrCardError> {
    let px = QR_CX - PLAQUE_SIDE / 2.0;
    let py = QR_CY - PLAQUE_SIDE / 2.0;
    let _ = write!(
        s,
        r#"<rect x="{x}" y="{y}" width="{side}" height="{side}" rx="{r}" ry="{r}" fill="{ground}"/>"#,
        x = f(px),
        y = f(py),
        side = f(PLAQUE_SIDE),
        r = f(PLAQUE_RADIUS),
    );
    match logo {
        Some(l) => push_logo(s, l),
        None => s.push_str(&embed_asset(
            MARK_SVG,
            QR_CX,
            QR_CY,
            MARK_SIZE,
            MARK_SIZE,
            mark_primary,
            mark_secondary,
        )?),
    }
    Ok(())
}

/// Where a shop's logo lands in the mark slot, in trim-relative millimetres.
///
/// Shared with the raster path in [`super::render_qr_card_png`], which has to
/// paint the same logo onto the same pixels after resvg has been through the
/// document. One function, so a change to the fit cannot land in one output and
/// not the other.
pub(super) fn logo_rect_mm(logo: &CardLogo) -> (f32, f32, f32, f32) {
    let (w, h) = (logo.width.max(1) as f32, logo.height.max(1) as f32);
    let scale = (MARK_SIZE / w).min(MARK_SIZE / h);
    let (dw, dh) = (w * scale, h * scale);
    (QR_CX - dw / 2.0, QR_CY - dh / 2.0, dw, dh)
}

/// A shop's mark, as a `data:` URI inside an `<image>`.
///
/// Uploads are raster where Madar's mark is vector, so there is no markup to
/// splice in the way [`embed_asset`] does — and a file reference would make the
/// SVG depend on a path that whoever we hand it to cannot resolve, since these
/// documents are returned inline in a JSON response and printed elsewhere. The
/// element is given the exact fitted rectangle rather than a square and a
/// `preserveAspectRatio` to sort out, so a wide wordmark is not letterboxed into
/// a shape it was never drawn for; the attribute is still written, because a
/// consumer that decides to letterbox anyway should at least centre it.
fn push_logo(s: &mut String, logo: &CardLogo) {
    let (x, y, w, h) = logo_rect_mm(logo);
    let _ = write!(
        s,
        r#"<image x="{x}" y="{y}" width="{w}" height="{h}" preserveAspectRatio="xMidYMid meet" href="data:image/png;base64,{d}"/>"#,
        x = f(x),
        y = f(y),
        w = f(w),
        h = f(h),
        d = B64.encode(&logo.png),
    );
}

/// The "madar" wordmark, embedded from the brand asset (not font-rendered).
/// The asset already carries the terracotta tittle.
fn push_label(s: &mut String) -> Result<(), QrCardError> {
    s.push_str(&embed_asset(
        LABEL_SVG,
        QR_CX,
        LABEL_CENTER_Y,
        LABEL_MAX_W,
        LABEL_MAX_H,
        TEAL,
        TEAL_LIGHT,
    )?);
    Ok(())
}

/// Madar's credit on a branded card: the mark, and the words under it.
///
/// Small, low, and under the caption. It is not negotiable — the card is a
/// Madar product whoever's mark is on the front — but it is also not the point
/// of the card, and setting it at wordmark size on a shop's card would read as
/// Madar branding a shop rather than a shop being served by Madar.
///
/// The MARK rather than the wordmark, for the same reason it is small: the
/// wordmark is a claim to the card and the mark is a signature on it. Stacked
/// rather than set side by side, because centring a horizontal lockup means
/// measuring the text, and a measurement that is a shade wrong puts a symbol
/// off-centre on something a shop is about to print five hundred of.
///
/// The words are IBM Plex, which is the only place on this card that is ours
/// rather than the shop's, and reads as such against the Manrope everywhere
/// else.
fn push_powered_by(s: &mut String, ink: &str) -> Result<(), QrCardError> {
    s.push_str(&embed_asset(
        MARK_SVG,
        QR_CX,
        POWERED_BASELINE - POWERED_MARK_GAP - POWERED_MARK_H / 2.0,
        POWERED_MARK_H,
        POWERED_MARK_H,
        ink,
        ink,
    )?);
    let _ = write!(
        s,
        r#"<text x="{cx}" y="{y}" font-family="{POWERED_FAMILY}" font-size="{fs}" fill="{ink}" fill-opacity="{op}" text-anchor="middle">{POWERED_TEXT}</text>"#,
        cx = f(QR_CX),
        y = f(POWERED_BASELINE),
        fs = f(POWERED_SIZE),
        op = f(POWERED_OPACITY),
    );
    Ok(())
}

fn push_caption(s: &mut String, caption: Option<&str>, ink: &str) {
    let Some(text) = caption.map(str::trim).filter(|t| !t.is_empty()) else {
        return;
    };
    let arabic = text.chars().any(is_arabic);
    let (family, dir) = if arabic {
        ("Cairo", r#" direction="rtl""#)
    } else {
        ("Manrope", "")
    };
    let _ = write!(
        s,
        r#"<text x="{cx}" y="{y}" font-family="{family}" font-weight="500" font-size="{fs}" fill="{ink}" fill-opacity="{op}" text-anchor="middle"{dir}>{t}</text>"#,
        cx = f(QR_CX),
        y = f(CAPTION_BASELINE),
        fs = f(CAPTION_SIZE),
        op = f(CAPTION_OPACITY),
        t = xml_escape(text),
    );
}

/// Thin navy hairlines at the four trim corners, living only in the bleed
/// margin (never crossing into the trim area).
fn push_crop_marks(s: &mut String, b: f32, ink: &str) {
    let len = b * 0.8;
    let hair = 0.15_f32;
    let xs = [b, b + TRIM_W];
    let ys = [b, b + TRIM_H];
    s.push_str(r#"<g stroke=""#);
    s.push_str(ink);
    let _ = write!(s, r#"" stroke-width="{}">"#, f(hair));
    for (ci, &cx) in xs.iter().enumerate() {
        for (ri, &cy) in ys.iter().enumerate() {
            let hx = if ci == 0 { cx - len } else { cx + len };
            let vy = if ri == 0 { cy - len } else { cy + len };
            let _ = write!(
                s,
                r#"<line x1="{x1}" y1="{cy}" x2="{cx}" y2="{cy}"/><line x1="{cx}" y1="{y1}" x2="{cx}" y2="{cy}"/>"#,
                x1 = f(hx),
                cy = f(cy),
                cx = f(cx),
                y1 = f(vy),
            );
        }
    }
    s.push_str("</g>");
}

// ── asset embedding ──────────────────────────────────────────────────────────

/// Embed a brand SVG asset under a positioning `<g>`: parse its own `viewBox`,
/// scale to fit `max_w × max_h` (preserving aspect), and centre on `(cx, cy)`.
/// Brand CSS classes are inlined to fills so multiple assets can share one
/// document without `<style>`/id collisions.
///
/// `primary`/`secondary` are what Madar's two teals become. The mark is the
/// fallback for a shop with no usable logo, so on a branded card it lands on
/// the shop's plaque — and a teal mark on a gold ground is not a fallback, it
/// is two brands arguing. Passing Madar's own tokens back in is the identity
/// substitution, which is exactly what the unbranded card does.
fn embed_asset(
    asset: &str,
    cx: f32,
    cy: f32,
    max_w: f32,
    max_h: f32,
    primary: &str,
    secondary: &str,
) -> Result<String, QrCardError> {
    let (vx, vy, vw, vh) = parse_viewbox(asset)?;
    if vw <= 0.0 || vh <= 0.0 {
        return Err(QrCardError::SvgParse("asset viewBox has zero size".into()));
    }
    let scale = (max_w / vw).min(max_h / vh);
    let tx = cx - scale * (vx + vw / 2.0);
    let ty = cy - scale * (vy + vh / 2.0);
    let inner = inline_brand_classes(extract_svg_inner(asset)?, primary, secondary);
    Ok(format!(
        r#"<g transform="translate({tx},{ty}) scale({s})">{inner}</g>"#,
        tx = f(tx),
        ty = f(ty),
        s = f(scale),
    ))
}

/// Parse `viewBox="minx miny w h"` from an SVG root.
fn parse_viewbox(svg: &str) -> Result<(f32, f32, f32, f32), QrCardError> {
    const KEY: &str = "viewBox=\"";
    let i = svg
        .find(KEY)
        .ok_or_else(|| QrCardError::SvgParse("asset: no viewBox".into()))?;
    let rest = &svg[i + KEY.len()..];
    let end = rest
        .find('"')
        .ok_or_else(|| QrCardError::SvgParse("asset: unterminated viewBox".into()))?;
    let nums: Vec<f32> = rest[..end]
        .split_whitespace()
        .filter_map(|t| t.parse().ok())
        .collect();
    if nums.len() != 4 {
        return Err(QrCardError::SvgParse("asset: malformed viewBox".into()));
    }
    Ok((nums[0], nums[1], nums[2], nums[3]))
}

/// Recolour an asset to the card's palette: Madar's two teals wherever the
/// asset writes them literally, then the brand CSS classes resolved to inline
/// fills so several assets can share one document without `<style>`/id
/// collisions.
///
/// The literal swap runs FIRST, and in one pass. Run after the class
/// substitution it would re-examine the colours it had itself just inserted,
/// and a shop whose ink happened to be one of Madar's teals would have it
/// swapped a second time into the other one.
fn inline_brand_classes(svg: &str, primary: &str, secondary: &str) -> String {
    swap_brand_tokens(svg, primary, secondary)
        .replace(r#"class="cls-1""#, &format!(r#"fill="{primary}""#))
        .replace(r#"class="cls-2""#, &format!(r#"fill="{secondary}""#))
}

/// One left-to-right pass swapping `TEAL` for `primary` and `TEAL_LIGHT` for
/// `secondary`, so neither replacement can ever see the other's output.
fn swap_brand_tokens(svg: &str, primary: &str, secondary: &str) -> String {
    let mut out = String::with_capacity(svg.len());
    let mut rest = svg;
    while !rest.is_empty() {
        if let Some(tail) = rest.strip_prefix(TEAL) {
            out.push_str(primary);
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix(TEAL_LIGHT) {
            out.push_str(secondary);
            rest = tail;
        } else {
            let c = rest.chars().next().expect("non-empty remainder");
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Format an f32 for SVG with fixed precision (deterministic output) and no
/// trailing-zero noise.
fn f(v: f32) -> String {
    let mut out = format!("{v:.4}");
    if out.contains('.') {
        while out.ends_with('0') {
            out.pop();
        }
        if out.ends_with('.') {
            out.pop();
        }
    }
    if out == "-0" {
        out = "0".to_string();
    }
    out
}

fn is_arabic(c: char) -> bool {
    matches!(c,
        '\u{0600}'..='\u{06FF}' | // Arabic
        '\u{0750}'..='\u{077F}' | // Arabic Supplement
        '\u{08A0}'..='\u{08FF}' | // Arabic Extended-A
        '\u{FB50}'..='\u{FDFF}' | // Arabic Presentation Forms-A
        '\u{FE70}'..='\u{FEFF}') // Arabic Presentation Forms-B
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Return the child markup of an `<svg>…</svg>` document (drops the outer tag,
/// the XML prolog, and any leading comment) so it can be embedded under a `<g>`.
fn extract_svg_inner(svg: &str) -> Result<&str, QrCardError> {
    let open = svg
        .find("<svg")
        .ok_or_else(|| QrCardError::SvgParse("asset: no <svg> root".into()))?;
    let gt = svg[open..]
        .find('>')
        .map(|i| open + i + 1)
        .ok_or_else(|| QrCardError::SvgParse("asset: unterminated <svg>".into()))?;
    let close = svg
        .rfind("</svg>")
        .ok_or_else(|| QrCardError::SvgParse("asset: no </svg>".into()))?;
    if close < gt {
        return Err(QrCardError::SvgParse("asset: malformed".into()));
    }
    Ok(&svg[gt..close])
}
