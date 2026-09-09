//! One way in for every uploaded image.
//!
//! Every route that accepts a picture — a menu photograph, an org logo, the
//! loyalty card's banner — hands its bytes to [`process_upload`] and stores
//! what comes back. Nothing a client sent is ever written to disk verbatim,
//! for three reasons:
//!
//!  * **Size.** A phone camera exports 4000-6000 px and 4-8 MB. Nothing in this
//!    system renders a logo above 1417 px or a card photo above 1125 px, so
//!    those pixels are bytes every customer downloads and no customer ever
//!    sees.
//!  * **Metadata.** A phone photo carries EXIF, and EXIF carries GPS. A shop
//!    uploading a picture of its own counter should not thereby be publishing
//!    the coordinates of its own counter. Re-encoding drops every ancillary
//!    chunk, so this costs nothing extra.
//!  * **Trust.** The uploaded extension and the multipart `Content-Type` are
//!    both client-supplied strings. Decoding is the only honest test of what a
//!    file actually is, and it is the same test the wallet/card renderers will
//!    apply later — better to fail here, where a human is watching, than on a
//!    card nobody is looking at.
//!
//! ## Why the output format follows the pixels, not the route
//!
//! JPEG has no alpha channel. Re-encoding a logo as JPEG does not merely lose
//! its transparency, it changes what the rest of the system decides about it:
//! [`crate::orgs::branding::is_mark`] calls a logo repaintable when more than
//! 10% of it is clear, and a flattened logo is 0% clear — so every mark would
//! stop being a mark, and every shop's wallet pass, printed QR card and web
//! card would show its logo as an opaque tile with a black or white box round
//! it. So an image carrying transparency is re-encoded as PNG and an opaque one
//! becomes JPEG, and the choice is made by looking at the decoded alpha channel
//! rather than by believing the uploaded file's extension or MIME type.

use crate::errors::AppError;
use image::{DynamicImage, ImageEncoder, ImageFormat, ImageReader};
use std::io::Cursor;

/// The ceiling on a stored file.
///
/// Every stored image is served to customers over the public menu and the card
/// pages, so this is a bandwidth budget, not a disk one. 2 MB is generous for a
/// 2000 px JPEG and unreachable for a logo.
pub const MAX_BYTES: usize = 2 * 1024 * 1024;

/// The ceiling on what we will hold in memory while reading a multipart field.
///
/// The upload is buffered whole before it can be decoded, so without this an
/// unauthenticated-size POST is an out-of-memory on a 4 GB VPS. It is
/// deliberately far above [`MAX_BYTES`]: a 6 MB camera export is a perfectly
/// ordinary thing to send us, it just is not a thing we keep.
pub const MAX_RAW_BYTES: usize = 20 * 1024 * 1024;

/// Longest edge kept for a photograph (a menu item, the loyalty card banner).
///
/// The largest consumer of a photograph is Apple's `strip@3x`, 1125x432, and
/// the web card banner is 1032x336. Both are produced with `resize_to_fill`,
/// which crops to the target ratio, so a PORTRAIT photo has to reach 1125 on
/// its SHORT edge — at 3:4 that is a long edge of 1500. 2000 leaves headroom
/// above that for a squarer crop, and still throws away three quarters of the
/// area of a 4032 px phone photo that nothing would ever have displayed.
pub const MAX_PHOTO_EDGE: u32 = 2000;

/// Longest edge kept for a logo.
///
/// A logo's largest consumer is the printed A6 QR card, which caps it at
/// `qr_card::brand::MAX_LOGO_PX` = 1417 px (A6 at 300 dpi); the wallet badge is
/// 512 px and Apple's pass logo smaller again. 1500 sits just above the print
/// ceiling, so the print path still scales DOWN into its own cap rather than up
/// — a logo that arrived below it would print soft, which is exactly the
/// failure a cap must not introduce. Against that, a 6000 px logo export loses
/// 94% of its pixels here and no consumer can tell.
pub const MAX_LOGO_EDGE: u32 = 1500;

