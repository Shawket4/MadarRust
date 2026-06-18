//! Vector composition of the branded A6 QR card.
//!
//! The whole card is built as a single SVG document in **millimetres** (the
//! viewBox unit is 1 mm), then handed to [`super::render::rasterize`]. Building
//! in real units gives exact A6 output and lets the same document be returned
//! as SVG for unlimited-scale print.
//!
//! The Sufrix **mark** and **label** (wordmark) are supplied as brand assets and
//! embedded verbatim — never reconstructed in code. Each asset is parsed for its
//! own `viewBox`, then scaled-to-fit and centred on its target point, so dropping
//! in a redrawn asset of any dimensions just works.

use std::fmt::Write as _;

use super::render::Matrix;
use super::{QrCardError, QrCardOptions, CREAM, NAVY, TERRACOTTA};

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
const QR_CY: f32 = QR_TOP + QR_SIZE / 2.0; // 57.0
const QUIET: u32 = 4; // modules of quiet zone, drawn in cream

const PLAQUE_SIDE: f32 = 21.0;
const PLAQUE_RADIUS: f32 = 4.5;
const MARK_SIZE: f32 = 15.0; // ≤ 22% of QR width (15.4 mm); ≥3 mm cream clear space each side

// "Sufrix" wordmark asset, centred in the space below the QR.
const LABEL_CENTER_Y: f32 = 112.0;
const LABEL_MAX_W: f32 = 46.0;
const LABEL_MAX_H: f32 = 15.0;

// Optional caption sits just under the wordmark.
const CAPTION_BASELINE: f32 = 128.0;
const CAPTION_SIZE: f32 = 4.0;
const CAPTION_OPACITY: f32 = 0.72;

/// The Sufrix mark — embedded verbatim, the single source of truth.
const MARK_SVG: &str = include_str!("../../assets/sufrix-mark.svg");
/// The Sufrix "Sufrix" wordmark (includes the terracotta tittle) — embedded verbatim.
const LABEL_SVG: &str = include_str!("../../assets/sufrix-label.svg");

/// Compose the full card SVG for a given QR matrix + options.
pub fn build_card_svg(m: &Matrix, opts: &QrCardOptions) -> Result<String, QrCardError> {
    let b = opts.bleed_mm.clamp(0.0, 20.0);
    let canvas_w = TRIM_W + 2.0 * b;
    let canvas_h = TRIM_H + 2.0 * b;

    let mut s = String::with_capacity(16 * 1024);
    let _ = write!(
        s,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{cw}mm" height="{ch}mm" viewBox="0 0 {cw} {ch}">"#,
        cw = f(canvas_w),
        ch = f(canvas_h),
    );

    // Cream bleed background across the whole canvas (also the QR quiet zone).
    let _ = write!(
        s,
        r#"<rect x="0" y="0" width="{cw}" height="{ch}" fill="{CREAM}"/>"#,
        cw = f(canvas_w),
        ch = f(canvas_h),
    );

    // Trim-relative content, shifted into the bleed.
    let _ = write!(s, r#"<g transform="translate({b},{b})">"#, b = f(b));

    push_frame(&mut s);
    push_qr_modules(&mut s, m);
    push_centre(&mut s)?;
    push_label(&mut s)?;
    push_caption(&mut s, opts.caption.as_deref());

    s.push_str("</g>");

    if opts.crop_marks && b > 0.0 {
        push_crop_marks(&mut s, b);
    }

    s.push_str("</svg>");
    Ok(s)
}

fn push_frame(s: &mut String) {
    let _ = write!(
        s,
        r#"<rect x="{x}" y="{x}" width="{w}" height="{h}" rx="{r}" ry="{r}" fill="none" stroke="{NAVY}" stroke-width="{sw}"/>"#,
        x = f(FRAME_INSET),
        w = f(TRIM_W - 2.0 * FRAME_INSET),
        h = f(TRIM_H - 2.0 * FRAME_INSET),
        r = f(FRAME_RADIUS),
        sw = f(FRAME_STROKE),
    );
}

/// Dark modules as grid-snapped navy rects. One module =
/// `70 mm / (matrix + 2*quiet)`; the quiet zone is left as cream background.
fn push_qr_modules(s: &mut String, m: &Matrix) {
    let n = m.size as u32;
    let module = QR_SIZE / (n + 2 * QUIET) as f32;
    s.push_str(r#"<g fill=""#);
    s.push_str(NAVY);
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

/// Centre cream plaque + embedded mark (drawn over the QR centre; ECC High
/// recovers the obscured ~9% of module area).
fn push_centre(s: &mut String) -> Result<(), QrCardError> {
    let px = QR_CX - PLAQUE_SIDE / 2.0;
    let py = QR_CY - PLAQUE_SIDE / 2.0;
    let _ = write!(
        s,
        r#"<rect x="{x}" y="{y}" width="{side}" height="{side}" rx="{r}" ry="{r}" fill="{CREAM}"/>"#,
        x = f(px),
        y = f(py),
        side = f(PLAQUE_SIDE),
        r = f(PLAQUE_RADIUS),
    );
    s.push_str(&embed_asset(MARK_SVG, QR_CX, QR_CY, MARK_SIZE, MARK_SIZE)?);
    Ok(())
}

/// The "Sufrix" wordmark, embedded from the brand asset (not font-rendered).
/// The asset already carries the terracotta tittle.
fn push_label(s: &mut String) -> Result<(), QrCardError> {
    s.push_str(&embed_asset(
        LABEL_SVG,
        QR_CX,
        LABEL_CENTER_Y,
        LABEL_MAX_W,
        LABEL_MAX_H,
    )?);
    Ok(())
}

fn push_caption(s: &mut String, caption: Option<&str>) {
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
        r#"<text x="{cx}" y="{y}" font-family="{family}" font-weight="500" font-size="{fs}" fill="{NAVY}" fill-opacity="{op}" text-anchor="middle"{dir}>{t}</text>"#,
        cx = f(QR_CX),
        y = f(CAPTION_BASELINE),
        fs = f(CAPTION_SIZE),
        op = f(CAPTION_OPACITY),
        t = xml_escape(text),
    );
}

/// Thin navy hairlines at the four trim corners, living only in the bleed
/// margin (never crossing into the trim area).
fn push_crop_marks(s: &mut String, b: f32) {
    let len = b * 0.8;
    let hair = 0.15_f32;
    let xs = [b, b + TRIM_W];
    let ys = [b, b + TRIM_H];
    s.push_str(r#"<g stroke=""#);
    s.push_str(NAVY);
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
fn embed_asset(
    asset: &str,
    cx: f32,
    cy: f32,
    max_w: f32,
    max_h: f32,
) -> Result<String, QrCardError> {
    let (vx, vy, vw, vh) = parse_viewbox(asset)?;
    if vw <= 0.0 || vh <= 0.0 {
        return Err(QrCardError::SvgParse("asset viewBox has zero size".into()));
    }
    let scale = (max_w / vw).min(max_h / vh);
    let tx = cx - scale * (vx + vw / 2.0);
    let ty = cy - scale * (vy + vh / 2.0);
    let inner = inline_brand_classes(extract_svg_inner(asset)?);
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

/// Replace the brand CSS classes used by the assets with inline fills.
fn inline_brand_classes(svg: &str) -> String {
    svg.replace(r#"class="cls-1""#, &format!(r#"fill="{NAVY}""#))
        .replace(r#"class="cls-2""#, &format!(r#"fill="{TERRACOTTA}""#))
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
        '\u{FE70}'..='\u{FEFF}')  // Arabic Presentation Forms-B
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
