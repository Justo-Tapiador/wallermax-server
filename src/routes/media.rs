//! The media library (F9, the `[cms] media_*` keys) — still zero
//! JavaScript.
//!
//! Editors upload images at `/admin/media` through a plain
//! `multipart/form-data` form; every file is sniffed, whitelisted and
//! fully decoded under limits before anything touches the disk (the
//! pipeline lives in [`crate::media`]). The stored names are flat,
//! server-generated hex stems, so the public serving routes
//! (`GET /media/{id}/{name}`, `GET /media/thumb/{id}`) can never be
//! talked into a traversal, and every URL is immutable by construction
//! (a new upload is a new id and a new name) — hence the
//! `Cache-Control: immutable` year-long caching.
//!
//! The media directory (`[cms] media_dir`, default `media/`) is its own
//! directory, created at startup: the library **never writes into
//! `public/`**, and media is served by these routes rather than the
//! static file family so the caching rules and the row lookup stay
//! ours.
//!
//! Roles: uploading, alt-text editing and deleting are `CmsEditor`
//! (admin/editor); serving is public, like `/p/{slug}`. The alt text is
//! what page authors should paste as the Markdown `![alt](url)` alt
//! segment — the detail page shows both snippets ready to copy.

use axum::extract::{FromRequest, Multipart, Path, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::db::{MediaRecord, NewMedia};
use crate::error::AppError;
use crate::routes::cms::{
    clamp_page, cms_context, html_error_page, listing_query, pages_for, pagination_value,
    render_view, see_other, CmsEditor, PageParts,
};
use crate::state::AppState;
use crate::util::{format_timestamp, read_form};

/// The media grid's page size (F10): 24 thumbnails — four rows of the
/// desktop grid, dense enough to scan, light enough to render. The
/// panel sizes its own grids; `index_page_size` stays a public-listing
/// key.
const MEDIA_PAGE_SIZE: i64 = 24;

/// Upper bound on the alt text (the SEO `meta_description` budget).
const MAX_ALT_TEXT: usize = 500;

/// A multipart text field longer than this is refused outright (the
/// alt field is the only text field, and it is validated to
/// `MAX_ALT_TEXT` anyway — this is the belt under the braces).
const MAX_TEXT_FIELD: usize = 2_048;

/// Route fragment for this module (merged while the CMS is enabled).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/media", get(list_media).post(upload_media))
        .route("/admin/media/{id}", get(media_detail))
        .route("/admin/media/{id}/alt", post(update_alt))
        .route("/admin/media/{id}/delete", post(delete_media))
        // Static segment first: axum's matcher prefers it over `{id}`,
        // so `/media/thumb/5` never collides with `/media/5/<name>`.
        .route("/media/thumb/{id}", get(serve_thumb))
        .route("/media/{id}/{filename}", get(serve_media))
}

// ─── Admin: `GET/POST /admin/media` ─────────────────────────────────

/// `GET /admin/media`: the upload form and the newest items, one page
/// of the grid at a time (F10). Uploads and deletions land back on
/// page one — the freshest row is the one you want to see next.
async fn list_media(
    State(state): State<AppState>,
    _editor: CmsEditor,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let (_, requested) = listing_query(parts.uri());
    render_media_list(&state, &parts, None, "", requested).await
}

