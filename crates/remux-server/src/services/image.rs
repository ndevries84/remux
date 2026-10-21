use std::{collections::HashSet, io::Cursor, path::PathBuf};

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use futures::StreamExt;
use image::{DynamicImage, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
use imageproc::drawing::draw_text_mut;
use uuid::Uuid;

use crate::{
    api::image::detect_content_type,
    db,
    db::ImageKind,
    sdks::remux::{
        CollectionFilter, CollectionFontFamily, CollectionFontWeight,
        CollectionImageConfig, CollectionOverlay, CollectionPosterLayout,
    },
};

/// Width/height of generated landscape library placeholder images (16:9).
const OUT_W: u32 = 960;
const OUT_H: u32 = 540;

/// Width/height of generated portrait (2:3) collection posters.
const PORTRAIT_W: u32 = 600;
const PORTRAIT_H: u32 = 900;

/// Portrait posters reserve the lower part of the canvas for the overlay; the
/// poster field occupies everything above this fraction of the height.
const PORTRAIT_BAND_TOP: f32 = 0.62;

/// The shape of a generated collection image. Landscape is the classic 16:9
/// tile every Jellyfin client expects from a library/collection Primary (and
/// what Thumb serves); Portrait is a 2:3 poster for clients that ask for it.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum_macros::Display,
    strum_macros::EnumString,
)]
#[strum(serialize_all = "snake_case")]
pub enum CollectionArtShape {
    Landscape,
    Portrait,
}

impl CollectionArtShape {
    /// Output size in pixels as `(width, height)`.
    pub const fn dimensions(self) -> (u32, u32) {
        match self {
            Self::Landscape => (OUT_W, OUT_H),
            Self::Portrait => (PORTRAIT_W, PORTRAIT_H),
        }
    }

    /// `width / height` of the generated image.
    pub fn aspect_ratio(self) -> f32 {
        let (w, h) = self.dimensions();
        w as f32 / h as f32
    }

    /// File stem used for the stored render (`primary` keeps the historical
    /// landscape file name so existing art stays valid).
    fn file_stem(self) -> &'static str {
        match self {
            Self::Landscape => "primary",
            Self::Portrait => "primary-portrait",
        }
    }
}

/// Prefix that marks a Primary image tag as "serve the portrait render".
/// Listings built for portrait-preferring clients advertise it; every other
/// tag (or none) selects the landscape image, exactly as before.
const PORTRAIT_TAG_PREFIX: &str = "portrait-";

/// The Primary tag advertised to portrait-preferring clients for a generated
/// collection whose landscape Primary tag is `tag`. It changes whenever `tag`
/// does, so regenerated art busts client caches in either shape.
pub fn portrait_art_tag(tag: &str) -> String {
    format!("{PORTRAIT_TAG_PREFIX}{tag}")
}

/// Whether an image request's `tag` selects the portrait render.
pub fn is_portrait_art_tag(tag: &str) -> bool {
    tag.starts_with(PORTRAIT_TAG_PREFIX)
}

/// Thumb tag of a collection whose Primary is generated. Derived purely from
/// the (landscape) Primary tag, so it is distinct from Primary yet changes
/// exactly when the art is regenerated.
pub fn collection_thumb_tag(primary_tag: &str) -> String {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("collection-thumb|{primary_tag}").as_bytes(),
    )
    .to_string()
}

/// Where the overlay (text / streaming logo) sits on the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayPlacement {
    /// Left-aligned in the text column beside the poster field (landscape).
    LeftColumn,
    /// Centred on the whole canvas (no posters).
    Centre,
    /// Centred in the lower band under the poster field (portrait).
    LowerBand,
}

impl OverlayPlacement {
    fn for_layout(shape: CollectionArtShape, layout: CollectionPosterLayout) -> Self {
        match (shape, layout) {
            (_, CollectionPosterLayout::None) => Self::Centre,
            (CollectionArtShape::Landscape, _) => Self::LeftColumn,
            (CollectionArtShape::Portrait, _) => Self::LowerBand,
        }
    }
}

/// Black overlay opacity — matches Jellyfin's `0x78` (≈ 47%).
const OVERLAY_ALPHA: f32 = 0x78 as f32 / 255.0;

/// Maximum text width as a fraction of image width before scaling down.
const MAX_TEXT_FRACTION: f32 = 0.90;

/// Standard poster dimensions (2:3 ratio) used in fan layout.
const POSTER_W: u32 = 190;
const POSTER_H: u32 = 285;
/// Poster size used by the portrait Row/Scatter layouts.
const PORTRAIT_POSTER_W: u32 = 180;
const PORTRAIT_POSTER_H: u32 = 270;

/// A three-by-three poster sheet gives the grid enough depth to overflow the frame.
const GRID_POSTER_LIMIT: usize = 9;
/// A grid cell should use its own source poster whenever the collection has
/// enough artwork. Repeating posters makes the projected sheet look synthetic.
const GRID_SOURCE_POSTER_LIMIT: usize = GRID_POSTER_LIMIT;
const GRID_COLUMNS: usize = 3;
const GRID_POSTER_W: u32 = 260;
const GRID_POSTER_H: u32 = 390;
const GRID_GUTTER: u32 = 10;

/// Inset for text overlays, so copy does not sit flush with the image edge.
const TEXT_LEFT_MARGIN: i32 = 115;

/// Side of the square buffer used for rotation (must exceed sqrt(POSTER_W²+POSTER_H²) ≈ 342).
const ROTATION_CANVAS: u32 = 380;

/// Rounded-corner radius on each poster (pixels).
const CORNER_RADIUS: u32 = 10;

/// Left text area: left edge of posters starts here (fan occupies right ~55% of canvas).
const TEXT_AREA_END: u32 = (OUT_W as f32 * 0.42) as u32;

static FONT_BOLD: &[u8] = include_bytes!("../../assets/fonts/LiberationSans-Bold.ttf");
static FONT_ROBOTO: &[u8] = include_bytes!("../../assets/fonts/Roboto.ttf");
static FONT_OPEN_SANS: &[u8] = include_bytes!("../../assets/fonts/OpenSans.ttf");
static FONT_LATO: &[u8] = include_bytes!("../../assets/fonts/Lato.ttf");
static FONT_MONTSERRAT: &[u8] = include_bytes!("../../assets/fonts/Montserrat.ttf");
static FONT_POPPINS: &[u8] = include_bytes!("../../assets/fonts/Poppins.ttf");
static FONT_OSWALD: &[u8] = include_bytes!("../../assets/fonts/Oswald.ttf");
static FONT_RALEWAY: &[u8] = include_bytes!("../../assets/fonts/Raleway.ttf");
static FONT_MERRIWEATHER: &[u8] = include_bytes!("../../assets/fonts/Merriweather.ttf");
static FONT_PLAYFAIR_DISPLAY: &[u8] =
    include_bytes!("../../assets/fonts/PlayfairDisplay.ttf");
static FONT_BEBAS_NEUE: &[u8] = include_bytes!("../../assets/fonts/BebasNeue.ttf");

/// Glyph-coverage fallback chain for text overlays: `ab_glyph` renders a
/// whole string with one font, so a codepoint missing from the user's chosen
/// font (a symbol like ♥ that not every display font includes) draws as
/// nothing rather than falling back to a font that has it. Tried in this
/// order for any character the primary font lacks; Merriweather is first
/// since it happens to carry the widest symbol coverage of the bundled set.
/// This does not add support for color/pictographic emoji — `ab_glyph` can
/// only rasterize monochrome vector outlines, so a true emoji codepoint has
/// no glyph in any of these fonts either and still renders as nothing.
const FALLBACK_FONT_BYTES: &[&[u8]] = &[
    FONT_MERRIWEATHER,
    FONT_ROBOTO,
    FONT_OPEN_SANS,
    FONT_LATO,
    FONT_MONTSERRAT,
    FONT_POPPINS,
    FONT_OSWALD,
    FONT_RALEWAY,
    FONT_PLAYFAIR_DISPLAY,
    FONT_BEBAS_NEUE,
    FONT_BOLD,
];
static PROVIDER_LOGO_NETFLIX: &[u8] =
    include_bytes!("../../assets/provider-logos/netflix.png");
static PROVIDER_LOGO_PRIME_VIDEO: &[u8] =
    include_bytes!("../../assets/provider-logos/prime-video.png");
static PROVIDER_LOGO_DISNEY_PLUS: &[u8] =
    include_bytes!("../../assets/provider-logos/disney-plus.png");
static PROVIDER_LOGO_MAX: &[u8] = include_bytes!("../../assets/provider-logos/max.png");
static PROVIDER_LOGO_HULU: &[u8] =
    include_bytes!("../../assets/provider-logos/hulu.png");
static PROVIDER_LOGO_PARAMOUNT_PLUS: &[u8] =
    include_bytes!("../../assets/provider-logos/paramount-plus.png");
static PROVIDER_LOGO_CRUNCHYROLL: &[u8] =
    include_bytes!("../../assets/provider-logos/crunchyroll.png");
static PROVIDER_LOGO_APPLE_TV: &[u8] =
    include_bytes!("../../assets/provider-logos/apple-tv.png");
static PROVIDER_LOGO_VIAPLAY: &[u8] =
    include_bytes!("../../assets/provider-logos/viaplay.png");
static PROVIDER_LOGO_SKYSHOWTIME: &[u8] =
    include_bytes!("../../assets/provider-logos/skyshowtime.png");
static PROVIDER_LOGO_PEACOCK: &[u8] =
    include_bytes!("../../assets/provider-logos/peacock.png");
static PROVIDER_LOGO_MUBI: &[u8] =
    include_bytes!("../../assets/provider-logos/mubi.png");
static PROVIDER_LOGO_PLUTO_TV: &[u8] =
    include_bytes!("../../assets/provider-logos/pluto-tv.png");
static PROVIDER_LOGO_YOUTUBE: &[u8] =
    include_bytes!("../../assets/provider-logos/youtube.png");
// Transparent wordmarks sourced from Wikimedia Commons (see
// assets/provider-logos/commons/original/ for the full downloaded catalog
// and its raw imageinfo). Only assets that are genuinely alpha-transparent —
// verified by inspection, not just declared PNG — are curated here; TMDB's
// own logo tiles are opaque and are never used as a fallback.
static PROVIDER_LOGO_AMC_PLUS: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/amc-plus.png");
static PROVIDER_LOGO_ADN: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/adn.png");
static PROVIDER_LOGO_FUNIMATION_NOW: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/funimation-now.png");
static PROVIDER_LOGO_ILLICO_PLUS: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/illico-plus.png");
static PROVIDER_LOGO_SKY_MAIS: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/sky-mais.png");
static PROVIDER_LOGO_C_MORE: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/c-more.png");
static PROVIDER_LOGO_FOXTEL_NOW: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/foxtel-now.png");
static PROVIDER_LOGO_JIOHOTSTAR: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/jiohotstar.png");
static PROVIDER_LOGO_WETV: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/wetv.png");
static PROVIDER_LOGO_CHORKI: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/chorki.png");
static PROVIDER_LOGO_TVP_VOD: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/tvp-vod.png");
static PROVIDER_LOGO_KWELITV: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/kwelitv.png");
static PROVIDER_LOGO_VIDIO: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/vidio.png");
static PROVIDER_LOGO_VIVAMAX: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/vivamax.png");
static PROVIDER_LOGO_RUUTU: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/ruutu.png");
static PROVIDER_LOGO_SAMSUNG_TV_PLUS: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/samsung-tv-plus.png");
static PROVIDER_LOGO_CRAFTSY: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/craftsy.png");
static PROVIDER_LOGO_MX_PLAYER: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/mx-player.png");
static PROVIDER_LOGO_AXN_NOW: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/axn-now.png");
static PROVIDER_LOGO_CLARO_VIDEO: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/claro-video.png");
static PROVIDER_LOGO_DIMSUM: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/dimsum.png");
static PROVIDER_LOGO_CATCHPLAY: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/catchplay.png");
static PROVIDER_LOGO_FANATIZ: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/fanatiz.png");
static PROVIDER_LOGO_MOVISTAR_PLUS: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/movistar-plus.png");
static PROVIDER_LOGO_DISCOVERY_PLUS: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/discovery-plus.png");
static PROVIDER_LOGO_KOCOWA: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/kocowa.png");
static PROVIDER_LOGO_TOKU: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/toku.png");
static PROVIDER_LOGO_WATCHA: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/watcha.png");
static PROVIDER_LOGO_WAKANIM: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/wakanim.png");
static PROVIDER_LOGO_DPLAY: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/dplay.png");
static PROVIDER_LOGO_FILMDOO: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/filmdoo.png");
static PROVIDER_LOGO_GLOBOPLAY: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/globoplay.png");
static PROVIDER_LOGO_MTV_KATSOMO: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/mtv-katsomo.png");
static PROVIDER_LOGO_U_NEXT: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/u-next.png");
static PROVIDER_LOGO_RCTI_PLUS: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/rcti-plus.png");
static PROVIDER_LOGO_NFL_PLUS: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/nfl-plus.png");
static PROVIDER_LOGO_NASA_PLUS: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/nasa-plus.png");
static PROVIDER_LOGO_IWANTTFC: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/iwanttfc.png");
static PROVIDER_LOGO_KAPAMILYA_ONLINE_LIVE: &[u8] = include_bytes!(
    "../../assets/provider-logos/commons/curated/kapamilya-online-live.png"
);
static PROVIDER_LOGO_HAYU: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/hayu.png");
static PROVIDER_LOGO_WATCH_IT: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/watch-it.png");
static PROVIDER_LOGO_STAN: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/stan.png");
static PROVIDER_LOGO_SHUDDER: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/shudder.png");
static PROVIDER_LOGO_ROXI: &[u8] =
    include_bytes!("../../assets/provider-logos/commons/curated/roxi.png");

#[allow(clippy::incompatible_msrv)]
static HTTP_CLIENT: std::sync::LazyLock<reqwest::Client> =
    std::sync::LazyLock::new(|| {
        reqwest::Client::builder()
            .user_agent("remux-server/1.0")
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("failed to build image http client")
    });

