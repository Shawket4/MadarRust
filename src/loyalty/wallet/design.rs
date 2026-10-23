//! A designed card (REWARDS_DASHBOARD_DESIGN.md §3): what a shop built in the
//! dashboard's card designer, worn by its passes.
//!
//! Opt-in. With nothing published and one stamp track, [`for_member`] answers
//! `None` and every pass is built by the classic code path, byte for byte.
//! With a published design — or more than one track, which gets madar-card's
//! `multi` template until the shop publishes one — this module supplies:
//!
//! - the strip (Apple, @1/2/3x and `ar.lproj`) and the hero (Google): madar-card
//!   draws the SVG, resvg rasterises it with ONLY the bundled faces
//!   (`assets/fonts`: Manrope SemiBold, Cairo Medium, SIL OFL 1.1 — the QR
//!   card's font database), over the shop's photograph (scrimmed where Apple
//!   writes a primary field, the classic look's own `scrim`), with the static
//!   footer and the logo composited on their cells;
//! - which data each OS-drawn field shows ([`DesignedCard::restyle_apple`],
//!   [`DesignedCard::restyle_google`]) and the colours;
//! - the Google hero's URL, per progress (`/public/loyalty/brand/{org}/card/…`).
//!
//! What the OS draws (field text, the QR) stays the OS's.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use madar_card::render::{CardState, SvgOpts, TrackState};
use madar_card::{CardDesign, FieldSource, GroundKind, Store};
use resvg::{tiny_skia, usvg};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::MemberRow;
use crate::loyalty::earn::Mode;
use crate::loyalty::settings::LoyaltySettings;
use crate::loyalty::tracks::TrackDef;
use crate::orgs::branding::OrgBrand;

/// A design as one shop's cards wear it, with one member's figures.
#[derive(Debug, Clone)]
pub struct DesignedCard {
    pub org_id: Uuid,
    /// The published revision; 0 for the automatic multi-track design.
    pub revision: i32,
    pub design: CardDesign,
    pub defs: Vec<TrackDef>,
    pub mode: Mode,
    pub track_one_size: i32,
    pub programme: String,
    pub programme_ar: Option<String>,
    /// The shop's photograph and logo (upload URLs), when it may wear them.
    pub photo_url: Option<String>,
    pub logo_url: Option<String>,
    pub logo_is_mark: bool,
    /// This member: balance per track (track 1 included).
    pub balances: HashMap<u8, i32>,
    pub member_name: String,
}