/// Renders the listing with an optional inline form error (and the
/// half-typed alt text kept — nothing typed is ever lost) at the
/// requested page, clamped to the real range (F10).
async fn render_media_list(
    state: &AppState,
    parts: &PageParts,
    error: Option<&str>,
    alt_text: &str,
    requested_page: i64,
) -> Response {
    let Ok(cms) = cms_context(state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let total = match cms.media.count().await {
        Ok(total) => total,
        Err(message) => {
            tracing::error!(%message, "media count failed");
            return AppError::internal("media storage failure").into_response();
        }
    };
    let page = clamp_page(requested_page, pages_for(total, MEDIA_PAGE_SIZE).max(1));

    let items = match cms
        .media
        .list_paged(MEDIA_PAGE_SIZE, (page - 1) * MEDIA_PAGE_SIZE)
        .await
    {
        Ok(items) => items,
        Err(message) => {
            tracing::error!(%message, "media listing failed");
            return AppError::internal("media storage failure").into_response();
        }
    };

    let media_items: Vec<Value> = items.iter().map(media_item_data).collect();
    render_view(
        state,
        parts,
        "admin/media_list.jhs",
        vec![
            ("media_items", Value::Array(media_items)),
            (
                "form_error",
                error
                    .map(|message| Value::String(message.to_owned()))
                    .unwrap_or(Value::Null),
            ),
            ("alt_text", Value::String(alt_text.to_owned())),
            (
                "paginacion",
                pagination_value(page, total, MEDIA_PAGE_SIZE, "/admin/media"),
            ),
        ],
        StatusCode::OK,
    )
    .await
}

/// The upload: `POST /admin/media` as `multipart/form-data`.
///
/// The request is taken whole (not through the `Multipart` extractor
/// argument) so the same [`PageParts`] that renders the error page is
/// captured before the body is consumed — the panel's "nothing typed
/// is ever lost" convention. Every field is streamed with its byte
/// cap enforced while reading, never buffered unchecked.
async fn upload_media(
    State(state): State<AppState>,
    editor: CmsEditor,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let max_bytes = state.config().cms.media_max_bytes;
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let mut multipart = match Multipart::from_request(request, &state).await {
        Ok(multipart) => multipart,
        Err(rejection) => {
            return render_media_list(&state, &parts, Some(&rejection.to_string()), "", 1).await;
        }
    };

    // Read the form: the `alt` text field and the `file` part. Unknown
    // fields are drained (browsers and curl attach different extras).
    let mut alt_text = String::new();
    let mut original_name: Option<String> = None;
    let mut file_bytes: Option<Vec<u8>> = None;
    while let Some(mut field) = match multipart.next_field().await {
        Ok(field) => field,
        Err(error) => {
            return render_media_list(
                &state,
                &parts,
                Some(&format!("No se pudo leer la subida: {error}")),
                &alt_text,
                1,
            )
            .await;
        }
    } {
        let name = field.name().map(str::to_owned);
        match name.as_deref() {
            Some("alt") => {
                let (text, overflow) = match read_capped(&mut field, MAX_TEXT_FIELD).await {
                    Ok(pair) => pair,
                    Err(error) => {
                        return render_media_list(
                            &state,
                            &parts,
                            Some(&format!("No se pudo leer el texto alternativo: {error}")),
                            "",
                            1,
                        )
                        .await;
                    }
                };
                if overflow {
                    return render_media_list(
                        &state,
                        &parts,
                        Some("El texto alternativo es demasiado largo (máximo 500 caracteres)."),
                        "",
                        1,
                    )
                    .await;
                }
                alt_text = String::from_utf8_lossy(&text).trim().to_owned();
            }
            Some("file") => {
                original_name = field.file_name().map(sanitize_original_name);
                let (buffer, overflow) = match read_capped(&mut field, max_bytes as usize).await {
                    Ok(pair) => pair,
                    Err(error) => {
                        return render_media_list(
                            &state,
                            &parts,
                            Some(&format!("No se pudo leer el archivo: {error}")),
                            &alt_text,
                            1,
                        )
                        .await;
                    }
                };
                if overflow {
                    return render_media_list(
                        &state,
                        &parts,
                        Some(&format!(
                            "El archivo supera el límite de {} configurado en \
                             [cms] media_max_bytes.",
                            human_bytes(max_bytes as i64)
                        )),
                        &alt_text,
                        1,
                    )
                    .await;
                }
                file_bytes = Some(buffer);
            }
            // Drain without buffering: the body limit bounds the
            // request as a whole; the library stores nothing it was
            // not asked for.
            _ => while let Ok(Some(_)) = field.chunk().await {},
        }
    }

    let Some(file_bytes) = file_bytes else {
        return render_media_list(
            &state,
            &parts,
            Some("Elige un archivo: la subida no traía el campo «file»."),
            &alt_text,
            1,
        )
        .await;
    };
    if file_bytes.is_empty() {
        return render_media_list(&state, &parts, Some("El archivo está vacío."), &alt_text, 1)
            .await;
    }

    // Sniff, whitelist and decode under limits — off the async runtime
    // (decode is CPU work; the F8 preview set the spawn_blocking
    // precedent). The bytes come back together with the result: the
    // validated file is what gets written, byte for byte.
    let prepared = match tokio::task::spawn_blocking(move || {
        crate::media::prepare(&file_bytes).map(|prepared| (prepared, file_bytes))
    })
    .await
    {
        Ok(Ok(pair)) => pair,
        Ok(Err(error)) => {
            return render_media_list(&state, &parts, Some(error), &alt_text, 1).await;
        }
        Err(join_error) => {
            tracing::error!(%join_error, "the image pipeline panicked");
            return render_media_list(
                &state,
                &parts,
                Some("No se pudo procesar la imagen (error interno)."),
                &alt_text,
                1,
            )
            .await;
        }
    };
    let (prepared, file_bytes) = prepared;

    if alt_text.chars().count() > MAX_ALT_TEXT {
        return render_media_list(
            &state,
            &parts,
            Some("El texto alternativo no puede pasar de 500 caracteres."),
            &alt_text,
            1,
        )
        .await;
    }

    // Flat, server-generated names: `<32-hex>.<ext>` and
    // `<32-hex>_t.png`. Nothing the client sent influences them.
    let stem = crate::media::new_stem();
    let stored_name = format!("{}.{}", stem, prepared.format.extension());
    let thumb_name = format!("{stem}_t.png");
    let original_name = original_name.unwrap_or_else(|| String::from("imagen"));

    let full_path = cms.media_root.join(&stored_name);
    let thumb_path = cms.media_root.join(&thumb_name);
    if let Err(error) = tokio::fs::write(&full_path, &file_bytes).await {
        tracing::error!(%error, path = %full_path.display(), "media write failed");
        return render_media_list(
            &state,
            &parts,
            Some("No se pudo guardar el archivo en el disco."),
            &alt_text,
            1,
        )
        .await;
    }
    if let Err(error) = tokio::fs::write(&thumb_path, &prepared.thumbnail).await {
        // The row was never inserted; roll the full file back so the
        // disk and the database stay in step.
        let _ = tokio::fs::remove_file(&full_path).await;
        tracing::error!(%error, path = %thumb_path.display(), "thumbnail write failed");
        return render_media_list(
            &state,
            &parts,
            Some("No se pudo guardar la miniatura en el disco."),
            &alt_text,
            1,
        )
        .await;
    }

    let new_media = NewMedia {
        stored_name: stored_name.clone(),
        thumb_name: thumb_name.clone(),
        original_name,
        mime_type: prepared.format.mime().to_owned(),
        bytes: file_bytes.len() as i64,
        width: prepared.width as i64,
        height: prepared.height as i64,
        alt_text: alt_text.clone(),
        created_by: Some(editor.user.user_id),
    };

    match cms.media.create(&new_media).await {
        Ok(record) => see_other(&format!("/admin/media/{}", record.id)),
        Err(message) => {
            // Row insert failed: remove both files again.
            let _ = tokio::fs::remove_file(&full_path).await;
            let _ = tokio::fs::remove_file(&thumb_path).await;
            tracing::error!(%message, "media insert failed");
            render_media_list(
                &state,
                &parts,
                Some("No se pudo guardar (error interno)."),
                &alt_text,
                1,
            )
            .await
        }
    }
}

/// `GET /admin/media/{id}`: the detail page — full image, alt form,
/// the copy-paste snippets and the delete button.
async fn media_detail(
    State(state): State<AppState>,
    _editor: CmsEditor,
    Path(id): Path<i64>,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let record = match cms.media.find_by_id(id).await {
        Ok(Some(record)) => record,
        Ok(None) => return media_not_found(),
        Err(message) => {
            tracing::error!(%message, "media lookup failed");
            return AppError::internal("media storage failure").into_response();
        }
    };

    render_view(
        &state,
        &parts,
        "admin/media_detail.jhs",
        detail_data(&record, None),
        StatusCode::OK,
    )
    .await
}

/// `POST /admin/media/{id}/alt`: replace the alt text.
async fn update_alt(
    State(state): State<AppState>,
    _editor: CmsEditor,
    Path(id): Path<i64>,
    request: Request,
) -> Response {
    let parts = PageParts::of(&request);
    let max_body = state.config().server.max_body_size_bytes;
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    #[derive(Deserialize)]
    struct AltForm {
        alt: String,
    }
    let form = match read_form::<AltForm>(request, max_body).await {
        Ok(form) => form,
        Err(message) => {
            return render_detail_with_error(&state, &parts, id, &message).await;
        }
    };

    let alt_text = form.alt.trim();
    if alt_text.chars().count() > MAX_ALT_TEXT {
        return render_detail_with_error(
            &state,
            &parts,
            id,
            "El texto alternativo no puede pasar de 500 caracteres.",
        )
        .await;
    }

    match cms.media.update_alt(id, alt_text).await {
        Ok(Some(_)) => see_other(&format!("/admin/media/{id}?ok=alt")),
        Ok(None) => media_not_found(),
        Err(message) => {
            tracing::error!(%message, "alt update failed");
            render_detail_with_error(&state, &parts, id, "No se pudo guardar (error interno).")
                .await
        }
    }
}

/// `POST /admin/media/{id}/delete`: remove the row, then the files.
///
/// Pages that still reference the URL keep their `<img>`/Markdown
/// pointing at a now-404 — the detail page warns about exactly that
/// before the button is pressed; finding references is F10's search
/// business.
async fn delete_media(
    State(state): State<AppState>,
    _editor: CmsEditor,
    Path(id): Path<i64>,
) -> Response {
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let record = match cms.media.find_by_id(id).await {
        Ok(Some(record)) => record,
        Ok(None) => return media_not_found(),
        Err(message) => {
            tracing::error!(%message, "media lookup failed");
            return AppError::internal("media storage failure").into_response();
        }
    };

    match cms.media.delete(id).await {
        Ok(true) => {
            for name in [record.stored_name, record.thumb_name] {
                let path = cms.media_root.join(name);
                if let Err(error) = tokio::fs::remove_file(&path).await {
                    // The row is gone; a leftover file is an orphan on
                    // disk, not a broken page — warn and move on.
                    tracing::warn!(%error, path = %path.display(), "media file left behind");
                }
            }
            see_other("/admin/media?ok=eliminado")
        }
        Ok(false) => media_not_found(),
        Err(message) => {
            tracing::error!(%message, "media delete failed");
            AppError::internal("media storage failure").into_response()
        }
    }
}

/// Re-renders the detail page with an inline error (the alt form's
/// bounce-back).
async fn render_detail_with_error(
    state: &AppState,
    parts: &PageParts,
    id: i64,
    error: &str,
) -> Response {
    let Ok(cms) = cms_context(state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };
    let Some(record) = cms.media.find_by_id(id).await.ok().flatten() else {
        return media_not_found();
    };

    render_view(
        state,
        parts,
        "admin/media_detail.jhs",
        detail_data(&record, Some(error)),
        StatusCode::OK,
    )
    .await
}

// ─── Public: `GET /media/{id}/{name}` and `GET /media/thumb/{id}` ───

/// Serves the stored file. The URL name must match the row exactly
/// (the canonical name comes from the detail page's snippets), the
/// lookup key is the id, and the stored names are flat and
/// server-generated — a wrong name, an unknown id or a traversal
/// attempt all land in the same 404.
async fn serve_media(
    State(state): State<AppState>,
    Path((id, filename)): Path<(i64, String)>,
) -> Response {
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let record = match cms.media.find_by_id(id).await {
        Ok(Some(record)) => record,
        Ok(None) => return media_not_found(),
        Err(message) => {
            tracing::error!(%message, "media lookup failed");
            return AppError::internal("media storage failure").into_response();
        }
    };

    if record.stored_name != filename {
        return media_not_found();
    }

    let path = cms.media_root.join(&record.stored_name);
    match tokio::fs::read(&path).await {
        Ok(bytes) => file_response(&bytes, &record.mime_type),
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "media file missing on disk");
            media_not_found()
        }
    }
}