fn collection_font_bytes(font: CollectionFontFamily) -> &'static [u8] {
    match font {
        CollectionFontFamily::Roboto => FONT_ROBOTO,
        CollectionFontFamily::OpenSans => FONT_OPEN_SANS,
        CollectionFontFamily::Lato => FONT_LATO,
        CollectionFontFamily::Montserrat => FONT_MONTSERRAT,
        CollectionFontFamily::Poppins => FONT_POPPINS,
        CollectionFontFamily::Oswald => FONT_OSWALD,
        CollectionFontFamily::Raleway => FONT_RALEWAY,
        CollectionFontFamily::Merriweather => FONT_MERRIWEATHER,
        CollectionFontFamily::PlayfairDisplay => FONT_PLAYFAIR_DISPLAY,
        CollectionFontFamily::BebasNeue => FONT_BEBAS_NEUE,
    }
}

// ---------------------------------------------------------------------------
// Image processing options
// ---------------------------------------------------------------------------

/// Parameters controlling server-side image transformation.
#[derive(Debug, Clone, Default)]
pub struct ImageProcessOptions {
    pub fill_width: Option<u32>,
    pub fill_height: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
    /// JPEG encode quality (0–100). `None` → default 90.
    pub quality: Option<u8>,
    /// Gaussian blur sigma in pixels.
    pub blur: Option<u32>,
    pub background_color: Option<String>,
    /// "jpg" / "jpeg" / "png". `None` → jpeg.
    pub format: Option<String>,
}

impl ImageProcessOptions {
    /// Returns true when any transformation is requested.
    ///
    /// Disabled below (always `false`): different clients request wildly
    /// different exact dimensions for the same item, and `apply_sizing`'s
    /// crop-to-fill can cut into a generated collection image's text overlay
    /// depending on which aspect ratio a given client happens to ask for
    /// (#435). Until sizing is reworked to never crop into overlay content,
    /// not resizing at all is safer than resizing inconsistently per client.
    /// The real logic is kept below, dead, for when that rework happens.
    #[allow(unreachable_code)]
    pub fn needs_processing(&self) -> bool {
        return false;
        self.fill_width
            .is_some()
            || self
                .fill_height
                .is_some()
            || self
                .width
                .is_some()
            || self
                .height
                .is_some()
            || self
                .max_width
                .is_some()
            || self
                .max_height
                .is_some()
            || self
                .quality
                .is_some()
            || self
                .blur
                .is_some()
            || self
                .background_color
                .is_some()
            || self
                .format
                .is_some()
    }

    fn output_format(&self) -> ImageFormat {
        match self
            .format
            .as_deref()
        {
            Some("png") => ImageFormat::Png,
            _ => ImageFormat::Jpeg,
        }
    }

    pub fn content_type(&self) -> &'static str {
        match self
            .format
            .as_deref()
        {
            Some("png") => "image/png",
            _ => "image/jpeg",
        }
    }

    /// Stable cache key derived from source identifier + all transform params.
    fn cache_key(&self, source: &str) -> String {
        let key_data = format!(
            "v2|{}|fw={:?}|fh={:?}|w={:?}|h={:?}|mw={:?}|mh={:?}|q={:?}|bl={:?}|bg={:?}|fmt={:?}",
            source,
            self.fill_width,
            self.fill_height,
            self.width,
            self.height,
            self.max_width,
            self.max_height,
            self.quality,
            self.blur,
            self.background_color,
            self.format,
        );
        Uuid::new_v5(&Uuid::NAMESPACE_URL, key_data.as_bytes()).to_string()
    }
}

// ---------------------------------------------------------------------------
// ImageService
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ImageService;

impl ImageService {
    /// Returns the directory for a library item's local images.
    pub fn image_dir(data_dir: &std::path::Path, id: Uuid) -> PathBuf {
        data_dir
            .join("meta")
            .join("library")
            .join(id.to_string())
    }

    /// Returns the local path for a specific image type with the given extension.
    pub fn image_path(
        data_dir: &std::path::Path,
        id: Uuid,
        image_type: &str,
        ext: &str,
    ) -> PathBuf {
        Self::image_dir(data_dir, id).join(format!(
            "{}.{}",
            image_type.to_lowercase(),
            ext
        ))
    }

    /// Read an already-rendered collection image of `shape` from disk, if any.
    async fn read_existing_art(
        data_dir: &std::path::Path,
        id: Uuid,
        shape: CollectionArtShape,
    ) -> Option<Vec<u8>> {
        for ext in ["jpg", "png"] {
            let p = Self::image_path(data_dir, id, shape.file_stem(), ext);
            if let Ok(bytes) = tokio::fs::read(&p).await {
                return Some(bytes);
            }
        }
        None
    }

    /// Generate the collection's art, write it to disk, register the landscape
    /// render in `media_images` (as generated), and return the bytes for
    /// `wanted`.
    ///
    /// Both shapes are always rendered together when either is missing, so the
    /// first portrait listing works even though only landscape existed before
    /// (and vice versa). A shape that is already on disk is never re-rendered
    /// here; call [`Self::invalidate_collection_image`] to force that.
    ///
    /// Only the landscape render gets a `media_images` row (its id is the
    /// image tag every client keys its cache on); the portrait render is a
    /// sibling file derived from the same row.
    pub async fn library_image(
        data_dir: &std::path::Path,
        id: Uuid,
        name: &str,
        db: &sqlx::SqlitePool,
        wanted: CollectionArtShape,
    ) -> anyhow::Result<Vec<u8>> {
        let landscape =
            Self::read_existing_art(data_dir, id, CollectionArtShape::Landscape).await;
        let portrait =
            Self::read_existing_art(data_dir, id, CollectionArtShape::Portrait).await;

        let mut missing = Vec::new();
        if landscape.is_none() {
            missing.push(CollectionArtShape::Landscape);
        }
        if portrait.is_none() {
            missing.push(CollectionArtShape::Portrait);
        }
        let mut have = [
            (CollectionArtShape::Landscape, landscape),
            (CollectionArtShape::Portrait, portrait),
        ];

        if !missing.is_empty() {
            let rendered =
                Self::generate(id, name, db, None, None, None, &missing).await?;
            for (shape, bytes) in rendered {
                let ct = detect_content_type(&bytes);
                let ext = ext_for_content_type(ct);
                let path = Self::image_path(data_dir, id, shape.file_stem(), ext);
                Self::write_image_to_disk(&path, &bytes).await?;
                if shape == CollectionArtShape::Landscape {
                    let (w, h) = shape.dimensions();
                    // INSERT OR IGNORE — don't replace if already exists (stable UUID for cache)
                    sqlx::query(
                        "INSERT OR IGNORE INTO media_images (id, media_id, image_type, image_index, path, width, height, is_generated) VALUES (?, ?, 'primary', 0, ?, ?, ?, 1)"
                    )
                    .bind(Uuid::new_v4())
                    .bind(id)
                    .bind(path.to_string_lossy().as_ref())
                    .bind(w as i64)
                    .bind(h as i64)
                    .execute(db)
                    .await?;
                }
                if let Some(slot) = have
                    .iter_mut()
                    .find(|(s, _)| *s == shape)
                {
                    slot.1 = Some(bytes);
                }
            }
        }

        have.into_iter()
            .find(|(s, _)| *s == wanted)
            .and_then(|(_, bytes)| bytes)
            .ok_or_else(|| anyhow::anyhow!("collection {id} art not generated"))
    }

    /// Render a collection image from draft, unsaved state — no disk write, no
    /// DB write, no cache row. `config`/`smart_filter` override whatever is
    /// currently stored on the collection, so the caller can preview an
    /// in-progress edit before it's saved. Used by the dashboard's "Update
    /// preview" action, which must never mutate the live collection.
    pub async fn generate_preview(
        id: Uuid,
        name: &str,
        db: &sqlx::SqlitePool,
        config: &CollectionImageConfig,
        smart_filter: Option<&CollectionFilter>,
    ) -> anyhow::Result<(Vec<u8>, &'static str)> {
        // The dashboard previews the landscape tile (it is a non-Moonfin client).
        let bytes = Self::generate(
            id,
            name,
            db,
            Some(config),
            smart_filter,
            None,
            &[CollectionArtShape::Landscape],
        )
        .await?
        .into_iter()
        .next()
        .map(|(_, bytes)| bytes)
        .ok_or_else(|| anyhow::anyhow!("preview render produced no image"))?;
        let ct = detect_content_type(&bytes);
        Ok((bytes, ct))
    }

