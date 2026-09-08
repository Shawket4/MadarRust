//! The stepper, drawn as an image for a Wallet pass.
//!
//! A pass field holds text, so the stepper was `●─●─●─○─○` — and iOS shrinks a
//! secondary field to fit its width, so a twelve-step programme arrived as a
//! cramped grey smear. The web card draws the same object with real geometry;
//! this draws it with the same geometry, into the one place a pass will accept
//! a picture.
//!
//! ## Why it is drawn rather than composed
//! There is no layout engine here and no font: a checkmark is two strokes and a
//! step is a filled disc, which is the whole vocabulary. Everything is rendered
//! at [`SUPERSAMPLE`]× and scaled down, so the curves are anti-aliased by the
//! resampler rather than by arithmetic in this file.
//!
//! The canvas is TRANSPARENT. Apple composites the strip over the pass's own
//! background, so a drawn-in ground would be a rectangle of very nearly the
//! right colour sitting on the card — visible at exactly the wrong moments.

use image::{Rgba, RgbaImage};

/// Rendered this many times larger, then scaled down. The downscale is what
/// gives smooth edges; drawing a circle at final size gives a staircase.
const SUPERSAMPLE: u32 = 4;

/// Past this the steps stop being countable and become texture — the same cap
/// the web card and the text stepper use, so all three agree about when a
/// programme has outgrown being drawn.
pub const MAX_STEPS: i32 = 12;

/// A colour as the pass carries it: `#RRGGBB`.
fn rgba(hex: &str, alpha: u8) -> Rgba<u8> {
    let (r, g, b) = crate::orgs::branding::parse_hex(hex).unwrap_or((255, 255, 255));
    Rgba([r, g, b, alpha])
}

/// Composite one pixel, respecting what is already there.
fn blend(img: &mut RgbaImage, x: i64, y: i64, c: Rgba<u8>) {
    if x < 0 || y < 0 || x >= img.width() as i64 || y >= img.height() as i64 {
        return;
    }
    let dst = img.get_pixel_mut(x as u32, y as u32);
    let a = c.0[3] as u32;
    if a == 0 {
        return;
    }
    if a == 255 || dst.0[3] == 0 {
        *dst = c;
        return;
    }
    let inv = 255 - a;
    for i in 0..3 {
        dst.0[i] = ((c.0[i] as u32 * a + dst.0[i] as u32 * inv) / 255) as u8;
    }
    dst.0[3] = dst.0[3].max(c.0[3]);
}

fn fill_disc(img: &mut RgbaImage, cx: f64, cy: f64, r: f64, c: Rgba<u8>) {
    let r2 = r * r;
    for y in (cy - r).floor() as i64..=(cy + r).ceil() as i64 {
        for x in (cx - r).floor() as i64..=(cx + r).ceil() as i64 {
            let dx = x as f64 + 0.5 - cx;
            let dy = y as f64 + 0.5 - cy;
            if dx * dx + dy * dy <= r2 {
                blend(img, x, y, c);
            }
        }
    }
}

/// A ring: the disc minus its middle.
fn stroke_circle(img: &mut RgbaImage, cx: f64, cy: f64, r: f64, w: f64, c: Rgba<u8>) {
    let outer = r * r;
    let inner = (r - w).max(0.0) * (r - w).max(0.0);
    for y in (cy - r).floor() as i64..=(cy + r).ceil() as i64 {
        for x in (cx - r).floor() as i64..=(cx + r).ceil() as i64 {
            let dx = x as f64 + 0.5 - cx;
            let dy = y as f64 + 0.5 - cy;
            let d = dx * dx + dy * dy;
            if d <= outer && d >= inner {
                blend(img, x, y, c);
            }
        }
    }
}

/// A stroke between two points, with round ends so segments meet cleanly.
fn line(img: &mut RgbaImage, x0: f64, y0: f64, x1: f64, y1: f64, w: f64, c: Rgba<u8>) {
    let steps = ((x1 - x0).abs().max((y1 - y0).abs()) * 2.0).ceil().max(1.0);
    for i in 0..=steps as i64 {
        let t = i as f64 / steps;
        fill_disc(img, x0 + (x1 - x0) * t, y0 + (y1 - y0) * t, w / 2.0, c);
    }
}

