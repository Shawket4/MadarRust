//! Low-level rendering primitives for the QR card:
//! QR matrix generation, the (font-loaded, system-font-free) usvg pipeline that
//! rasterises a composed SVG to PNG, and the plain receipt-QR encoder.
//!
//! These functions are pure and side-effect-free (no I/O, no globals besides a
//! read-only font cache), so the public `render_qr_card_*` API stays unit
//! testable and deterministic.

use std::sync::{Arc, OnceLock};

use qrcode::types::Color;
use qrcode::{EcLevel, QrCode};
use resvg::tiny_skia;
use resvg::usvg;

use super::QrCardError;
use super::brand::CardLogo;

/// Bundled fonts (committed under `assets/fonts/`, SIL OFL). Embedded at compile
/// time so there is no runtime filesystem dependency and no system-font path.
pub const MANROPE_SEMIBOLD: &[u8] = include_bytes!("../../assets/fonts/Manrope-SemiBold.ttf");
pub const MANROPE_MEDIUM: &[u8] = include_bytes!("../../assets/fonts/Manrope-Medium.ttf");
pub const CAIRO_MEDIUM: &[u8] = include_bytes!("../../assets/fonts/Cairo-Medium.ttf");
/// Madar's own credit line, and only that — see `layout::POWERED_FAMILY`. A
/// second family for one small line is worth it: it is the one thing on a
/// branded card that is ours rather than the shop's, and it should not be
/// wearing the same type as the shop's own name.
pub const IBM_PLEX_MEDIUM: &[u8] =
    include_bytes!("../../assets/fonts/IBMPlexSansArabic-Medium.ttf");

/// A square QR matrix as row-major dark/light booleans (`true` == dark module).
pub struct Matrix {
    pub size: usize,
    pub dark: Vec<bool>,
}

impl Matrix {
    #[inline]
    pub fn is_dark(&self, row: usize, col: usize) -> bool {
        self.dark[row * self.size + col]
    }
}

/// Encode `data` into a QR matrix at the given error-correction level.
pub fn qr_matrix(data: &str, ec: EcLevel) -> Result<Matrix, QrCardError> {
    let code = QrCode::with_error_correction_level(data.as_bytes(), ec)
        .map_err(|e| QrCardError::QrEncode(e.to_string()))?;
    Ok(Matrix {
        size: code.width(),
        dark: code
            .to_colors()
            .into_iter()
            .map(|c| c == Color::Dark)
            .collect(),
    })
}

/// Process-wide font database holding only our three bundled faces — never the
/// system fonts. Built once; cloned (cheaply, it's an `Arc`) per render.
fn fontdb() -> Arc<usvg::fontdb::Database> {
    static DB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_font_data(MANROPE_SEMIBOLD.to_vec());
        db.load_font_data(MANROPE_MEDIUM.to_vec());
        db.load_font_data(CAIRO_MEDIUM.to_vec());
        db.load_font_data(IBM_PLEX_MEDIUM.to_vec());
        Arc::new(db)
    })
    .clone()
}

/// Pixel count for a physical millimetre length at a given DPI.
/// `px(mm, dpi) = round(mm / 25.4 * dpi)`.
#[inline]
pub fn px(mm: f32, dpi: u32) -> u32 {
    (mm / 25.4 * dpi as f32).round() as u32
}

/// Rasterise a composed SVG document (authored in millimetres) to a PNG.
///
/// usvg resolves physical units at 96 DPI, so the canvas is rendered at
/// `scale = dpi/96` onto a pixmap sized exactly `px(canvas_w, dpi) ×
/// px(canvas_h, dpi)` — guaranteeing the output matches the computed pixel
/// dimensions to the pixel.
pub fn rasterize(
    svg: &str,
    canvas_w_mm: f32,
    canvas_h_mm: f32,
    dpi: u32,
) -> Result<Vec<u8>, QrCardError> {
    encode_pixmap(&rasterize_pixmap(svg, canvas_w_mm, canvas_h_mm, dpi)?)
}

/// The same render, stopping one step short of PNG.
///
/// A shop's logo has to be painted onto the card after resvg has finished with
/// it (see [`super::render_qr_card_png`]), and encoding to PNG only to decode
/// it again so something can be drawn on top would be pure waste — so the
/// pixmap is handed out and encoded once, at the end.
pub fn rasterize_pixmap(
    svg: &str,
    canvas_w_mm: f32,
    canvas_h_mm: f32,
    dpi: u32,
) -> Result<tiny_skia::Pixmap, QrCardError> {
    let mut opt = usvg::Options {
        dpi: 96.0,
        ..usvg::Options::default()
    };
    opt.fontdb = fontdb();

    let tree = usvg::Tree::from_str(svg, &opt).map_err(|e| QrCardError::SvgParse(e.to_string()))?;

    let pw = px(canvas_w_mm, dpi);
    let ph = px(canvas_h_mm, dpi);
    let mut pixmap = tiny_skia::Pixmap::new(pw, ph)
        .ok_or_else(|| QrCardError::Render("zero-size pixmap".into()))?;

    let scale = dpi as f32 / 96.0;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    Ok(pixmap)
}

