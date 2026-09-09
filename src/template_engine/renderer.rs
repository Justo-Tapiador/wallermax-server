//! The rendering backend abstraction (v0.10.0).
//!
//! [`TemplateRenderer`] is the seam the whole template pipeline talks
//! to: the middleware's [`render_response`](crate::middleware::templates)
//! funnel, the CMS handlers' `render_string` calls — every `.jhs` render
//! in the process, for the public tree, the views tree and CMS page
//! bodies alike. Two backends implement it:
//!
//! | Backend                 | Runtime                                    |
//! |-------------------------|--------------------------------------------|
//! | [`JhsEngine`]           | the in-process sandboxed boa engine (the hardened default since v0.5.0) |
//! | [`SidecarRenderer`](super::sidecar::SidecarRenderer) | the Node sidecar running the original node-jhs2 engine |
//!
//! and a third composition, `AutoRenderer`, tries the sidecar first and
//! falls back to boa when the sidecar process is unavailable — see
//! [`crate::template_engine::sidecar`]. The backend is selected with
//! `[templates] backend` (`"boa"` | `"sidecar"` | `"auto"`), and every
//! implementation reports its live identity through
//! [`TemplateRenderer::backend_name`] (v0.10.1): `GET /health` and the
//! `wallermax_template_backend` metric read it.
//!
//! Both methods are **synchronous and CPU/IO-bound**: callers run them
//! inside `spawn_blocking` exactly like the boa engine always required.

use serde_json::{Map, Value};

use super::engine::{JhsEngine, JhsError, RenderOutput};

/// A `.jhs` rendering backend.
///
/// Implementations must be `Send + Sync`: the renderer is shared
/// through an `Arc` across the blocking pool.
pub trait TemplateRenderer: Send + Sync {
    /// Renders the template at `template_path` (absolute, or relative
    /// to the configured views path) with the given data.
    ///
    /// # Errors
    ///
    /// Returns the backend's [`JhsError`]: file errors, include
    /// resolution errors, template execution errors — or
    /// [`JhsError::Sidecar`] when the sidecar backend is selected but
    /// its process cannot be reached.
    fn render(
        &self,
        template_path: &str,
        data: &Map<String, Value>,
    ) -> Result<RenderOutput, JhsError>;

    /// Renders a template from a raw string (the CMS seam: page bodies
    /// stored in the database).
    ///
    /// # Errors
    ///
    /// Same error model as [`TemplateRenderer::render`].
    fn render_string(
        &self,
        template: &str,
        data: &Map<String, Value>,
    ) -> Result<RenderOutput, JhsError>;

    /// The effective backend identity of this renderer: `"boa"`,
    /// `"sidecar"`, `"auto"` resolved to whichever half is currently
    /// serving, or `"unavailable"` (the strict sidecar failed to
    /// spawn). Metadata for `GET /health` and the
    /// `wallermax_template_backend` Prometheus gauge (v0.10.1) — pure
    /// in-memory reads, no I/O, safe on any path.
    fn backend_name(&self) -> &'static str;
}

impl TemplateRenderer for JhsEngine {
    fn render(
        &self,
        template_path: &str,
        data: &Map<String, Value>,
    ) -> Result<RenderOutput, JhsError> {
        JhsEngine::render(self, template_path, data)
    }

    fn render_string(
        &self,
        template: &str,
        data: &Map<String, Value>,
    ) -> Result<RenderOutput, JhsError> {
        JhsEngine::render_string(self, template, data)
    }

    fn backend_name(&self) -> &'static str {
        "boa"
    }
}

/// A backend whose sidecar failed to start: every render answers the
/// spawn failure so the error surfaces wherever rendering is attempted
/// (and `ensure_ready` aborts the real server startup first).
pub(crate) struct BrokenRenderer {
    message: String,
}

impl BrokenRenderer {
    /// Wraps the spawn-failure description.
    pub(crate) fn new(message: String) -> Self {
        Self { message }
    }
}

impl TemplateRenderer for BrokenRenderer {
    fn render(
        &self,
        _template_path: &str,
        _data: &Map<String, Value>,
    ) -> Result<RenderOutput, JhsError> {
        Err(JhsError::Sidecar(self.message.clone()))
    }

    fn render_string(
        &self,
        _template: &str,
        _data: &Map<String, Value>,
    ) -> Result<RenderOutput, JhsError> {
        Err(JhsError::Sidecar(self.message.clone()))
    }

    fn backend_name(&self) -> &'static str {
        // The strict server refuses to start in this state (the
        // `ensure_ready` gate fires first), so this is a diagnostic
        // value for embedders and tests, not a serving mode.
        "unavailable"
    }
}