/// A stored-ready image: the bytes to write, and what to call the file.
pub struct ProcessedImage {
    /// Re-encoded bytes. Never the uploaded bytes.
    pub bytes: Vec<u8>,
    /// `"png"` or `"jpg"`, chosen by [`process_upload`] from the pixels.
    pub extension: &'static str,
    /// The decoded, already-scaled image these bytes were written from.
    ///
    /// Handed back so a caller that has to make a decision ABOUT the picture —
    /// `orgs` derives a palette and the `is_mark` flag from a logo — decides it
    /// from what actually landed on disk, and does not decode the file a second
    /// time. A flag derived from the original while the card renderer reads the
    /// scaled file is a flag that can disagree with its own image.
    pub image: DynamicImage,
}

/// Decode, cap, strip and re-encode one uploaded image.
///
/// `max_edge` is the longest edge to keep — [`MAX_LOGO_EDGE`] or
/// [`MAX_PHOTO_EDGE`]. Fails with [`AppError::BadRequest`] on anything that is
/// not a still image we can decode, because that is a thing the person who
/// pressed the button can fix and a 500 is not.
pub fn process_upload(raw: &[u8], max_edge: u32) -> Result<ProcessedImage, AppError> {
    let reader = ImageReader::new(Cursor::new(raw))
        .with_guessed_format()
        .map_err(|_| AppError::BadRequest("Could not decode image".into()))?;

    // Checked before decoding, because decoding an animation silently yields
    // frame one and the shop would never be told that the other ninety-nine
    // were dropped.
    if let Some(format) = reader.format() {
        reject_if_animated(raw, format)?;
    }

    let img = reader
        .decode()
        .map_err(|e| AppError::BadRequest(format!("Invalid image: {}", e)))?;

    if img.width() == 0 || img.height() == 0 {
        return Err(AppError::BadRequest("Image has no pixels".into()));
    }

    let img = fit_within(img, max_edge);

    if has_transparency(&img) {
        let bytes = encode_png_within_budget(&img, max_edge);
        Ok(ProcessedImage {
            bytes,
            extension: "png",
            image: img,
        })
    } else {
        let bytes = encode_jpeg_within_budget(&img)?;
        Ok(ProcessedImage {
            bytes,
            extension: "jpg",
            image: img,
        })
    }
}

/// Refuse an animated GIF or WebP.
///
/// Rejecting rather than keeping, because keeping means storing the uploaded
/// bytes untouched — no cap, no metadata strip, no format decision — which is
/// the exact hole this module exists to close, and re-encoding an animation is
/// a feature nothing in the product asks for. And rejecting rather than
/// silently flattening, because a shop that uploaded a looping logo and got a
/// still one back with no explanation would reasonably call that a bug.
fn reject_if_animated(raw: &[u8], format: ImageFormat) -> Result<(), AppError> {
    use image::AnimationDecoder;

    let animated = match format {
        ImageFormat::Gif => image::codecs::gif::GifDecoder::new(Cursor::new(raw))
            .map(|d| d.into_frames().take(2).count() > 1)
            .unwrap_or(false),
        ImageFormat::WebP => image::codecs::webp::WebPDecoder::new(Cursor::new(raw))
            .map(|d| d.has_animation())
            .unwrap_or(false),
        _ => false,
    };

    if animated {
        return Err(AppError::BadRequest(
            "Animated images are not supported — please upload a still picture.".into(),
        ));
    }
    Ok(())
}

/// Scale so the longest edge is at most `max_edge`, keeping the aspect ratio.
///
/// Lanczos3 rather than the cheaper filters because a logo is line art: on
/// `Triangle` a downscaled wordmark comes out visibly soft, and soft is what a
/// shop notices on a printed card. Never upscales — an image already inside the
/// cap is returned untouched, since inventing pixels only makes the file bigger
/// and the picture blurrier.
pub fn fit_within(img: DynamicImage, max_edge: u32) -> DynamicImage {
    if img.width().max(img.height()) <= max_edge {
        return img;
    }
    img.resize(max_edge, max_edge, image::imageops::FilterType::Lanczos3)
}

