use axum::{
    Json,
    extract::{Path, State},
    http::{StatusCode, header},
    response::IntoResponse,
};
use axum_extra::extract::Query;
use remux_macros::{delete, get, post};
use serde::Deserialize;
use tracing::warn;
use uuid::Uuid;

use crate::{
    AppState, OptionExt, ResultExt, api, db,
    db::{ImageKind, auth},
    sdks,
    services::image::{
        CollectionArtShape, ImageProcessOptions, ImageService, is_portrait_art_tag,
    },
};
use axum_anyhow::ApiResult as Result;

static IMAGE_CLIENT: std::sync::LazyLock<reqwest::Client> =
    std::sync::LazyLock::new(|| {
        reqwest::Client::builder()
            .user_agent("remux-server/1.0")
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("failed to build image proxy client")
    });

/// Fetch an upstream URL, returning (bytes, content_type).
async fn fetch_upstream(url: &str) -> anyhow::Result<(Vec<u8>, String)> {
    let resp = remux_utils::retry! {
        attempts: 3,
        delay: 500,
        { IMAGE_CLIENT.get(url).send().await }
    }
    .map_err(|e| anyhow::anyhow!("image fetch failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("upstream image returned {status}");
    }
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| {
            v.to_str()
                .ok()
        })
        .unwrap_or("image/jpeg")
        .to_string();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| anyhow::anyhow!("image read failed: {e}"))?
        .to_vec();
    Ok((bytes, ct))
}