/// Serves the thumbnail (PNG). A missing thumbnail falls back to the
/// full file rather than 404-ing the listing.
async fn serve_thumb(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let Ok(cms) = cms_context(&state) else {
        return AppError::internal("the CMS is not initialized").into_response();
    };

    let record = match cms.media.find_by_id(id).await {
        Ok(Some(record)) => record,
        Ok(None) => return media_not_found(),
        Err(message) => {
            tracing::error!(%message, "media lookup failed");
            return AppError::internal("media storage failure").into_response();
        }
    };

    let thumb_path = cms.media_root.join(&record.thumb_name);
    if let Ok(bytes) = tokio::fs::read(&thumb_path).await {
        return file_response(&bytes, "image/png");
    }

    let full_path = cms.media_root.join(&record.stored_name);
    match tokio::fs::read(&full_path).await {
        Ok(bytes) => file_response(&bytes, &record.mime_type),
        Err(error) => {
            tracing::warn!(%error, path = %full_path.display(), "media file missing on disk");
            media_not_found()
        }
    }
}

// ─── Helpers ────────────────────────────────────────────────────────

/// Streams one multipart field into a buffer, stopping (and reporting)
/// the moment the byte cap is exceeded — the file is never buffered
/// unchecked.
async fn read_capped(
    field: &mut axum::extract::multipart::Field<'_>,
    cap: usize,
) -> Result<(Vec<u8>, bool), axum::extract::multipart::MultipartError> {
    let mut buffer = Vec::new();
    while let Some(chunk) = field.chunk().await? {
        if buffer.len() + chunk.len() > cap {
            return Ok((buffer, true));
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok((buffer, false))
}

/// The immutable-by-construction media response: sniffed mime type,
/// year-long public caching (every upload is a fresh id + fresh name,
/// and the bytes under a URL never change).
fn file_response(bytes: &[u8], mime_type: &str) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime_type)
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
        .body(axum::body::Body::from(bytes.to_vec()))
        .expect("valid media response")
}

