//! THE ingestion function (contract §11.3 + §11.10).
//!
//! [`stage`] is the only thing an HTTP handler calls: a size cap, a magic-byte
//! sniff, a staged copy under a server-generated name and a queued
//! `asset_jobs` row. It never decodes.
//!
//! [`ingest`] is the one pipeline (worker, backfill binary, preset reconcile):
//! source bytes → `source_hash` (per-org dedup BEFORE any conversion) → sniff →
//! full decode (animated images refused) → EXIF orientation applied → metadata
//! dropped by re-encoding from pixels → `thumb`/`tile`/`full` (+ `original` for
//! logos and card images) as WebP, or Lottie JSON → zstd-19 → `content_hash` =
//! sha256 of each final file → `<org_id>/<content_hash>.<ext>` written
//! atomically → `asset_groups` + `assets` rows with the encoder settings.

use std::io::Cursor;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use hmac::{Hmac, Mac};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use sqlx::{PgPool, Row};
use tokio::sync::Semaphore;
use utoipa::ToSchema;
use uuid::Uuid;

use super::{AssetStore, sha256_hex, write_atomic};
use crate::errors::AppError;

/// Bumping this re-ingests on the next backfill run (new `encoder` string →
/// source dedup misses → new group). Old rows stay valid.
pub const ENCODER_VERSION: &str = "madar-ingest/1";
/// What we will hold in memory for one upload.
pub const MAX_RAW_BYTES: usize = 20 * 1024 * 1024;
/// URL imports are smaller: a remote server decides their size.
pub const MAX_URL_BYTES: usize = 10 * 1024 * 1024;
pub const URL_TIMEOUT: Duration = Duration::from_secs(10);
/// Decompression-bomb guard: a 12000×12000 RGBA frame is already 576 MB.
pub const MAX_DECODE_EDGE: u32 = 12_000;
pub const THUMB_EDGE: u32 = 128;
pub const TILE_EDGE: u32 = 512;
pub const FULL_EDGE: u32 = 1600;
/// Originals (logo/card) keep their pixels but not an absurd canvas.
pub const ORIGINAL_EDGE: u32 = 4096;
pub const WEBP_QUALITY: f32 = 80.0;
pub const WEBP_METHOD: i32 = 6;
pub const ZSTD_LEVEL: i32 = 19;

// ── Types ───────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AssetPurpose {
    MenuItemPhoto,
    CategoryPhoto,
    BundlePhoto,
    OrgLogo,
    LoyaltyCardImage,
    StepAnimation,
    DeliveryMenuPhoto,
    ImportPhoto,
    AiPhoto,
}

impl AssetPurpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MenuItemPhoto => "menu_item_photo",
            Self::CategoryPhoto => "category_photo",
            Self::BundlePhoto => "bundle_photo",
            Self::OrgLogo => "org_logo",
            Self::LoyaltyCardImage => "loyalty_card_image",
            Self::StepAnimation => "step_animation",
            Self::DeliveryMenuPhoto => "delivery_menu_photo",
            Self::ImportPhoto => "import_photo",
            Self::AiPhoto => "ai_photo",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "menu_item_photo" => Self::MenuItemPhoto,
            "category_photo" => Self::CategoryPhoto,
            "bundle_photo" => Self::BundlePhoto,
            "org_logo" => Self::OrgLogo,
            "loyalty_card_image" => Self::LoyaltyCardImage,
            "step_animation" => Self::StepAnimation,
            "delivery_menu_photo" => Self::DeliveryMenuPhoto,
            "import_photo" => Self::ImportPhoto,
            "ai_photo" => Self::AiPhoto,
            _ => return None,
        })
    }
    pub fn is_animation(self) -> bool {
        self == Self::StepAnimation
    }
    /// §11.10: originals are kept only where wallet/print need a crisp source.
    pub fn keeps_original(self) -> bool {
        matches!(self, Self::OrgLogo | Self::LoyaltyCardImage)
    }
    /// `asset_groups.profile`: which conversion recipe made the group. Dedup
    /// (by source or by pixels) never crosses profiles.
    pub fn profile(self) -> &'static str {
        if self.keeps_original() {
            "keeps_original"
        } else {
            "photo"
        }
    }
}