/// The org's design as stored: `(revision, published)`.
pub async fn published(pool: &PgPool, org_id: Uuid) -> Option<(i32, CardDesign)> {
    let row: Option<(i32, Option<Value>)> =
        sqlx::query_as("SELECT revision, published FROM loyalty_card_designs WHERE org_id = $1")
            .bind(org_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    let (rev, json) = row?;
    let d: CardDesign = serde_json::from_value(json?).ok()?;
    Some((rev, d))
}

/// The pass colours a shop's card has today (the classic look's).
pub fn colours_of(brand: &OrgBrand) -> madar_card::Colours {
    madar_card::Colours {
        background: brand.palette.background.clone(),
        foreground: brand.palette.foreground.clone(),
        label: brand.palette.accent.clone(),
    }
}

/// What a design is checked against for this shop.
pub fn ctx_of(defs: &[TrackDef], mode: Mode, track_one_size: i32, brand: &OrgBrand) -> madar_card::Ctx {
    madar_card::Ctx {
        mode: match mode {
            Mode::Points => madar_card::tracks::Mode::Points,
            Mode::Visits => madar_card::tracks::Mode::Visits,
        },
        tracks: defs
            .iter()
            .map(|d| madar_card::TrackRef {
                position: d.position,
                size: if d.position == 1 { track_one_size } else { d.size.unwrap_or(1) },
            })
            .collect(),
        has_photo: brand.custom_branding && brand.card_image_url.is_some(),
    }
}

/// A shop without its own branding wears Madar's palette and no photograph,
/// whatever a design says (the branding tier is a super admin's switch).
pub fn enforce_branding(mut d: CardDesign, brand: &OrgBrand) -> CardDesign {
    if !brand.custom_branding {
        d.colours = colours_of(brand);
        if d.ground.kind == GroundKind::Photo {
            d.ground.kind = GroundKind::Colour;
        }
    }
    d
}

/// The design this member's card wears, or `None` for the classic look.
pub async fn for_member(
    pool: &PgPool,
    member: &MemberRow,
    settings: &LoyaltySettings,
    brand: &OrgBrand,
) -> Option<Arc<DesignedCard>> {
    let mode = settings.mode();
    let defs = crate::loyalty::tracks::load(pool, member.org_id, mode).await.ok()?;
    let rewards = if settings.reward_any_item {
        Vec::new()
    } else {
        crate::loyalty::settings::rewards_org_in(pool, member.org_id, settings)
            .await
            .unwrap_or_default()
    };
    let track_one_size = crate::loyalty::model::reward_target(settings, &rewards);
    let (revision, design) = match published(pool, member.org_id).await {
        Some(p) => p,
        None if defs.len() > 1 => {
            let ctx = ctx_of(&defs, mode, track_one_size, brand);
            let tracks: Vec<madar_card::Track> = defs.iter().map(TrackDef::to_card).collect();
            (0, madar_card::templates::default_design(&colours_of(brand), &ctx, &tracks))
        }
        None => return None,
    };
    let mut balances = crate::loyalty::tracks::balances(pool, member.id).await.ok()?;
    balances.insert(1, member.balance_in(mode));
    Some(Arc::new(DesignedCard {
        org_id: member.org_id,
        revision,
        design: enforce_branding(design, brand),
        defs,
        mode,
        track_one_size,
        programme: settings.program_name.clone(),
        programme_ar: settings.program_name_ar.clone(),
        photo_url: brand.custom_branding.then(|| brand.card_image_url.clone()).flatten(),
        logo_url: brand.custom_branding.then(|| brand.logo_url.clone()).flatten(),
        logo_is_mark: brand.logo_is_mark,
        balances,
        member_name: member.name.clone(),
    }))
}

/// A track's figures for the strip.
fn states(defs: &[TrackDef], track_one_size: i32, balances: &HashMap<u8, i32>) -> Vec<TrackState> {
    defs.iter()
        .map(|d| TrackState {
            position: d.position,
            size: if d.position == 1 { track_one_size } else { d.size.unwrap_or(1) },
            balance: balances.get(&d.position).copied().unwrap_or(0),
            colour: d.colour.clone(),
            shape: d
                .shape
                .as_deref()
                .and_then(|s| serde_json::from_value(Value::String(s.into())).ok()),
        })
        .collect()
}

impl DesignedCard {
    pub fn state(&self, arabic: bool) -> CardState {
        CardState {
            tracks: states(&self.defs, self.track_one_size, &self.balances),
            arabic,
        }
    }

    fn def(&self, t: u8) -> Option<&TrackDef> {
        self.defs.iter().find(|d| d.position == t)
    }

    fn size(&self, t: u8) -> i32 {
        if t == 1 {
            self.track_one_size
        } else {
            self.def(t).and_then(|d| d.size).unwrap_or(1)
        }
    }

    fn balance(&self, t: u8) -> i32 {
        self.balances.get(&t).copied().unwrap_or(0)
    }

    /// A track's name for a label: its own, else what track 1 always said.
    fn track_label(&self, t: u8, fallback: &str) -> String {
        self.def(t)
            .map(|d| d.name_en.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| fallback.to_string())
    }

    /// `(key, English label, value)` for a field, or `None` when it has
    /// nothing to say (a rewards row with none earned; a missing track).
    pub fn field(&self, src: FieldSource) -> Option<(String, String, String)> {
        if let Some(t) = src.track() {
            self.def(t)?;
        }
        let balance_word = super::google::balance_label(self.mode);
        match src {
            FieldSource::Balance { track } => Some((
                format!("balance_t{track}"),
                self.track_label(track, if track == 1 { balance_word } else { "Stamps" }),
                self.balance(track).to_string(),
            )),
            FieldSource::Progress { track } => Some((
                format!("progress_t{track}"),
                self.track_label(track, &self.programme),
                madar_card::text::progress_line(self.balance(track), self.size(track)),
            )),
            FieldSource::Rewards { track } => {
                let line = madar_card::text::earned_line(self.balance(track), self.size(track))?;
                let n = madar_card::tracks::track_card(self.balance(track), self.size(track)).rewards_ready;
                let ready = if n > 1 { "Rewards ready" } else { "Reward ready" };
                let label = match self.def(track).map(|d| d.name_en.trim()).filter(|s| !s.is_empty()) {
                    Some(name) if track > 1 => format!("{name} · {ready}"),
                    _ => ready.to_string(),
                };
                Some((format!("rewards_t{track}"), label, line))
            }
            FieldSource::Member => Some(("member".into(), "Member".into(), self.member_name.clone())),
        }
    }

    /// The English → Arabic pairs the designed fields add to `pass.strings`.
    pub fn strings(&self) -> Vec<super::i18n::Pair> {
        let mut out = vec![
            super::i18n::Pair { en: "Reward ready".into(), ar: "مكافأة جاهزة".into() },
            super::i18n::Pair { en: "Rewards ready".into(), ar: "مكافآت جاهزة".into() },
            super::i18n::Pair { en: "Stamps".into(), ar: "الأختام".into() },
        ];
        for d in self.defs.iter().filter(|d| !d.name_en.trim().is_empty() && !d.name_ar.trim().is_empty()) {
            out.push(super::i18n::Pair { en: d.name_en.trim().into(), ar: d.name_ar.trim().into() });
            for ready in [("Reward ready", "مكافأة جاهزة"), ("Rewards ready", "مكافآت جاهزة")] {
                out.push(super::i18n::Pair {
                    en: format!("{} · {}", d.name_en.trim(), ready.0),
                    ar: format!("{} · {}", d.name_ar.trim(), ready.1),
                });
            }
        }
        out.retain(|p| p.en != p.ar);
        out
    }

    /// Apple: the field groups and colours the design names. The notice a
    /// message put on the card stays in the auxiliary row, last.
    pub fn restyle_apple(&self, pass: &mut Value) {
        let notice: Vec<Value> = pass["storeCard"]["auxiliaryFields"]
            .as_array()
            .map(|a| a.iter().filter(|f| f["key"] == "notice").cloned().collect())
            .unwrap_or_default();
        let card = &mut pass["storeCard"];
        let obj = card.as_object_mut();
        if let Some(o) = obj {
            for k in ["headerFields", "primaryFields", "secondaryFields", "auxiliaryFields"] {
                o.remove(k);
            }
        }
        let render = |list: &[FieldSource]| -> Vec<Value> {
            list.iter()
                .filter_map(|s| self.field(*s))
                .map(|(key, label, value)| json!({ "key": key, "label": label, "value": value }))
                .collect()
        };
        let a = &self.design.apple;
        let mut groups: Vec<(&str, Vec<Value>)> = vec![
            ("headerFields", render(&a.header)),
            ("primaryFields", render(&a.primary.into_iter().collect::<Vec<_>>())),
            ("secondaryFields", render(&a.secondary)),
            ("auxiliaryFields", render(&a.auxiliary)),
        ];
        groups[3].1.extend(notice);
        for (k, v) in groups {
            // An empty group reserves an empty row on the card: left out.
            if !v.is_empty() {
                card[k] = Value::Array(v);
            }
        }
        let c = &self.design.colours;
        pass["backgroundColor"] = json!(rgb(&c.background));
        pass["foregroundColor"] = json!(rgb(&c.foreground));
        pass["labelColor"] = json!(rgb(&c.label));
    }

    /// Google: its two face slots, localized like the classic card's.
    pub fn restyle_google(&self, object: &mut Value) {
        let ar: HashMap<String, String> = self.strings().into_iter().map(|p| (p.en, p.ar)).collect();
        let classic: HashMap<String, String> = [(super::google::balance_label(self.mode), match self.mode {
            Mode::Points => "نقاط",
            Mode::Visits => "طلبات",
        })]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let slot = |src: FieldSource| -> Value {
            match self.field(src) {
                None => Value::Null,
                Some((_, label, value)) => {
                    let mut v = json!({ "label": label, "balance": { "string": value } });
                    if let Some(a) = ar.get(&label).or_else(|| classic.get(&label)) {
                        v["localizedLabel"] = json!({
                            "defaultValue": { "language": "en", "value": label },
                            "translatedValues": [{ "language": "ar", "value": a }]
                        });
                    }
                    v
                }
            }
        };
        let g = &self.design.google;
        let first = slot(g.points);
        // An empty first slot (no rewards yet) takes the second's place.
        let second = g.secondary.map(slot).unwrap_or(Value::Null);
        if first.is_null() {
            object["loyaltyPoints"] = second;
            object["secondaryLoyaltyPoints"] = Value::Null;
        } else {
            object["loyaltyPoints"] = first;
            object["secondaryLoyaltyPoints"] = second;
        }
        if let Some(o) = object.as_object_mut() {
            o.retain(|_, v| !v.is_null());
        }
    }

    /// The progress key in the Google hero's URL: each track's progress on
    /// its current card, `1-0-4`, and the language.
    pub fn hero_state(&self, arabic: bool) -> String {
        let p: Vec<String> = self
            .defs
            .iter()
            .map(|d| {
                madar_card::tracks::track_card(self.balance(d.position), self.size(d.position))
                    .progress_to_next
                    .to_string()
            })
            .collect();
        format!("{}-{}", p.join("-"), if arabic { "ar" } else { "en" })
    }

    /// Google's `heroImage` for this member: the designed hero at their
    /// progress, in their language.
    pub fn google_hero(&self, arabic: bool) -> Option<Value> {
        let uri = super::absolute_api_url(&format!(
            "/public/loyalty/brand/{}/card/{}/{}.png",
            self.org_id,
            self.revision,
            self.hero_state(arabic)
        ))?;
        Some(json!({ "sourceUri": { "uri": uri } }))
    }

    /// What a cached image of this card depends on.
    fn cache_key(&self, what: &str) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        what.hash(&mut h);
        self.org_id.hash(&mut h);
        self.revision.hash(&mut h);
        serde_json::to_string(&self.design).unwrap_or_default().hash(&mut h);
        format!("{:?}", states(&self.defs, self.track_one_size, &self.balances)).hash(&mut h);
        self.photo_url.hash(&mut h);
        self.logo_url.hash(&mut h);
        h.finish()
    }

    /// The pass's strips: `strip.png` @1/2/3x and their `ar.lproj` copies.
    pub async fn apple_strips(self: &Arc<Self>) -> Arc<Vec<(String, Vec<u8>)>> {
        let key = self.cache_key("apple");
        let me = Arc::clone(self);
        let build = async move {
            tokio::task::spawn_blocking(move || Arc::new(me.apple_strips_now()))
                .await
                .unwrap_or_else(|_| Arc::new(Vec::new()))
        };
        if cfg!(test) {
            return build.await;
        }
        IMAGES.get_with(key, build).await
    }

    pub fn apple_strips_now(&self) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::with_capacity(6);
        for scale in 1..=3u32 {
            let name = if scale == 1 { "strip.png".to_string() } else { format!("strip@{scale}x.png") };
            for (arabic, path) in [(false, name.clone()), (true, format!("ar.lproj/{name}"))] {
                if let Some(png) = render_png(self, Store::Apple, scale, arabic) {
                    out.push((path, png));
                }
            }
        }
        out
    }
}