/// The tick on a completed step. Two strokes, proportioned to the disc.
fn check(img: &mut RgbaImage, cx: f64, cy: f64, r: f64, c: Rgba<u8>) {
    let w = r * 0.30;
    // Down-stroke to the elbow, then up to the tip. Sitting slightly low in the
    // disc, which is where a tick reads as centred.
    let elbow = (cx - r * 0.08, cy + r * 0.42);
    line(img, cx - r * 0.48, cy + r * 0.02, elbow.0, elbow.1, w, c);
    line(img, elbow.0, elbow.1, cx + r * 0.50, cy - r * 0.42, w, c);
}

/// Can this target be drawn as steps at all?
pub fn drawable(target: i32) -> bool {
    target > 0 && target <= MAX_STEPS
}

/// Draw the stepper at `w`×`h`, as a PNG.
///
/// `filled` is clamped: redemption leaves a remainder and an adjustment can
/// overshoot, and neither should produce a broken row.
pub fn render(
    filled: i32,
    target: i32,
    w: u32,
    h: u32,
    foreground: &str,
    accent: &str,
    on_accent: &str,
) -> Option<Vec<u8>> {
    if !drawable(target) || w == 0 || h == 0 {
        return None;
    }
    let filled = filled.clamp(0, target);
    let (bw, bh) = (w * SUPERSAMPLE, h * SUPERSAMPLE);
    let mut img = RgbaImage::from_pixel(bw, bh, Rgba([0, 0, 0, 0]));

    let cw = bw as f64;
    let ch = bh as f64;
    // Apple scales the strip to the pass's width and crops the height, so the
    // row sits centred with room to lose: a stepper drawn to the edges would be
    // a stepper with its top and bottom shaved off on some devices.
    let pad = cw * 0.07;
    let avail = cw - pad * 2.0;
    let n = target as f64;
    // Big enough to read, small enough that the gaps still separate them.
    let d = (ch * 0.58).min(avail / n * 0.84);
    let r = d / 2.0;
    let gap = if target > 1 {
        (avail - d * n) / (n - 1.0)
    } else {
        0.0
    };
    let cy = ch / 2.0;
    let cx = |i: i32| pad + r + i as f64 * (d + gap);

    let track = rgba(foreground, 90);
    let done = rgba(accent, 255);
    let ink = rgba(on_accent, 255);
    let ahead = rgba(foreground, 110);

    // The track first, so every step sits ON it rather than beside it.
    if target > 1 {
        let t = (d * 0.085).max(2.0);
        line(&mut img, cx(0), cy, cx(target - 1), cy, t, track);
        if filled > 1 {
            line(&mut img, cx(0), cy, cx(filled - 1), cy, t, done);
        }
    }

    for i in 0..target {
        let x = cx(i);
        if i < filled {
            fill_disc(&mut img, x, cy, r, done);
            check(&mut img, x, cy, r, ink);
        } else {
            // Opaque, so the track passes BEHIND an empty step rather than
            // through it — a ring with a line across it reads as crossed out.
            fill_disc(&mut img, x, cy, r, rgba(on_accent, 255));
            // The next one is ringed heavier: "you are here", without colour
            // alone carrying the state.
            let next = i == filled;
            stroke_circle(
                &mut img,
                x,
                cy,
                r,
                if next { d * 0.10 } else { d * 0.06 },
                if next { done } else { ahead },
            );
        }
    }

    let out = image::DynamicImage::ImageRgba8(img).resize_exact(
        w,
        h,
        image::imageops::FilterType::Lanczos3,
    );
    let mut buf = std::io::Cursor::new(Vec::new());
    out.write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some(buf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FG: &str = "#EFF3F4";
    const ACCENT: &str = "#C8607F";
    const GROUND: &str = "#7B1E3A";

    fn decode(png: &[u8]) -> image::RgbaImage {
        image::load_from_memory(png).unwrap().to_rgba8()
    }

    /// Writes the strip at Apple's @2x size, composited on the pass ground, so
    /// it can be LOOKED at. Ignored by default; there is no assertion that
    /// tells you whether a checkmark is the right weight.
    ///
    ///     cargo test --lib stepper::tests::preview -- --ignored --nocapture
    #[test]
    #[ignore = "writes a preview to look at"]
    fn preview() {
        let dir = std::env::temp_dir();
        for (name, filled, target) in [("a", 3, 5), ("b", 0, 5), ("c", 7, 12), ("d", 3, 3)] {
            let png = render(filled, target, 750, 288, FG, ACCENT, GROUND).unwrap();
            let strip = image::load_from_memory(&png).unwrap().to_rgba8();
            let (r, g, b) = crate::orgs::branding::parse_hex(GROUND).unwrap();
            let mut card = image::RgbaImage::from_pixel(750, 288, image::Rgba([r, g, b, 255]));
            image::imageops::overlay(&mut card, &strip, 0, 0);
            let at = dir.join(format!("madar-step-{name}.png"));
            card.save(&at).unwrap();
            println!("wrote {}", at.display());
        }
    }

    #[test]
    fn it_draws_at_the_size_it_is_asked_for() {
        let png = render(3, 5, 375, 144, FG, ACCENT, GROUND).unwrap();
        let img = decode(&png);
        assert_eq!((img.width(), img.height()), (375, 144));
    }

    #[test]
    fn the_ground_stays_transparent() {
        // Apple composites the strip over the pass's own background. A drawn-in
        // ground would be a rectangle of very nearly the right colour, which is
        // worse than none.
        let img = decode(&render(2, 5, 375, 144, FG, ACCENT, GROUND).unwrap());
        assert_eq!(img.get_pixel(2, 2).0[3], 0, "the corner must be clear");
        assert_eq!(
            img.get_pixel(img.width() - 3, 2).0[3],
            0,
            "and so must the other one"
        );
    }

    /// How many STEPS the row contains.
    ///
    /// Counted by column, not by sampling one row: a disc is tall and the track
    /// is a few pixels, so a column that crosses a step carries far more ink
    /// than one that only crosses the track between two. That distinction holds
    /// at every target, which sampling a fixed row does not — the discs get
    /// smaller as the target grows and a row chosen for five steps misses
    /// twelve entirely.
    fn steps_drawn(filled: i32, target: i32) -> usize {
        let img = decode(&render(filled, target, 600, 160, FG, ACCENT, GROUND).unwrap());
        let tall = img.height() / 8;
        let mut runs = 0;
        let mut inside = false;
        for x in 0..img.width() {
            let ink = (0..img.height())
                .filter(|&y| img.get_pixel(x, y).0[3] > 40)
                .count() as u32;
            let on_step = ink > tall;
            if on_step && !inside {
                runs += 1;
            }
            inside = on_step;
        }
        runs
    }

    #[test]
    fn it_draws_exactly_as_many_steps_as_the_target() {
        for target in [1, 2, 3, 5, 8, 12] {
            assert_eq!(steps_drawn(0, target), target as usize, "target {target}");
        }
    }

    /// Accent pixels in each third of the row.
    fn accent_by_third(filled: i32, target: i32) -> [usize; 3] {
        let img = decode(&render(filled, target, 600, 160, FG, ACCENT, GROUND).unwrap());
        let (r, g, b) = crate::orgs::branding::parse_hex(ACCENT).unwrap();
        let mut out = [0usize; 3];
        for (x, _y, p) in img.enumerate_pixels() {
            if p.0[3] > 200 && p.0[0] == r && p.0[1] == g && p.0[2] == b {
                out[(x as usize * 3 / img.width() as usize).min(2)] += 1;
            }
        }
        out
    }

    #[test]
    fn a_completed_step_is_filled_and_a_future_one_is_only_outlined() {
        // Three steps, one done. The first is a filled disc; the second is the
        // NEXT one, so it carries an accent ring; the third is neither.
        let [done, next, ahead] = accent_by_third(1, 3);
        assert!(
            done > next * 2,
            "a filled step must be far more than a ring: {done} vs {next}"
        );
        assert!(next > 0, "the next step is ringed in the accent");
        assert_eq!(ahead, 0, "a step not yet reached carries no accent");
    }

    #[test]
    fn a_finished_card_is_filled_all_the_way_across() {
        let [a, b, c] = accent_by_third(3, 3);
        assert!(a > 0 && b > 0 && c > 0, "{a} {b} {c}");
    }

    #[test]
    fn a_balance_outside_the_target_still_draws_a_whole_row() {
        // Redemption leaves a remainder; an adjustment can overshoot.
        assert_eq!(steps_drawn(99, 4), 4);
        assert_eq!(steps_drawn(-7, 4), 4);
        // And an overshoot is a finished card, not a broken one.
        let [a, b, c] = accent_by_third(99, 3);
        assert!(a > 0 && b > 0 && c > 0);
    }

    #[test]
    fn a_programme_too_big_to_count_is_not_drawn() {
        assert!(render(1, MAX_STEPS + 1, 375, 144, FG, ACCENT, GROUND).is_none());
        assert!(render(1, 0, 375, 144, FG, ACCENT, GROUND).is_none());
        assert!(render(1, 100, 375, 144, FG, ACCENT, GROUND).is_none());
        assert!(drawable(MAX_STEPS));
        assert!(!drawable(MAX_STEPS + 1));
    }
}