pub enum IngestSource {
    Bytes(bytes::Bytes),
    StagedFile(PathBuf),
    Url(url::Url),
    LegacyFile(PathBuf),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Upload,
    Url,
    Backfill,
    Preset,
    Import,
    Ai,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Url => "url",
            Self::Backfill => "backfill",
            Self::Preset => "preset",
            Self::Import => "import",
            Self::Ai => "ai",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "upload" => Self::Upload,
            "url" => Self::Url,
            "backfill" => Self::Backfill,
            "preset" => Self::Preset,
            "import" => Self::Import,
            "ai" => Self::Ai,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetTable {
    MenuItems,
    Categories,
    Bundles,
    Organizations,
    RecipeStepPresets,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetField {
    Image,
    Logo,
    BrandCardImage,
    Animation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssetTarget {
    pub table: AssetTable,
    pub id: Uuid,
    pub field: AssetField,
}

/// Static facts about one (table, field) slot. Column names come only from
/// here, never from input, so the dynamic SQL below is closed over this table.
pub(crate) struct Slot {
    pub table: &'static str,
    pub field: &'static str,
    pub group_col: &'static str,
    pub legacy_col: Option<&'static str>,
    /// Uploads sub-directory used for the synthesized legacy URL.
    pub legacy_dir: &'static str,
    #[allow(dead_code)]
    pub purpose: AssetPurpose,
}

impl AssetTarget {
    pub fn new(table: AssetTable, id: Uuid, field: AssetField) -> Self {
        Self { table, id, field }
    }

    pub(crate) fn slot(&self) -> Result<Slot, AppError> {
        use AssetField as F;
        use AssetTable as T;
        Ok(match (self.table, self.field) {
            (T::MenuItems, F::Image) => Slot {
                table: "menu_items",
                field: "image",
                group_col: "image_group_id",
                legacy_col: Some("image_url"),
                legacy_dir: "menu-items",
                purpose: AssetPurpose::MenuItemPhoto,
            },
            (T::Categories, F::Image) => Slot {
                table: "categories",
                field: "image",
                group_col: "image_group_id",
                legacy_col: Some("image_url"),
                legacy_dir: "categories",
                purpose: AssetPurpose::CategoryPhoto,
            },
            (T::Bundles, F::Image) => Slot {
                table: "bundles",
                field: "image",
                group_col: "image_group_id",
                legacy_col: Some("image_url"),
                legacy_dir: "bundles",
                purpose: AssetPurpose::BundlePhoto,
            },
            (T::Organizations, F::Logo) => Slot {
                table: "organizations",
                field: "logo",
                group_col: "logo_group_id",
                legacy_col: Some("logo_url"),
                legacy_dir: "logos",
                purpose: AssetPurpose::OrgLogo,
            },
            (T::Organizations, F::BrandCardImage) => Slot {
                table: "organizations",
                field: "brand_card_image",
                group_col: "brand_card_image_group_id",
                legacy_col: Some("brand_card_image"),
                legacy_dir: "card",
                purpose: AssetPurpose::LoyaltyCardImage,
            },
            (T::RecipeStepPresets, F::Animation) => Slot {
                table: "recipe_step_presets",
                field: "animation",
                group_col: "animation_group_id",
                legacy_col: None,
                legacy_dir: "",
                purpose: AssetPurpose::StepAnimation,
            },
            _ => {
                return Err(AppError::BadRequest(format!(
                    "no asset slot {:?}.{:?}",
                    self.table, self.field
                )));
            }
        })
    }

    pub(crate) fn from_db(table: &str, id: Uuid, field: &str) -> Option<Self> {
        use AssetField as F;
        use AssetTable as T;
        let (t, f) = match (table, field) {
            ("menu_items", "image") => (T::MenuItems, F::Image),
            ("categories", "image") => (T::Categories, F::Image),
            ("bundles", "image") => (T::Bundles, F::Image),
            ("organizations", "logo") => (T::Organizations, F::Logo),
            ("organizations", "brand_card_image") => (T::Organizations, F::BrandCardImage),
            ("recipe_step_presets", "animation") => (T::RecipeStepPresets, F::Animation),
            _ => return None,
        };
        Some(Self::new(t, id, f))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, ToSchema)]
pub struct AssetRef {
    pub id: Uuid,
    pub org_id: Option<Uuid>,
    pub group_id: Uuid,
    pub hash: String,
    pub kind: String,
    pub variant: String,
    pub ext: String,
    pub content_type: String,
    pub bytes: i64,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub has_alpha: bool,
    /// Signed, org-scoped (§11.4), 24 h bucketed.
    pub url: String,
}

#[derive(Clone, Debug)]
pub struct IngestOutcome {
    pub group_id: Uuid,
    pub org_id: Option<Uuid>,
    pub variants: Vec<AssetRef>,
    pub deduped: bool,
}

impl IngestOutcome {
    /// A small image stores one row for byte-identical variants (the largest
    /// name wins), so `thumb` falls back to `tile` falls back to `full`.
    pub fn variant(&self, name: &str) -> Option<&AssetRef> {
        let exact = |n: &str| self.variants.iter().find(|v| v.variant == n);
        match name {
            "thumb" => exact("thumb")
                .or_else(|| exact("tile"))
                .or_else(|| exact("full")),
            "tile" => exact("tile").or_else(|| exact("full")),
            n => exact(n),
        }
    }
    /// The POS variant: `tile` for images, `animation` for Lottie.
    pub fn pos(&self) -> Option<&AssetRef> {
        self.variant("tile").or_else(|| self.variant("animation"))
    }
    /// The largest display variant (what legacy URLs redirect to).
    pub fn full(&self) -> Option<&AssetRef> {
        self.variant("full").or_else(|| self.variant("animation"))
    }
    pub fn stored_bytes(&self) -> i64 {
        self.variants.iter().map(|v| v.bytes).sum()
    }
}

// ── Sniffing ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sniffed {
    Image(ImageFormat),
    Lottie,
}

/// Magic bytes only (plus a cheap JSON shape check for Lottie). Client MIME
/// types and extensions never reach this function.
pub fn sniff(raw: &[u8]) -> Result<Sniffed, AppError> {
    if raw.is_empty() {
        return Err(AppError::BadRequest("Empty file".into()));
    }
    if let Some(t) = infer::get(raw) {
        let fmt = match t.mime_type() {
            "image/jpeg" => Some(ImageFormat::Jpeg),
            "image/png" => Some(ImageFormat::Png),
            "image/gif" => Some(ImageFormat::Gif),
            "image/webp" => Some(ImageFormat::WebP),
            "image/bmp" => Some(ImageFormat::Bmp),
            _ => None,
        };
        return fmt.map(Sniffed::Image).ok_or_else(|| {
            AppError::BadRequest(format!("Unsupported file type: {}", t.mime_type()))
        });
    }
    let first = raw.iter().find(|b| !b.is_ascii_whitespace());
    if first == Some(&b'{') && looks_like_lottie(raw) {
        return Ok(Sniffed::Lottie);
    }
    Err(AppError::BadRequest(
        "Unsupported file type: not an image or a Lottie animation".into(),
    ))
}

fn looks_like_lottie(raw: &[u8]) -> bool {
    lottie_meta(raw).is_some()
}

/// `(w, h)` when `raw` is a JSON object with a string `v` and an array `layers`.
pub fn lottie_meta(raw: &[u8]) -> Option<(Option<i32>, Option<i32>)> {
    let v: serde_json::Value = serde_json::from_slice(raw).ok()?;
    let o = v.as_object()?;
    if !o.get("v").is_some_and(|x| x.is_string()) || !o.get("layers").is_some_and(|x| x.is_array())
    {
        return None;
    }
    let dim = |k: &str| o.get(k).and_then(|x| x.as_f64()).map(|f| f.round() as i32);
    Some((dim("w"), dim("h")))
}

fn check_purpose(purpose: AssetPurpose, sniffed: Sniffed) -> Result<(), AppError> {
    match (purpose.is_animation(), sniffed) {
        (true, Sniffed::Lottie) | (false, Sniffed::Image(_)) => Ok(()),
        (true, _) => Err(AppError::BadRequest(
            "Expected a Lottie JSON animation".into(),
        )),
        (false, _) => Err(AppError::BadRequest("Expected an image".into())),
    }
}

/// Display label: NFC-ish cleanup without a unicode-normalization dep — strip
/// control chars and path separators, trim, cap at 120 chars.
pub fn sanitize_label(label: Option<&str>) -> Option<String> {
    let s: String = label?
        .chars()
        .filter(|c| !c.is_control() && *c != '/' && *c != '\\')
        .collect();
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    Some(s.chars().take(120).collect())
}

// ── Stage (request path) ────────────────────────────────────────────────────

pub async fn stage(
    pool: &PgPool,
    org_id: Option<Uuid>,
    purpose: AssetPurpose,
    source: IngestSource,
    label: Option<&str>,
    target: AssetTarget,
    actor: Option<Uuid>,
) -> Result<Uuid, AppError> {
    stage_with(
        pool,
        &AssetStore::from_env(),
        org_id,
        purpose,
        source,
        label,
        target,
        actor,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn stage_with(
    pool: &PgPool,
    store: &AssetStore,
    org_id: Option<Uuid>,
    purpose: AssetPurpose,
    source: IngestSource,
    label: Option<&str>,
    target: AssetTarget,
    actor: Option<Uuid>,
) -> Result<Uuid, AppError> {
    if org_id.is_none() && !purpose.is_animation() {
        return Err(AppError::BadRequest(
            "An image must belong to an organisation".into(),
        ));
    }
    let slot = target.slot()?;
    let job_id = Uuid::new_v4();
    let label = sanitize_label(label);
    let (staged_path, source_url, source_kind) = match source {
        IngestSource::Url(u) => {
            check_url_syntax(&u)?;
            (None, Some(u.to_string()), SourceKind::Url)
        }
        other => {
            let raw = match other {
                IngestSource::Bytes(b) => b.to_vec(),
                IngestSource::StagedFile(p) | IngestSource::LegacyFile(p) => {
                    read_capped_file(&p, MAX_RAW_BYTES).await?
                }
                IngestSource::Url(_) => unreachable!(),
            };
            if raw.len() > MAX_RAW_BYTES {
                return Err(AppError::BadRequest(
                    "File too large (max 20 MB raw)".into(),
                ));
            }
            let sniffed = sniff(&raw)?;
            check_purpose(purpose, sniffed)?;
            let path = store.staging_dir().join(job_id.to_string());
            let p2 = path.clone();
            tokio::task::spawn_blocking(move || write_atomic(&p2, &raw))
                .await
                .map_err(|_| AppError::Internal)?
                .map_err(|e| {
                    tracing::error!(error = %e, "asset staging write failed");
                    AppError::Internal
                })?;
            (
                Some(path.to_string_lossy().to_string()),
                None,
                SourceKind::Upload,
            )
        }
    };
    let res = sqlx::query(
        "INSERT INTO asset_jobs (id, org_id, purpose, source_kind, staged_path, source_url, label, \
                                 target_table, target_id, target_field, created_by) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,(SELECT id FROM users WHERE id = $11))",
    )
    .bind(job_id)
    .bind(org_id)
    .bind(purpose.as_str())
    .bind(source_kind.as_str())
    .bind(&staged_path)
    .bind(&source_url)
    .bind(&label)
    .bind(slot.table)
    .bind(target.id)
    .bind(slot.field)
    .bind(actor)
    .execute(pool)
    .await;
    if let Err(e) = res {
        if let Some(p) = staged_path {
            let _ = tokio::fs::remove_file(p).await;
        }
        return Err(e.into());
    }
    Ok(job_id)
}

async fn read_capped_file(path: &Path, cap: usize) -> Result<Vec<u8>, AppError> {
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|_| AppError::NotFound("Source file not found".into()))?;
    if meta.len() as usize > cap {
        return Err(AppError::BadRequest("File too large".into()));
    }
    tokio::fs::read(path)
        .await
        .map_err(|_| AppError::NotFound("Source file not readable".into()))
}

// ── URL imports (SSRF-guarded) ──────────────────────────────────────────────

pub fn check_url_syntax(u: &url::Url) -> Result<(), AppError> {
    if u.scheme() != "https" {
        return Err(AppError::BadRequest("Image URLs must use https".into()));
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err(AppError::BadRequest(
            "Image URLs must not carry credentials".into(),
        ));
    }
    match u.host() {
        Some(url::Host::Ipv4(ip)) if !is_public_ip(IpAddr::V4(ip)) => Err(AppError::BadRequest(
            "Image URL points at a private address".into(),
        )),
        Some(url::Host::Ipv6(ip)) if !is_public_ip(IpAddr::V6(ip)) => Err(AppError::BadRequest(
            "Image URL points at a private address".into(),
        )),
        Some(url::Host::Domain(d))
            if d.eq_ignore_ascii_case("localhost")
                || d.ends_with(".localhost")
                || d.ends_with(".internal")
                || d.ends_with(".local") =>
        {
            Err(AppError::BadRequest(
                "Image URL points at a private address".into(),
            ))
        }
        None => Err(AppError::BadRequest("Image URL has no host".into())),
        _ => Ok(()),
    }
}

pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || o[0] == 0
                || (o[0] == 100 && (64..=127).contains(&o[1])) // CGNAT
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
                || o[0] >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link local
                || (s[0] == 0x2001 && s[1] == 0x0db8)) // documentation
        }
    }
}