    /// Disk-cached wrapper around `synthetic_thumb`, keyed on the *source*
    /// Backdrop/Logo row ids rather than the media id: `sync_from_media`
    /// regenerates an image's id whenever a refresh replaces it, so keying
    /// on those ids means a stale composite is never served — a changed
    /// Backdrop or Logo naturally misses the old cache file and regenerates.
    /// Never registered in `media_images` — purely a cache entry, wiped like
    /// any other by `ClearImageCacheTask`. Returns the derived cache key
    /// alongside the bytes so the caller can reuse it as `process_image`'s
    /// `source_key`, giving per-size variants the same change-detection.
    pub async fn cached_synthetic_thumb(
        data_dir: &std::path::Path,
        backdrop_id: Uuid,
        logo_id: Option<Uuid>,
        backdrop_path: &str,
        logo_path: Option<&str>,
        title: &str,
    ) -> anyhow::Result<(Vec<u8>, String)> {
        let cache_key = Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            format!("synthetic-thumb|{backdrop_id}|{logo_id:?}").as_bytes(),
        )
        .to_string();
        let path = Self::cache_dir(data_dir).join(format!("{cache_key}.jpg"));
        if path.exists() {
            return Ok((tokio::fs::read(&path).await?, cache_key));
        }
        let bytes = Self::synthetic_thumb(backdrop_path, logo_path, title).await?;
        Self::write_image_to_disk(&path, &bytes).await?;
        Ok((bytes, cache_key))
    }

    /// Composite a landscape Thumb from a Backdrop and, if available, a Logo —
    /// for items with no dedicated Thumb (TMDB has no matching English
    /// title-card backdrop for them). Runs entirely in memory: no disk write,
    /// no `media_images` row. A later refresh that adds a real Thumb, or a
    /// better Backdrop/Logo, is picked up on the very next request with
    /// nothing to invalidate.
    pub async fn synthetic_thumb(
        backdrop_path: &str,
        logo_path: Option<&str>,
        title: &str,
    ) -> anyhow::Result<Vec<u8>> {
        let backdrop = if backdrop_path.contains("://") {
            Self::fetch_rgb(backdrop_path).await?
        } else {
            Self::read_rgb(backdrop_path).await?
        };
        let logo = match logo_path {
            Some(path) if path.contains("://") => Self::fetch_rgba(path)
                .await
                .ok(),
            Some(path) => Self::read_rgba(path)
                .await
                .ok(),
            None => None,
        };

        let title = title.to_string();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
            let resized = image::imageops::resize(
                &backdrop,
                OUT_W,
                OUT_H,
                image::imageops::FilterType::Lanczos3,
            );
            let dimmed = apply_dark_overlay(resized);

            match logo {
                Some(logo) => {
                    let mut canvas = DynamicImage::ImageRgb8(dimmed).into_rgba8();
                    let max_logo_h = OUT_H * 3 / 4;
                    let max_logo_w = (OUT_W as f32 * 0.9) as u32;
                    let scale = (max_logo_h as f32 / logo.height() as f32)
                        .min(max_logo_w as f32 / logo.width() as f32);
                    let lw = (logo.width() as f32 * scale) as u32;
                    let lh = (logo.height() as f32 * scale) as u32;
                    let scaled = image::imageops::resize(
                        &logo,
                        lw,
                        lh,
                        image::imageops::FilterType::Lanczos3,
                    );
                    let lx = (OUT_W.saturating_sub(lw) / 2) as i64;
                    let ly = (OUT_H.saturating_sub(lh) / 2) as i64;
                    image::imageops::overlay(&mut canvas, &scaled, lx, ly);
                    encode_jpeg_rgba(canvas)
                }
                None => encode_jpeg(draw_label(dimmed, &title)?),
            }
        })
        .await?
    }

    /// Save an uploaded image for `id`/`image_type`, write to disk, update DB.
    pub async fn save_image(
        data_dir: &std::path::Path,
        id: Uuid,
        kind: ImageKind,
        bytes: &[u8],
        db: &sqlx::SqlitePool,
    ) -> anyhow::Result<()> {
        let ext = ext_for_content_type(detect_content_type(bytes));
        let path = Self::image_path(data_dir, id, &kind.to_string(), ext);
        Self::write_image_to_disk(&path, bytes).await?;
        let (img_w, img_h) = image::load_from_memory(bytes)
            .map(|img| (img.width() as i64, img.height() as i64))
            .ok()
            .unzip();
        db::MediaImage::save(
            db,
            id,
            kind,
            path.to_string_lossy()
                .as_ref(),
            img_w,
            img_h,
        )
        .await
        .map_err(anyhow::Error::from)?;
        Ok(())
    }

    /// Delete the local image for `id`/`kind` and remove from media_images.
    pub async fn delete_image(
        data_dir: &std::path::Path,
        id: Uuid,
        kind: ImageKind,
        db: &sqlx::SqlitePool,
    ) -> anyhow::Result<()> {
        // Look up the stored path from the DB rather than reconstructing it —
        // the extension varies by format (gif, png, webp, jpg).
        if let Ok(images) = db::MediaImage::get_for_media(db, &id).await {
            if let Some(img) = images.get(kind) {
                if img
                    .path
                    .starts_with('/')
                {
                    let _ =
                        tokio::fs::remove_file(std::path::Path::new(&img.path)).await;
                }
            }
        }
        // Sweep all known extensions for this kind — previous uploads of different
        // formats leave orphan files behind, and the generated placeholder is always
        // primary.jpg regardless of what the DB row points to.
        for ext in ["jpg", "png", "gif", "webp"] {
            let _ = tokio::fs::remove_file(Self::image_path(
                data_dir,
                id,
                &kind.to_string(),
                ext,
            ))
            .await;
            // The generated portrait render lives beside the landscape Primary.
            if kind == ImageKind::Primary {
                let _ = tokio::fs::remove_file(Self::image_path(
                    data_dir,
                    id,
                    CollectionArtShape::Portrait.file_stem(),
                    ext,
                ))
                .await;
            }
        }
        db::MediaImage::delete_for_type(db, id, kind)
            .await
            .map_err(anyhow::Error::from)?;
        // Touch updated_at so the synthetic image tag (derived from it) changes,
        // busting client caches that keyed on the old tag.
        let _ = sqlx::query("UPDATE media SET updated_at = ? WHERE id = ?")
            .bind(chrono::Utc::now().naive_utc())
            .bind(id)
            .execute(db)
            .await;
        Ok(())
    }

    /// Serve a locally-stored image file, returning (bytes, content_type).
    pub async fn serve_local(
        path: &PathBuf,
    ) -> anyhow::Result<(Vec<u8>, &'static str)> {
        let bytes = tokio::fs::read(path).await?;
        let ct = detect_content_type(&bytes);
        Ok((bytes, ct))
    }

    /// Directory for processed image cache.
    pub fn cache_dir(data_dir: &std::path::Path) -> PathBuf {
        data_dir
            .join("cache")
            .join("images")
    }

    /// Apply image transformations described by `opts`, returning (bytes, content_type).
    ///
    /// * If no processing is needed, the raw bytes are returned as-is.
    /// * Processed results are cached at `cache_dir()/{uuid_key}.{ext}`.
    pub async fn process_image(
        data_dir: &std::path::Path,
        bytes: Vec<u8>,
        opts: &ImageProcessOptions,
        source_key: &str,
    ) -> anyhow::Result<(Vec<u8>, &'static str)> {
        if !opts.needs_processing() {
            let ct = detect_content_type(&bytes);
            return Ok((bytes, ct));
        }

        // GIFs are always served as-is to preserve animation.
        if detect_content_type(&bytes) == "image/gif" {
            return Ok((bytes, "image/gif"));
        }

        let cache_key = opts.cache_key(source_key);
        let cache_dir = Self::cache_dir(data_dir);

        // Check both extensions — alpha auto-detection may produce PNG even when opts say JPEG.
        for (ext, ct) in [("png", "image/png"), ("jpg", "image/jpeg")] {
            let path = cache_dir.join(format!("{cache_key}.{ext}"));
            if path.exists() {
                let cached = tokio::fs::read(&path).await?;
                return Ok((cached, ct));
            }
        }

        let opts_clone = opts.clone();
        let (processed, content_type) = tokio::task::spawn_blocking(move || {
            process_image_sync(&bytes, &opts_clone)
        })
        .await??;

        let ext = if content_type == "image/png" {
            "png"
        } else {
            "jpg"
        };
        tokio::fs::create_dir_all(&cache_dir).await?;
        tokio::fs::write(cache_dir.join(format!("{cache_key}.{ext}")), &processed)
            .await?;

        Ok((processed, content_type))
    }

    async fn write_image_to_disk(path: &PathBuf, bytes: &[u8]) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        tokio::fs::write(path, bytes).await?;
        Ok(())
    }

    /// Regenerate the collection image: delete cached files (both shapes) and
    /// the DB row, then re-generate on next request. Call after
    /// `collection_image_config` changes.
    ///
    /// Only touches server-generated art: a Primary that is uploaded or comes
    /// from a provider (`is_generated = false`) is left exactly as it is, and
    /// the call returns `Ok(false)`. Returns `Ok(true)` when it invalidated
    /// (including the case where nothing was generated yet).
    pub async fn invalidate_collection_image(
        data_dir: &std::path::Path,
        id: Uuid,
        db: &sqlx::SqlitePool,
    ) -> anyhow::Result<bool> {
        let images = db::MediaImage::get_for_media(db, &id)
            .await
            .map_err(anyhow::Error::from)?;
        if images
            .get(ImageKind::Primary)
            .is_some_and(|row| !row.is_generated)
        {
            return Ok(false);
        }
        Self::delete_image(data_dir, id, ImageKind::Primary, db).await?;
        Ok(true)
    }

    /// `config_override`/`smart_filter_override` take precedence over whatever
    /// is stored on the collection row when present — the preview path
    /// (`generate_preview`) supplies unsaved draft state this way, while the
    /// persisting path (`library_image`) passes `None` for both and reads the
    /// stored config as before. `background_override` similarly stands in for
    /// a locally-picked-but-not-yet-uploaded custom background. Every shape in
    /// `shapes` is rendered from one shared fetch of the posters/background.
    async fn generate(
        id: Uuid,
        name: &str,
        db: &sqlx::SqlitePool,
        config_override: Option<&CollectionImageConfig>,
        smart_filter_override: Option<&CollectionFilter>,
        background_override: Option<Vec<u8>>,
        shapes: &[CollectionArtShape],
    ) -> anyhow::Result<Vec<(CollectionArtShape, Vec<u8>)>> {
        let collection = db::Media::get_by_id(db, &id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("collection {id} not found"))?;
        let config = config_override
            .cloned()
            .or_else(|| {
                collection
                    .collection_image_config
                    .clone()
            });
        let layout = config
            .as_ref()
            .map(|c| c.layout)
            .unwrap_or_default();
        let background_color = config
            .as_ref()
            .and_then(|config| {
                config
                    .background_color
                    .as_ref()
                    .and_then(|color| parse_hex_color(color.as_ref()))
            })
            .or_else(|| {
                config
                    .as_ref()
                    .and_then(|config| match &config.overlay {
                        CollectionOverlay::StreamingLogo { provider_name, .. } => {
                            provider_name
                                .as_deref()
                                .and_then(crate::services::stream_provider::StreamProvider::brand_color)
                        }
                        _ => None,
                    })
                    .and_then(parse_hex_color)
            })
            .unwrap_or([0, 0, 0]);

        // Resolve a custom background before looking up the poster grid. It is
        // the entire composition when present, so poster downloads would be
        // wasted work.
        let custom_background = if let Some(bytes) = background_override {
            image::load_from_memory(&bytes)
                .ok()
                .map(|img| img.into_rgba8())
        } else {
            let custom_background_path = config
                .as_ref()
                .and_then(|_| {
                    collection
                        .images
                        .get(ImageKind::Backdrop)
                        .map(|image| {
                            image
                                .path
                                .clone()
                        })
                });
            if let Some(path) = custom_background_path {
                let result = if path.contains("://") {
                    Self::fetch_rgba(&path).await
                } else {
                    Self::read_rgba(&path).await
                };
                result.ok()
            } else {
                None
            }
        };

        let poster_limit = if layout == CollectionPosterLayout::Grid {
            shapes
                .iter()
                .map(|shape| GridSpec::for_shape(*shape).limit())
                .max()
                .unwrap_or(GRID_SOURCE_POSTER_LIMIT) as u32
        } else {
            4u32
        };
        let poster_urls =
            if custom_background.is_none() && layout != CollectionPosterLayout::None {
                collect_poster_urls(
                    id,
                    &collection,
                    db,
                    poster_limit,
                    smart_filter_override,
                )
                .await
            } else {
                Vec::new()
            };

        if poster_urls.is_empty() && config.is_none() {
            // Fallback: single backdrop with label (original behaviour).
            let source = match find_backdrop_url_from_collection(&collection, db).await
            {
                Ok(src) => {
                    let result = if src.contains("://") {
                        Self::fetch_rgb(&src).await
                    } else {
                        Self::read_rgb(&src).await
                    };
                    result.ok()
                }
                Err(_) => None,
            };
            let mut out = Vec::with_capacity(shapes.len());
            for &shape in shapes {
                let (w, h) = shape.dimensions();
                let bg = match &source {
                    Some(img) if shape == CollectionArtShape::Landscape => {
                        let resized = image::imageops::resize(
                            img,
                            w,
                            h,
                            image::imageops::FilterType::Lanczos3,
                        );
                        apply_dark_overlay(resized)
                    }
                    // A landscape backdrop is centre-cropped to the poster shape.
                    Some(img) => apply_dark_overlay(
                        DynamicImage::ImageRgb8(img.clone())
                            .resize_to_fill(w, h, image::imageops::FilterType::Lanczos3)
                            .into_rgb8(),
                    ),
                    None => solid_background(shape),
                };
                out.push((shape, encode_jpeg(draw_label(bg, name)?)?));
            }
            return Ok(out);
        }

        let posters =
            Self::load_poster_images(&poster_urls, poster_limit as usize).await;

        // Bundled provider wordmarks are transparent and avoid TMDB's opaque app tiles.
        let logo_image: Option<RgbaImage> = match config
            .as_ref()
            .map(|c| &c.overlay)
        {
            Some(CollectionOverlay::StreamingLogo { provider_name, .. }) => {
                provider_wordmark(provider_name.as_deref())
            }
            _ => None,
        };

        // CPU-bound fan composition + JPEG encode runs on the blocking pool.
        let name_owned = name.to_string();
        let config_owned = config;
        let shapes_owned = shapes.to_vec();
        let rendered =
            tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<_>> {
                let overlay = config_owned
                    .as_ref()
                    .map(|c| &c.overlay)
                    .unwrap_or(&CollectionOverlay::None);
                shapes_owned
                    .into_iter()
                    .map(|shape| {
                        let canvas = compose_collection_art(
                            shape,
                            layout,
                            background_color,
                            custom_background.as_ref(),
                            &posters,
                            overlay,
                            &name_owned,
                            logo_image.as_ref(),
                        )?;
                        Ok((shape, encode_jpeg_rgba(canvas)?))
                    })
                    .collect()
            })
            .await??;

        Ok(rendered)
    }

    async fn load_poster_images(urls: &[String], limit: usize) -> Vec<RgbaImage> {
        futures::stream::iter(
            urls.iter()
                .take(limit)
                .cloned()
                .map(|url| async move {
                    let img = if url.contains("://") {
                        Self::fetch_rgba(&url).await
                    } else {
                        Self::read_rgba(&url).await
                    };
                    img
                }),
        )
        // Keep source order stable while downloading up to eight images at
        // once. The old serial loop made a single generated image wait for
        // every remote poster in sequence.
        .buffered(8)
        .filter_map(|image| async move { image.ok() })
        .collect()
        .await
    }

    async fn fetch_rgb(url: &str) -> anyhow::Result<RgbImage> {
        let bytes = HTTP_CLIENT
            .get(url)
            .send()
            .await?
            .bytes()
            .await?;
        Ok(image::load_from_memory(&bytes)?.into_rgb8())
    }

    async fn read_rgb(path: &str) -> anyhow::Result<RgbImage> {
        let bytes = tokio::fs::read(path).await?;
        Ok(image::load_from_memory(&bytes)?.into_rgb8())
    }

    async fn fetch_rgba(url: &str) -> anyhow::Result<RgbaImage> {
        let bytes = HTTP_CLIENT
            .get(url)
            .send()
            .await?
            .bytes()
            .await?;
        Ok(image::load_from_memory(&bytes)?.into_rgba8())
    }

    async fn read_rgba(path: &str) -> anyhow::Result<RgbaImage> {
        let bytes = tokio::fs::read(path).await?;
        Ok(image::load_from_memory(&bytes)?.into_rgba8())
    }

    async fn fetch_and_resize(url: &str) -> anyhow::Result<RgbImage> {
        let img = Self::fetch_rgb(url).await?;
        Ok(image::imageops::resize(
            &img,
            OUT_W,
            OUT_H,
            image::imageops::FilterType::Lanczos3,
        ))
    }

    async fn read_local_and_resize(path: &str) -> anyhow::Result<RgbImage> {
        let img = Self::read_rgb(path).await?;
        Ok(image::imageops::resize(
            &img,
            OUT_W,
            OUT_H,
            image::imageops::FilterType::Lanczos3,
        ))
    }
}

/// Builds the query that finds a collection's own member items, for poster
/// selection. Regression: this used to filter only by `collection_media_kind`
/// with no membership scoping at all (no `parent_id`, no `filter_rules`), so
/// the generated collage pulled arbitrary movies/shows matching that kind from
/// the whole library rather than this collection's actual members. Mirrors the
/// scoping `/items?parentId=` already uses (`api/items.rs`):
/// - A "collection of collections" group container holds children via
///   `parent_id`, regardless of its own `collection_kind` — same as
///   `group_container_has_visible_content`'s walk above.
/// - A Manual collection holds members via `media_relations`; `parent` (in
///   addition to `parent_id`) is what makes `get_by_filter_inner` take that
///   join path instead of a literal `parent_id` scan.
/// - A Smart collection floats freely (no `parent_id`) and is scoped by
///   `filter_rules` alone. `smart_filter_override` lets a caller preview an
///   unsaved filter edit without it ever being persisted.
fn collection_item_filter(
    collection: &db::Media,
    smart_filter_override: Option<&CollectionFilter>,
) -> db::MediaFilter {
    match &collection.kind {
        db::MediaKind::Collection => {
            if collection.collection_media_kind
                == Some(db::CollectionMediaKind::Collection)
            {
                return db::MediaFilter {
                    kind: Some(vec![db::MediaKind::Collection]),
                    parent_id: Some(collection.id),
                    limit: Some(8),
                    ..Default::default()
                };
            }
            let kinds = match &collection.collection_media_kind {
                Some(db::CollectionMediaKind::Movie) => vec![db::MediaKind::Movie],
                Some(db::CollectionMediaKind::Series) => vec![db::MediaKind::Series],
                Some(db::CollectionMediaKind::Episode) => vec![db::MediaKind::Episode],
                Some(db::CollectionMediaKind::Mixed) => {
                    vec![db::MediaKind::Movie, db::MediaKind::Series]
                }
                Some(db::CollectionMediaKind::Music) => {
                    vec![db::MediaKind::Album, db::MediaKind::Artist]
                }
                Some(db::CollectionMediaKind::Playlist) => {
                    vec![db::MediaKind::Playlist]
                }
                Some(db::CollectionMediaKind::Collection) => {
                    unreachable!("handled above")
                }
                None => vec![db::MediaKind::Movie, db::MediaKind::Series],
            };
            if collection.collection_kind == Some(db::CollectionKind::Manual) {
                db::MediaFilter {
                    kind: Some(kinds),
                    parent_id: Some(collection.id),
                    parent: Some(collection.clone()),
                    limit: Some(8),
                    ..Default::default()
                }
            } else {
                db::MediaFilter {
                    kind: Some(kinds),
                    filter_rules: smart_filter_override
                        .cloned()
                        .or_else(|| {
                            collection
                                .parse_smart_filter()
                                .cloned()
                        }),
                    limit: Some(8),
                    ..Default::default()
                }
            }
        }
        _ => db::MediaFilter {
            parent_id: Some(collection.id),
            limit: Some(8),
            ..Default::default()
        },
    }
}