/// Rendered strips and heroes, per (org, revision, design, progress, pictures).
static IMAGES: LazyLock<moka::future::Cache<u64, Arc<Vec<(String, Vec<u8>)>>>> = LazyLock::new(|| {
    moka::future::Cache::builder()
        .max_capacity(2048)
        .time_to_live(Duration::from_secs(3600))
        .build()
});

fn rgb(hex: &str) -> String {
    match crate::orgs::branding::parse_hex(hex) {
        Some((r, g, b)) => format!("rgb({r}, {g}, {b})"),
        None => "rgb(255, 255, 255)".into(),
    }
}

/// Rasterise madar-card's SVG with the bundled faces only.
fn rasterize(svg: &str, w: u32, h: u32) -> Option<image::RgbaImage> {
    let opt = usvg::Options {
        fontdb: crate::qr_card::render::fontdb(),
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_str(svg, &opt).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(w, h)?;
    resvg::render(&tree, tiny_skia::Transform::identity(), &mut pixmap.as_mut());
    let mut img = image::RgbaImage::new(w, h);
    for (i, p) in pixmap.pixels().iter().enumerate() {
        let c = p.demultiply();
        let (x, y) = ((i as u32) % w, (i as u32) / w);
        img.put_pixel(x, y, image::Rgba([c.red(), c.green(), c.blue(), c.alpha()]));
    }
    Some(img)
}

/// One image of the card: Apple's strip at `scale`, or Google's hero.
pub fn render_image(card: &DesignedCard, store: Store, scale: u32, arabic: bool) -> Option<image::RgbaImage> {
    let canvas = madar_card::grid::Canvas::of(store, scale);
    let (w, h) = (canvas.w, canvas.h);
    let d = &card.design;
    let primary = store == Store::Apple && d.apple.primary.is_some();

    // The ground: the shop's photograph (veiled by the SVG, scrimmed where
    // Apple writes the primary field), else transparent under the SVG's own.
    let mut base = image::RgbaImage::from_pixel(w, h, image::Rgba([0, 0, 0, 0]));
    if d.ground.kind == GroundKind::Photo {
        if let Some(img) = card.photo_url.as_deref().and_then(crate::orgs::branding::read_upload) {
            let mut scaled = img
                .resize_to_fill(w, h, image::imageops::FilterType::CatmullRom)
                .to_rgba8();
            if primary {
                // Apple writes the primary field from the leading edge.
                if arabic {
                    image::imageops::flip_horizontal_in_place(&mut scaled);
                }
                super::apple::scrim(&mut scaled, &d.colours.foreground);
                if arabic {
                    image::imageops::flip_horizontal_in_place(&mut scaled);
                }
            }
            base = scaled;
        } else {
            // A photograph that went missing: the card's own ground.
            base = super::look::ground(w, h, &d.colours.background);
        }
    }
    let svg = madar_card::render::svg(d, &card.state(arabic), canvas, &SvgOpts::default());
    let layer = rasterize(&svg, w, h)?;
    image::imageops::overlay(&mut base, &layer, 0, 0);

    // The footer, the endorsement every design carries.
    let light = crate::orgs::branding::parse_hex(&d.colours.foreground)
        .map(|(r, g, b)| crate::orgs::branding::luminance(r, g, b) > 0.5)
        .unwrap_or(true);
    let f = madar_card::render::footer_rect(canvas, arabic);
    if let Some(mut foot) = super::look::footer(scale.clamp(1, 3).max(if store == Store::Google { 3 } else { 1 }), light, arabic) {
        if store == Store::Google {
            let fh = f.h.round().max(1.0) as u32;
            let fw = (foot.width() as f64 * fh as f64 / foot.height().max(1) as f64).round() as u32;
            foot = image::imageops::resize(&foot, fw, fh, image::imageops::FilterType::Lanczos3);
        }
        let x = if arabic { (f.x + f.w) as i64 - foot.width() as i64 } else { f.x as i64 };
        let y = (f.y + (f.h - foot.height() as f64) / 2.0).round() as i64;
        image::imageops::overlay(&mut base, &foot, x, y);
    }

    // The logo cells: the shop's mark (tinted like the header logo), else the present.
    for p in madar_card::render::layout(d, canvas, arabic, false) {
        if p.what != madar_card::render::Placed::Logo {
            continue;
        }
        let logo = match card.logo_url.as_deref().and_then(crate::orgs::branding::read_logo) {
            Some(img) if card.logo_is_mark => crate::orgs::branding::tint_mark(&img, &d.colours.foreground),
            Some(img) => img,
            None => match image::load_from_memory(super::look::present_logo(&d.colours.background)) {
                Ok(i) => i,
                Err(_) => continue,
            },
        };
        let fitted = logo.resize(
            p.rect.w.max(1.0) as u32,
            p.rect.h.max(1.0) as u32,
            image::imageops::FilterType::Lanczos3,
        );
        let x = p.rect.x + (p.rect.w - f64::from(fitted.width())) / 2.0;
        let y = p.rect.y + (p.rect.h - f64::from(fitted.height())) / 2.0;
        image::imageops::overlay(&mut base, &fitted.to_rgba8(), x.round() as i64, y.round() as i64);
    }

    // A pass image is opaque: whatever is still transparent takes the ground.
    let (br, bg, bb) = crate::orgs::branding::parse_hex(&d.colours.background).unwrap_or((0x0D, 0x62, 0x73));
    for px in base.pixels_mut() {
        let a = f64::from(px.0[3]) / 255.0;
        let mix = |c: u8, g: u8| (f64::from(c) * a + f64::from(g) * (1.0 - a)).round() as u8;
        px.0 = [mix(px.0[0], br), mix(px.0[1], bg), mix(px.0[2], bb), 255];
    }
    Some(base)
}

pub fn render_png(card: &DesignedCard, store: Store, scale: u32, arabic: bool) -> Option<Vec<u8>> {
    let img = render_image(card, store, scale, arabic)?;
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .ok()?;
    Some(out.into_inner())
}

/// The Google hero for an org at a revision and a progress state, for the
/// public route. Every number in `state` is checked against the shop's
/// tracks, so the route renders nothing a card could not show.
pub async fn hero_for_state(pool: &PgPool, org_id: Uuid, revision: i32, state: &str) -> Option<Vec<u8>> {
    let (progress, lang) = state.rsplit_once('-')?;
    let arabic = match lang {
        "ar" => true,
        "en" => false,
        _ => return None,
    };
    let settings = crate::loyalty::settings::load_scope(pool, org_id, None).await.ok()??;
    let brand = crate::orgs::branding::load(pool, org_id).await.ok()?;
    let mode = settings.mode();
    let defs = crate::loyalty::tracks::load(pool, org_id, mode).await.ok()?;
    let rewards = if settings.reward_any_item {
        Vec::new()
    } else {
        crate::loyalty::settings::rewards_org_in(pool, org_id, &settings).await.unwrap_or_default()
    };
    let track_one_size = crate::loyalty::model::reward_target(&settings, &rewards);
    let design = match published(pool, org_id).await {
        Some((rev, d)) if rev == revision => d,
        None if revision == 0 && defs.len() > 1 => {
            let ctx = ctx_of(&defs, mode, track_one_size, &brand);
            let tracks: Vec<madar_card::Track> = defs.iter().map(TrackDef::to_card).collect();
            madar_card::templates::default_design(&colours_of(&brand), &ctx, &tracks)
        }
        _ => return None,
    };
    let parts: Vec<&str> = progress.split('-').collect();
    if parts.len() != defs.len() {
        return None;
    }
    let mut balances = HashMap::new();
    for (d, p) in defs.iter().zip(parts) {
        let v: i32 = p.parse().ok()?;
        let size = if d.position == 1 { track_one_size } else { d.size.unwrap_or(1) };
        if v < 0 || v >= size.max(1) {
            return None;
        }
        balances.insert(d.position, v);
    }
    let card = Arc::new(DesignedCard {
        org_id,
        revision,
        design: enforce_branding(design, &brand),
        defs,
        mode,
        track_one_size,
        programme: settings.program_name.clone(),
        programme_ar: settings.program_name_ar.clone(),
        photo_url: brand.custom_branding.then(|| brand.card_image_url.clone()).flatten(),
        logo_url: brand.custom_branding.then(|| brand.logo_url.clone()).flatten(),
        logo_is_mark: brand.logo_is_mark,
        balances,
        member_name: String::new(),
    });
    let key = card.cache_key(if arabic { "google-ar" } else { "google-en" });
    let c = Arc::clone(&card);
    let build = async move {
        tokio::task::spawn_blocking(move || {
            Arc::new(
                render_png(&c, Store::Google, 1, arabic)
                    .map(|b| vec![("hero.png".to_string(), b)])
                    .unwrap_or_default(),
            )
        })
        .await
        .unwrap_or_else(|_| Arc::new(Vec::new()))
    };
    let set = if cfg!(test) { build.await } else { IMAGES.get_with(key, build).await };
    set.first().map(|(_, b)| b.clone())
}

/// What the stored-pass fingerprint adds for a designed card: the revision,
/// the design and every track's balance (track 1's is already in it).
pub async fn fingerprint_extra(pool: &PgPool, member: &MemberRow) -> String {
    let rev: Option<(i32, Option<Value>)> =
        sqlx::query_as("SELECT revision, published FROM loyalty_card_designs WHERE org_id = $1")
            .bind(member.org_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    let tracks: Vec<(i16, i32)> = sqlx::query_as(
        "SELECT position, balance FROM loyalty_track_balances WHERE customer_id = $1 ORDER BY position",
    )
    .bind(member.id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let defs: Vec<(i16, Option<i32>, String, String)> = sqlx::query_as(
        "SELECT position, size, name_en, name_ar FROM loyalty_stamp_tracks WHERE org_id = $1 ORDER BY position",
    )
    .bind(member.org_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    if rev.is_none() && tracks.is_empty() && defs.is_empty() {
        // A classic card: nothing added, so its stored passes keep matching.
        return String::new();
    }
    format!("{rev:?}|{tracks:?}|{defs:?}")
}

/// The brand Google's class is written with: a designed card's background.
pub fn brand_for_google(brand: &OrgBrand, copy: &super::CardCopy) -> OrgBrand {
    let mut b = brand.clone();
    if let Some(d) = &copy.design {
        b.palette.background = d.design.colours.background.clone();
    }
    b
}

/// The copy with this member's design on it (the Google paths build their
/// copy without a member).
pub async fn with_design(
    pool: &PgPool,
    member: &MemberRow,
    settings: &LoyaltySettings,
    brand: &OrgBrand,
    copy: &super::CardCopy,
) -> super::CardCopy {
    let mut c = copy.clone();
    if c.design.is_none() {
        c.design = for_member(pool, member, settings, brand).await;
    }
    c
}