async fn fetch_url(u: &url::Url) -> Result<Vec<u8>, AppError> {
    check_url_syntax(u)?;
    let host = u
        .host_str()
        .ok_or_else(|| AppError::BadRequest("Image URL has no host".into()))?
        .to_string();
    let port = u.port_or_known_default().unwrap_or(443);
    let addrs: Vec<std::net::SocketAddr> =
        tokio::time::timeout(URL_TIMEOUT, tokio::net::lookup_host((host.as_str(), port)))
            .await
            .map_err(|_| AppError::BadRequest("Image URL did not resolve in time".into()))?
            .map_err(|_| AppError::BadRequest("Image URL did not resolve".into()))?
            .collect();
    // Every resolved address must be public, and the request is pinned to the
    // one we checked (no DNS rebinding between check and connect).
    let addr = match addrs.first() {
        Some(a) if addrs.iter().all(|a| is_public_ip(a.ip())) => *a,
        Some(_) => {
            return Err(AppError::BadRequest(
                "Image URL resolves to a private address".into(),
            ));
        }
        None => return Err(AppError::BadRequest("Image URL did not resolve".into())),
    };
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(URL_TIMEOUT)
        .resolve(&host, addr)
        .build()
        .map_err(|_| AppError::Internal)?;
    let mut resp = client
        .get(u.as_str())
        .send()
        .await
        .map_err(|e| AppError::BadRequest(format!("Could not fetch image URL: {e}")))?;
    if !resp.status().is_success() {
        return Err(AppError::BadRequest(format!(
            "Image URL returned {}",
            resp.status()
        )));
    }
    if resp
        .content_length()
        .is_some_and(|l| l as usize > MAX_URL_BYTES)
    {
        return Err(AppError::BadRequest(
            "Image URL too large (max 10 MB)".into(),
        ));
    }
    let mut out = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| AppError::BadRequest(format!("Image URL read failed: {e}")))?
    {
        out.extend_from_slice(&chunk);
        if out.len() > MAX_URL_BYTES {
            return Err(AppError::BadRequest(
                "Image URL too large (max 10 MB)".into(),
            ));
        }
    }
    Ok(out)
}