/// Most child collections consulted when a parent has to borrow posters.
const CHILD_COLLECTION_LIMIT: u32 = 12;
/// Most posters taken from any single child collection, so one large child
/// cannot crowd the others out of the interleave.
const POSTERS_PER_CHILD: u32 = 6;

/// Primary (else backdrop) image path of a content item.
fn poster_path(m: &db::Media) -> Option<String> {
    m.images
        .get(ImageKind::Primary)
        .map(|i| {
            i.path
                .clone()
        })
        .or_else(|| {
            m.images
                .get(ImageKind::Backdrop)
                .map(|i| {
                    i.path
                        .clone()
                })
        })
}

/// Query for the child collections of a "collection of collections". A smart
/// parent (e.g. `collection_id in [...]`) selects them by filter; a manual
/// group container holds them via `parent_id`. Returns `None` for collections
/// that are not collections of collections.
fn child_collections_filter(
    collection: &db::Media,
    smart_filter_override: Option<&CollectionFilter>,
) -> Option<db::MediaFilter> {
    if collection.kind != db::MediaKind::Collection
        || collection.collection_media_kind != Some(db::CollectionMediaKind::Collection)
    {
        return None;
    }
    let mut filter = db::MediaFilter {
        kind: Some(vec![db::MediaKind::Collection]),
        limit: Some(CHILD_COLLECTION_LIMIT),
        ..Default::default()
    };
    if collection.collection_kind == Some(db::CollectionKind::Smart) {
        filter.filter_rules = Some(
            smart_filter_override
                .cloned()
                .or_else(|| {
                    collection
                        .parse_smart_filter()
                        .cloned()
                })?,
        );
    } else {
        filter.parent_id = Some(collection.id);
    }
    Some(filter)
}

/// Round-robin merge of per-source path lists, de-duplicated, up to `limit`.
fn interleave_paths(sources: Vec<Vec<String>>, limit: usize) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let longest = sources
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(0);
    for i in 0..longest {
        for source in &sources {
            if let Some(path) = source.get(i) {
                if out.len() < limit && seen.insert(path.clone()) {
                    out.push(path.clone());
                }
            }
        }
    }
    out
}

/// Posters borrowed from a parent's child collections. Each child is resolved
/// exactly as the generator resolves a normal collection (smart children by
/// evaluating their own filter), because a smart child has no stored
/// membership to read. Server-side art, so no user policy is applied.
async fn collect_child_poster_urls(
    collection: &db::Media,
    db: &sqlx::SqlitePool,
    limit: u32,
    smart_filter_override: Option<&CollectionFilter>,
) -> Vec<String> {
    let Some(filter) = child_collections_filter(collection, smart_filter_override)
    else {
        return Vec::new();
    };
    let Ok(children) = db::Media::get_by_filter(db, &filter).await else {
        return Vec::new();
    };
    let mut per_child = Vec::new();
    for child in children
        .records
        .iter()
        .filter(|c| c.id != collection.id)
    {
        let child_filter = db::MediaFilter {
            limit: Some(POSTERS_PER_CHILD.saturating_mul(2)),
            ..collection_item_filter(child, None)
        };
        let Ok(items) = db::Media::get_by_filter(db, &child_filter).await else {
            continue;
        };
        let paths: Vec<String> = items
            .records
            .iter()
            // Never reuse a nested collection's own generated art as a poster.
            .filter(|m| m.kind != db::MediaKind::Collection)
            .filter_map(poster_path)
            .take(POSTERS_PER_CHILD as usize)
            .collect();
        if !paths.is_empty() {
            per_child.push(paths);
        }
    }
    interleave_paths(per_child, limit as usize)
}

/// Collect distinct poster image paths/URLs from collection items.
async fn collect_poster_urls(
    _id: Uuid,
    collection: &db::Media,
    db: &sqlx::SqlitePool,
    limit: u32,
    smart_filter_override: Option<&CollectionFilter>,
) -> Vec<String> {
    // Inspect extra records so duplicate artwork does not consume a grid cell.
    let filter = db::MediaFilter {
        limit: Some(limit.saturating_mul(2)),
        ..collection_item_filter(collection, smart_filter_override)
    };
    let own: Vec<String> = match db::Media::get_by_filter(db, &filter).await {
        Ok(result) => {
            let mut seen = HashSet::new();
            result
                .records
                .iter()
                .filter_map(poster_path)
                .filter(|path| seen.insert(path.clone()))
                .take(limit as usize)
                .collect()
        }
        Err(_) => Vec::new(),
    };
    if own.len() >= limit as usize {
        return own;
    }
    // A parent whose own query found too few posters (a smart collection of
    // collections finds none: it has no stored members) borrows from its
    // children, keeping any posters it already found first.
    let borrowed =
        collect_child_poster_urls(collection, db, limit, smart_filter_override).await;
    if borrowed.is_empty() {
        return own;
    }
    let mut seen: HashSet<String> = own
        .iter()
        .cloned()
        .collect();
    let mut out = own;
    for path in borrowed {
        if out.len() >= limit as usize {
            break;
        }
        if seen.insert(path.clone()) {
            out.push(path);
        }
    }
    out
}

/// Find a backdrop or poster URL from collection items (for the legacy single-image path).
async fn find_backdrop_url_from_collection(
    collection: &db::Media,
    db: &sqlx::SqlitePool,
) -> anyhow::Result<String> {
    let filter = db::MediaFilter {
        limit: Some(8),
        ..collection_item_filter(collection, None)
    };
    let items = db::Media::get_by_filter(db, &filter)
        .await?
        .records;

    items
        .iter()
        .find_map(|m| {
            m.images
                .get(ImageKind::Backdrop)
                .map(|i| {
                    i.path
                        .clone()
                })
        })
        .or_else(|| {
            items
                .iter()
                .find_map(|m| {
                    m.images
                        .get(ImageKind::Primary)
                        .map(|i| {
                            i.path
                                .clone()
                        })
                })
        })
        .ok_or_else(|| {
            anyhow::anyhow!("no image found for collection {}", collection.id)
        })
}

/// Pure CPU composition of one collection image in `shape`: background (custom
/// or brand gradient), poster field, then the overlay (plus, for the portrait
/// shape, a scrim under the lower band so text stays legible).
#[allow(clippy::too_many_arguments)]
fn compose_collection_art(
    shape: CollectionArtShape,
    layout: CollectionPosterLayout,
    background_color: [u8; 3],
    custom_background: Option<&RgbaImage>,
    posters: &[RgbaImage],
    overlay: &CollectionOverlay,
    name: &str,
    logo: Option<&RgbaImage>,
) -> anyhow::Result<RgbaImage> {
    let (out_w, out_h) = shape.dimensions();
    let has_custom_background = custom_background.is_some();
    let mut canvas = custom_background
        .map(|image| {
            // A background that does not match the shape is centre-cropped to fill it.
            DynamicImage::ImageRgba8(image.clone())
                .resize_to_fill(out_w, out_h, image::imageops::FilterType::Lanczos3)
                .into_rgba8()
        })
        .unwrap_or_else(|| gradient_background(background_color, shape));

    // Posters-only portrait art (no overlay) has no text band to reserve, so the
    // poster sheet covers the whole canvas instead of the upper field.
    if shape == CollectionArtShape::Portrait
        && matches!(overlay, CollectionOverlay::None)
        && layout != CollectionPosterLayout::None
        && !has_custom_background
        && !posters.is_empty()
    {
        let count = posters
            .len()
            .min(GRID_POSTER_LIMIT);
        stamp_portrait_cover(&mut canvas, &posters[..count]);
        apply_bottom_scrim(&mut canvas, 0.68, 0.55);
        return Ok(canvas);
    }
    if shape == CollectionArtShape::Portrait
        && posters.is_empty()
        && !has_custom_background
    {
        canvas = portrait_fallback_gradient(background_color);
        if matches!(overlay, CollectionOverlay::None) {
            // Nothing else identifies the tile, so print the name on it.
            let name_overlay = CollectionOverlay::Text {
                text: None,
                font_size: Some(80),
                font_family: None,
                font_weight: Some(CollectionFontWeight::Bold),
            };
            apply_overlay_sync(
                &mut canvas,
                &name_overlay,
                name,
                None,
                OverlayPlacement::Centre,
            )?;
            return Ok(canvas);
        }
    }

    let max_n = if layout == CollectionPosterLayout::Grid {
        GridSpec::for_shape(shape).limit()
    } else {
        4
    };
    let n = posters
        .len()
        .min(max_n);
    if layout != CollectionPosterLayout::None && n > 0 && !has_custom_background {
        if layout == CollectionPosterLayout::Grid {
            stamp_grid(&mut canvas, &posters[..n], shape);
        } else {
            let positions = layout_positions(layout, n, shape);
            let (poster_w, poster_h) = match shape {
                CollectionArtShape::Landscape => (POSTER_W, POSTER_H),
                CollectionArtShape::Portrait => (PORTRAIT_POSTER_W, PORTRAIT_POSTER_H),
            };
            for idx in (0..n).rev() {
                let (cx, cy, angle_deg) = positions[idx];
                let resized = image::imageops::resize(
                    &posters[idx],
                    poster_w,
                    poster_h,
                    image::imageops::FilterType::Lanczos3,
                );
                rotate_and_stamp(&mut canvas, resized, cx, cy, angle_deg);
            }
        }
    }

    let placement = OverlayPlacement::for_layout(shape, layout);
    if placement == OverlayPlacement::LowerBand {
        apply_bottom_scrim(&mut canvas, PORTRAIT_BAND_TOP - 0.1, 0.75);
    }
    apply_overlay_sync(&mut canvas, overlay, name, logo, placement)?;
    Ok(canvas)
}

/// Poster positions (canvas center_x, center_y, clockwise angle°) for a given layout and n posters.
fn layout_positions(
    layout: CollectionPosterLayout,
    n: usize,
    shape: CollectionArtShape,
) -> Vec<(i64, i64, f32)> {
    if shape == CollectionArtShape::Portrait {
        return portrait_layout_positions(layout, n);
    }
    let positions = match layout {
        // No posters are stamped for either layout — None skips them entirely,
        // and Grid is rendered as one transformed 3×3 plane by `stamp_grid`.
        CollectionPosterLayout::None | CollectionPosterLayout::Grid => Vec::new(),
        // Clean horizontal shelf, barely overlapping.
        CollectionPosterLayout::Row => match n {
            1 => vec![(720, 270, 0.0)],
            2 => vec![(645, 270, 0.0), (795, 270, 0.0)],
            3 => vec![(570, 270, 0.0), (715, 270, 0.0), (860, 270, 0.0)],
            _ => vec![
                (515, 270, 0.0),
                (638, 270, 0.0),
                (762, 270, 0.0),
                (885, 270, 0.0),
            ],
        },
        // Wide artistic scatter with bold angles and height variation.
        CollectionPosterLayout::Scatter => match n {
            1 => vec![(720, 270, 0.0)],
            2 => vec![(628, 288, -14.0), (808, 248, 16.0)],
            3 => vec![(570, 298, -17.0), (718, 245, 10.0), (850, 285, -11.0)],
            _ => vec![
                (545, 302, -19.0),
                (668, 240, 13.0),
                (778, 296, -9.0),
                (882, 244, 17.0),
            ],
        },
    };

    if matches!(
        layout,
        CollectionPosterLayout::Row | CollectionPosterLayout::Scatter
    ) {
        positions
            .into_iter()
            .map(|(x, y, angle)| (x + 50, y, angle))
            .collect()
    } else {
        positions
    }
}

/// Portrait poster field: 2:3 posters stacked in the upper part of the canvas
/// (everything above `PORTRAIT_BAND_TOP`), leaving the lower band for the overlay.
fn portrait_layout_positions(
    layout: CollectionPosterLayout,
    n: usize,
) -> Vec<(i64, i64, f32)> {
    match layout {
        CollectionPosterLayout::None | CollectionPosterLayout::Grid => Vec::new(),
        // A tidy, barely overlapping 2x2 stagger.
        CollectionPosterLayout::Row => match n {
            1 => vec![(300, 260, 0.0)],
            2 => vec![(190, 235, 0.0), (410, 315, 0.0)],
            3 => vec![(185, 170, 0.0), (415, 235, 0.0), (300, 420, 0.0)],
            _ => vec![
                (185, 140, 0.0),
                (415, 205, 0.0),
                (185, 410, 0.0),
                (415, 475, 0.0),
            ],
        },
        CollectionPosterLayout::Scatter => match n {
            1 => vec![(300, 260, 0.0)],
            2 => vec![(185, 230, -12.0), (415, 310, 13.0)],
            3 => vec![(175, 160, -15.0), (425, 225, 11.0), (290, 425, -9.0)],
            _ => vec![
                (175, 150, -15.0),
                (425, 205, 12.0),
                (175, 405, 9.0),
                (425, 465, -13.0),
            ],
        },
    }
}

/// Apply rounded corners (CORNER_RADIUS px) to an RGBA image in-place.
fn apply_rounded_corners(img: &mut RgbaImage) {
    let w = img.width();
    let h = img.height();
    let r = CORNER_RADIUS as f32;
    for py in 0..h {
        for px in 0..w {
            let fx = px as f32;
            let fy = py as f32;
            let in_corner = (fx < r && fy < r)
                || (fx > w as f32 - r - 1.0 && fy < r)
                || (fx < r && fy > h as f32 - r - 1.0)
                || (fx > w as f32 - r - 1.0 && fy > h as f32 - r - 1.0);
            if in_corner {
                let cx = if fx < r { r } else { w as f32 - r - 1.0 };
                let cy = if fy < r { r } else { h as f32 - r - 1.0 };
                if ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt() > r {
                    img.get_pixel_mut(px, py)[3] = 0;
                }
            }
        }
    }
}

