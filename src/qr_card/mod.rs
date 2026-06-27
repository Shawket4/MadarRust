//! Madar branded QR card generator — rendering layer (pure) + dynamic
//! short-URL layer (Shlink-backed, server-to-server).
//!
//! ## Rendering (pure, no I/O)
//! - `render_qr_card_svg(&QrCardOptions) → Result<String, QrCardError>`
//! - `render_qr_card_png(&QrCardOptions) → Result<Vec<u8>, QrCardError>`
//! - `render_qr_receipt_png(short_url, module_px) → Result<Vec<u8>, QrCardError>`
//!
//! ## Dynamic short-URL layer
//! Handlers build the canonical long URL, create/look up a Shlink short URL,
//! render the QR of the *short* URL, and return an inline base64 data-URL.
//! Clients never supply a pre-made short URL.

pub mod db;
pub mod handlers;
pub mod layout;
pub mod render;
pub mod routes;
pub mod shlink;

#[cfg(test)]
mod tests;

use qrcode::EcLevel;

use crate::errors::AppError;

// ── Madar brand tokens (single source of truth) ─────────────────────────────
/// Teal deep — QR modules, frame, wordmark, primary text.
pub const TEAL: &str = "#0D6273";
/// Teal light — the precision accent (mark satellite / wordmark dot).
pub const TEAL_LIGHT: &str = "#2E94A6";
/// Paper — background and centre plaque.
pub const PAPER: &str = "#EFF3F4";

/// Encoded data longer than this is rejected: long payloads + a centre overlay
/// fail to scan on cheap cameras. Shlink short URLs are well under this.
const MAX_URL_LEN: usize = 512;

/// Default raster resolution — print quality.
const DEFAULT_DPI: u32 = 600;
/// DPI is clamped to this range (raster output only) to bound memory/CPU.
const MIN_DPI: u32 = 72;
const MAX_DPI: u32 = 2400;

/// Options for the branded A6 card.
#[derive(Clone, Debug)]
pub struct QrCardOptions {
    /// The string encoded into the QR (already a Shlink short URL).
    pub short_url: String,
    /// Dynamic line under the tagline, e.g. "Table 5" / "امسح للقائمة".
    pub caption: Option<String>,
    /// Raster resolution in DPI. Default 600 (print quality; clamped 72–2400).
    pub dpi: u32,
    /// Print bleed in mm added on every side. Default 0.0; use 3.0 for print.
    pub bleed_mm: f32,
    /// Draw trim crop marks (only meaningful when `bleed_mm > 0`). Default false.
    pub crop_marks: bool,
}

impl Default for QrCardOptions {
    fn default() -> Self {
        Self {
            short_url: String::new(),
            caption: None,
            dpi: DEFAULT_DPI,
            bleed_mm: 0.0,
            crop_marks: false,
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum QrCardError {
    #[error("invalid QR input: {0}")]
    InvalidInput(String),
    #[error("QR encoding failed: {0}")]
    QrEncode(String),
    #[error("SVG parse failed: {0}")]
    SvgParse(String),
    #[error("render failed: {0}")]
    Render(String),
    #[error("PNG encode failed: {0}")]
    Encode(String),
    #[error("font error: {0}")]
    Font(String),
}

/// Client-input faults map to 400; asset/render faults are server-side (500).
impl From<QrCardError> for AppError {
    fn from(e: QrCardError) -> Self {
        match e {
            QrCardError::InvalidInput(m) => AppError::BadRequest(m),
            QrCardError::QrEncode(m) => AppError::BadRequest(format!("QR encoding failed: {m}")),
            QrCardError::SvgParse(_)
            | QrCardError::Render(_)
            | QrCardError::Encode(_)
            | QrCardError::Font(_) => AppError::Internal,
        }
    }
}

/// Validate the payload before encoding.
fn validate(short_url: &str) -> Result<(), QrCardError> {
    if short_url.is_empty() {
        return Err(QrCardError::InvalidInput("short_url is empty".into()));
    }
    if !short_url.is_ascii() {
        return Err(QrCardError::InvalidInput(
            "short_url must be ASCII (a Shlink short URL)".into(),
        ));
    }
    if short_url.len() > MAX_URL_LEN {
        return Err(QrCardError::InvalidInput(format!(
            "short_url exceeds {MAX_URL_LEN} chars; keep the encoded data short"
        )));
    }
    Ok(())
}

/// Compose the branded A6 card as an SVG document.
pub fn render_qr_card_svg(opts: &QrCardOptions) -> Result<String, QrCardError> {
    validate(&opts.short_url)?;
    let matrix = render::qr_matrix(&opts.short_url, EcLevel::H)?;
    layout::build_card_svg(&matrix, opts)
}

/// Render the branded A6 card to a PNG at `opts.dpi`.
pub fn render_qr_card_png(opts: &QrCardOptions) -> Result<Vec<u8>, QrCardError> {
    let svg = render_qr_card_svg(opts)?;
    let b = opts.bleed_mm.clamp(0.0, 20.0);
    let dpi = opts.dpi.clamp(MIN_DPI, MAX_DPI);
    render::rasterize(&svg, 105.0 + 2.0 * b, 148.0 + 2.0 * b, dpi)
}

/// Render a plain black-on-white receipt QR sized at `module_px` pixels per module.
pub fn render_qr_receipt_png(short_url: &str, module_px: u32) -> Result<Vec<u8>, QrCardError> {
    validate(short_url)?;
    render::plain_qr_png(short_url, module_px.clamp(1, 40), 4)
}