// ── Conversion (CPU; runs on the blocking pool behind a semaphore) ──────────

/// Heavy work is bounded process-wide: `ASSET_WORKER_CONCURRENCY` (default 1)
/// conversions at a time, whoever asks (worker, preset reconcile, backfill).
fn convert_permits() -> &'static Semaphore {
    static SEM: OnceLock<Semaphore> = OnceLock::new();
    SEM.get_or_init(|| {
        let n = std::env::var("ASSET_WORKER_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|n| *n >= 1)
            .unwrap_or(1);
        Semaphore::new(n)
    })
}

unsafe extern "C" {
    fn WebPGetEncoderVersion() -> std::ffi::c_int;
}

pub fn libwebp_version() -> String {
    // SAFETY: a pure getter from the statically linked libwebp.
    let v = unsafe { WebPGetEncoderVersion() };
    format!("{}.{}.{}", (v >> 16) & 0xff, (v >> 8) & 0xff, v & 0xff)
}

pub fn encoder_string(kind: &str) -> String {
    if kind == "animation" {
        format!(
            "{ENCODER_VERSION} zstd-{} l{ZSTD_LEVEL}",
            zstd::zstd_safe::version_string()
        )
    } else {
        format!(
            "{ENCODER_VERSION} libwebp-{} q{} m{WEBP_METHOD} lanczos3",
            libwebp_version(),
            WEBP_QUALITY as i32
        )
    }
}

#[derive(Clone, Debug)]
pub struct EncodedVariant {
    pub variant: &'static str,
    pub ext: &'static str,
    pub content_type: &'static str,
    pub bytes: Vec<u8>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub has_alpha: bool,
    pub settings: serde_json::Value,
}

pub fn reject_if_animated(raw: &[u8], format: ImageFormat) -> Result<(), AppError> {
    use image::AnimationDecoder;
    let animated = match format {
        ImageFormat::Gif => image::codecs::gif::GifDecoder::new(Cursor::new(raw))
            .map(|d| d.into_frames().take(2).count() > 1)
            .unwrap_or(false),
        ImageFormat::WebP => image::codecs::webp::WebPDecoder::new(Cursor::new(raw))
            .map(|d| d.has_animation())
            .unwrap_or(false),
        ImageFormat::Png => png_has_actl(raw),
        _ => false,
    };
    if animated {
        return Err(AppError::BadRequest(
            "Animated images are not supported — please upload a still picture.".into(),
        ));
    }
    Ok(())
}

/// APNG = a PNG with an `acTL` chunk before the first `IDAT`.
fn png_has_actl(raw: &[u8]) -> bool {
    let mut i = 8usize;
    while i + 8 <= raw.len() {
        let len = u32::from_be_bytes([raw[i], raw[i + 1], raw[i + 2], raw[i + 3]]) as usize;
        let ty = &raw[i + 4..i + 8];
        if ty == b"acTL" {
            return true;
        }
        if ty == b"IDAT" {
            return false;
        }
        i = i.saturating_add(12).saturating_add(len);
    }
    false
}

pub fn has_transparency(img: &DynamicImage) -> bool {
    if !img.color().has_alpha() {
        return false;
    }
    img.to_rgba8().pixels().any(|p| p.0[3] < 250)
}

fn decode_still(raw: &[u8], format: ImageFormat) -> Result<DynamicImage, AppError> {
    reject_if_animated(raw, format)?;
    let dims = ImageReader::with_format(Cursor::new(raw), format)
        .into_dimensions()
        .map_err(|e| AppError::BadRequest(format!("Invalid image: {e}")))?;
    if dims.0 == 0 || dims.1 == 0 {
        return Err(AppError::BadRequest("Image has no pixels".into()));
    }
    if dims.0 > MAX_DECODE_EDGE || dims.1 > MAX_DECODE_EDGE {
        return Err(AppError::BadRequest("Image dimensions too large".into()));
    }
    let mut decoder = ImageReader::with_format(Cursor::new(raw), format)
        .into_decoder()
        .map_err(|e| AppError::BadRequest(format!("Invalid image: {e}")))?;
    let orientation = decoder.orientation().ok();
    let mut img = DynamicImage::from_decoder(decoder)
        .map_err(|e| AppError::BadRequest(format!("Invalid image: {e}")))?;
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }
    Ok(img)
}

fn fit(img: &DynamicImage, edge: u32) -> DynamicImage {
    if img.width().max(img.height()) <= edge {
        img.clone()
    } else {
        img.resize(edge, edge, image::imageops::FilterType::Lanczos3)
    }
}