/// Build a non-overlapping 3×3 poster sheet, then project it as a single
/// skewed plane. The plane starts beyond the text area and is intentionally
/// larger than the output canvas, so it reads as a background element.
fn stamp_grid(
    canvas: &mut RgbaImage,
    posters: &[RgbaImage],
    shape: CollectionArtShape,
) {
    let spec = GridSpec::for_shape(shape);
    let (grid_w, grid_h) = grid_dimensions(shape);
    let mut grid = RgbaImage::new(grid_w, grid_h);

    let prepared_posters: Vec<RgbaImage> = posters
        .iter()
        .map(|poster| {
            let mut resized = image::imageops::resize(
                poster,
                spec.poster_w,
                spec.poster_h,
                image::imageops::FilterType::Lanczos3,
            );
            apply_rounded_corners(&mut resized);
            resized
        })
        .collect();

    // Smaller collections still need a complete background plane. Reuse their
    // prepared posters cyclically instead of resizing the same image per cell.
    for idx in 0..spec.limit() {
        let poster = &prepared_posters[idx % prepared_posters.len()];
        let col = idx % spec.columns;
        let row = idx / spec.columns;
        let x = col as u32 * (spec.poster_w + GRID_GUTTER);
        let y = row as u32 * (spec.poster_h + GRID_GUTTER);
        image::imageops::overlay(&mut grid, poster, x.into(), y.into());
    }

    // An affine projection with a subtle clockwise rotation and horizontal
    // shear. Its centre sits off the right edge; most of the 3×3 sheet is
    // therefore clipped, leaving roughly the right third of the image visible.
    let k = spec.projection_scale;
    let (a, b, c, d) = (0.74f32 * k, -0.23f32 * k, 0.23f32 * k, 0.71f32 * k);
    let determinant = a * d - b * c;
    let (centre_x, centre_y) = spec.centre;
    let half_w = grid_w as f32 / 2.0;
    let half_h = grid_h as f32 / 2.0;
    let (out_w, out_h) = shape.dimensions();
    let mut projected = RgbaImage::new(out_w, out_h);

    for y in 0..out_h {
        for x in 0..out_w {
            let dx = x as f32 - centre_x;
            let dy = y as f32 - centre_y;
            let src_x = (d * dx - b * dy) / determinant + half_w;
            let src_y = (-c * dx + a * dy) / determinant + half_h;
            if src_x < 0.0
                || src_y < 0.0
                || src_x >= (grid_w - 1) as f32
                || src_y >= (grid_h - 1) as f32
            {
                continue;
            }

            let x0 = src_x as u32;
            let y0 = src_y as u32;
            let x1 = x0 + 1;
            let y1 = y0 + 1;
            let tx = src_x.fract();
            let ty = src_y.fract();
            let interpolate = |p00: u8, p10: u8, p01: u8, p11: u8| {
                let top = p00 as f32 + (p10 as f32 - p00 as f32) * tx;
                let bottom = p01 as f32 + (p11 as f32 - p01 as f32) * tx;
                (top + (bottom - top) * ty) as u8
            };
            let p00 = grid
                .get_pixel(x0, y0)
                .0;
            let p10 = grid
                .get_pixel(x1, y0)
                .0;
            let p01 = grid
                .get_pixel(x0, y1)
                .0;
            let p11 = grid
                .get_pixel(x1, y1)
                .0;
            projected.put_pixel(
                x,
                y,
                Rgba([
                    interpolate(p00[0], p10[0], p01[0], p11[0]),
                    interpolate(p00[1], p10[1], p01[1], p11[1]),
                    interpolate(p00[2], p10[2], p01[2], p11[2]),
                    interpolate(p00[3], p10[3], p01[3], p11[3]),
                ]),
            );
        }
    }

    image::imageops::overlay(canvas, &projected, 0, 0);
}

/// Poster sheet used by posters-only portrait art: a 3x3 grid of 2:3 posters
/// with thin gutters, rotated slightly about the canvas centre. The sheet is
/// larger than the rotated canvas, so the whole 600x900 image is covered and
/// posters bleed off every edge evenly. A single poster simply fills the
/// canvas.
fn stamp_portrait_cover(canvas: &mut RgbaImage, posters: &[RgbaImage]) {
    const COLS: u32 = 3;
    const ROWS: u32 = 3;
    const CELL_W: u32 = 250;
    const CELL_H: u32 = 375;
    const GUTTER: u32 = 8;
    const TILT_DEG: f32 = -7.0;

    let (out_w, out_h) = (canvas.width(), canvas.height());

    if posters.len() == 1 {
        let filled = DynamicImage::ImageRgba8(posters[0].clone())
            .resize_to_fill(out_w, out_h, image::imageops::FilterType::Lanczos3)
            .into_rgba8();
        image::imageops::overlay(canvas, &filled, 0, 0);
        return;
    }

    let sheet_w = COLS * CELL_W + (COLS - 1) * GUTTER;
    let sheet_h = ROWS * CELL_H + (ROWS - 1) * GUTTER;
    let mut sheet = RgbaImage::new(sheet_w, sheet_h);
    let prepared: Vec<RgbaImage> = posters
        .iter()
        .map(|poster| {
            let mut resized = image::imageops::resize(
                poster,
                CELL_W,
                CELL_H,
                image::imageops::FilterType::Lanczos3,
            );
            apply_rounded_corners(&mut resized);
            resized
        })
        .collect();
    // Shift each row by one so a short poster list never repeats down a column.
    for row in 0..ROWS {
        for col in 0..COLS {
            let idx = (row * COLS + col + row) as usize % prepared.len();
            let x = col * (CELL_W + GUTTER);
            let y = row * (CELL_H + GUTTER);
            image::imageops::overlay(&mut sheet, &prepared[idx], x.into(), y.into());
        }
    }

    let theta = TILT_DEG * std::f32::consts::PI / 180.0;
    let (sin_t, cos_t) = theta.sin_cos();
    let (half_sw, half_sh) = (sheet_w as f32 / 2.0, sheet_h as f32 / 2.0);
    let (cx, cy) = (out_w as f32 / 2.0, out_h as f32 / 2.0);
    let mut projected = RgbaImage::new(out_w, out_h);
    for y in 0..out_h {
        for x in 0..out_w {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            // Inverse rotation back into sheet space.
            let src_x = dx * cos_t + dy * sin_t + half_sw;
            let src_y = -dx * sin_t + dy * cos_t + half_sh;
            if src_x < 0.0
                || src_y < 0.0
                || src_x >= (sheet_w - 1) as f32
                || src_y >= (sheet_h - 1) as f32
            {
                continue;
            }
            let (x0, y0) = (src_x as u32, src_y as u32);
            let (tx, ty) = (src_x.fract(), src_y.fract());
            let p00 = sheet
                .get_pixel(x0, y0)
                .0;
            let p10 = sheet
                .get_pixel(x0 + 1, y0)
                .0;
            let p01 = sheet
                .get_pixel(x0, y0 + 1)
                .0;
            let p11 = sheet
                .get_pixel(x0 + 1, y0 + 1)
                .0;
            let mut out = [0u8; 4];
            for c in 0..4 {
                let top = p00[c] as f32 + (p10[c] as f32 - p00[c] as f32) * tx;
                let bottom = p01[c] as f32 + (p11[c] as f32 - p01[c] as f32) * tx;
                out[c] = (top + (bottom - top) * ty) as u8;
            }
            projected.put_pixel(x, y, Rgba(out));
        }
    }
    image::imageops::overlay(canvas, &projected, 0, 0);
}

/// Portrait background for art with no posters: the brand gradient, or a soft
/// slate gradient when the configured colour is (near) black, so the tile is
/// never a flat black rectangle.
fn portrait_fallback_gradient(color: [u8; 3]) -> RgbaImage {
    let (w, h) = CollectionArtShape::Portrait.dimensions();
    let dark = color
        .iter()
        .all(|c| *c < 48);
    if !dark {
        return gradient_background(color, CollectionArtShape::Portrait);
    }
    let top = [58.0f32, 70.0, 104.0];
    let bottom = [22.0f32, 26.0, 42.0];
    RgbaImage::from_fn(w, h, |_, y| {
        let t = y as f32 / (h - 1) as f32;
        Rgba([
            (top[0] + (bottom[0] - top[0]) * t) as u8,
            (top[1] + (bottom[1] - top[1]) * t) as u8,
            (top[2] + (bottom[2] - top[2]) * t) as u8,
            255,
        ])
    })
}

/// Poster-sheet geometry for the `Grid` layout in each shape.
struct GridSpec {
    columns: usize,
    rows: usize,
    poster_w: u32,
    poster_h: u32,
    /// Uniform scale applied to the affine projection matrix.
    projection_scale: f32,
    /// Where the projected sheet's centre lands on the output canvas.
    centre: (f32, f32),
}

impl GridSpec {
    fn for_shape(shape: CollectionArtShape) -> Self {
        match shape {
            CollectionArtShape::Landscape => Self {
                columns: GRID_COLUMNS,
                rows: GRID_POSTER_LIMIT.div_ceil(GRID_COLUMNS),
                poster_w: GRID_POSTER_W,
                poster_h: GRID_POSTER_H,
                projection_scale: 1.0,
                centre: (835.0, 320.0),
            },
            // 2 columns x 3 rows, bleeding off the top and sides of the
            // poster field above the overlay band.
            CollectionArtShape::Portrait => Self {
                columns: 2,
                rows: 3,
                poster_w: 200,
                poster_h: 300,
                projection_scale: 1.5,
                centre: (290.0, 215.0),
            },
        }
    }

    /// Number of posters the sheet holds.
    fn limit(&self) -> usize {
        self.columns * self.rows
    }
}

fn grid_dimensions(shape: CollectionArtShape) -> (u32, u32) {
    let spec = GridSpec::for_shape(shape);
    let (columns, rows) = (spec.columns as u32, spec.rows as u32);
    (
        columns * spec.poster_w + (columns - 1) * GRID_GUTTER,
        rows * spec.poster_h + (rows - 1) * GRID_GUTTER,
    )
}

/// Rotate `poster` by `angle_deg` (clockwise) and stamp it onto `canvas`
/// so its centre lands at (`cx`, `cy`).
fn rotate_and_stamp(
    canvas: &mut RgbaImage,
    mut poster: RgbaImage,
    cx: i64,
    cy: i64,
    angle_deg: f32,
) {
    apply_rounded_corners(&mut poster);

    let pw = poster.width();
    let ph = poster.height();

    // Embed the poster centred in a square big enough that rotation never clips it.
    let buf_size = ROTATION_CANVAS;
    let ox = ((buf_size - pw) / 2) as i64;
    let oy = ((buf_size - ph) / 2) as i64;
    let mut buf = RgbaImage::new(buf_size, buf_size);
    image::imageops::overlay(&mut buf, &poster, ox, oy);

    // Bilinear inverse-mapping rotation (no external imageproc dep needed).
    let bx = buf_size as i32;
    let by = buf_size as i32;
    let half_x = buf_size as f32 / 2.0;
    let half_y = buf_size as f32 / 2.0;
    let theta = angle_deg * std::f32::consts::PI / 180.0;
    let cos_a = theta.cos();
    let sin_a = theta.sin();
    let mut rotated = RgbaImage::new(buf_size, buf_size);
    for oy in 0..buf_size {
        for ox in 0..buf_size {
            let fx = ox as f32 - half_x;
            let fy = oy as f32 - half_y;
            // Inverse rotation (CW angle → CCW inverse)
            let src_x = fx * cos_a + fy * sin_a + half_x;
            let src_y = -fx * sin_a + fy * cos_a + half_y;
            if src_x >= 0.0
                && src_x < (bx - 1) as f32
                && src_y >= 0.0
                && src_y < (by - 1) as f32
            {
                let x0 = src_x as u32;
                let y0 = src_y as u32;
                let x1 = (x0 + 1).min(buf_size - 1);
                let y1 = (y0 + 1).min(buf_size - 1);
                let fx2 = src_x.fract();
                let fy2 = src_y.fract();
                let p00 = buf
                    .get_pixel(x0, y0)
                    .0;
                let p10 = buf
                    .get_pixel(x1, y0)
                    .0;
                let p01 = buf
                    .get_pixel(x0, y1)
                    .0;
                let p11 = buf
                    .get_pixel(x1, y1)
                    .0;
                let interp = |a: u8, b: u8, c: u8, d: u8| -> u8 {
                    let top = a as f32 + (b as f32 - a as f32) * fx2;
                    let bot = c as f32 + (d as f32 - c as f32) * fx2;
                    (top + (bot - top) * fy2) as u8
                };
                rotated.put_pixel(
                    ox,
                    oy,
                    Rgba([
                        interp(p00[0], p10[0], p01[0], p11[0]),
                        interp(p00[1], p10[1], p01[1], p11[1]),
                        interp(p00[2], p10[2], p01[2], p11[2]),
                        interp(p00[3], p10[3], p01[3], p11[3]),
                    ]),
                );
            }
        }
    }

    let stamp_x = cx - buf_size as i64 / 2;
    let stamp_y = cy - buf_size as i64 / 2;
    image::imageops::overlay(canvas, &rotated, stamp_x, stamp_y);
}