/// Does this image carry transparency worth keeping?
///
/// The threshold is a single pixel, not a proportion, and it is deliberately
/// generous: the cost of a false NO is a logo with a black box printed round it
/// on every card a shop owns, and the cost of a false YES is a slightly larger
/// file. `is_mark` only needs 10% of the image to be clear before it starts
/// repainting logos, so anything it could possibly call a mark has to survive
/// this test.
///
/// 250 rather than 255 absorbs the ±1 rounding a resample leaves in a
/// constant alpha channel; below it, a pixel is a real blend the artist drew.
pub fn has_transparency(img: &DynamicImage) -> bool {
    // Every JPEG decodes without an alpha channel at all, so the common case
    // never pays for the scan below.
    if !img.color().has_alpha() {
        return false;
    }
    const OPAQUE: u8 = 250;
    img.to_rgba8().pixels().any(|p| p.0[3] < OPAQUE)
}

/// Encode as JPEG, dropping quality until it fits the byte budget.
///
/// The ladder stops at 45 and stores that whatever it weighs: a picture that is
/// still over budget at quality 45 is a pathological one, and refusing the
/// upload at that point would leave the shop with no picture at all rather than
/// a slightly heavy one.
fn encode_jpeg_within_budget(img: &DynamicImage) -> Result<Vec<u8>, AppError> {
    // JPEG cannot express an alpha channel, so an RGBA buffer is rejected by
    // the encoder outright. Callers only reach here for images we already
    // decided are opaque, so dropping the channel loses nothing.
    let rgb = DynamicImage::ImageRgb8(img.to_rgb8());
    const QUALITIES: [u8; 5] = [85, 75, 65, 55, 45];

    let mut last: Option<Vec<u8>> = None;
    for quality in QUALITIES {
        let mut buf = Cursor::new(Vec::new());
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality)
            .encode_image(&rgb)
            .map_err(|e| AppError::BadRequest(format!("Encoding failed: {}", e)))?;
        let bytes = buf.into_inner();
        if bytes.len() <= MAX_BYTES {
            return Ok(bytes);
        }
        last = Some(bytes);
    }
    last.ok_or(AppError::Internal)
}

/// Encode as PNG, shrinking until it fits the byte budget.
///
/// PNG is lossless, so there is no quality knob to turn: the only way to make
/// the file smaller is to make the image smaller. The ladder is 100%, 75%, 50%,
/// 35%, 25% of `max_edge` — area falls as the square, so the last rung is a
/// sixteenth of the pixels and reaches well under the budget for anything a
/// shop uploads as a logo. In practice the first rung wins: a 1500 px logo is
/// flat colour on transparency and compresses to a few hundred KB.
fn encode_png_within_budget(img: &DynamicImage, max_edge: u32) -> Vec<u8> {
    const LADDER: [u32; 5] = [100, 75, 50, 35, 25];

    let mut last = Vec::new();
    for percent in LADDER {
        let edge = (max_edge as u64 * percent as u64 / 100).max(1) as u32;
        let scaled = fit_within(img.clone(), edge);
        let bytes = encode_png(&scaled);
        if bytes.len() <= MAX_BYTES {
            return bytes;
        }
        last = bytes;
    }
    last
}