fn encode_webp(
    img: &DynamicImage,
    alpha: bool,
    lossless: bool,
    edge: u32,
    variant: &'static str,
) -> Result<EncodedVariant, AppError> {
    let (w, h) = (img.width(), img.height());
    let buf;
    let encoder = if alpha {
        buf = img.to_rgba8().into_raw();
        webp::Encoder::from_rgba(&buf, w, h)
    } else {
        buf = img.to_rgb8().into_raw();
        webp::Encoder::from_rgb(&buf, w, h)
    };
    let mut cfg = webp::WebPConfig::new().map_err(|_| AppError::Internal)?;
    cfg.quality = if lossless { 100.0 } else { WEBP_QUALITY };
    cfg.method = WEBP_METHOD;
    cfg.lossless = lossless as i32;
    cfg.alpha_quality = 100;
    cfg.exact = 0;
    let mem = encoder.encode_advanced(&cfg).map_err(|e| {
        tracing::error!(?e, "webp encode failed");
        AppError::Internal
    })?;
    Ok(EncodedVariant {
        variant,
        ext: "webp",
        content_type: "image/webp",
        bytes: mem.to_vec(),
        width: Some(w as i32),
        height: Some(h as i32),
        has_alpha: alpha,
        settings: serde_json::json!({
            "quality": if lossless { 100 } else { WEBP_QUALITY as i32 },
            "method": WEBP_METHOD,
            "lossless": lossless,
            "max_edge": edge,
            "filter": "lanczos3",
            "libwebp_version": libwebp_version(),
        }),
    })
}

/// Pure conversion: bytes in, encoded variants out. Deterministic for the same
/// input and encoder build.
pub fn convert(
    raw: &[u8],
    sniffed: Sniffed,
    purpose: AssetPurpose,
) -> Result<Vec<EncodedVariant>, AppError> {
    match sniffed {
        Sniffed::Lottie => {
            let (w, h) = lottie_meta(raw)
                .ok_or_else(|| AppError::BadRequest("Invalid Lottie JSON".into()))?;
            let z = zstd::bulk::compress(raw, ZSTD_LEVEL).map_err(|_| AppError::Internal)?;
            Ok(vec![EncodedVariant {
                variant: "animation",
                ext: "lottie.zst",
                content_type: "application/zstd",
                bytes: z,
                width: w,
                height: h,
                has_alpha: false,
                settings: serde_json::json!({
                    "codec": "zstd",
                    "level": ZSTD_LEVEL,
                    "zstd_version": zstd::zstd_safe::version_string(),
                }),
            }])
        }
        Sniffed::Image(fmt) => {
            let img = decode_still(raw, fmt)?;
            let alpha = has_transparency(&img);
            let lossless = alpha && purpose.keeps_original();
            // Normalize to 8-bit sRGB-assumed pixels once; ICC/EXIF/XMP never
            // make it past this point.
            let base = if alpha {
                DynamicImage::ImageRgba8(img.to_rgba8())
            } else {
                DynamicImage::ImageRgb8(img.to_rgb8())
            };
            let mut out = Vec::with_capacity(4);
            let full = fit(&base, FULL_EDGE);
            let tile = fit(&full, TILE_EDGE);
            let thumb = fit(&tile, THUMB_EDGE);
            out.push(encode_webp(&thumb, alpha, lossless, THUMB_EDGE, "thumb")?);
            out.push(encode_webp(&tile, alpha, lossless, TILE_EDGE, "tile")?);
            out.push(encode_webp(&full, alpha, lossless, FULL_EDGE, "full")?);
            if purpose.keeps_original() {
                let orig = fit(&base, ORIGINAL_EDGE);
                out.push(encode_webp(&orig, alpha, true, ORIGINAL_EDGE, "original")?);
            }
            Ok(out)
        }
    }
}

// ── Ingest (worker / backfill / presets) ────────────────────────────────────

pub async fn ingest(
    pool: &PgPool,
    org_id: Option<Uuid>,
    purpose: AssetPurpose,
    source: IngestSource,
    source_kind: SourceKind,
    label: Option<&str>,
    actor: Option<Uuid>,
) -> Result<IngestOutcome, AppError> {
    ingest_with(
        pool,
        &AssetStore::from_env(),
        org_id,
        purpose,
        source,
        source_kind,
        label,
        actor,
    )
    .await
}