/// Sync overlay renderer — called from inside `spawn_blocking`.
/// `logo_image` is the already-downloaded streaming logo (if any). `centered`
/// is set for [`CollectionPosterLayout::None`], where there is no poster grid
/// reserving space on one side, so the overlay is centered on the full canvas
/// instead of left-aligned in the text box next to it.
fn apply_overlay_sync(
    canvas: &mut RgbaImage,
    overlay: &CollectionOverlay,
    fallback_name: &str,
    logo_image: Option<&RgbaImage>,
    placement: OverlayPlacement,
) -> anyhow::Result<()> {
    let (out_w, out_h) = (canvas.width(), canvas.height());
    let centered = placement != OverlayPlacement::LeftColumn;
    // Configured font sizes are authored against the 960px-wide landscape tile.
    let width_scale = out_w as f32 / OUT_W as f32;
    // Vertical extent the overlay is centred in: the whole canvas, or the lower
    // band beneath the poster field.
    let (area_top, area_h) = if placement == OverlayPlacement::LowerBand {
        let top = out_h as f32 * PORTRAIT_BAND_TOP;
        (top, out_h as f32 - top)
    } else {
        (0.0, out_h as f32)
    };
    match overlay {
        CollectionOverlay::None => {}
        CollectionOverlay::Text {
            text,
            font_size,
            font_family,
            font_weight,
        } => {
            let label = text
                .as_deref()
                .unwrap_or(fallback_name);
            let size = (*font_size).unwrap_or(90) as f32 * width_scale;
            let font_bytes = collection_font_bytes(font_family.unwrap_or_default());
            let mut font = FontRef::try_from_slice(font_bytes)
                .map_err(|e| anyhow::anyhow!("font load: {e:?}"))?;
            use ab_glyph::VariableFont;
            let weight = match font_weight.unwrap_or_default() {
                CollectionFontWeight::Regular => 400.0,
                CollectionFontWeight::Bold => 700.0,
            };
            let _ = font.set_variation(b"wght", weight);
            let fallbacks = fallback_fonts(weight);

            let max_w = if centered {
                out_w as f32 * 0.85
            } else {
                TEXT_AREA_END as f32 - TEXT_LEFT_MARGIN as f32
            };
            let scale = PxScale::from(size);

            let lines = wrap_words(&font, scale, label, max_w);

            let sf = font.as_scaled(scale);
            let line_h = sf.ascent() - sf.descent();
            let line_gap = line_h * 0.18;
            let step = line_h + line_gap;
            let total_h = step * lines.len() as f32 - line_gap;
            let start_y = (area_top + (area_h - total_h) / 2.0) as i32;

            for (i, line) in lines
                .iter()
                .enumerate()
            {
                let y = start_y + (i as f32 * step) as i32;
                let x = if centered {
                    ((out_w as f32 - measure_text_width(&font, scale, line)) / 2.0)
                        as i32
                } else {
                    TEXT_LEFT_MARGIN
                };
                draw_text_with_fallback(
                    canvas,
                    Rgba([255, 255, 255, 255]),
                    x,
                    y,
                    scale,
                    &font,
                    &fallbacks,
                    line,
                );
            }
        }
        CollectionOverlay::StreamingLogo { .. } => {
            if let Some(logo) = logo_image {
                let max_logo_h = match placement {
                    OverlayPlacement::Centre => out_h * 9 / 25,
                    OverlayPlacement::LeftColumn => out_h * 3 / 10,
                    OverlayPlacement::LowerBand => (out_h as f32 * 0.14) as u32,
                };
                let max_logo_w = if centered {
                    (out_w as f32 * 0.7) as u32
                } else {
                    (out_w as f32 * 0.34) as u32
                };
                let scale = (max_logo_h as f32 / logo.height() as f32)
                    .min(max_logo_w as f32 / logo.width() as f32);
                let lw = (logo.width() as f32 * scale) as u32;
                let lh = (logo.height() as f32 * scale) as u32;
                let scaled = image::imageops::resize(
                    logo,
                    lw,
                    lh,
                    image::imageops::FilterType::Lanczos3,
                );
                let lx = if centered {
                    (out_w.saturating_sub(lw) / 2) as i64
                } else {
                    TEXT_LEFT_MARGIN as i64
                };
                let ly = (area_top + (area_h - lh as f32).max(0.0) / 2.0) as i64;
                let mut shadow = scaled.clone();
                for pixel in shadow.pixels_mut() {
                    if pixel[3] > 0 {
                        pixel[0] = 0;
                        pixel[1] = 0;
                        pixel[2] = 0;
                        pixel[3] = (pixel[3] as f32 * 0.35) as u8;
                    }
                }
                image::imageops::overlay(canvas, &shadow, lx + 3, ly + 3);
                image::imageops::overlay(canvas, &scaled, lx, ly);
            }
        }
    }
    Ok(())
}

fn provider_wordmark(provider_name: Option<&str>) -> Option<RgbaImage> {
    let bytes = match provider_name?
        .to_ascii_lowercase()
        .as_str()
    {
        "netflix" => PROVIDER_LOGO_NETFLIX,
        "amazon prime video" | "prime video" => PROVIDER_LOGO_PRIME_VIDEO,
        "disney plus" | "disney+" => PROVIDER_LOGO_DISNEY_PLUS,
        "max" | "hbo max" => PROVIDER_LOGO_MAX,
        "hulu" => PROVIDER_LOGO_HULU,
        "paramount plus" | "paramount+" => PROVIDER_LOGO_PARAMOUNT_PLUS,
        "crunchyroll" => PROVIDER_LOGO_CRUNCHYROLL,
        "apple tv plus" | "apple tv+" | "apple tv" => PROVIDER_LOGO_APPLE_TV,
        "viaplay" => PROVIDER_LOGO_VIAPLAY,
        "skyshowtime" => PROVIDER_LOGO_SKYSHOWTIME,
        "peacock" => PROVIDER_LOGO_PEACOCK,
        "mubi" => PROVIDER_LOGO_MUBI,
        "pluto tv" => PROVIDER_LOGO_PLUTO_TV,
        "youtube" | "youtube premium" => PROVIDER_LOGO_YOUTUBE,
        "amc+" | "amc plus" => PROVIDER_LOGO_AMC_PLUS,
        "adn" | "animation digital network" => PROVIDER_LOGO_ADN,
        "funimation now" | "funimation" => PROVIDER_LOGO_FUNIMATION_NOW,
        "illico+" | "illico plus" => PROVIDER_LOGO_ILLICO_PLUS,
        "sky mais" | "skymais" => PROVIDER_LOGO_SKY_MAIS,
        "c more" | "cmore" => PROVIDER_LOGO_C_MORE,
        "foxtel now" => PROVIDER_LOGO_FOXTEL_NOW,
        "jiohotstar" => PROVIDER_LOGO_JIOHOTSTAR,
        "wetv" => PROVIDER_LOGO_WETV,
        "chorki" => PROVIDER_LOGO_CHORKI,
        "tvp vod" => PROVIDER_LOGO_TVP_VOD,
        "kwelitv" => PROVIDER_LOGO_KWELITV,
        "vidio" => PROVIDER_LOGO_VIDIO,
        "vivamax" => PROVIDER_LOGO_VIVAMAX,
        "ruutu" => PROVIDER_LOGO_RUUTU,
        "samsung tv plus" => PROVIDER_LOGO_SAMSUNG_TV_PLUS,
        "craftsy" => PROVIDER_LOGO_CRAFTSY,
        "mx player" => PROVIDER_LOGO_MX_PLAYER,
        "axn now" | "axnnow" => PROVIDER_LOGO_AXN_NOW,
        "claro video" => PROVIDER_LOGO_CLARO_VIDEO,
        "dimsum" => PROVIDER_LOGO_DIMSUM,
        "catchplay+" | "catchplay" => PROVIDER_LOGO_CATCHPLAY,
        "fanatiz" => PROVIDER_LOGO_FANATIZ,
        "movistar plus+" | "movistar+" | "movistar plus" => PROVIDER_LOGO_MOVISTAR_PLUS,
        "discovery+" | "discovery plus" => PROVIDER_LOGO_DISCOVERY_PLUS,
        "kocowa" => PROVIDER_LOGO_KOCOWA,
        "toku" => PROVIDER_LOGO_TOKU,
        "watcha" => PROVIDER_LOGO_WATCHA,
        "wakanim" => PROVIDER_LOGO_WAKANIM,
        "dplay" => PROVIDER_LOGO_DPLAY,
        "filmdoo" => PROVIDER_LOGO_FILMDOO,
        "globoplay" => PROVIDER_LOGO_GLOBOPLAY,
        "mtv katsomo" => PROVIDER_LOGO_MTV_KATSOMO,
        "u-next" | "unext" => PROVIDER_LOGO_U_NEXT,
        "rcti+" | "rcti plus" => PROVIDER_LOGO_RCTI_PLUS,
        "nfl+" | "nfl plus" => PROVIDER_LOGO_NFL_PLUS,
        "nasa+" | "nasa plus" => PROVIDER_LOGO_NASA_PLUS,
        "iwanttfc" | "iwant" => PROVIDER_LOGO_IWANTTFC,
        "kapamilya online live" => PROVIDER_LOGO_KAPAMILYA_ONLINE_LIVE,
        "hayu" => PROVIDER_LOGO_HAYU,
        "watch it" => PROVIDER_LOGO_WATCH_IT,
        "stan" => PROVIDER_LOGO_STAN,
        "shudder" => PROVIDER_LOGO_SHUDDER,
        "roxi" => PROVIDER_LOGO_ROXI,
        _ => return None,
    };
    image::load_from_memory(bytes)
        .ok()
        .map(|image| image.into_rgba8())
}

/// Break `text` into lines where each line fits within `max_w` pixels at `scale`.
fn wrap_words(
    font: &FontRef<'_>,
    scale: PxScale,
    text: &str,
    max_w: f32,
) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{current} {word}")
        };
        if !current.is_empty() && measure_text_width(font, scale, &candidate) > max_w {
            lines.push(current);
            current = word.to_string();
        } else {
            current = candidate;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(text.to_string());
    }
    lines
}

fn solid_background(shape: CollectionArtShape) -> RgbImage {
    let (w, h) = shape.dimensions();
    RgbImage::from_pixel(w, h, Rgb([0, 0, 0]))
}

fn parse_hex_color(value: &str) -> Option<[u8; 3]> {
    let value = value.strip_prefix('#')?;
    if value.len() != 6 {
        return None;
    }
    Some([
        u8::from_str_radix(&value[0..2], 16).ok()?,
        u8::from_str_radix(&value[2..4], 16).ok()?,
        u8::from_str_radix(&value[4..6], 16).ok()?,
    ])
}

fn gradient_background(color: [u8; 3], shape: CollectionArtShape) -> RgbaImage {
    let (out_w, out_h) = shape.dimensions();
    let mut image = RgbaImage::new(out_w, out_h);
    for y in 0..out_h {
        let t = y as f32 / (out_h.saturating_sub(1)) as f32;
        let factor = 0.7 + t * 0.3;
        let pixel = Rgba([
            (color[0] as f32 * factor) as u8,
            (color[1] as f32 * factor) as u8,
            (color[2] as f32 * factor) as u8,
            255,
        ]);
        for x in 0..out_w {
            image.put_pixel(x, y, pixel);
        }
    }
    image
}

/// Blend a semi-transparent black overlay over the image to darken it,
/// matching Jellyfin's `SKColors.Black.WithAlpha(0x78)` step.
fn apply_dark_overlay(mut img: RgbImage) -> RgbImage {
    let inv = 1.0 - OVERLAY_ALPHA;
    for pixel in img.pixels_mut() {
        pixel[0] = (pixel[0] as f32 * inv) as u8;
        pixel[1] = (pixel[1] as f32 * inv) as u8;
        pixel[2] = (pixel[2] as f32 * inv) as u8;
    }
    img
}

/// Darken the bottom of the canvas with a vertical black gradient (alpha 0 at
/// `start_frac` of the height rising to `max_alpha` at the bottom edge), so the
/// overlay stays legible over the poster field.
fn apply_bottom_scrim(canvas: &mut RgbaImage, start_frac: f32, max_alpha: f32) {
    let (w, h) = (canvas.width(), canvas.height());
    let start = (h as f32 * start_frac) as u32;
    let span = h
        .saturating_sub(start)
        .max(1) as f32;
    for y in start..h {
        let t = (y - start) as f32 / span;
        let keep = 1.0 - max_alpha * t;
        for x in 0..w {
            let pixel = canvas.get_pixel_mut(x, y);
            pixel[0] = (pixel[0] as f32 * keep) as u8;
            pixel[1] = (pixel[1] as f32 * keep) as u8;
            pixel[2] = (pixel[2] as f32 * keep) as u8;
        }
    }
}

/// Render the library name centered on the image in white text.
fn draw_label(mut img: RgbImage, name: &str) -> anyhow::Result<RgbImage> {
    let (out_w, out_h) = (img.width(), img.height());
    let font = FontRef::try_from_slice(FONT_BOLD)
        .map_err(|e| anyhow::anyhow!("font load failed: {e:?}"))?;

    // Start at ~20% of image height and scale down until it fits within 90% width.
    // Sized from the shorter side so the label reads the same in either shape.
    let mut scale = PxScale::from(out_w.min(out_h) as f32 * 0.20);
    let max_width = out_w as f32 * MAX_TEXT_FRACTION;
    let mut tw = measure_text_width(&font, scale, name);
    if tw > max_width {
        scale = PxScale::from(scale.x * max_width / tw);
        tw = measure_text_width(&font, scale, name);
    }

    let text_height = {
        let sf = font.as_scaled(scale);
        sf.ascent() - sf.descent()
    };

    let x = ((out_w as f32 - tw) / 2.0) as i32;
    let y = ((out_h as f32 - text_height) / 2.0) as i32;

    draw_text_mut(&mut img, Rgb([255, 255, 255]), x, y, scale, &font, name);

    Ok(img)
}

fn measure_text_width(font: &FontRef<'_>, scale: PxScale, text: &str) -> f32 {
    let scaled = font.as_scaled(scale);
    let mut width = 0.0f32;
    let mut prev: Option<ab_glyph::GlyphId> = None;
    for c in text.chars() {
        let glyph_id = scaled.glyph_id(c);
        if let Some(p) = prev {
            width += scaled.kern(p, glyph_id);
        }
        width += scaled.h_advance(glyph_id);
        prev = Some(glyph_id);
    }
    width
}

/// Loads the fallback chain (see `FALLBACK_FONT_BYTES`) at the given variable
/// weight, matching whatever the primary font was set to. Built fresh per
/// call — cheap, since this only parses font headers, not glyph outlines.
fn fallback_fonts(weight: f32) -> Vec<FontRef<'static>> {
    use ab_glyph::VariableFont;
    FALLBACK_FONT_BYTES
        .iter()
        .filter_map(|bytes| FontRef::try_from_slice(bytes).ok())
        .map(|mut font| {
            let _ = font.set_variation(b"wght", weight);
            font
        })
        .collect()
}