/// Write one PNG.
///
/// Always RGBA8, so a 16-bit or palettised upload is normalised to one shape
/// the card renderers already handle, and `Best` compression because these
/// files are written once and then served to every customer who opens the menu.
fn encode_png(img: &DynamicImage) -> Vec<u8> {
    let rgba = img.to_rgba8();
    let mut buf = Cursor::new(Vec::new());
    let encoder = image::codecs::png::PngEncoder::new_with_quality(
        &mut buf,
        image::codecs::png::CompressionType::Best,
        image::codecs::png::FilterType::Adaptive,
    );
    // Writing from the raw buffer cannot fail for an in-memory Cursor with
    // dimensions that came out of a decoded image, but if it somehow did we
    // would rather store nothing than store a truncated file.
    match encoder.write_image(
        rgba.as_raw(),
        rgba.width(),
        rgba.height(),
        image::ExtendedColorType::Rgba8,
    ) {
        Ok(()) => buf.into_inner(),
        Err(e) => {
            tracing::error!("PNG encoding failed: {}", e);
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    /// A square logo-ish image: an opaque disc on a transparent field, so most
    /// of the frame is clear and `is_mark` would call it a mark.
    fn transparent_mark(size: u32) -> Vec<u8> {
        let mut img = RgbaImage::from_pixel(size, size, Rgba([0, 0, 0, 0]));
        let r = size as f64 * 0.28;
        let c = size as f64 / 2.0;
        for y in 0..size {
            for x in 0..size {
                let dx = x as f64 - c;
                let dy = y as f64 - c;
                if dx * dx + dy * dy <= r * r {
                    img.put_pixel(x, y, Rgba([20, 20, 20, 255]));
                }
            }
        }
        let mut buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    /// An opaque "photograph": a gradient with noise, so it is neither flat
    /// (which any encoder squashes to nothing) nor transparent.
    fn opaque_photo(w: u32, h: u32) -> Vec<u8> {
        let mut img = RgbaImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let r = ((x * 7 + y * 3) % 256) as u8;
                let g = ((x * 3 + y * 11) % 256) as u8;
                let b = ((x * 13 + y * 5) % 256) as u8;
                img.put_pixel(x, y, Rgba([r, g, b, 255]));
            }
        }
        let mut buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    /// The whole point of the module: a logo keeps its alpha, so `is_mark` can
    /// still see through it and the card can still repaint it.
    #[test]
    fn transparent_png_keeps_its_alpha() {
        let raw = transparent_mark(600);
        let out = process_upload(&raw, MAX_LOGO_EDGE).unwrap();

        assert_eq!(out.extension, "png", "a mark must not be flattened to JPEG");

        let decoded = image::load_from_memory(&out.bytes).unwrap();
        assert!(
            has_transparency(&decoded),
            "the stored file lost its alpha channel"
        );

        // And specifically: still clear enough to satisfy is_mark's >10% test,
        // which is what decides whether a shop's pass repaints its logo.
        let rgba = decoded.to_rgba8();
        let total = (rgba.width() * rgba.height()) as f64;
        let clear = rgba.pixels().filter(|p| p.0[3] < 16).count() as f64;
        assert!(
            clear / total > 0.10,
            "only {:.1}% clear — is_mark would stop calling this a mark",
            100.0 * clear / total
        );
        assert!(crate::orgs::branding::is_mark(&decoded));
    }

    #[test]
    fn opaque_image_becomes_a_smaller_jpeg() {
        let raw = opaque_photo(800, 600);
        let out = process_upload(&raw, MAX_PHOTO_EDGE).unwrap();

        assert_eq!(out.extension, "jpg");
        assert!(
            out.bytes.len() < raw.len(),
            "JPEG ({} bytes) should beat the source PNG ({} bytes)",
            out.bytes.len(),
            raw.len()
        );
        // Same picture, same shape — the cap did not apply at 800x600.
        let decoded = image::load_from_memory(&out.bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (800, 600));
    }

    #[test]
    fn oversized_image_is_capped_and_keeps_its_ratio() {
        // 3000x1500 is 2:1 and twice the photo cap on its long edge.
        let raw = opaque_photo(3000, 1500);
        let out = process_upload(&raw, MAX_PHOTO_EDGE).unwrap();

        let decoded = image::load_from_memory(&out.bytes).unwrap();
        assert_eq!(decoded.width(), MAX_PHOTO_EDGE);
        assert_eq!(decoded.height(), MAX_PHOTO_EDGE / 2);
        assert!(out.bytes.len() <= MAX_BYTES);
    }

    #[test]
    fn logo_cap_is_applied_to_logos() {
        let raw = transparent_mark(4000);
        let out = process_upload(&raw, MAX_LOGO_EDGE).unwrap();
        let decoded = image::load_from_memory(&out.bytes).unwrap();
        assert_eq!(decoded.width().max(decoded.height()), MAX_LOGO_EDGE);
        assert!(out.bytes.len() <= MAX_BYTES);
    }

    #[test]
    fn an_image_inside_the_cap_is_not_upscaled() {
        let raw = opaque_photo(120, 90);
        let out = process_upload(&raw, MAX_PHOTO_EDGE).unwrap();
        let decoded = image::load_from_memory(&out.bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (120, 90));
    }

    /// The 400's message, or a failure naming what came back instead.
    ///
    /// `unwrap_err` is unavailable here because `ProcessedImage` holds a
    /// `DynamicImage`, which is not `Debug`.
    fn bad_request_message(result: Result<ProcessedImage, AppError>) -> String {
        match result {
            Err(AppError::BadRequest(msg)) => msg,
            Err(other) => panic!("expected a 400, got {:?}", other),
            Ok(ok) => panic!("expected a 400, got {} stored bytes", ok.bytes.len()),
        }
    }

    #[test]
    fn undecodable_file_is_a_bad_request_not_a_panic() {
        bad_request_message(process_upload(
            b"this is not an image at all",
            MAX_PHOTO_EDGE,
        ));

        // An empty body takes the same path.
        bad_request_message(process_upload(&[], MAX_PHOTO_EDGE));

        // And so does a file whose header says PNG but whose body is garbage —
        // the case a Content-Type allowlist alone would wave straight through.
        let mut liar = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        liar.extend_from_slice(b"nope nope nope");
        bad_request_message(process_upload(&liar, MAX_PHOTO_EDGE));
    }

    #[test]
    fn animated_gif_is_refused_rather_than_flattened() {
        // Two frames of a 4x4 GIF, built with the encoder so the bytes are real.
        let mut raw = Cursor::new(Vec::new());
        {
            let mut enc = image::codecs::gif::GifEncoder::new(&mut raw);
            for shade in [0u8, 255u8] {
                let frame = RgbaImage::from_pixel(4, 4, Rgba([shade, shade, shade, 255]));
                enc.encode_frame(image::Frame::new(frame)).unwrap();
            }
        }
        let bytes = raw.into_inner();

        let msg = bad_request_message(process_upload(&bytes, MAX_PHOTO_EDGE));
        assert!(
            msg.contains("Animated"),
            "the message must say why: {}",
            msg
        );
    }

    #[test]
    fn a_still_gif_is_still_accepted() {
        let mut raw = Cursor::new(Vec::new());
        {
            let mut enc = image::codecs::gif::GifEncoder::new(&mut raw);
            let mut frame = RgbaImage::from_pixel(40, 20, Rgba([10, 120, 200, 255]));
            frame.put_pixel(0, 0, Rgba([255, 255, 255, 255]));
            enc.encode_frame(image::Frame::new(frame)).unwrap();
        }
        let out = process_upload(&raw.into_inner(), MAX_PHOTO_EDGE).unwrap();
        assert_eq!(out.extension, "jpg");
    }

    /// Metadata goes because we re-encode, and this is the one that matters:
    /// a phone photo's EXIF carries GPS.
    #[test]
    fn exif_does_not_survive_the_round_trip() {
        let base = opaque_photo(64, 64);
        let jpeg = process_upload(&base, MAX_PHOTO_EDGE).unwrap().bytes;

        // Splice an APP1/Exif segment in after SOI, the way a camera does.
        let payload = b"Exif\0\0II*\0\x08\0\0\0GPS-SECRET-LOCATION";
        let len = (payload.len() + 2) as u16;
        let mut with_exif = vec![0xff, 0xd8, 0xff, 0xe1];
        with_exif.extend_from_slice(&len.to_be_bytes());
        with_exif.extend_from_slice(payload);
        with_exif.extend_from_slice(&jpeg[2..]);

        let out = process_upload(&with_exif, MAX_PHOTO_EDGE).unwrap();
        assert!(
            !out.bytes
                .windows(payload.len())
                .any(|w| w == payload.as_slice()),
            "EXIF survived re-encoding — uploads would publish GPS coordinates"
        );
    }
}