/// The shared 404 for every public media miss.
fn media_not_found() -> Response {
    html_error_page(
        StatusCode::NOT_FOUND,
        "No encontrado",
        "Ese archivo de medios no existe (o su nombre no es el canónico).",
    )
}

/// One media row as template data: the URLs, the display-ready sizes
/// and dates, the alt text.
fn media_item_data(record: &MediaRecord) -> Value {
    json!({
        "id": record.id,
        "original_name": record.original_name,
        "stored_name": record.stored_name,
        "url": format!("/media/{}/{}", record.id, record.stored_name),
        "thumb_url": format!("/media/thumb/{}", record.id),
        "mime_type": record.mime_type,
        "width": record.width,
        "height": record.height,
        "bytes": record.bytes,
        "size_h": human_bytes(record.bytes),
        "alt_text": record.alt_text,
        "created_at_h": format_timestamp(record.created_at),
    })
}

/// The detail page's data: the row plus the ready-to-copy snippets
/// (the alt text lives in both, so editing it updates the suggestion).
fn detail_data(record: &MediaRecord, error: Option<&str>) -> Vec<(&'static str, Value)> {
    let item = media_item_data(record);
    let url = item["url"].as_str().unwrap_or_default().to_owned();
    vec![
        ("item", item),
        (
            "markdown_snippet",
            Value::String(format!("![{}]({url})", record.alt_text)),
        ),
        (
            "html_snippet",
            Value::String(format!(
                "<img src=\"{url}\" alt=\"{}\" width=\"{}\" height=\"{}\">",
                record.alt_text, record.width, record.height
            )),
        ),
        (
            "form_error",
            error
                .map(|message| Value::String(message.to_owned()))
                .unwrap_or(Value::Null),
        ),
    ]
}

