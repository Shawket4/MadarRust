//! Fixtures shared by more than one integration test binary.
//!
//! Test helpers used to live beside the code they exercised, under
//! `#[cfg(test)]`, and a module that needed a neighbour's fixture reached
//! across into it — `menu`'s tests built their photos with `assets`'s. That
//! only worked while every test compiled into one binary. Each suite is its
//! own binary now, so anything two of them share lives here instead, and a
//! suite reaches for a sibling's internals never.
//!
//! Keep this small. A fixture only one suite uses belongs in that suite.

#![allow(dead_code)] // each binary compiles this and uses a different slice of it

use std::io::Cursor;

use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use madar_rust::assets::AssetStore;

/// A deterministic PNG of a given size.
///
/// `seed` shifts the pattern so two photos in one test are distinguishable —
/// including by content hash, which is what the dedupe paths assert on.
pub fn photo_png(w: u32, h: u32, seed: u32) -> Vec<u8> {
    let mut img = RgbaImage::new(w, h);
    for (x, y, p) in img.enumerate_pixels_mut() {
        *p = Rgba([
            ((x * 7 + y * 3 + seed) % 256) as u8,
            ((x * 3 + y * 11) % 256) as u8,
            ((x * 13 + y * 5 + seed * 3) % 256) as u8,
            255,
        ]);
    }
    let mut buf = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(img)
        .write_to(&mut buf, ImageFormat::Png)
        .unwrap();
    buf.into_inner()
}

/// An `AssetStore` under a temporary directory.
///
/// The `TempDir` comes back with it and must be held for as long as the store
/// is used: dropping it deletes the files out from under the store.
pub fn tmp_store() -> (tempfile::TempDir, AssetStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = AssetStore::new(dir.path().join("assets"), dir.path().join("uploads"));
    std::fs::create_dir_all(&store.uploads_dir).unwrap();
    (dir, store)
}