pub async fn load_source(source: IngestSource) -> Result<Vec<u8>, AppError> {
    match source {
        IngestSource::Bytes(b) => Ok(b.to_vec()),
        IngestSource::StagedFile(p) | IngestSource::LegacyFile(p) => {
            read_capped_file(&p, MAX_RAW_BYTES).await
        }
        IngestSource::Url(u) => fetch_url(&u).await,
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn ingest_with(
    pool: &PgPool,
    store: &AssetStore,
    org_id: Option<Uuid>,
    purpose: AssetPurpose,
    source: IngestSource,
    source_kind: SourceKind,
    label: Option<&str>,
    actor: Option<Uuid>,
) -> Result<IngestOutcome, AppError> {
    let raw = load_source(source).await?;
    ingest_bytes(pool, store, org_id, purpose, raw, source_kind, label, actor).await
}

#[allow(clippy::too_many_arguments)]
pub async fn ingest_bytes(
    pool: &PgPool,
    store: &AssetStore,
    org_id: Option<Uuid>,
    purpose: AssetPurpose,
    raw: Vec<u8>,
    source_kind: SourceKind,
    label: Option<&str>,
    actor: Option<Uuid>,
) -> Result<IngestOutcome, AppError> {
    if org_id.is_none() && !purpose.is_animation() {
        return Err(AppError::BadRequest(
            "An image must belong to an organisation".into(),
        ));
    }
    if raw.len() > MAX_RAW_BYTES {
        return Err(AppError::BadRequest(
            "File too large (max 20 MB raw)".into(),
        ));
    }
    // 1. source_hash of the bytes AS RECEIVED, before anything touches them.
    let source_hash = sha256_hex(&raw);
    let sniffed = sniff(&raw)?;
    check_purpose(purpose, sniffed)?;
    let kind = if purpose.is_animation() {
        "animation"
    } else {
        "image"
    };
    let encoder = encoder_string(kind);

    // 2. Per-org dedup on the source. Scoped to this org only (or to the global
    //    preset namespace): other orgs are never consulted. Never across
    //    profiles: a logo must not reuse the photo group made from the same file
    //    (it has no `original`).
    let profile = purpose.profile();
    if let Some(found) = find_group_by_source(pool, org_id, &source_hash, &encoder, profile).await?
    {
        return Ok(found);
    }

    // 3. Convert, bounded.
    let permit = convert_permits()
        .acquire()
        .await
        .map_err(|_| AppError::Internal)?;
    let variants = tokio::task::spawn_blocking(move || convert(&raw, sniffed, purpose))
        .await
        .map_err(|_| AppError::Internal)??;
    drop(permit);

    // 4. content_hash per file. Byte-identical display variants of ONE group
    //    (an image smaller than the bound) share one row: keep the largest
    //    variant name. A kept `original` always gets its own row, even when it
    //    is the same file as `full` (a small lossless logo): readers ask for
    //    `original` by name, and rows may share a file.
    let mut hashed: Vec<(String, EncodedVariant)> = variants
        .into_iter()
        .map(|v| (sha256_hex(&v.bytes), v))
        .collect();
    let rank = |v: &str| match v {
        "full" => 0,
        "original" => 1,
        "tile" => 2,
        "thumb" => 3,
        _ => 4,
    };
    hashed.sort_by_key(|(_, v)| rank(v.variant));
    let mut seen = std::collections::HashSet::new();
    hashed.retain(|(h, v)| v.variant == "original" || seen.insert(h.clone()));

    // 5. Content dedup, before any file is written: a group of the same profile
    //    in this org whose variants are byte-identical to ours (e.g. the same
    //    photo with different metadata) → reuse that group.
    if let Some((hash, v)) = hashed
        .iter()
        .find(|(_, v)| v.variant == "full" || v.variant == "animation")
    {
        let candidates: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT a.group_id FROM assets a JOIN asset_groups g ON g.id = a.group_id \
             WHERE a.org_id IS NOT DISTINCT FROM $1 AND a.hash = $2 AND a.variant = $3 AND a.encoder = $4 \
               AND g.profile = $5",
        )
        .bind(org_id)
        .bind(hash)
        .bind(v.variant)
        .bind(&encoder)
        .bind(profile)
        .fetch_all(pool)
        .await?;
        let mut ours: Vec<(&str, &str)> = hashed
            .iter()
            .map(|(h, v)| (v.variant, h.as_str()))
            .collect();
        ours.sort();
        for group_id in candidates {
            let variants = group_variants(pool, org_id, group_id).await?;
            let mut theirs: Vec<(&str, &str)> = variants
                .iter()
                .map(|v| (v.variant.as_str(), v.hash.as_str()))
                .collect();
            theirs.sort();
            if theirs == ours {
                return Ok(IngestOutcome {
                    group_id,
                    org_id,
                    variants,
                    deduped: true,
                });
            }
        }
    }

    // 6. Rows and files together (content-addressed; an existing file IS these
    //    bytes, and may already be shared by other groups' rows).
    let group_id = Uuid::new_v4();
    let label = sanitize_label(label);
    let new_group = NewGroup {
        group_id,
        org_id,
        kind,
        source_hash: &source_hash,
        encoder: &encoder,
        profile,
        label: label.as_deref(),
        source_kind,
        actor,
    };
    if let Err(e) = store_group(pool, store, &new_group, &hashed).await {
        let unique =
            matches!(&e, StoreError::Db(sqlx::Error::Database(d)) if d.is_unique_violation());
        // A concurrent ingest (backfill vs worker) of the same source won the
        // race: its group is the answer.
        if unique
            && let Some(found) =
                find_group_by_source(pool, org_id, &source_hash, &encoder, profile).await?
        {
            return Ok(found);
        }
        return Err(match e {
            StoreError::Db(e) => e.into(),
            StoreError::Io(e) => {
                tracing::error!(error = %e, "asset write failed");
                AppError::Internal
            }
        });
    }
    let variants = group_variants(pool, org_id, group_id).await?;
    Ok(IngestOutcome {
        group_id,
        org_id,
        variants,
        deduped: false,
    })
}

struct NewGroup<'a> {
    group_id: Uuid,
    org_id: Option<Uuid>,
    kind: &'a str,
    source_hash: &'a str,
    encoder: &'a str,
    profile: &'a str,
    label: Option<&'a str>,
    source_kind: SourceKind,
    actor: Option<Uuid>,
}

enum StoreError {
    Db(sqlx::Error),
    Io(std::io::Error),
}

impl From<sqlx::Error> for StoreError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