/// Whether `font` has a real glyph for `c`. `ab_glyph` returns glyph id 0
/// (`.notdef`) for anything outside the font's coverage, which is otherwise
/// indistinguishable from a legitimately empty glyph.
fn font_covers(font: &FontRef<'_>, c: char) -> bool {
    c.is_whitespace()
        || font
            .glyph_id(c)
            .0
            != 0
}

/// Draws `text` at `(x, y)`, substituting the first fallback font that has a
/// given character instead of leaving it blank when `primary` doesn't cover
/// it — e.g. a symbol like ♥ that only some of the bundled fonts include.
/// Splits `text` into runs by resolved font and draws each run separately;
/// kerning across a font switch is not applied, a minor cosmetic gap next to
/// an already-uncommon fallback character. This does not add support for
/// color/pictographic emoji: `ab_glyph` only rasterizes monochrome vector
/// outlines, so a true emoji codepoint has no glyph in any of these fonts
/// either and still renders as nothing.
fn draw_text_with_fallback<'f>(
    canvas: &mut RgbaImage,
    color: Rgba<u8>,
    x: i32,
    y: i32,
    scale: PxScale,
    primary: &'f FontRef<'f>,
    fallbacks: &'f [FontRef<'f>],
    text: &str,
) {
    let font_for = |c: char| -> &'f FontRef<'f> {
        if font_covers(primary, c) {
            return primary;
        }
        fallbacks
            .iter()
            .find(|f| font_covers(f, c))
            .unwrap_or(primary)
    };

    let mut cursor_x = x as f32;
    let mut run = String::new();
    let mut run_font: Option<&'f FontRef<'f>> = None;

    for c in text.chars() {
        let f = font_for(c);
        let same = run_font.is_some_and(|rf| std::ptr::eq(rf, f));
        if !same && !run.is_empty() {
            let font = run_font.unwrap();
            draw_text_mut(canvas, color, cursor_x as i32, y, scale, font, &run);
            cursor_x += measure_text_width(font, scale, &run);
            run.clear();
        }
        run_font = Some(f);
        run.push(c);
    }
    if !run.is_empty() {
        let font = run_font.unwrap();
        draw_text_mut(canvas, color, cursor_x as i32, y, scale, font, &run);
    }
}

fn ext_for_content_type(ct: &str) -> &'static str {
    match ct {
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "jpg",
    }
}

fn encode_jpeg(img: RgbImage) -> anyhow::Result<Vec<u8>> {
    let mut buf = Cursor::new(Vec::new());
    img.write_to(&mut buf, ImageFormat::Jpeg)?;
    Ok(buf.into_inner())
}

/// Generated collection art is fully opaque photographic content. JPEG avoids
/// the disproportionately expensive lossless PNG compression of a poster grid.
fn encode_jpeg_rgba(img: RgbaImage) -> anyhow::Result<Vec<u8>> {
    let mut buf = Cursor::new(Vec::new());
    let rgb = DynamicImage::ImageRgba8(img).into_rgb8();
    use image::codecs::jpeg::JpegEncoder;
    JpegEncoder::new_with_quality(&mut buf, 90)
        .encode_image(&DynamicImage::ImageRgb8(rgb))?;
    Ok(buf.into_inner())
}

// ---------------------------------------------------------------------------
// Image processing helpers (sync — run in spawn_blocking)
// ---------------------------------------------------------------------------

fn process_image_sync(
    bytes: &[u8],
    opts: &ImageProcessOptions,
) -> anyhow::Result<(Vec<u8>, &'static str)> {
    let img = image::load_from_memory(bytes)?;
    let has_alpha = img
        .color()
        .has_alpha();
    let img = apply_sizing(img, opts);
    let img = if let Some(sigma) = opts.blur {
        img.blur(sigma as f32)
    } else {
        img
    };

    // Auto-preserve transparency: use PNG when the source has alpha and the caller
    // didn't explicitly request a lossy format.
    let use_png = matches!(
        opts.format
            .as_deref(),
        Some("png")
    ) || (has_alpha
        && !matches!(
            opts.format
                .as_deref(),
            Some("jpeg" | "jpg")
        ));

    let quality = opts
        .quality
        .unwrap_or(90);
    let mut buf = Cursor::new(Vec::<u8>::new());
    if use_png {
        img.write_to(&mut buf, ImageFormat::Png)?;
        Ok((buf.into_inner(), "image/png"))
    } else {
        use image::codecs::jpeg::JpegEncoder;
        img.write_with_encoder(JpegEncoder::new_with_quality(&mut buf, quality))?;
        Ok((buf.into_inner(), "image/jpeg"))
    }
}

