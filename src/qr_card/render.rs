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

/// Bundled fonts (committed under `assets/fonts/`, SIL OFL). Embedded at compile
/// time so there is no runtime filesystem dependency and no system-font path.
pub const MANROPE_SEMIBOLD: &[u8] = include_bytes!("../../assets/fonts/Manrope-SemiBold.ttf");
pub const MANROPE_MEDIUM: &[u8] = include_bytes!("../../assets/fonts/Manrope-Medium.ttf");
pub const CAIRO_MEDIUM: &[u8] = include_bytes!("../../assets/fonts/Cairo-Medium.ttf");

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

    pixmap
        .encode_png()
        .map_err(|e| QrCardError::Encode(e.to_string()))
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