/// Insert the group + its rows and write any missing files, in one transaction
/// that holds the per-file locks. On any failure nothing is committed and every
/// file this attempt created is removed again unless a committed row (another
/// group's) references it by then: a failed attempt leaves no orphan.
async fn store_group(
    pool: &PgPool,
    store: &AssetStore,
    g: &NewGroup<'_>,
    hashed: &[(String, EncodedVariant)],
) -> Result<(), StoreError> {
    let mut files: Vec<(String, &str, &EncodedVariant)> = hashed
        .iter()
        .map(|(h, v)| (AssetStore::key(g.org_id, h, v.ext), h.as_str(), v))
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files.dedup_by(|a, b| a.0 == b.0);

    let mut created: Vec<(String, &'static str)> = Vec::new();
    let res = store_group_tx(pool, store, g, hashed, &files, &mut created).await;
    if res.is_err() && !created.is_empty() {
        // The failed transaction is gone (its locks with it). Re-take the locks
        // so nobody is between "file exists, skip the write" and committing a
        // row that references it, then delete what is still unreferenced.
        let cleanup = async {
            let mut tx = pool.begin().await?;
            created.sort();
            for (hash, ext) in &created {
                super::lock_file_key(&mut tx, &AssetStore::key(g.org_id, hash, ext)).await?;
            }
            for (hash, ext) in &created {
                super::remove_file_if_unreferenced(&mut tx, store, g.org_id, hash, ext).await?;
            }
            tx.commit().await
        };
        if let Err(e) = cleanup.await {
            tracing::error!(error = %e, "asset orphan cleanup failed");
        }
    }
    res
}

async fn store_group_tx(
    pool: &PgPool,
    store: &AssetStore,
    g: &NewGroup<'_>,
    hashed: &[(String, EncodedVariant)],
    files: &[(String, &str, &EncodedVariant)],
    created: &mut Vec<(String, &'static str)>,
) -> Result<(), StoreError> {
    let mut tx = pool.begin().await?;
    for (key, _, _) in files {
        super::lock_file_key(&mut tx, key).await?;
    }
    sqlx::query(
        "INSERT INTO asset_groups (id, org_id, kind, source_hash, encoder, profile, label) VALUES ($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(g.group_id)
    .bind(g.org_id)
    .bind(g.kind)
    .bind(g.source_hash)
    .bind(g.encoder)
    .bind(g.profile)
    .bind(g.label)
    .execute(&mut *tx)
    .await?;
    for (hash, v) in hashed {
        sqlx::query(
            "INSERT INTO assets (org_id, hash, group_id, encoder, encoder_settings, kind, variant, ext, \
                                 content_type, bytes, width, height, has_alpha, source_hash, source_kind, label, created_by) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17)",
        )
        .bind(g.org_id)
        .bind(hash)
        .bind(g.group_id)
        .bind(g.encoder)
        .bind(&v.settings)
        .bind(g.kind)
        .bind(v.variant)
        .bind(v.ext)
        .bind(v.content_type)
        .bind(v.bytes.len() as i64)
        .bind(v.width)
        .bind(v.height)
        .bind(v.has_alpha)
        .bind(g.source_hash)
        .bind(g.source_kind.as_str())
        .bind(g.label)
        .bind(g.actor)
        .execute(&mut *tx)
        .await?;
    }
    for (key, hash, v) in files {
        let path = store.path_for_key(key);
        let bytes = v.bytes.clone();
        let wrote = tokio::task::spawn_blocking(move || -> std::io::Result<bool> {
            match std::fs::metadata(&path) {
                Ok(m) if m.len() == bytes.len() as u64 => Ok(false),
                _ => write_atomic(&path, &bytes).map(|_| true),
            }
        })
        .await
        .map_err(|e| StoreError::Io(std::io::Error::other(e)))?;
        match wrote {
            Ok(true) => created.push((hash.to_string(), v.ext)),
            Ok(false) => {}
            Err(e) => return Err(StoreError::Io(e)),
        }
    }
    #[cfg(test)]
    if g.org_id
        .is_some_and(|o| FAIL_BEFORE_COMMIT.lock().unwrap().contains(&o))
    {
        return Err(StoreError::Io(std::io::Error::other(
            "injected failure before commit",
        )));
    }
    tx.commit().await?;
    Ok(())
}

/// Test hook: `store_group` for these orgs fails after writing its files.
#[cfg(test)]
pub(crate) static FAIL_BEFORE_COMMIT: std::sync::Mutex<Vec<Uuid>> =
    std::sync::Mutex::new(Vec::new());

async fn find_group_by_source(
    pool: &PgPool,
    org_id: Option<Uuid>,
    source_hash: &str,
    encoder: &str,
    profile: &str,
) -> Result<Option<IngestOutcome>, AppError> {
    let gid: Option<Uuid> = sqlx::query_scalar(
        "SELECT g.id FROM asset_groups g WHERE g.org_id IS NOT DISTINCT FROM $1 AND g.source_hash = $2 \
           AND g.encoder = $3 AND g.profile = $4 \
           AND EXISTS (SELECT 1 FROM assets a WHERE a.group_id = g.id) \
         ORDER BY g.created_at LIMIT 1",
    )
    .bind(org_id)
    .bind(source_hash)
    .bind(encoder)
    .bind(profile)
    .fetch_optional(pool)
    .await?;
    let Some(group_id) = gid else { return Ok(None) };
    let variants = group_variants(pool, org_id, group_id).await?;
    Ok(Some(IngestOutcome {
        group_id,
        org_id,
        variants,
        deduped: true,
    }))
}

pub(crate) const ASSET_COLS: &str = "a.id, a.org_id, a.group_id, a.hash, a.kind, a.variant, a.ext, a.content_type, a.bytes, a.width, a.height, a.has_alpha";

pub(crate) fn asset_ref_from_row(r: &sqlx::postgres::PgRow, ttl: Duration) -> AssetRef {
    let org_id: Option<Uuid> = r.get("org_id");
    let hash: String = r.get("hash");
    let ext: String = r.get("ext");
    AssetRef {
        id: r.get("id"),
        org_id,
        group_id: r.get("group_id"),
        url: signed_url(org_id, &hash, &ext, ttl),
        hash,
        kind: r.get("kind"),
        variant: r.get("variant"),
        ext,
        content_type: r.get("content_type"),
        bytes: r.get("bytes"),
        width: r.get("width"),
        height: r.get("height"),
        has_alpha: r.get("has_alpha"),
    }
}

pub const DASHBOARD_TTL: Duration = Duration::from_secs(24 * 3600);
pub const PUBLIC_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
pub const WALLET_TTL: Duration = Duration::from_secs(365 * 24 * 3600);

pub async fn group_variants(
    pool: &PgPool,
    org_id: Option<Uuid>,
    group_id: Uuid,
) -> Result<Vec<AssetRef>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {ASSET_COLS} FROM assets a WHERE a.group_id = $1 AND a.org_id IS NOT DISTINCT FROM $2 \
         ORDER BY CASE a.variant WHEN 'thumb' THEN 0 WHEN 'tile' THEN 1 WHEN 'full' THEN 2 WHEN 'original' THEN 3 ELSE 4 END"
    ))
    .bind(group_id)
    .bind(org_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| asset_ref_from_row(r, DASHBOARD_TTL))
        .collect())
}

// ── Attach ──────────────────────────────────────────────────────────────────

/// The legacy uploads-relative path minted for a group so old clients (which
/// read `image_url`) get a stable URL. Served by the `/uploads` fallback as a
/// 302 to a signed URL of the `full` variant.
pub fn synthesized_legacy_rel(org_id: Uuid, legacy_dir: &str, group_id: Uuid) -> String {
    format!("{org_id}/{legacy_dir}/{group_id}.webp")
}

fn uploads_base() -> String {
    std::env::var("UPLOADS_BASE_URL")
        .map(|b| b.trim_end_matches('/').to_string())
        .unwrap_or_else(|_| "/uploads".into())
}