/// Formats a byte count for the panel: `812 B`, `21,4 KB`, `1,2 MB`
/// (Spanish decimal comma, powers of two under the hood).
fn human_bytes(bytes: i64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    if bytes < 0 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let mut text = if unit == 0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    };
    if text.contains('.') {
        text = text.replace('.', ",");
    }
    format!("{text} {}", UNITS[unit])
}

/// Keeps the display name of an upload honest: last path segment only
/// (clients send anything from bare names to full Windows paths),
/// control characters stripped, length capped in characters.
fn sanitize_original_name(raw: &str) -> String {
    let base: String = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    let trimmed = base.trim();
    if trimmed.is_empty() {
        String::from("imagen")
    } else if trimmed.chars().count() > 200 {
        trimmed.chars().take(200).collect()
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_counts_format_in_spanish() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(812), "812 B");
        assert_eq!(human_bytes(1024), "1,0 KB");
        assert_eq!(human_bytes(21_944_320), "20,9 MB");
        assert_eq!(human_bytes(-5), "-5 B");
    }

    #[test]
    fn original_names_lose_paths_and_controls() {
        assert_eq!(sanitize_original_name("logo.png"), "logo.png");
        assert_eq!(
            sanitize_original_name("C:\\Users\\pepe\\logo.png"),
            "logo.png"
        );
        assert_eq!(
            sanitize_original_name("/tmp/foto gato.jpg"),
            "foto gato.jpg"
        );
        assert_eq!(sanitize_original_name("..\\..\\x"), "x");
        assert_eq!(sanitize_original_name("\u{1}we\u{7}ird"), "weird");
        assert_eq!(sanitize_original_name(""), "imagen");
        assert_eq!(sanitize_original_name("   "), "imagen");
        let long = "a".repeat(300);
        assert_eq!(sanitize_original_name(&long).chars().count(), 200);
    }
}
