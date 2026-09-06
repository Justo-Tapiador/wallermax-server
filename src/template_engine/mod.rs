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
//! The engine is split in two halves:
//!
//! | Module      | Responsibility                                                |
//! |-------------|---------------------------------------------------------------|
//! | [`parser`]  | Compiles `.jhs` source into one JavaScript program            |
//! | [`engine`]  | Sandbox execution, caching, error model, public API           |
//!
//! Deliberate divergences from the original engine (documented in each
//! module):
//!
//! - **No `require`, `Buffer` or `include`** inside templates: the sandbox
//!   exposes no host I/O at all, so template code cannot touch the file
//!   system, spawn processes or load modules.
//! - **Loop iteration limit** instead of the original's ineffective
//!   5-second `vm` timeout: `<?jhs while(true){} ?>` reliably throws
//!   instead of hanging the worker (the Node engine hangs forever on
//!   modern V8).
//! - **Cache invalidation by mtime**: the original caches compiled
//!   templates forever; this engine recompiles a file when its mtime
//!   changes.
//! - `console.*` output is **captured** and routed to `tracing` logs
//!   instead of the process stdout.

pub mod engine;
pub mod parser;

pub use engine::{ConsoleLine, JhsEngine, JhsError, JhsOptions, RenderOutput};
pub use parser::TagOptions;
