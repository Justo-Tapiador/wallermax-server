//! Service index (`GET /api`, plus `GET /` while static serving is off).

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::state::AppState;

/// Service discovery payload.
#[derive(Serialize)]
struct IndexResponse {
    service: &'static str,
    version: &'static str,
    description: &'static str,
    endpoints: Vec<&'static str>,
}

/// `GET /`: basic service information and endpoint discovery.
///
/// With static file serving enabled this handler is only mounted at
/// `/api` (the root serves the static index file); otherwise it is
/// mounted at both `/` and `/api`.
///
/// The endpoint list reflects the mounted route families, so
/// authentication routes appear only while `[auth]` is enabled and the
/// root entry changes shape while `[static]` is enabled.
async fn index(State(state): State<AppState>) -> Json<IndexResponse> {
    let mut endpoints = Vec::new();

    if state.config().static_files.enabled {
        endpoints.extend([
            "GET / (static index + files)",
            "GET /api",
            "GET /health",
            "GET /api/stats",
            "POST /api/echo",
        ]);
    } else {
        endpoints.extend([
            "GET /",
            "GET /api",
            "GET /health",
            "GET /api/stats",
            "POST /api/echo",
        ]);
    }

    if state.config().metrics.enabled {
        endpoints.push("GET /metrics");
    }

    if state.auth_enabled() {
        endpoints.extend([
            "POST /api/auth/register",
            "POST /api/auth/login",
            "GET /api/auth/me",
            "GET /api/admin/users",
        ]);
        if state.refresh_enabled() {
            endpoints.extend([
                "POST /api/auth/refresh",
                "POST /api/auth/logout",
                "POST /api/auth/logout_all",
            ]);
        }
    }

    if state.cms_enabled() {
        endpoints.extend([
            "GET /p (public pages index)",
            "GET /p/{slug} (CMS page)",
            "GET /admin (CMS panel)",
            "POST /perfil/password (self password change)",
        ]);
    }

    if state.external_api_enabled() {
        endpoints.push("GET/POST /api/ext/{name} (external API proxy)");
    }

    Json(IndexResponse {
        service: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
        description: "A modular, secure and high-performance web server written in Rust.",
        endpoints,
    })
}

/// Route fragment for this module.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api", get(index))
}

/// Route fragment mounting the JSON index at `/` — used only while
/// static file serving is disabled.
pub(super) fn root_routes() -> Router<AppState> {
    Router::new().route("/", get(index))
}