pub fn encode_pixmap(pixmap: &tiny_skia::Pixmap) -> Result<Vec<u8>, QrCardError> {
    pixmap
        .encode_png()
        .map_err(|e| QrCardError::Encode(e.to_string()))
}

/// Paint a shop's logo into its slot on an already-rasterised card.
///
/// The logo is resampled to the slot's exact pixel size first and then blitted
/// at an integer offset, rather than blitted through a scaling transform. That
/// keeps the resampling in Lanczos3 — visibly better than tiny-skia's pattern
/// filtering on the kind of hard-edged mark most shops upload — and it keeps
/// the mark landing on the same pixel the SVG's `<image>` would have covered,
/// with no half-pixel drift between the two outputs.
pub fn draw_logo(
    pixmap: &mut tiny_skia::Pixmap,
    logo: &CardLogo,
    x_mm: f32,
    y_mm: f32,
    w_mm: f32,
    h_mm: f32,
    dpi: u32,
) -> Result<(), QrCardError> {
    let (tw, th) = (px(w_mm, dpi).max(1), px(h_mm, dpi).max(1));
    let resized = image::load_from_memory(&logo.png)
        .map_err(|e| QrCardError::Render(format!("logo decode failed: {e}")))?
        .resize_exact(tw, th, image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    let src = to_pixmap(&resized)
        .ok_or_else(|| QrCardError::Render("logo pixmap allocation failed".into()))?;
    pixmap.draw_pixmap(
        px(x_mm, dpi) as i32,
        px(y_mm, dpi) as i32,
        src.as_ref(),
        &tiny_skia::PixmapPaint::default(),
        tiny_skia::Transform::identity(),
        None,
    );
    Ok(())
}

/// Straight RGBA to tiny-skia's premultiplied RGBA.
///
/// tiny-skia stores colour already multiplied by alpha and rejects any pixel
/// whose channels exceed its alpha, so the conversion cannot be a memcpy. The
/// rounded product `(c * a + 127) / 255` is never greater than `a`, so
/// `from_rgba` never rejects a pixel this produces; the `?` is there because
/// the compiler cannot know that, not because a logo can fail it.
fn to_pixmap(img: &image::RgbaImage) -> Option<tiny_skia::Pixmap> {
    let mut pm = tiny_skia::Pixmap::new(img.width(), img.height())?;
    let premul = |c: u8, a: u8| ((c as u16 * a as u16 + 127) / 255) as u8;
    for (dst, src) in pm.pixels_mut().iter_mut().zip(img.pixels()) {
        let [r, g, b, a] = src.0;
        *dst = tiny_skia::PremultipliedColorU8::from_rgba(
            premul(r, a),
            premul(g, a),
            premul(b, a),
            a,
        )?;
    }
    Some(pm)
}

/// Render a plain, unbranded QR as black modules on white — the shape that
/// scans most reliably on thermal receipt printers (no centre overlay, so a
/// lower ECC level keeps the matrix compact). Output is a square PNG of
/// `(matrix + 2*quiet) * module_px` per side. Deterministic.
pub fn plain_qr_png(data: &str, module_px: u32, quiet: u32) -> Result<Vec<u8>, QrCardError> {
    let m = qr_matrix(data, EcLevel::M)?;
    let n = m.size as u32;
    let side = (n + 2 * quiet) * module_px;

    let white = image::Rgba([255, 255, 255, 255]);
    let black = image::Rgba([0, 0, 0, 255]);
    let mut img = image::RgbaImage::from_pixel(side, side, white);

    for row in 0..n {
        for col in 0..n {
            if m.is_dark(row as usize, col as usize) {
                let x0 = (col + quiet) * module_px;
                let y0 = (row + quiet) * module_px;
                for dy in 0..module_px {
                    for dx in 0..module_px {
                        img.put_pixel(x0 + dx, y0 + dy, black);
                    }
                }
            }
        }
    }

    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png)
        .map_err(|e| QrCardError::Encode(e.to_string()))?;
    Ok(buf.into_inner())
}