/// Point a row at the outcome's group. The legacy URL column is written ONLY
/// when it is NULL or holds a URL this function minted earlier; a legacy URL
/// from before the asset store is never overwritten.
pub async fn attach(
    conn: &mut sqlx::PgConnection,
    target: &AssetTarget,
    outcome: &IngestOutcome,
) -> Result<(), AppError> {
    let slot = target.slot()?;
    if slot.table == "recipe_step_presets" {
        return Err(AppError::BadRequest(
            "recipe step presets are attached by slug in recipes::steps::reconcile".into(),
        ));
    }
    let org_id = outcome
        .org_id
        .ok_or_else(|| AppError::BadRequest("image assets belong to an org".into()))?;
    let row_org: Option<Uuid> = if slot.table == "organizations" {
        sqlx::query_scalar("SELECT id FROM organizations WHERE id = $1")
            .bind(target.id)
            .fetch_optional(&mut *conn)
            .await?
    } else {
        sqlx::query_scalar(&format!("SELECT org_id FROM {} WHERE id = $1", slot.table))
            .bind(target.id)
            .fetch_optional(&mut *conn)
            .await?
    };
    match row_org {
        None => return Err(AppError::NotFound(format!("{} row not found", slot.table))),
        Some(o) if o != org_id => {
            return Err(AppError::Forbidden(
                "asset belongs to a different org".into(),
            ));
        }
        _ => {}
    }
    sqlx::query(&format!(
        "UPDATE {} SET {} = $1 WHERE id = $2",
        slot.table, slot.group_col
    ))
    .bind(outcome.group_id)
    .bind(target.id)
    .execute(&mut *conn)
    .await?;

    if let (Some(legacy_col), Some(full)) = (slot.legacy_col, outcome.full()) {
        let current: Option<String> = sqlx::query_scalar(&format!(
            "SELECT {legacy_col} FROM {} WHERE id = $1",
            slot.table
        ))
        .bind(target.id)
        .fetch_one(&mut *conn)
        .await?;
        let minted = match current.as_deref() {
            None => true,
            Some(url) => is_minted_legacy_url(&mut *conn, org_id, slot.legacy_dir, url).await?,
        };
        if minted {
            let rel = synthesized_legacy_rel(org_id, slot.legacy_dir, outcome.group_id);
            let url = format!("{}/{}", uploads_base(), rel);
            sqlx::query(
                "INSERT INTO asset_legacy_paths (legacy_path, org_id, asset_id) VALUES ($1,$2,$3) \
                 ON CONFLICT (legacy_path) DO UPDATE SET asset_id = EXCLUDED.asset_id",
            )
            .bind(&rel)
            .bind(org_id)
            .bind(full.id)
            .execute(&mut *conn)
            .await?;
            let extra = if slot.legacy_col == Some("logo_url") {
                ", brand_logo_source = $1"
            } else {
                ""
            };
            sqlx::query(&format!(
                "UPDATE {} SET {legacy_col} = $1{extra} WHERE id = $2",
                slot.table
            ))
            .bind(&url)
            .bind(target.id)
            .execute(&mut *conn)
            .await?;
        }
    }
    mark_org_bundles_dirty(&mut *conn, org_id).await?;
    Ok(())
}

async fn is_minted_legacy_url(
    conn: &mut sqlx::PgConnection,
    org_id: Uuid,
    legacy_dir: &str,
    url: &str,
) -> Result<bool, AppError> {
    let Some(rel) = super::legacy_rel_from_url(url) else {
        return Ok(false);
    };
    let prefix = format!("{org_id}/{legacy_dir}/");
    let Some(stem) = rel
        .strip_prefix(&prefix)
        .and_then(|f| f.strip_suffix(".webp"))
    else {
        return Ok(false);
    };
    let Ok(gid) = Uuid::parse_str(stem) else {
        return Ok(false);
    };
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM asset_groups WHERE id = $1 AND org_id = $2)",
    )
    .bind(gid)
    .bind(org_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(exists)
}

pub async fn mark_org_bundles_dirty(
    conn: &mut sqlx::PgConnection,
    org_id: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO asset_bundle_dirty (branch_id, dirty_since) \
         SELECT id, now() FROM branches WHERE org_id = $1 \
         ON CONFLICT (branch_id) DO UPDATE SET dirty_since = now()",
    )
    .bind(org_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

// ── Readers ─────────────────────────────────────────────────────────────────

pub async fn read_asset_bytes(
    pool: &PgPool,
    org_id: Option<Uuid>,
    asset_id: Uuid,
) -> Result<(AssetRef, Vec<u8>), AppError> {
    read_asset_bytes_with(pool, &AssetStore::from_env(), org_id, asset_id).await
}

pub async fn read_asset_bytes_with(
    pool: &PgPool,
    store: &AssetStore,
    org_id: Option<Uuid>,
    asset_id: Uuid,
) -> Result<(AssetRef, Vec<u8>), AppError> {
    let row = sqlx::query(&format!(
        "SELECT {ASSET_COLS} FROM assets a WHERE a.id = $1 AND (a.org_id IS NOT DISTINCT FROM $2 OR a.org_id IS NULL)"
    ))
    .bind(asset_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Asset not found".into()))?;
    let r = asset_ref_from_row(&row, DASHBOARD_TTL);
    let path = store.path_for_key(&AssetStore::key(r.org_id, &r.hash, &r.ext));
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| AppError::NotFound("Asset file missing".into()))?;
    Ok((r, bytes))
}

// ── Signed URLs ─────────────────────────────────────────────────────────────

type HmacSha256 = Hmac<Sha256>;

/// `exp` rounded UP to the next 24 h boundary, so a URL (and the browser cache
/// key) is stable for a day.
pub fn bucketed_exp(now: i64, ttl: Duration) -> i64 {
    const DAY: i64 = 86_400;
    let raw = now + ttl.as_secs() as i64;
    ((raw + DAY - 1) / DAY) * DAY
}

pub fn sign(key: &str, exp: i64) -> String {
    let mut mac = HmacSha256::new_from_slice(&super::url_secret()).expect("hmac accepts any key");
    mac.update(format!("{key}:{exp}").as_bytes());
    format!("{:x}", mac.finalize().into_bytes())
}

pub fn signed_url(org_id: Option<Uuid>, hash: &str, ext: &str, ttl: Duration) -> String {
    let key = AssetStore::key(org_id, hash, ext);
    let exp = bucketed_exp(chrono::Utc::now().timestamp(), ttl);
    format!(
        "{}/assets/{key}?exp={exp}&sig={}",
        super::api_base(),
        sign(&key, exp)
    )
}

/// Constant-time check of `sig` for `key` at `exp` (expiry checked by caller).
pub fn verify_signature(key: &str, exp: i64, sig: &str) -> bool {
    let Some(sig_bytes) = decode_hex(sig) else {
        return false;
    };
    let mut mac = HmacSha256::new_from_slice(&super::url_secret()).expect("hmac accepts any key");
    mac.update(format!("{key}:{exp}").as_bytes());
    mac.verify_slice(&sig_bytes).is_ok()
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() != 64 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}