async fn items_images_inner(
    state: AppState,
    id: Uuid,
    image_type: api::ImageType,
    q: api::ImageQuery,
) -> Result<impl IntoResponse> {
    let opts = ImageProcessOptions {
        fill_width: q.fill_width,
        fill_height: q.fill_height,
        width: q.width,
        height: q.height,
        max_width: q.max_width,
        max_height: q.max_height,
        quality: q.quality,
        blur: q.blur,
        background_color: q
            .background_color
            .clone(),
        format: q
            .format
            .clone(),
    };

    // Resolve to (raw_bytes, content_type, source_key_for_cache, is_remote).
    // is_remote=true means the bytes came from an external URL and must not be
    // re-encoded — proxy them as-is.
    let (bytes, raw_ct, source_key, is_remote): (Vec<u8>, String, String, bool) =
        if let Some(url) = q
            .tag
            .as_ref()
            .filter(|t| t.contains("://"))
        {
            let (b, ct) = fetch_upstream(url)
                .await
                .context_not_found("image fetch failed")?;
            (b, ct, url.clone(), true)
        } else {
            let key = id.to_string();
            if let Some(media) = db::Media::get_by_id(
                &state
                    .ctx
                    .db,
                &id,
            )
            .await?
            {
                let kind: ImageKind = image_type
                    .to_string()
                    .parse()
                    .unwrap_or(ImageKind::Primary);
                let is_collection = matches!(media.kind, db::MediaKind::Collection);
                // A collection's Primary is server-generated unless an uploaded
                // or provider image occupies the slot. Only generated art has
                // a portrait variant; uploaded art is served to everyone
                // exactly as stored.
                let primary_generated = media
                    .images
                    .get(ImageKind::Primary)
                    .is_none_or(|row| row.is_generated);
                // Image requests often carry no auth headers, so the shape is
                // selected by the tag that the listing advertised: only
                // listings built for portrait-preferring clients carry it.
                let wants_portrait = is_collection
                    && kind == ImageKind::Primary
                    && primary_generated
                    && q.tag
                        .as_deref()
                        .is_some_and(is_portrait_art_tag);
                // Thumb falls back to a synthesized Backdrop+Logo composite
                // (below) when a Backdrop exists; only falls back to Primary
                // outright when there's no Backdrop to synthesize from either.
                // Collections have no dedicated Thumb of their own — Thumb is
                // just the generated Primary artwork (same image, same tag;
                // see `ImageTags` in api/models.rs). Backdrop, unlike Thumb, is
                // NOT derived from Primary: a collection's backdrop must be a
                // real, separately-uploaded image (typically textless) or none
                // at all — it must never silently alias the text-overlaid
                // Primary artwork.
                let img_row = media
                    .images
                    .get(kind)
                    .or_else(|| {
                        if kind == ImageKind::Thumb
                            && !is_collection
                            && media
                                .images
                                .get(ImageKind::Backdrop)
                                .is_none()
                        {
                            media
                                .images
                                .get(ImageKind::Primary)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        if kind == ImageKind::Thumb && is_collection {
                            media
                                .images
                                .get(ImageKind::Primary)
                        } else {
                            None
                        }
                    });

                // A collection's Primary/Thumb can always be regenerated on
                // demand (see the `library_image` branch below) — so for a
                // collection, a DB row whose file has since gone missing (or
                // whose remote URL no longer resolves) should fall through to
                // regenerating it instead of 404ing outright, same as having no
                // row at all. Backdrop is deliberately excluded: it must be a
                // real, separately-uploaded image or nothing — never silently
                // substituted with the (text-overlaid) generated artwork.
                let can_regenerate = is_collection
                    && matches!(
                        image_type,
                        api::ImageType::Primary | api::ImageType::Thumb
                    );
                let portrait_render = if wants_portrait {
                    let bytes = ImageService::library_image(
                        &state
                            .ctx
                            .config
                            .data_dir,
                        id,
                        &media.title,
                        &state
                            .ctx
                            .db,
                        CollectionArtShape::Portrait,
                    )
                    .await
                    .context_not_found("no backdrop available for library")?;
                    let row_id = db::MediaImage::get_for_media(
                        &state
                            .ctx
                            .db,
                        &id,
                    )
                    .await
                    .unwrap_or_default()
                    .primary
                    .into_iter()
                    .find(|i| i.image_index == 0)
                    .map(|i| {
                        i.id.to_string()
                    })
                    .unwrap_or_else(|| format!("placeholder:{id}"));
                    Some((
                        bytes,
                        "image/jpeg".to_string(),
                        format!("{row_id}:portrait"),
                        false,
                    ))
                } else {
                    None
                };
                let served_from_row = if portrait_render.is_some() {
                    portrait_render
                } else {
                    match img_row {
                        Some(img) => {
                            let source_key = img
                                .id
                                .to_string();
                            if img
                                .path
                                .starts_with('/')
                            {
                                let path = std::path::PathBuf::from(&img.path);
                                match ImageService::serve_local(&path).await {
                                    Ok((b, ct)) => {
                                        Some((b, ct.to_string(), source_key, false))
                                    }
                                    Err(e) if can_regenerate => {
                                        warn!(id = %id, error = %e, "stored collection image file missing, regenerating");
                                        None
                                    }
                                    Err(e) => {
                                        return Err(e)
                                            .context_not_found("image file not found");
                                    }
                                }
                            } else {
                                // Always proxy external URLs rather than redirecting — some clients
                                // (e.g. Infuse) do not follow redirects for image requests.
                                match fetch_upstream(&img.path).await {
                                    Ok((b, ct)) => Some((b, ct, source_key, true)),
                                    Err(e) if can_regenerate => {
                                        warn!(id = %id, error = %e, "stored collection image fetch failed, regenerating");
                                        None
                                    }
                                    Err(e) => {
                                        return Err(e)
                                            .context_not_found("image fetch failed");
                                    }
                                }
                            }
                        }
                        None => None,
                    }
                };

                if let Some(t) = served_from_row {
                    t
                } else if kind == ImageKind::Thumb
                    && !is_collection
                    && let Some(backdrop) = media
                        .images
                        .get(ImageKind::Backdrop)
                {
                    let logo = media
                        .images
                        .get(ImageKind::Logo);
                    let (bytes, cache_key) = ImageService::cached_synthetic_thumb(
                        &state
                            .ctx
                            .config
                            .data_dir,
                        backdrop.id,
                        logo.map(|l| l.id),
                        &backdrop.path,
                        logo.map(|l| {
                            l.path
                                .as_str()
                        }),
                        &media.title,
                    )
                    .await
                    .context_internal("thumb generation failed")?;
                    (bytes, "image/jpeg".to_string(), cache_key, false)
                } else if can_regenerate {
                    let b = ImageService::library_image(
                        &state
                            .ctx
                            .config
                            .data_dir,
                        id,
                        &media.title,
                        &state
                            .ctx
                            .db,
                        CollectionArtShape::Landscape,
                    )
                    .await
                    .context_not_found("no backdrop available for library")?;
                    // Reload the newly-inserted image row to get its stable UUID
                    let img_row = db::MediaImage::get_for_media(
                        &state
                            .ctx
                            .db,
                        &id,
                    )
                    .await
                    .unwrap_or_default()
                    .primary
                    .into_iter()
                    .find(|i| i.image_index == 0);
                    let source_key = img_row
                        .map(|i| {
                            i.id.to_string()
                        })
                        .unwrap_or_else(|| format!("placeholder:{id}"));
                    (b, "image/jpeg".to_string(), source_key, false)
                } else {
                    return Err(anyhow::anyhow!("image not found"))
                        .context_not_found("image not found");
                }
            } else {
                // Not in DB — cached search result.
                let url = state
                    .ctx
                    .store
                    .get::<db::Media>(key.clone())
                    .and_then(|m| match image_type {
                        api::ImageType::Primary | api::ImageType::Thumb => m
                            .get_image(ImageKind::Primary)
                            .map(str::to_owned),
                        api::ImageType::Backdrop => m
                            .get_image(ImageKind::Backdrop)
                            .map(str::to_owned),
                        api::ImageType::Logo | api::ImageType::LogoImageAspectRatio => {
                            m.get_image(ImageKind::Logo)
                                .map(str::to_owned)
                        }
                    });
                let url = url.context_not_found("media image not found")?;
                let (b, ct) = fetch_upstream(&url)
                    .await
                    .context_not_found("image fetch failed")?;
                (b, ct, url, true)
            }
        };

    // Apply resize/quality/blur/format transforms (cached) for local images only.
    // Remote images are proxied as-is — re-encoding adds latency with no benefit.
    let (final_bytes, content_type): (Vec<u8>, String) = if is_remote {
        (bytes, raw_ct)
    } else {
        let (b, ct) = ImageService::process_image(
            &state
                .ctx
                .config
                .data_dir,
            bytes,
            &opts,
            &source_key,
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context_internal("image processing failed")?;
        (b, ct.to_string())
    };

    Ok((
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (header::CACHE_CONTROL, "max-age=86400".to_string()),
        ],
        final_bytes,
    )
        .into_response())
}

// --- GET ---

#[get("/items/{id}/images")]
pub async fn get_item_image_infos(
    State(state): State<AppState>,
    _session: auth::AuthSession,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse> {
    #[derive(serde::Serialize)]
    #[serde(rename_all = "PascalCase")]
    struct ImageInfo {
        image_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        image_index: Option<i64>,
        image_tag: String,
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        width: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        height: Option<i64>,
        size: u64,
    }

    let images = db::MediaImage::get_for_media(
        &state
            .ctx
            .db,
        &id,
    )
    .await?;

    let infos: Vec<ImageInfo> = images
        .into_iter()
        .map(|img| {
            let size = if img
                .path
                .starts_with('/')
            {
                std::fs::metadata(&img.path)
                    .map(|m| m.len())
                    .unwrap_or(0)
            } else {
                0
            };
            let image_type = match img
                .image_type
                .as_str()
            {
                "primary" => "Primary",
                "backdrop" => "Backdrop",
                "logo" => "Logo",
                "thumb" => "Thumb",
                other => other,
            }
            .to_owned();
            ImageInfo {
                image_type,
                image_index: if img.image_index == 0 {
                    None
                } else {
                    Some(img.image_index)
                },
                image_tag: img
                    .id
                    .simple()
                    .to_string(),
                path: img.path,
                width: img.width,
                height: img.height,
                size,
            }
        })
        .collect();

    Ok(axum::Json(infos))
}

#[get("/items/{id}/images/{image_type}")]
pub async fn items_images(
    State(state): State<AppState>,
    Path((id, image_type)): Path<(Uuid, api::ImageType)>,
    Query(q): Query<api::ImageQuery>,
) -> Result<impl IntoResponse> {
    items_images_inner(state, id, image_type, q).await
}

#[get("/items/{id}/images/{image_type}/{index}")]
pub async fn items_images_indexed(
    State(state): State<AppState>,
    Path((id, image_type, _index)): Path<(Uuid, api::ImageType, usize)>,
    Query(q): Query<api::ImageQuery>,
) -> Result<impl IntoResponse> {
    items_images_inner(state, id, image_type, q).await
}

/// Render a collection's generated image from draft, unsaved config — nothing
/// is written to disk or the DB. Lets the dashboard's "Update preview" show
/// what a config/filter change would produce without persisting it; only the
/// real Save action (`PATCH /items/{id}`) commits anything.
#[post("/items/{id}/imagepreview")]
pub async fn preview_collection_image(
    State(state): State<AppState>,
    _session: auth::AdminSession,
    Path(id): Path<Uuid>,
    Json(payload): Json<sdks::remux::PreviewCollectionImagePayload>,
) -> Result<impl IntoResponse> {
    let media = db::Media::get_by_id(
        &state
            .ctx
            .db,
        &id,
    )
    .await?
    .context_not_found("item not found")?;

    let (bytes, content_type) = ImageService::generate_preview(
        id,
        &media.title,
        &state
            .ctx
            .db,
        &payload.image_config,
        payload
            .smart_filter
            .as_ref(),
    )
    .await
    .context_internal("failed to generate preview")?;

    // The dashboard SDK client always reads responses as JSON text (see
    // Client::execute_arc), so raw image bytes can't go back as the body —
    // base64-envelope them instead. The dashboard turns this straight back
    // into a `data:` URL, matching the format its local-file picker already
    // produces client-side.
    use base64::Engine;
    Ok(axum::Json(api::CollectionImagePreviewResponse {
        content_type: content_type.to_string(),
        data: base64::engine::general_purpose::STANDARD.encode(&bytes),
    }))
}

// --- POST (upload) ---

async fn upload_item_image_inner(
    state: AppState,
    id: Uuid,
    kind: ImageKind,
    image: api::image::JellyfinImage,
) -> Result<impl IntoResponse> {
    // A GIF poster is always served as-is to preserve animation, so it can
    // never be composite source material for the collection image
    // configurator (which flattens everything to a static JPEG). Gate on the
    // upload itself rather than a mode/setting so this can't be misconfigured.
    let is_gif = crate::api::image::detect_content_type(&image.bytes) == "image/gif";
    let (is_collection_kind, is_collection_source, title) = {
        let media = db::Media::get_by_id(
            &state
                .ctx
                .db,
            &id,
        )
        .await?
        .context_not_found("item not found")?;
        let is_collection_kind = kind == ImageKind::Primary
            && matches!(
                media.kind,
                db::MediaKind::Collection | db::MediaKind::Folder
            );
        (
            is_collection_kind,
            is_collection_kind
                && !is_gif
                && media
                    .collection_image_config
                    .is_some(),
            media
                .title
                .clone(),
        )
    };
    let storage_kind = if is_collection_source {
        ImageKind::Backdrop
    } else {
        kind
    };

    ImageService::save_image(
        &state
            .ctx
            .config
            .data_dir,
        id,
        storage_kind,
        &image.bytes,
        &state
            .ctx
            .db,
    )
    .await
    .context_internal("failed to save image")?;

    if is_collection_source {
        // Keep the uploaded file as the source image and regenerate Primary
        // from it whenever the collection overlay changes.
        ImageService::delete_image(
            &state
                .ctx
                .config
                .data_dir,
            id,
            ImageKind::Primary,
            &state
                .ctx
                .db,
        )
        .await
        .context_internal("failed to clear generated image")?;
        ImageService::library_image(
            &state
                .ctx
                .config
                .data_dir,
            id,
            &title,
            &state
                .ctx
                .db,
            CollectionArtShape::Landscape,
        )
        .await
        .context_internal("failed to generate collection image")?;
    } else if is_collection_kind && is_gif {
        // Drop any existing image config: it no longer applies once the
        // collection has a raw GIF poster, and this also hides the
        // layout/overlay configurator in the dashboard for it.
        sqlx::query("UPDATE media SET collection_image_config = NULL WHERE id = ?")
            .bind(id)
            .execute(
                &state
                    .ctx
                    .db,
            )
            .await
            .context_internal("failed to clear collection image config")?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[post("/items/{id}/images/{image_type}")]
pub async fn upload_item_image(
    State(state): State<AppState>,
    _session: auth::AuthSession,
    Path((id, image_type)): Path<(Uuid, String)>,
    image: api::image::JellyfinImage,
) -> Result<impl IntoResponse> {
    upload_item_image_inner(state, id, parse_image_kind(&image_type), image).await
}

#[post("/items/{id}/images/{image_type}/{index}")]
pub async fn upload_item_image_indexed(
    State(state): State<AppState>,
    _session: auth::AuthSession,
    Path((id, image_type, _index)): Path<(Uuid, String, usize)>,
    image: api::image::JellyfinImage,
) -> Result<impl IntoResponse> {
    upload_item_image_inner(state, id, parse_image_kind(&image_type), image).await
}

// --- DELETE ---

async fn delete_item_image_inner(
    state: AppState,
    id: Uuid,
    kind: ImageKind,
) -> Result<impl IntoResponse> {
    let has_collection_source = {
        let media = db::Media::get_by_id(
            &state
                .ctx
                .db,
            &id,
        )
        .await?;
        kind == ImageKind::Primary
            && media
                .as_ref()
                .is_some_and(|item| {
                    matches!(
                        item.kind,
                        db::MediaKind::Collection | db::MediaKind::Folder
                    ) && item
                        .collection_image_config
                        .is_some()
                })
    };
    ImageService::delete_image(
        &state
            .ctx
            .config
            .data_dir,
        id,
        kind,
        &state
            .ctx
            .db,
    )
    .await
    .context_internal("failed to delete image")?;
    if has_collection_source {
        ImageService::delete_image(
            &state
                .ctx
                .config
                .data_dir,
            id,
            ImageKind::Backdrop,
            &state
                .ctx
                .db,
        )
        .await
        .context_internal("failed to delete custom collection source")?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[delete("/items/{id}/images/{image_type}")]
pub async fn delete_item_image(
    State(state): State<AppState>,
    _session: auth::AuthSession,
    Path((id, image_type)): Path<(Uuid, String)>,
) -> Result<impl IntoResponse> {
    delete_item_image_inner(state, id, parse_image_kind(&image_type)).await
}

#[delete("/items/{id}/images/{image_type}/{index}")]
pub async fn delete_item_image_indexed(
    State(state): State<AppState>,
    _session: auth::AuthSession,
    Path((id, image_type, _index)): Path<(Uuid, String, usize)>,
) -> Result<impl IntoResponse> {
    delete_item_image_inner(state, id, parse_image_kind(&image_type)).await
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RemoteImageDownloadQuery {
    #[serde(rename = "Type")]
    pub image_type: String,
    #[serde(rename = "ImageUrl")]
    pub image_url: String,
}

#[post("/items/{id}/remoteimages/download")]
pub async fn download_remote_image(
    State(state): State<AppState>,
    _session: auth::AdminSession,
    Path(id): Path<Uuid>,
    Query(q): Query<RemoteImageDownloadQuery>,
) -> Result<impl IntoResponse> {
    let (bytes, _ct) = fetch_upstream(&q.image_url)
        .await
        .context_bad_request("failed to fetch remote image")?;
    let kind = parse_image_kind(&q.image_type);
    ImageService::save_image(
        &state
            .ctx
            .config
            .data_dir,
        id,
        kind,
        &bytes,
        &state
            .ctx
            .db,
    )
    .await
    .context_internal("failed to save image")?;
    Ok(StatusCode::NO_CONTENT)
}

/// Convert a Jellyfin URL path segment (e.g. "Thumb", "Primary") to `ImageKind`.
/// Routes through `api::ImageType` so Thumb→Primary and LogoImageAspectRatio→Logo
/// semantics are preserved.
fn parse_image_kind(s: &str) -> ImageKind {
    s.parse::<api::ImageType>()
        .map(|t| {
            t.to_string()
                .parse()
                .unwrap_or(ImageKind::Primary)
        })
        .unwrap_or(ImageKind::Primary)
}

#[cfg(test)]
mod tests {
    use axum_test::TestServer;
    use http::header::{AUTHORIZATION, HeaderValue};
    use serde_json::{Value, json};
    use uuid::Uuid;

    use crate::{
        db,
        db::ImageKind,
        integration_test::{TestGuard, new_test_server},
        services::image::ImageService,
    };

    /// Log in as the seeded user from a client reporting `client`, returning
    /// `(user_id, Authorization header value)`.
    async fn login_as(server: &TestServer, client: &str) -> (String, String) {
        let device_id = format!("dev-{}", client.replace(' ', "-"));
        let base = format!(
            "MediaBrowser Client=\"{client}\", Device=\"Test\", DeviceId=\"{device_id}\", Version=\"1.0.0\""
        );
        let body: Value = server
            .post("/users/authenticatebyname")
            .add_header(AUTHORIZATION, HeaderValue::from_str(&base).unwrap())
            .json(&json!({ "Username": "test", "Pw": "test" }))
            .await
            .json();
        let token = body["AccessToken"]
            .as_str()
            .unwrap();
        let user_id = body["User"]["Id"]
            .as_str()
            .unwrap()
            .to_string();
        (user_id, format!("{base}, Token=\"{token}\""))
    }

    async fn insert_collection(
        db: &sqlx::SqlitePool,
        title: &str,
        group: bool,
    ) -> db::Media {
        let now = chrono::Utc::now().naive_utc();
        let mut c = db::Media {
            title: title.to_string(),
            kind: db::MediaKind::Collection,
            collection_kind: Some(if group {
                db::CollectionKind::Manual
            } else {
                db::CollectionKind::Smart
            }),
            collection_media_kind: Some(if group {
                db::CollectionMediaKind::Collection
            } else {
                db::CollectionMediaKind::Series
            }),
            created_at: now,
            updated_at: now,
            ..Default::default()
        };
        c.save(db)
            .await
            .unwrap();
        c
    }

    /// A group container with one child collection (the child is what
    /// `/items?parentId=` lists, like a BoxSet inside a library).
    async fn group_with_child(db: &sqlx::SqlitePool) -> (db::Media, db::Media) {
        let group = insert_collection(db, "Streaming", true).await;
        let child = insert_collection(db, "Netflix", false).await;
        db::Media::set_parent_id(db, &[child.id], Some(group.id))
            .await
            .unwrap();
        (group, child)
    }

    /// The listed child's `(Primary tag, Thumb tag, PrimaryImageAspectRatio)`.
    async fn list_child(
        server: &TestServer,
        user_id: &str,
        auth: &str,
        group: Uuid,
    ) -> (String, String, f64) {
        let body: Value = server
            .get(&format!("/users/{user_id}/items"))
            .add_header(AUTHORIZATION, HeaderValue::from_str(auth).unwrap())
            .add_query_param("parentId", group.to_string())
            .await
            .json();
        let item = &body["Items"][0];
        (
            item["ImageTags"]["Primary"]
                .as_str()
                .unwrap()
                .to_string(),
            item["ImageTags"]["Thumb"]
                .as_str()
                .unwrap()
                .to_string(),
            item["PrimaryImageAspectRatio"]
                .as_f64()
                .unwrap(),
        )
    }

    /// `GET /items/{id}/images/{kind}[?tag=]` -> `(content type, decoded size)`.
    async fn get_image(
        server: &TestServer,
        id: Uuid,
        kind: &str,
        tag: Option<&str>,
    ) -> (String, (u32, u32)) {
        let mut req = server.get(&format!("/items/{id}/images/{kind}"));
        if let Some(tag) = tag {
            req = req.add_query_param("tag", tag);
        }
        let resp = req.await;
        let ct = resp
            .header("content-type")
            .to_str()
            .unwrap()
            .to_string();
        let img = image::load_from_memory(&resp.as_bytes()).unwrap();
        (ct, (img.width(), img.height()))
    }

    const LANDSCAPE: (u32, u32) = (960, 540);
    const PORTRAIT: (u32, u32) = (600, 900);

    async fn server_with_clients() -> (TestServer, TestGuard) {
        new_test_server()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn generated_collection_art_is_portrait_only_for_moonfin() {
        let (server, guard) = server_with_clients().await;
        let db = &guard
            .0
            .db;
        let (group, child) = group_with_child(db).await;
        let (moonfin_user, moonfin) = login_as(&server, "Moonfin for Android").await;
        let (web_user, web) = login_as(&server, "Jellyfin Web").await;
        let (swift_user, swift) = login_as(&server, "Swiftfin iOS").await;
        let (other_user, other) = login_as(&server, "Some Unknown Client").await;

        // --- Before anything has been generated (no media_images row yet) ---
        let (m_primary, m_thumb, m_ar) =
            list_child(&server, &moonfin_user, &moonfin, group.id).await;
        assert!(m_primary.starts_with("portrait-"), "{m_primary}");
        assert!((m_ar - 2.0 / 3.0).abs() < 0.001, "{m_ar}");
        assert!(!m_thumb.starts_with("portrait-"));
        assert_ne!(m_thumb, m_primary);

        let mut legacy_tags = Vec::new();
        for (user, auth) in [
            (&web_user, &web),
            (&swift_user, &swift),
            (&other_user, &other),
        ] {
            let (primary, thumb, ar) = list_child(&server, user, auth, group.id).await;
            // Exactly today's behaviour: synthetic `<id>-<ts>` tag, 16:9.
            assert!(
                primary.starts_with(
                    &child
                        .id
                        .to_string()
                ),
                "{primary}"
            );
            assert!(!primary.starts_with("portrait-"));
            assert!((ar - 16.0 / 9.0).abs() < 0.001, "{ar}");
            assert_ne!(thumb, primary, "Thumb is distinct and available to all");
            legacy_tags.push((primary, thumb));
        }
        assert!(
            legacy_tags
                .windows(2)
                .all(|w| w[0] == w[1]),
            "every non-Moonfin client sees identical tags"
        );
        let (legacy_primary, legacy_thumb) = legacy_tags[0].clone();
        assert_eq!(m_primary, format!("portrait-{legacy_primary}"));
        assert_eq!(m_thumb, legacy_thumb);

        // --- Image requests (unauthenticated, shape chosen by tag) ---
        let (ct, size) =
            get_image(&server, child.id, "primary", Some(&m_primary)).await;
        assert_eq!((ct.as_str(), size), ("image/jpeg", PORTRAIT));
        for tag in [Some(legacy_primary.as_str()), None, Some("whatever")] {
            let (ct, size) = get_image(&server, child.id, "primary", tag).await;
            assert_eq!((ct.as_str(), size), ("image/jpeg", LANDSCAPE), "{tag:?}");
        }
        for tag in [Some(m_thumb.as_str()), None] {
            let (_, size) = get_image(&server, child.id, "thumb", tag).await;
            assert_eq!(size, LANDSCAPE, "Thumb is always wide: {tag:?}");
        }

        // --- After generation: row exists, landscape AR for stock clients ---
        let (m_primary2, m_thumb2, m_ar2) =
            list_child(&server, &moonfin_user, &moonfin, group.id).await;
        assert!(m_primary2.starts_with("portrait-"));
        assert!((m_ar2 - 2.0 / 3.0).abs() < 0.001);
        let (w_primary2, w_thumb2, w_ar2) =
            list_child(&server, &web_user, &web, group.id).await;
        assert!(!w_primary2.starts_with("portrait-"));
        assert!((w_ar2 - 16.0 / 9.0).abs() < 0.001, "{w_ar2}");
        assert_eq!(m_primary2, format!("portrait-{w_primary2}"));
        assert_eq!(m_thumb2, w_thumb2);
        assert_ne!(w_thumb2, w_primary2);
        for (kind, tag, expected) in [
            ("primary", m_primary2.as_str(), PORTRAIT),
            ("primary", w_primary2.as_str(), LANDSCAPE),
            ("thumb", w_thumb2.as_str(), LANDSCAPE),
        ] {
            let (_, size) = get_image(&server, child.id, kind, Some(tag)).await;
            assert_eq!(size, expected, "{kind} {tag}");
        }

        // --- Regeneration changes every tag, in both shapes ---
        assert!(
            ImageService::invalidate_collection_image(
                &guard
                    .0
                    .config
                    .data_dir,
                child.id,
                db,
            )
            .await
            .unwrap()
        );
        // Force the new row to exist before listing again.
        get_image(&server, child.id, "primary", None).await;
        let (m_primary3, m_thumb3, _) =
            list_child(&server, &moonfin_user, &moonfin, group.id).await;
        let (w_primary3, w_thumb3, _) =
            list_child(&server, &web_user, &web, group.id).await;
        assert_ne!(m_primary3, m_primary2);
        assert_ne!(w_primary3, w_primary2);
        assert_ne!(w_thumb3, w_thumb2, "Thumb tag busts on regeneration too");
        assert_eq!(m_thumb3, w_thumb3);
    }

    #[tokio::test]
    async fn legacy_generated_art_gets_a_portrait_variant_without_a_new_landscape() {
        let (server, guard) = server_with_clients().await;
        let db = &guard
            .0
            .db;
        let data_dir = &guard
            .0
            .config
            .data_dir;
        let (group, child) = group_with_child(db).await;
        let (moonfin_user, moonfin) = login_as(&server, "Moonfin for iOS").await;

        // Pre-portrait state: only the landscape render + its (backfilled-style) row.
        get_image(&server, child.id, "primary", None).await;
        let landscape = ImageService::image_path(data_dir, child.id, "primary", "jpg");
        let portrait =
            ImageService::image_path(data_dir, child.id, "primary-portrait", "jpg");
        let before = tokio::fs::read(&landscape)
            .await
            .unwrap();
        let _ = tokio::fs::remove_file(&portrait).await;
        let row = db::MediaImage::get_for_media(db, &child.id)
            .await
            .unwrap()
            .primary
            .remove(0);

        let (tag, _, _) = list_child(&server, &moonfin_user, &moonfin, group.id).await;
        assert_eq!(tag, format!("portrait-{}", row.id));
        let (_, size) = get_image(&server, child.id, "primary", Some(&tag)).await;
        assert_eq!(size, PORTRAIT);
        assert_eq!(
            tokio::fs::read(&landscape)
                .await
                .unwrap(),
            before
        );
        assert!(portrait.exists());
    }

    #[tokio::test]
    async fn uploaded_collection_art_is_served_unchanged_to_every_client() {
        let (server, guard) = server_with_clients().await;
        let db = &guard
            .0
            .db;
        let data_dir = &guard
            .0
            .config
            .data_dir;
        let (group, child) = group_with_child(db).await;
        let (moonfin_user, moonfin) = login_as(&server, "Moonfin for Android TV").await;
        let (web_user, web) = login_as(&server, "Jellyfin Web").await;

        // The user's own 300x450 poster.
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            300,
            450,
            image::Rgb([10, 200, 30]),
        ))
        .write_to(
            &mut std::io::Cursor::new(&mut buf),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
        ImageService::save_image(data_dir, child.id, ImageKind::Primary, &buf, db)
            .await
            .unwrap();
        let row = db::MediaImage::get_for_media(db, &child.id)
            .await
            .unwrap()
            .primary
            .remove(0);
        assert!(!row.is_generated);

        let (m_primary, m_thumb, m_ar) =
            list_child(&server, &moonfin_user, &moonfin, group.id).await;
        let (w_primary, w_thumb, w_ar) =
            list_child(&server, &web_user, &web, group.id).await;
        assert_eq!(
            m_primary,
            row.id
                .to_string()
        );
        assert_eq!(m_primary, w_primary, "same tag for every client");
        assert_eq!(m_thumb, w_thumb);
        assert_eq!(m_thumb, m_primary, "Thumb serves the same uploaded image");
        assert!(
            (m_ar - 300.0 / 450.0).abs() < 0.001,
            "AR from the real image"
        );
        assert_eq!(m_ar, w_ar);

        // Even a portrait-selecting tag cannot reshape or regenerate it.
        for (kind, tag) in [
            ("primary", Some(m_primary.as_str())),
            ("primary", Some("portrait-anything")),
            ("primary", None),
            ("thumb", Some(m_thumb.as_str())),
        ] {
            let (_, size) = get_image(&server, child.id, kind, tag).await;
            assert_eq!(size, (300, 450), "{kind} {tag:?}");
        }
        assert!(
            !ImageService::image_path(data_dir, child.id, "primary-portrait", "jpg")
                .exists()
        );

        // Invalidation (config change / refresh / admin regenerate) skips it.
        assert!(
            !ImageService::invalidate_collection_image(data_dir, child.id, db)
                .await
                .unwrap()
        );
        let after = db::MediaImage::get_for_media(db, &child.id)
            .await
            .unwrap()
            .primary
            .remove(0);
        assert_eq!(after.id, row.id);
        assert_eq!(
            tokio::fs::read(&after.path)
                .await
                .unwrap(),
            buf
        );
    }
}
