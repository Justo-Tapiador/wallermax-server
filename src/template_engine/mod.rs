//! Dynamic template engine (the `[templates]` section).
//!
//! Renders `.jhs` templates — HTML with embedded JavaScript, PHP-style —
//! inside a sandboxed JS engine, mirroring the semantics of the original
//! Node.js engine `node-jhs2` (MIT, `Justo-Tapiador/node-jhs2`), whose
//! public repository is the ground truth this port was validated against.
//!
//! Template syntax:
//!
//! - `<?jhs ... ?>` — a JavaScript code block;
//! - `<?= expr ?>`  — an output expression (HTML-escaped by default).
//!
//! The engine is split in five modules:
//!
//! | Module             | Responsibility                                             |
//!|--------------------|------------------------------------------------------------|
//! | [`parser`]         | Compiles `.jhs` source into one JavaScript program         |
//! | [`engine`]         | Sandboxed execution, caching, error model, public API        |
//! | [`require_bridge`] | The v0.9.0 `require()` bridge: module banner, polyfills and |
//! |                    | CommonJS loading of local JS modules                       |
//! | [`renderer`]       | The v0.10.0 backend abstraction shared by every render      |
//! | [`sidecar`]        | The Node sidecar backend: original-engine rendering in a    |
//! |                    | supervised child process, plus the auto fallback            |
//!
//! Deliberate divergences from the original engine (documented in each
//! module):
//!
//! - **`require()` is a native bridge, not Node's**: the original
//!   wrapped the real Node `require` with a small `banned_require`
//!   blocklist. This port keeps the banner posture — configurable in
//!   `[templates] forbidden_modules` — but modules come from a Rust
//!   polyfill registry (`crypto`) and from pure-JS files under the
//!   modules directory, because there is no Node runtime behind
//!   boa_engine. Native addons and host-touching built-ins simply
//!   cannot exist here; see [`require_bridge`].
//! - **Loop iteration limit** instead of the original's ineffective
//!   5-second `vm` timeout: `<?jhs while(true){} ?>` reliably throws
//!   instead of hanging the worker (the Node engine hangs forever on
//!   modern V8).
//! - **Cache invalidation by mtime**: the original caches compiled
//!   templates forever; this engine recompiles a file when its mtime
//!   changes (module sources follow the same rule).
//! - **Module instances are per render** (the original shares them for
//!   the process lifetime): module state can never leak between
//!   requests.
//! - `console.*` output is **captured** and routed to `tracing` logs
//!   instead of the process stdout.
//! - The Express `res`/`req` objects the original's host app passed as
//!   render data are rebuilt as a safe shim: `res.redirect()` records a
//!   local-path-only redirect intent that the route layer honours, and
//!   `req` arrives as template data with a sanitized header allowlist.
//!
//! Since v0.10.0 a second backend exists side by side with the boa
//! sandbox: the **Node sidecar** (see [`sidecar`]) runs the ORIGINAL
//! `node-jhs2` engine in a supervised child process, so templates get
//! the real Node runtime behind `require()` (minus the
//! `forbidden_modules` banner) — `require('url')` works, the compiler
//! is the original's, and runaway renders are hard-killed by a
//! wall-clock worker budget. `[templates] backend` picks the engine:
//! `"boa"` (in-process, hardened default), `"sidecar"` (strict — the
//! server refuses to start without Node) or `"auto"` (sidecar when
//! available, transparent boa fallback). Both backends implement the
//! same [`TemplateRenderer`] seam, so EVERY render in the process —
//! public `.jhs` files, auto-routed views and CMS page bodies alike —
//! flows through the selected backend.

pub mod engine;
pub mod parser;
pub mod renderer;
pub mod require_bridge;
pub mod sidecar;

pub use engine::{ConsoleLine, JhsEngine, JhsError, JhsOptions, RedirectIntent, RenderOutput};
pub use parser::TagOptions;
pub use renderer::TemplateRenderer;
pub use require_bridge::{RequireOptions, DEFAULT_FORBIDDEN_MODULES};
pub use sidecar::{AutoRenderer, SidecarOptions, SidecarRenderer};