/// Resize `img` according to sizing params (fill → exact → max priority order).
fn apply_sizing(img: DynamicImage, opts: &ImageProcessOptions) -> DynamicImage {
    let orig_w = img.width();
    let orig_h = img.height();

    // Priority 1: fill — cover the requested box, including upscaling local
    // generated art when the client asks for a larger rendition. With both
    // dimensions supplied this deliberately crops the excess so the response
    // has the exact dimensions requested by Jellyfin clients.
    if opts
        .fill_width
        .is_some()
        || opts
            .fill_height
            .is_some()
    {
        return match (opts.fill_width, opts.fill_height) {
            (Some(width), Some(height)) => {
                img.resize_to_fill(width, height, image::imageops::FilterType::Lanczos3)
            }
            (Some(width), None) => {
                img.resize(width, u32::MAX, image::imageops::FilterType::Lanczos3)
            }
            (None, Some(height)) => {
                img.resize(u32::MAX, height, image::imageops::FilterType::Lanczos3)
            }
            (None, None) => unreachable!("fill sizing requires a dimension"),
        };
    }

    // Priority 2: exact width / height (missing dimension maintains AR).
    if opts
        .width
        .is_some()
        || opts
            .height
            .is_some()
    {
        let nw = opts
            .width
            .unwrap_or(u32::MAX);
        let nh = opts
            .height
            .unwrap_or(u32::MAX);
        return img.resize(nw, nh, image::imageops::FilterType::Lanczos3);
    }

    // Priority 3: max — cap size, scale down only.
    let cap_w = opts
        .max_width
        .unwrap_or(u32::MAX);
    let cap_h = opts
        .max_height
        .unwrap_or(u32::MAX);
    if orig_w > cap_w || orig_h > cap_h {
        return img.resize(cap_w, cap_h, image::imageops::FilterType::Lanczos3);
    }

    img
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_posters() -> Vec<RgbaImage> {
        (0..9u32)
            .map(|i| {
                RgbaImage::from_fn(200, 300, |x, y| {
                    Rgba([
                        (i * 28 + x / 2) as u8,
                        (255 - ((i * 20 + y / 3) % 255)) as u8,
                        ((x + y + i * 40) % 256) as u8,
                        255,
                    ])
                })
            })
            .collect()
    }

    fn test_overlays() -> Vec<(&'static str, CollectionOverlay)> {
        vec![
            ("none", CollectionOverlay::None),
            (
                "text",
                CollectionOverlay::Text {
                    text: Some("Marvel Cinematic Universe".into()),
                    font_size: Some(90),
                    font_family: None,
                    font_weight: None,
                },
            ),
        ]
    }

    fn compose(
        shape: CollectionArtShape,
        layout: CollectionPosterLayout,
        n: usize,
        overlay: &CollectionOverlay,
    ) -> RgbaImage {
        compose_collection_art(
            shape,
            layout,
            [120, 30, 60],
            None,
            &test_posters()[..n],
            overlay,
            "Fallback",
            None,
        )
        .unwrap()
    }

    #[test]
    fn composition_matches_the_shape_for_every_layout_and_overlay() {
        for shape in [CollectionArtShape::Landscape, CollectionArtShape::Portrait] {
            for layout in [
                CollectionPosterLayout::Grid,
                CollectionPosterLayout::Row,
                CollectionPosterLayout::Scatter,
                CollectionPosterLayout::None,
            ] {
                for (_, overlay) in test_overlays() {
                    for n in [0usize, 1, 3, 9] {
                        // Grid needs at least one poster (callers never pass 0).
                        let n = if layout == CollectionPosterLayout::Grid {
                            n.max(1)
                        } else {
                            n
                        };
                        let canvas = compose(shape, layout, n, &overlay);
                        assert_eq!(
                            (canvas.width(), canvas.height()),
                            shape.dimensions(),
                            "{shape} {layout:?} n={n}"
                        );
                    }
                }
            }
        }
        assert_eq!(CollectionArtShape::Landscape.dimensions(), (960, 540));
        assert_eq!(CollectionArtShape::Portrait.dimensions(), (600, 900));
        assert!((CollectionArtShape::Portrait.aspect_ratio() - 2.0 / 3.0).abs() < 1e-6);
    }

    /// Fraction of a coarse pixel grid whose colour is not (near) black.
    fn lit_fraction(img: &RgbaImage) -> f32 {
        let (mut lit, mut total) = (0u32, 0u32);
        for gy in 0..30u32 {
            for gx in 0..20u32 {
                let p = img.get_pixel(gx * 30 + 15, gy * 30 + 15);
                total += 1;
                if p[0]
                    .max(p[1])
                    .max(p[2])
                    > 24
                {
                    lit += 1;
                }
            }
        }
        lit as f32 / total as f32
    }

    #[test]
    fn portrait_posters_only_art_covers_the_whole_canvas() {
        for layout in [
            CollectionPosterLayout::Grid,
            CollectionPosterLayout::Row,
            CollectionPosterLayout::Scatter,
        ] {
            for n in [1usize, 2, 3, 4, 6, 9] {
                let canvas = compose(
                    CollectionArtShape::Portrait,
                    layout,
                    n,
                    &CollectionOverlay::None,
                );
                assert_eq!((canvas.width(), canvas.height()), (600, 900));
                let frac = lit_fraction(&canvas);
                assert!(frac > 0.9, "{layout:?} n={n}: only {frac} covered");
                // Corners are covered too, not just the middle.
                for (x, y) in [(2, 2), (597, 2), (2, 800), (597, 800)] {
                    let p = canvas.get_pixel(x, y);
                    assert!(
                        p[0].max(p[1])
                            .max(p[2])
                            > 24,
                        "{layout:?} n={n} ({x},{y})"
                    );
                }
            }
        }
    }

    #[test]
    fn portrait_without_posters_is_never_flat_black() {
        let canvas = portrait_fallback_gradient([0, 0, 0]);
        assert_eq!((canvas.width(), canvas.height()), (600, 900));
        assert!(lit_fraction(&canvas) > 0.99);
        assert_ne!(canvas.get_pixel(10, 10), canvas.get_pixel(10, 890));
    }

    #[test]
    fn portrait_scrim_darkens_the_overlay_band_but_not_the_poster_field() {
        // A blank text overlay draws nothing but keeps the overlay band (and
        // so the scrim) in play; a bare `None` overlay skips the band.
        let text = CollectionOverlay::Text {
            text: Some(" ".into()),
            font_size: Some(90),
            font_family: None,
            font_weight: None,
        };
        // No posters, no visible overlay: the canvas is the gradient plus the scrim only.
        let portrait = compose(
            CollectionArtShape::Portrait,
            CollectionPosterLayout::Row,
            0,
            &text,
        );
        let plain = gradient_background([120, 30, 60], CollectionArtShape::Portrait);
        // Untouched above the band, darker at the very bottom.
        assert_eq!(portrait.get_pixel(10, 10), plain.get_pixel(10, 10));
        assert!(portrait.get_pixel(10, 899)[0] < plain.get_pixel(10, 899)[0]);
        // Landscape never gets a scrim.
        let landscape = compose(
            CollectionArtShape::Landscape,
            CollectionPosterLayout::Row,
            0,
            &text,
        );
        assert_eq!(
            landscape,
            gradient_background([120, 30, 60], CollectionArtShape::Landscape)
        );
    }

    #[test]
    fn portrait_text_overlay_is_centred_in_the_lower_band() {
        let overlay = CollectionOverlay::Text {
            text: Some("Marvel".into()),
            font_size: Some(90),
            font_family: None,
            font_weight: None,
        };
        let canvas = compose(
            CollectionArtShape::Portrait,
            CollectionPosterLayout::Row,
            0,
            &overlay,
        );
        // White glyph pixels exist, all of them below the poster field and
        // horizontally centred around the middle.
        let white: Vec<(u32, u32)> = canvas
            .enumerate_pixels()
            .filter(|(_, _, p)| p[0] > 240 && p[1] > 240 && p[2] > 240)
            .map(|(x, y, _)| (x, y))
            .collect();
        assert!(!white.is_empty());
        let band_top = (900.0 * PORTRAIT_BAND_TOP) as u32;
        assert!(
            white
                .iter()
                .all(|(_, y)| *y >= band_top)
        );
        let min_x = white
            .iter()
            .map(|(x, _)| *x)
            .min()
            .unwrap() as i32;
        let max_x = white
            .iter()
            .map(|(x, _)| *x)
            .max()
            .unwrap() as i32;
        assert!((min_x + max_x - 600).abs() < 40, "{min_x}..{max_x}");
    }

    #[test]
    fn art_tags_select_the_shape_and_thumb_tag_tracks_the_primary_tag() {
        let tag = Uuid::new_v4().to_string();
        let portrait = portrait_art_tag(&tag);
        assert!(is_portrait_art_tag(&portrait));
        assert!(!is_portrait_art_tag(&tag));
        assert!(!is_portrait_art_tag(&format!(
            "{}-1700000000",
            Uuid::new_v4()
        )));

        let thumb = collection_thumb_tag(&tag);
        assert_ne!(thumb, tag);
        assert!(!is_portrait_art_tag(&thumb));
        assert_eq!(thumb, collection_thumb_tag(&tag), "stable");
        assert_ne!(
            thumb,
            collection_thumb_tag(&Uuid::new_v4().to_string()),
            "changes when the primary tag changes"
        );
    }

    #[test]
    fn grid_is_four_by_four_with_gutters() {
        assert_eq!(
            grid_dimensions(CollectionArtShape::Landscape),
            (
                3 * GRID_POSTER_W + 2 * GRID_GUTTER,
                3 * GRID_POSTER_H + 2 * GRID_GUTTER,
            )
        );
    }

    #[test]
    fn fill_dimensions_upscale_and_crop_to_the_requested_size() {
        let source = DynamicImage::new_rgba8(960, 540);
        let opts = ImageProcessOptions {
            fill_width: Some(1170),
            fill_height: Some(657),
            ..Default::default()
        };

        let result = apply_sizing(source, &opts);

        assert_eq!((result.width(), result.height()), (1170, 657));
    }

    #[test]
    fn missing_streaming_logo_does_not_render_placeholder_text() {
        let mut canvas = RgbaImage::from_pixel(OUT_W, OUT_H, Rgba([18, 18, 22, 255]));
        let expected = canvas.clone();

        apply_overlay_sync(
            &mut canvas,
            &CollectionOverlay::StreamingLogo {
                provider_id: 0,
                provider_name: None,
                logo_path: None,
            },
            "Collection",
            None,
            OverlayPlacement::LeftColumn,
        )
        .unwrap();

        assert_eq!(canvas, expected);
    }

    #[tokio::test]
    async fn synthetic_thumb_composites_backdrop_and_logo_at_thumb_size() {
        let dir = tempfile::tempdir().unwrap();
        let backdrop_path = dir
            .path()
            .join("backdrop.jpg");
        RgbImage::from_pixel(1280, 720, Rgb([40, 60, 80]))
            .save(&backdrop_path)
            .unwrap();
        let logo_path = dir
            .path()
            .join("logo.png");
        RgbaImage::from_pixel(500, 200, Rgba([255, 255, 255, 255]))
            .save(&logo_path)
            .unwrap();

        let bytes = ImageService::synthetic_thumb(
            backdrop_path
                .to_str()
                .unwrap(),
            Some(
                logo_path
                    .to_str()
                    .unwrap(),
            ),
            "Some Title",
        )
        .await
        .unwrap();

        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (OUT_W, OUT_H));
        assert_eq!(detect_content_type(&bytes), "image/jpeg");
    }

    #[tokio::test]
    async fn synthetic_thumb_falls_back_to_title_text_without_a_logo() {
        let dir = tempfile::tempdir().unwrap();
        let backdrop_path = dir
            .path()
            .join("backdrop.jpg");
        RgbImage::from_pixel(1280, 720, Rgb([40, 60, 80]))
            .save(&backdrop_path)
            .unwrap();

        let bytes = ImageService::synthetic_thumb(
            backdrop_path
                .to_str()
                .unwrap(),
            None,
            "Some Title",
        )
        .await
        .unwrap();

        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (OUT_W, OUT_H));
    }

    #[tokio::test]
    async fn cached_synthetic_thumb_persists_under_the_image_cache_dir() {
        let source_dir = tempfile::tempdir().unwrap();
        let backdrop_path = source_dir
            .path()
            .join("backdrop.jpg");
        RgbImage::from_pixel(1280, 720, Rgb([40, 60, 80]))
            .save(&backdrop_path)
            .unwrap();

        let data_dir = tempfile::tempdir().unwrap();
        let backdrop_id = Uuid::new_v4();

        let (first, cache_key) = ImageService::cached_synthetic_thumb(
            data_dir.path(),
            backdrop_id,
            None,
            backdrop_path
                .to_str()
                .unwrap(),
            None,
            "Some Title",
        )
        .await
        .unwrap();
        let cache_path =
            ImageService::cache_dir(data_dir.path()).join(format!("{cache_key}.jpg"));
        assert!(cache_path.exists(), "composite should be written to disk");

        // Deleting the source backdrop proves a second call for the same
        // backdrop/logo ids is served from the cache file, not regenerated.
        std::fs::remove_file(&backdrop_path).unwrap();
        let (second, _) = ImageService::cached_synthetic_thumb(
            data_dir.path(),
            backdrop_id,
            None,
            backdrop_path
                .to_str()
                .unwrap(),
            None,
            "Some Title",
        )
        .await
        .unwrap();
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn cached_synthetic_thumb_regenerates_when_the_backdrop_changes() {
        let source_dir = tempfile::tempdir().unwrap();
        let backdrop_path = source_dir
            .path()
            .join("backdrop.jpg");
        RgbImage::from_pixel(1280, 720, Rgb([40, 60, 80]))
            .save(&backdrop_path)
            .unwrap();

        let data_dir = tempfile::tempdir().unwrap();

        let (_, first_key) = ImageService::cached_synthetic_thumb(
            data_dir.path(),
            Uuid::new_v4(),
            None,
            backdrop_path
                .to_str()
                .unwrap(),
            None,
            "Some Title",
        )
        .await
        .unwrap();

        // A refresh that replaces the Backdrop gets a new `media_images` row
        // id (see `sync_from_media`) — the cache key must follow it so the
        // stale composite is never served for the new artwork.
        let (_, second_key) = ImageService::cached_synthetic_thumb(
            data_dir.path(),
            Uuid::new_v4(),
            None,
            backdrop_path
                .to_str()
                .unwrap(),
            None,
            "Some Title",
        )
        .await
        .unwrap();

        assert_ne!(first_key, second_key);
    }

    async fn test_db() -> sqlx::SqlitePool {
        let db = crate::db::connect("sqlite::memory:", 10_000)
            .await
            .unwrap();
        crate::db::migrate(&db)
            .await
            .unwrap();
        db
    }

    async fn insert_collection(db: &sqlx::SqlitePool) -> Uuid {
        let now = chrono::Utc::now().naive_utc();
        let mut collection = db::Media {
            title: "Marvel".to_string(),
            kind: db::MediaKind::Collection,
            created_at: now,
            updated_at: now,
            ..Default::default()
        };
        collection
            .save(db)
            .await
            .unwrap();
        collection.id
    }

    fn dims(bytes: &[u8]) -> (u32, u32) {
        let img = image::load_from_memory(bytes).unwrap();
        (img.width(), img.height())
    }

    async fn primary_row(db: &sqlx::SqlitePool, id: Uuid) -> Option<db::MediaImage> {
        db::MediaImage::get_for_media(db, &id)
            .await
            .unwrap()
            .primary
            .into_iter()
            .next()
    }

    #[tokio::test]
    async fn library_image_renders_both_shapes_and_registers_a_generated_landscape_row()
    {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db().await;
        let id = insert_collection(&db).await;

        // Asking for the portrait render generates (and stores) both.
        let portrait = ImageService::library_image(
            dir.path(),
            id,
            "Marvel",
            &db,
            CollectionArtShape::Portrait,
        )
        .await
        .unwrap();
        assert_eq!(dims(&portrait), (600, 900));
        let landscape = ImageService::library_image(
            dir.path(),
            id,
            "Marvel",
            &db,
            CollectionArtShape::Landscape,
        )
        .await
        .unwrap();
        assert_eq!(dims(&landscape), (960, 540));

        // The row is the landscape render (so legacy clients keep AR 16:9) and
        // is marked generated; its id is the tag, stable across both requests.
        let row = primary_row(&db, id)
            .await
            .unwrap();
        assert!(row.is_generated);
        assert_eq!((row.width, row.height), (Some(960), Some(540)));
        let again = ImageService::library_image(
            dir.path(),
            id,
            "Marvel",
            &db,
            CollectionArtShape::Portrait,
        )
        .await
        .unwrap();
        assert_eq!(again, portrait, "stored render is reused, not regenerated");
        assert_eq!(
            primary_row(&db, id)
                .await
                .unwrap()
                .id,
            row.id
        );
    }

    #[tokio::test]
    async fn library_image_adds_a_missing_portrait_without_touching_existing_landscape()
    {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db().await;
        let id = insert_collection(&db).await;
        // Pre-existing (legacy) landscape-only art, as produced before portrait existed.
        let legacy = ImageService::generate(
            id,
            "Marvel",
            &db,
            None,
            None,
            None,
            &[CollectionArtShape::Landscape],
        )
        .await
        .unwrap()
        .remove(0)
        .1;
        let landscape_path = ImageService::image_path(dir.path(), id, "primary", "jpg");
        ImageService::write_image_to_disk(&landscape_path, &legacy)
            .await
            .unwrap();

        let portrait = ImageService::library_image(
            dir.path(),
            id,
            "Marvel",
            &db,
            CollectionArtShape::Portrait,
        )
        .await
        .unwrap();
        assert_eq!(dims(&portrait), (600, 900));
        assert_eq!(
            tokio::fs::read(&landscape_path)
                .await
                .unwrap(),
            legacy
        );
    }

    #[tokio::test]
    async fn invalidate_removes_both_generated_renders_and_the_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db().await;
        let id = insert_collection(&db).await;
        ImageService::library_image(
            dir.path(),
            id,
            "Marvel",
            &db,
            CollectionArtShape::Portrait,
        )
        .await
        .unwrap();
        let old_tag = primary_row(&db, id)
            .await
            .unwrap()
            .id;

        assert!(
            ImageService::invalidate_collection_image(dir.path(), id, &db)
                .await
                .unwrap()
        );
        assert!(
            primary_row(&db, id)
                .await
                .is_none()
        );
        for shape in [CollectionArtShape::Landscape, CollectionArtShape::Portrait] {
            assert!(
                ImageService::read_existing_art(dir.path(), id, shape)
                    .await
                    .is_none()
            );
        }

        // Regeneration mints a new row id, i.e. a new image tag.
        ImageService::library_image(
            dir.path(),
            id,
            "Marvel",
            &db,
            CollectionArtShape::Landscape,
        )
        .await
        .unwrap();
        assert_ne!(
            primary_row(&db, id)
                .await
                .unwrap()
                .id,
            old_tag
        );
    }

    #[tokio::test]
    async fn invalidate_never_touches_an_uploaded_primary() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db().await;
        let id = insert_collection(&db).await;
        // A user upload goes through `save_image` (is_generated stays false).
        let upload =
            encode_jpeg(RgbImage::from_pixel(300, 450, Rgb([10, 200, 30]))).unwrap();
        ImageService::save_image(dir.path(), id, ImageKind::Primary, &upload, &db)
            .await
            .unwrap();
        let row = primary_row(&db, id)
            .await
            .unwrap();
        assert!(!row.is_generated);

        assert!(
            !ImageService::invalidate_collection_image(dir.path(), id, &db)
                .await
                .unwrap()
        );

        let after = primary_row(&db, id)
            .await
            .unwrap();
        assert_eq!(after.id, row.id, "tag unchanged");
        assert_eq!(
            tokio::fs::read(&after.path)
                .await
                .unwrap(),
            upload
        );
    }

    #[test]
    fn interleave_round_robins_across_sources_and_dedupes() {
        let s = |v: &[&str]| {
            v.iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
        };
        let merged = interleave_paths(
            vec![s(&["a1", "a2", "dup"]), s(&["b1", "dup", "b3"]), s(&["c1"])],
            5,
        );
        assert_eq!(merged, s(&["a1", "b1", "c1", "a2", "dup"]));
    }

    async fn save_movie_with_poster(db: &sqlx::SqlitePool, n: u32) -> Uuid {
        let mut m = db::Media {
            title: format!("Movie {n}"),
            kind: db::MediaKind::Movie,
            external_ids: db::ExternalIds {
                imdb: db::NonEmptyString::try_new(format!("tt_art_{n}")).ok(),
                ..Default::default()
            },
            ..Default::default()
        };
        m.set_image(ImageKind::Primary, format!("/posters/{n}.jpg"));
        m.save(db)
            .await
            .unwrap();
        m.id
    }

    async fn save_collection(
        db: &sqlx::SqlitePool,
        title: &str,
        kind: db::CollectionKind,
        media_kind: db::CollectionMediaKind,
        filter: Option<CollectionFilter>,
    ) -> db::Media {
        let mut c = db::Media {
            title: title.into(),
            kind: db::MediaKind::Collection,
            collection_kind: Some(kind),
            collection_media_kind: Some(media_kind),
            collection_smart_filter: filter,
            ..Default::default()
        };
        c.save(db)
            .await
            .unwrap();
        c
    }

    fn collection_id_filter(ids: Vec<Uuid>) -> CollectionFilter {
        CollectionFilter {
            groups: vec![remux_sdks::remux::FilterGroup {
                rules: vec![remux_sdks::remux::FilterRule::CollectionId {
                    op: remux_sdks::remux::SetOp::In,
                    ids,
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// A smart parent whose filter is `collection_id in [smart children]` has
    /// no stored members; it must borrow posters from its children.
    #[tokio::test]
    async fn smart_parent_borrows_posters_from_smart_children() {
        let (_server, guard) = crate::integration_test::new_test_server()
            .await
            .unwrap();
        let db = &guard
            .0
            .db;
        for n in 0..4 {
            save_movie_with_poster(db, n).await;
        }
        let child_a = save_collection(
            db,
            "Child A",
            db::CollectionKind::Smart,
            db::CollectionMediaKind::Movie,
            Some(CollectionFilter::default()),
        )
        .await;
        let child_b = save_collection(
            db,
            "Child B",
            db::CollectionKind::Smart,
            db::CollectionMediaKind::Movie,
            Some(CollectionFilter::default()),
        )
        .await;
        let parent = save_collection(
            db,
            "Parent",
            db::CollectionKind::Smart,
            db::CollectionMediaKind::Collection,
            Some(collection_id_filter(vec![child_a.id, child_b.id])),
        )
        .await;

        let urls = collect_poster_urls(parent.id, &parent, db, 9, None).await;
        assert_eq!(urls.len(), 4, "{urls:?}");
        let unique: HashSet<_> = urls
            .iter()
            .collect();
        assert_eq!(unique.len(), urls.len());
    }

    /// A collection that already finds its own posters is left alone.
    #[tokio::test]
    async fn collection_with_own_posters_is_unchanged() {
        let (_server, guard) = crate::integration_test::new_test_server()
            .await
            .unwrap();
        let db = &guard
            .0
            .db;
        for n in 0..3 {
            save_movie_with_poster(db, n).await;
        }
        let smart = save_collection(
            db,
            "Plain Smart",
            db::CollectionKind::Smart,
            db::CollectionMediaKind::Movie,
            Some(CollectionFilter::default()),
        )
        .await;
        let own = collect_poster_urls(smart.id, &smart, db, 3, None).await;
        assert_eq!(own.len(), 3, "{own:?}");
        assert!(child_collections_filter(&smart, None).is_none());
    }
}
