//! Sandboxed execution of compiled `.jhs` programs.
//!
//! Every render runs in a **fresh [`boa_engine::Context`]**, so templates
//! can never share state across requests. The context receives:
//!
//! - a hidden `RawString` sentinel (closure-scoped, exactly like the
//!   original engine): `raw()` wraps values in it and `__escape` lets
//!   them bypass auto-escaping, but template code cannot forge or detect
//!   the sentinel itself;
//! - `__escape` (the auto-escape aware escaper), `escapeHtml` and `raw`;
//! - a **captured** `console` (output is returned to the caller instead
//!   of printing to stdout);
//! - the render data, injected as global variables through
//!   `Object.defineProperty` — `CreateDataProperty` semantics, so a data
//!   key named `__proto__` cannot pollute the global object's prototype.
//!
//! Data keys colliding with the sandbox helpers (`__escape`,
//! `escapeHtml`, `raw`, `console`, `JSON`, `require`, `res`, …) are
//! ignored, mirroring the original engine where the helpers are
//! assigned **after** the data spread and therefore always win.
//!
//! Since v0.9.0 the sandbox additionally exposes the original engine's
//! `require()` — rebuilt as a native bridge (see [`require_bridge`]):
//! a configurable module **banner** (`[templates]
//! forbidden_modules`, the hardened descendant of node-jhs2's
//! `banned_require`), the `crypto` polyfill implemented in Rust, and
//! CommonJS loading of pure-JS modules from the modules directory.
//! There is still no Node.js behind the sandbox: no `Buffer`, no
//! `process`, no native addons, no file system outside the modules
//! directory. A loop iteration limit (boa's runtime limits) bounds
//! runaway loops — the original Node engine hangs forever on
//! `<?jhs while(true){} ?>` because its `vm` timeout cannot interrupt a
//! tight loop — and the same limit covers module code.
//!
//! Templates also get an Express-shaped `res` shim: `res.redirect()`
//! records a redirect intent (local paths only) that the route layer
//! turns into the actual HTTP redirect, and the middleware injects a
//! `req` global (`method`, `url`, `path`, `query`, sanitized
//! `headers`) as render data.
//!
//! The cache keeps compiled programs keyed by path and invalidated by
//! mtime, so editing a template takes effect on the next request
//! without a restart (the original engine caches forever).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use boa_engine::{Context, JsValue, Source};
use serde_json::{Map, Value};

use super::parser::{self, TagOptions};
use super::require_bridge::{self, ModuleSource, RequireOptions};

/// Maximum nesting depth of the compile-time `include()` resolution
/// (a partial including a partial including ...).
const INCLUDE_MAX_DEPTH: usize = 8;

/// Maximum total bytes of embedded partial sources during one
/// resolution pass — the include-bomb guard.
const INCLUDE_MAX_BYTES: usize = 1_048_576;

/// Maximum length of an `include()` name.
const INCLUDE_MAX_NAME_LEN: usize = 64;

/// Sandbox and caching options (mirrors the original constructor).
#[derive(Debug, Clone)]
pub struct JhsOptions {
    /// Directory `.jhs` files are resolved against when a relative path
    /// is rendered (the original's `viewsPath`).
    pub views_path: PathBuf,
    /// Cache compiled templates (invalidated by mtime).
    pub cache: bool,
    /// HTML-escape dynamic output (`<?= ?>` and `echo()`); literal
    /// template text is never escaped either way.
    pub auto_escape: bool,
    /// Custom delimiters.
    pub tags: TagOptions,
    /// Upper bound on loop iterations inside one template render.
    ///
    /// Zero is rejected by the configuration validation.
    pub loop_iteration_limit: u64,
    /// The `require()` bridge: module banner, modules directory and
    /// the switch that installs `require` in the sandbox (v0.9.0).
    pub require: RequireOptions,
}

impl Default for JhsOptions {
    fn default() -> Self {
        Self {
            views_path: PathBuf::from("views"),
            cache: true,
            auto_escape: true,
            tags: TagOptions::default(),
            loop_iteration_limit: 10_000_000,
            require: RequireOptions::default(),
        }
    }
}

/// Data keys that may not shadow the sandbox helpers.
const PROTECTED_GLOBALS: [&str; 13] = [
    "__escape",
    "escapeHtml",
    "raw",
    "console",
    "JSON",
    "__jhsConsoleLines",
    "include",
    "__jhsEchoPart",
    "require",
    "res",
    "__jhsResolve",
    "__jhsModuleLoad",
    "__jhsModuleCache",
];

/// Error produced while rendering a template.
#[derive(Debug)]
pub enum JhsError {
    /// The template file could not be read.
    Io(std::io::Error),
    /// A `<?jhs include("name") ?>` block could not be resolved: the
    /// name is invalid, the partial is missing, or the resolution
    /// exceeded its depth/size budget. The message is author-facing.
    Include(String),
    /// The template threw while executing; the message carries the
    /// original engine's wording, `Template execution error (<path>): …`.
    Execution {
        /// Label under which the template ran (file path or `<string>`).
        path: String,
        /// The wrapped engine message.
        message: String,
    },
}

impl std::fmt::Display for JhsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JhsError::Io(error) => write!(formatter, "template file error: {error}"),
            JhsError::Include(message) => write!(formatter, "template include error: {message}"),
            JhsError::Execution { message, .. } => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for JhsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            JhsError::Io(error) => Some(error),
            JhsError::Include(_) | JhsError::Execution { .. } => None,
        }
    }
}

/// One line captured from `console.*` calls inside a template.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct ConsoleLine {
    /// Console method used: `log`, `info`, `warn`, `error`, `debug` or `trace`.
    pub level: String,
    /// Space-joined stringified arguments.
    pub message: String,
}

/// The result of a render: the HTML plus everything the template wrote
/// to the (captured) console, plus the redirect a `res.redirect()`
/// call requested (v0.9.0).
#[derive(Debug, Clone, Default)]
pub struct RenderOutput {
    /// Rendered HTML.
    pub html: String,
    /// Console lines captured during execution.
    pub console: Vec<ConsoleLine>,
    /// The redirect recorded by `res.redirect()`, if any. The route
    /// layer answers it with an HTTP redirect instead of the HTML.
    pub redirect: Option<RedirectIntent>,
}

/// A redirect requested from inside a template through
/// `res.redirect(location[, status])`.
///
/// Locations are validated to be local paths (starting with a single
/// `/`) at call time — the same anti-open-redirect posture as the
/// auth forms — so the route layer can hand the value to the
/// `Location` header as-is.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct RedirectIntent {
    /// Target path (always local, `/…`).
    pub location: String,
    /// HTTP status: 301, 302 (the default), 303, 307 or 308.
    #[serde(default = "default_redirect_status")]
    pub status: u16,
}

/// The default redirect status (`302 Found`, Node's `res.redirect`
/// default).
fn default_redirect_status() -> u16 {
    302
}

/// The `.jhs` template engine. Cheap to share: rendering takes `&self`,
/// the compiled-template cache sits behind a mutex and every render
/// builds a fresh sandbox.
#[derive(Debug)]
pub struct JhsEngine {
    options: JhsOptions,
    cache: Mutex<HashMap<PathBuf, CachedTemplate>>,
    /// Module sources for the `require()` bridge, invalidated by mtime
    /// exactly like the compiled templates. Shared through an `Arc`
    /// because the native `require` closure cannot borrow the engine.
    module_sources: Arc<Mutex<HashMap<PathBuf, ModuleSource>>>,
}

#[derive(Debug, Clone)]
struct CachedTemplate {
    program: String,
    mtime: Option<SystemTime>,
    /// Partial files embedded by the compile-time `include()`
    /// resolution, with their mtimes at compile time — a change in any
    /// of them invalidates the cached program exactly like a change in
    /// the main file does.
    includes: Vec<(PathBuf, Option<SystemTime>)>,
}

impl JhsEngine {
    /// Creates an engine from the given options.
    pub fn new(options: JhsOptions) -> Self {
        Self {
            options,
            cache: Mutex::new(HashMap::new()),
            module_sources: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The configured options.
    pub fn options(&self) -> &JhsOptions {
        &self.options
    }

    /// Renders the template at `template_path` (absolute, or relative to
    /// the configured views path) with the given data.
    ///
    /// Standalone `<?jhs include("name") ?>` blocks are resolved against
    /// the views directory at compile time.
    ///
    /// # Errors
    ///
    /// Returns [`JhsError::Io`] when the file cannot be read,
    /// [`JhsError::Include`] when an embedded partial cannot be resolved
    /// and [`JhsError::Execution`] when the template throws.
    pub fn render(
        &self,
        template_path: &str,
        data: &Map<String, Value>,
    ) -> Result<RenderOutput, JhsError> {
        let full_path = if Path::new(template_path).is_absolute() {
            PathBuf::from(template_path)
        } else {
            self.options.views_path.join(template_path)
        };
        let label = full_path.display().to_string();
        let program = self.load_program(&full_path)?;
        self.execute(&program, data, &label)
    }

    /// Renders a template from a raw string (labelled `<string>`),
    /// resolving standalone `<?jhs include("name") ?>` blocks against
    /// the views directory first.
    ///
    /// This is the CMS seam: page content stored in the database is
    /// ordinary `.jhs` source, so CMS pages can embed the same shared
    /// partials (header, footer) as the built-in views.
    ///
    /// # Errors
    ///
    /// Returns [`JhsError::Include`] when an embedded partial cannot be
    /// resolved and [`JhsError::Execution`] when the template throws.
    pub fn render_string(
        &self,
        template: &str,
        data: &Map<String, Value>,
    ) -> Result<RenderOutput, JhsError> {
        let resolved = self.resolve_includes(template, 0, &mut 0, &mut Vec::new())?;
        let program = parser::compile(&resolved, &self.options.tags);
        self.execute(&program, data, "<string>")
    }

    /// Empties the compiled-template and module-source caches.
    pub fn clear_cache(&self) {
        self.cache.lock().expect("template cache mutex").clear();
        self.module_sources
            .lock()
            .expect("module source cache mutex")
            .clear();
    }

    /// Loads (and caches) the compiled program for `path`.
    ///
    /// A cache hit requires the main file's mtime **and** every embedded
    /// partial's mtime to be unchanged; editing a partial recompiles the
    /// templates that embed it.
    fn load_program(&self, path: &Path) -> Result<String, JhsError> {
        let mtime = std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok();

        if self.options.cache {
            if let Some(hit) = self.cache.lock().expect("template cache mutex").get(path) {
                if hit.mtime == mtime
                    && hit
                        .includes
                        .iter()
                        .all(|(partial, at)| include_mtime(partial) == *at)
                {
                    return Ok(hit.program.clone());
                }
            }
        }

        let source = std::fs::read_to_string(path).map_err(JhsError::Io)?;
        let mut includes = Vec::new();
        let resolved = self.resolve_includes(&source, 0, &mut 0, &mut includes)?;
        let program = parser::compile(&resolved, &self.options.tags);

        if self.options.cache {
            self.cache.lock().expect("template cache mutex").insert(
                path.to_path_buf(),
                CachedTemplate {
                    program: program.clone(),
                    mtime,
                    includes,
                },
            );
        }

        Ok(program)
    }

    /// Resolves standalone include blocks in `source` by embedding the
    /// referenced partials (read from the views directory) in their
    /// place, recursively.
    ///
    /// Only a code block whose whole body is a single `include("name")`
    /// call is resolved — the include must stand alone:
    ///
    /// ```text
    /// <?jhs include("partials/header") ?>
    /// ```
    ///
    /// Any other use (inside an expression, with extra statements) is
    /// left untouched and fails at execution time on the sandbox's
    /// descriptive `include` stub. The embedded partial shares the
    /// render's data — it is compiled into the same program — so
    /// `user`, `path`, `query`, `pages` … are available inside it.
    ///
    /// `budget` caps the total bytes of embedded sources and `tracked`
    /// collects the partial paths (for cache invalidation).
    fn resolve_includes(
        &self,
        source: &str,
        depth: usize,
        budget: &mut usize,
        tracked: &mut Vec<(PathBuf, Option<SystemTime>)>,
    ) -> Result<String, JhsError> {
        let tags = &self.options.tags;
        let mut out = String::with_capacity(source.len());
        let mut cursor = 0;

        while let Some(open_offset) = source[cursor..].find(tags.open_tag.as_str()) {
            let open_at = cursor + open_offset;
            let body_start = open_at + tags.open_tag.len();

            let Some(close_offset) = source[body_start..].find(tags.close_tag.as_str()) else {
                // Unclosed code tag: keep the remainder verbatim, exactly
                // like the parser would.
                out.push_str(&source[cursor..]);
                return Ok(out);
            };
            let close_at = body_start + close_offset;
            let body = &source[body_start..close_at];

            // Literal text between the previous tag and this one is kept,
            // exactly like the parser's own pass two.
            if open_at > cursor {
                out.push_str(&source[cursor..open_at]);
            }

            if let Some(name) = standalone_include(body) {
                let partial = self.read_include(name, depth, budget, tracked)?;
                out.push_str(&partial);
            } else {
                out.push_str(&source[open_at..close_at + tags.close_tag.len()]);
            }

            cursor = close_at + tags.close_tag.len();
        }

        out.push_str(&source[cursor..]);
        Ok(out)
    }

    /// Reads the partial named `name` from the views directory,
    /// resolving its own includes one level deeper.
    fn read_include(
        &self,
        name: &str,
        depth: usize,
        budget: &mut usize,
        tracked: &mut Vec<(PathBuf, Option<SystemTime>)>,
    ) -> Result<String, JhsError> {
        if depth >= INCLUDE_MAX_DEPTH {
            return Err(JhsError::Include(format!(
                "`{name}` exceeds the maximum include nesting depth ({INCLUDE_MAX_DEPTH})"
            )));
        }

        let path = include_path(&self.options.views_path, name)?;
        let mtime = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok();
        let source = std::fs::read_to_string(&path).map_err(|error| {
            JhsError::Include(format!(
                "`{name}` could not be read from the views directory ({error})"
            ))
        })?;

        *budget += source.len();
        if *budget > INCLUDE_MAX_BYTES {
            return Err(JhsError::Include(format!(
                "embedding `{name}` exceeds the total include size budget \
                 ({INCLUDE_MAX_BYTES} bytes)"
            )));
        }

        tracked.push((path, mtime));
        self.resolve_includes(&source, depth + 1, budget, tracked)
    }

    /// Runs `program` in a fresh sandbox with `data` injected.
    fn execute(
        &self,
        program: &str,
        data: &Map<String, Value>,
        path: &str,
    ) -> Result<RenderOutput, JhsError> {
        let mut context = Context::default();
        context
            .runtime_limits_mut()
            .set_loop_iteration_limit(self.options.loop_iteration_limit);

        if self.options.require.enabled {
            let source_cache = self.options.cache.then(|| Arc::clone(&self.module_sources));
            require_bridge::install(&mut context, &self.options.require, source_cache.as_ref());
        }

        eval(
            &mut context,
            &prelude(self.options.auto_escape, self.options.require.enabled),
            path,
        )?;
        let injection = data_injection(data);
        if !injection.is_empty() {
            eval(&mut context, &injection, path)?;
        }
        let result = eval(&mut context, program, path)?;
        let console = console_lines(&mut context, path)?;
        let redirect = redirect_intent(&mut context, path)?;
        let html = result_value(result, &mut context, path)?;

        Ok(RenderOutput {
            html,
            console,
            redirect,
        })
    }
}

/// Evaluates `source`, wrapping any engine error with the original
/// engine's message format.
fn eval(context: &mut Context, source: &str, path: &str) -> Result<JsValue, JhsError> {
    context
        .eval(Source::from_bytes(source))
        .map_err(|error| execution_error(path, &error.to_string()))
}

/// Builds the `Template execution error` variant.
fn execution_error(path: &str, message: &str) -> JhsError {
    JhsError::Execution {
        path: path.to_owned(),
        message: format!("Template execution error ({path}): {message}"),
    }
}

/// Parses the include target of a **standalone** code block: the whole
/// body must be a single `include("name")` (or single-quoted) call,
/// optionally surrounded by whitespace.
///
/// Returns the raw name; validation happens in [`include_path`].
fn standalone_include(body: &str) -> Option<&str> {
    let body = body.trim();
    let rest = body.strip_prefix("include")?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('(')?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('"').or_else(|| rest.strip_prefix('\''))?;
    let end = rest.find(['"', '\''])?;
    let name = &rest[..end];
    // Everything after the closing quote must be `)` (with whitespace).
    let tail = rest[end + 1..].trim();
    if tail != ")" || name.is_empty() {
        return None;
    }
    Some(name)
}

/// Builds the path of the partial `name` inside `views_dir`, rejecting
/// every shape that could escape it.
///
/// Valid names are relative paths made of `[A-Za-z0-9_-]` segments
/// joined by `/`, without a leading or trailing slash, with at most
/// [`INCLUDE_MAX_NAME_LEN`] characters; a `.jhs` extension is optional
/// and always normalised onto the resolved path.
fn include_path(views_dir: &Path, name: &str) -> Result<PathBuf, JhsError> {
    // The `.jhs` extension is optional and normalised away before the
    // name is validated.
    let base = name.strip_suffix(".jhs").unwrap_or(name);

    let invalid = |reason: &str| {
        JhsError::Include(format!("`{name}` is not a valid include name ({reason})"))
    };

    if base.len() > INCLUDE_MAX_NAME_LEN {
        return Err(invalid("too long"));
    }
    if base.is_empty() {
        return Err(invalid("empty name"));
    }
    if base.contains('\\')
        || base.starts_with('/')
        || base.ends_with('/')
        || base.contains("//")
        || base
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '/'))
    {
        return Err(invalid(
            "expected relative `[A-Za-z0-9_-]` segments joined by `/`",
        ));
    }
    if base
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(invalid("`..` and `.` segments are not allowed"));
    }

    Ok(views_dir.join(format!("{base}.jhs")))
}

/// The current mtime of a partial, when it can be read.
fn include_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

/// Builds the sandbox prelude: hidden sentinel, escapers, captured
/// console, the `res` shim and — while `require` is enabled — the
/// public `require` wrapper and the module cache.
fn prelude(auto_escape: bool, require_enabled: bool) -> String {
    let auto = if auto_escape { "true" } else { "false" };
    let mut prelude = String::from(
        "(function(){\n\
         \x20 function RawString(str) { this.value = String(str); }\n\
         \x20 var __lines = [];\n\
         \x20 function capture(level) {\n\
         \x20   return function() {\n\
         \x20     var parts = [];\n\
         \x20     for (var i = 0; i < arguments.length; i++) parts.push(String(arguments[i]));\n\
         \x20     __lines.push({ level: level, message: parts.join(' ') });\n\
         \x20   };\n\
         \x20 }\n\
         \x20 function escapeHtml(unsafe) {\n\
         \x20   if (unsafe === null || unsafe === undefined) return '';\n\
         \x20   return String(unsafe)\n\
         \x20     .replace(/&/g, '&amp;')\n\
         \x20     .replace(/</g, '&lt;')\n\
         \x20     .replace(/>/g, '&gt;')\n\
         \x20     .replace(/\"/g, '&quot;')\n\
         \x20     .replace(/'/g, '&#039;');\n\
         \x20 }\n\
         \x20 var AUTO = ",
    ) + auto
        + ";\n\
     \x20 globalThis.__escape = AUTO\n\
     \x20   ? function(val) { return val instanceof RawString ? val.value : escapeHtml(val); }\n\
     \x20   : function(str) { return str instanceof RawString ? str.value : str; };\n\
     \x20 globalThis.escapeHtml = escapeHtml;\n\
     \x20 globalThis.raw = function(str) { return new RawString(str); };\n\
     \x20 globalThis.__jhsEchoPart = function(arg) {\n\
     \x20   return arg instanceof RawString ? arg.value : __escape(String(arg));\n\
     \x20 };\n\
     \x20 globalThis.include = function(name) {\n\
     \x20   throw new Error('include() is resolved at compile time and must stand alone in its own tag: <?jhs include(\"name\") ?>');\n\
     \x20 };\n\
     \x20 globalThis.console = {\n\
     \x20   log: capture('log'),\n\
     \x20   info: capture('info'),\n\
     \x20   warn: capture('warn'),\n\
     \x20   error: capture('error'),\n\
     \x20   debug: capture('debug'),\n\
     \x20   trace: capture('trace')\n\
     \x20 };\n\
     \x20 globalThis.__jhsConsoleLines = function() { return JSON.stringify(__lines); };\n\
     \x20 globalThis.__jhsRedirectIntent = null;\n\
     \x20 Object.defineProperty(globalThis, 'res', {\n\
     \x20   value: {\n\
     \x20     redirect: function(location, status) {\n\
     \x20       if (typeof location !== 'string' || location.length === 0) {\n\
     \x20         throw new TypeError('res.redirect(location) expects a target path string');\n\
     \x20       }\n\
     \x20       if (location.charAt(0) !== '/' || location.indexOf('//') === 0) {\n\
     \x20         throw new Error(\"res.redirect() only accepts local paths starting with '/' (open-redirect protection)\");\n\
     \x20       }\n\
     \x20       var code = status === undefined ? 302 : status;\n\
     \x20       if ([301, 302, 303, 307, 308].indexOf(code) === -1) {\n\
     \x20         throw new Error('res.redirect() status must be one of 301, 302, 303, 307 or 308');\n\
     \x20       }\n\
     \x20       if (globalThis.__jhsRedirectIntent === null) {\n\
     \x20         globalThis.__jhsRedirectIntent = { location: location, status: code };\n\
     \x20       }\n\
     \x20     }\n\
     \x20   },\n\
     \x20   writable: false, configurable: false, enumerable: false\n\
     \x20 });\n";

    if require_enabled {
        // The JavaScript half of the require() bridge: the native
        // `__jhsResolve` (banner, resolution, sandbox checks, source)
        // hands back a plain descriptor, and this loader owns the
        // registry and evaluates bodies through `new Function` — the
        // body is a *parameter*, so a module cannot break out of its
        // wrapper, and the closure is created on the ordinary compiler
        // path (a nested `Context::eval` inside the native call leaves
        // fresh closures un-callable).
        prelude.push_str(
            "     \x20 globalThis.__jhsModuleCache = {};\n\
             \x20 globalThis.__jhsModuleLoad = function (spec, base) {\n\
             \x20   var descriptor = globalThis.__jhsResolve(spec, base);\n\
             \x20   if (descriptor.crypto) {\n\
             \x20     return globalThis.__jhsModuleCache[descriptor.key];\n\
             \x20   }\n\
             \x20   var cache = globalThis.__jhsModuleCache;\n\
             \x20   if (Object.prototype.hasOwnProperty.call(cache, descriptor.key)) {\n\
             \x20     return cache[descriptor.key].exports;\n\
             \x20   }\n\
             \x20   var module = { exports: {} };\n\
             \x20   cache[descriptor.key] = module;\n\
             \x20   var Wrapper;\n\
             \x20   try {\n\
             \x20     Wrapper = new Function('exports', 'require', 'module', '__filename', '__dirname', descriptor.source);\n\
             \x20   } catch (err) {\n\
             \x20     throw new Error(\"module '\" + descriptor.file + \"' failed to load: \" + (err && err.message ? err.message : err));\n\
             \x20   }\n\
             \x20   Wrapper(module.exports, function (spec) {\n\
             \x20     return globalThis.__jhsModuleLoad(spec, descriptor.base);\n\
             \x20   }, module, descriptor.file, descriptor.dir);\n\
             \x20   return module.exports;\n\
             \x20 };\n\
             \x20 globalThis.require = function (spec) {\n\
             \x20   if (typeof spec !== 'string') {\n\
             \x20     throw new TypeError('require() expects a module name string (got ' + typeof spec + ')');\n\
             \x20   }\n\
             \x20   return globalThis.__jhsModuleLoad(spec, '');\n\
             \x20 };\n\
             ",
        );
    }

    prelude.push_str("     })();");
    prelude
}

/// Builds the data injection: `JSON.parse` plus `Object.defineProperty`
/// (own properties only; a `__proto__` key cannot reach the prototype).
///
/// Keys colliding with the sandbox helpers are skipped, matching the
/// original engine where the helpers are assigned after the data spread.
fn data_injection(data: &Map<String, Value>) -> String {
    let injectable: Map<String, Value> = data
        .iter()
        .filter(|(key, _)| !PROTECTED_GLOBALS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if injectable.is_empty() {
        return String::new();
    }
    let json = Value::Object(injectable).to_string();
    format!(
        "(function(){{\n\
         \x20 var data = JSON.parse({json_literal});\n\
         \x20 Object.keys(data).forEach(function(key){{\n\
         \x20   Object.defineProperty(globalThis, key, {{\n\
         \x20     value: data[key], writable: true, configurable: true, enumerable: true\n\
         \x20   }});\n\
         \x20 }});\n\
         }})();",
        json_literal = js_string_literal(&json)
    )
}

/// Reads the captured console lines back out of the sandbox.
fn console_lines(context: &mut Context, path: &str) -> Result<Vec<ConsoleLine>, JhsError> {
    let value = eval(context, "__jhsConsoleLines()", path)?;
    let text = value
        .as_string()
        .map(|string| string.to_std_string_escaped())
        .unwrap_or_else(|| String::from("[]"));
    serde_json::from_str(&text)
        .map_err(|error| execution_error(path, &format!("console capture failed: {error}")))
}

/// Reads the redirect recorded by `res.redirect()` back out of the
/// sandbox, if the template issued one.
fn redirect_intent(context: &mut Context, path: &str) -> Result<Option<RedirectIntent>, JhsError> {
    let value = eval(
        context,
        "(function(){ var intent = globalThis.__jhsRedirectIntent;\n\
         \x20 return intent === null ? '' : JSON.stringify(intent); })()",
        path,
    )?;
    let text = value
        .as_string()
        .map(|string| string.to_std_string_escaped())
        .unwrap_or_default();
    if text.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| execution_error(path, &format!("redirect capture failed: {error}")))
}

/// Converts the program result to the final HTML string.
///
/// Strings are returned as-is, `undefined` renders as the empty string
/// (a bare top-level `return;`) and every other value goes through
/// JavaScript's `String()` conversion.
fn result_value(value: JsValue, context: &mut Context, path: &str) -> Result<String, JhsError> {
    if value.is_undefined() {
        return Ok(String::new());
    }
    if let Some(string) = value.as_string() {
        return Ok(string.to_std_string_escaped());
    }
    value
        .to_string(context)
        .map(|string| string.to_std_string_escaped())
        .map_err(|error| execution_error(path, &error.to_string()))
}

/// Escapes `text` into a double-quoted JavaScript string literal.
fn js_string_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn engine() -> JhsEngine {
        JhsEngine::new(JhsOptions {
            cache: false,
            ..JhsOptions::default()
        })
    }

    fn engine_no_escape() -> JhsEngine {
        JhsEngine::new(JhsOptions {
            cache: false,
            auto_escape: false,
            ..JhsOptions::default()
        })
    }

    fn data(value: Value) -> Map<String, Value> {
        value.as_object().expect("object data").clone()
    }

    fn render(template: &str) -> String {
        engine()
            .render_string(template, &Map::new())
            .expect("renders")
            .html
    }

    // ── Compile-time include() ─────────────────────────────────────────

    /// A fixture views tree with a partial, rebuilt per test.
    struct IncludeFixture {
        dir: PathBuf,
    }

    impl IncludeFixture {
        fn create(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "wallermax-jhs-include-{}-{tag}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("partials")).expect("fixture dirs");
            std::fs::write(dir.join("partials/header.jhs"), "[hola <?= user ?>]")
                .expect("partial write");
            Self { dir }
        }

        fn engine(&self) -> JhsEngine {
            JhsEngine::new(JhsOptions {
                views_path: self.dir.clone(),
                cache: false,
                ..JhsOptions::default()
            })
        }

        fn write(&self, path: &str, source: &str) {
            std::fs::write(self.dir.join(path), source).expect("fixture write");
        }
    }

    impl Drop for IncludeFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn standalone_include_blocks_embed_the_partial() {
        let fixture = IncludeFixture::create("embed");
        fixture.write("main.jhs", "A<?jhs include(\"partials/header\") ?>B");
        let output = fixture
            .engine()
            .render("main.jhs", &data(json!({ "user": "ana" })))
            .expect("renders");
        assert_eq!(output.html, "A[hola ana]B");
    }

    #[test]
    fn includes_resolve_from_raw_strings_too() {
        let fixture = IncludeFixture::create("string");
        let output = fixture
            .engine()
            .render_string(
                "<?jhs include(\"partials/header.jhs\") ?>!",
                &data(json!({ "user": "ana" })),
            )
            .expect("renders");
        assert_eq!(output.html, "[hola ana]!");
    }

    #[test]
    fn nested_includes_resolve_recursively() {
        // Include names always resolve from the views root, regardless
        // of the partial that embeds them.
        let fixture = IncludeFixture::create("nested");
        fixture.write(
            "partials/outer.jhs",
            "<out><?jhs include(\"partials/header\") ?></out>",
        );
        fixture.write("main.jhs", "X<?jhs include(\"partials/outer\") ?>Y");
        let output = fixture
            .engine()
            .render("main.jhs", &data(json!({ "user": "bo" })))
            .expect("renders");
        assert_eq!(output.html, "X<out>[hola bo]</out>Y");
    }

    #[test]
    fn include_needs_its_own_tag() {
        // `echo(include(...))` is not standalone: the sandbox stub throws
        // with the descriptive message.
        let fixture = IncludeFixture::create("nonstandalone");
        let error = fixture
            .engine()
            .render_string("<?jhs echo(include(\"partials/header\")); ?>", &Map::new())
            .expect_err("must fail");
        assert!(error.to_string().contains("must stand alone"), "{error}");
    }

    #[test]
    fn missing_partials_report_include_errors() {
        let fixture = IncludeFixture::create("missing");
        let error = fixture
            .engine()
            .render_string("<?jhs include(\"nope\") ?>", &Map::new())
            .expect_err("must fail");
        assert!(
            error.to_string().contains("template include error"),
            "{error}"
        );
    }

    #[test]
    fn traversal_names_are_rejected() {
        let fixture = IncludeFixture::create("traversal");
        for name in [
            "../secret",
            "partials/../../x",
            "/abs",
            "a\\b",
            "a//b",
            "a/./b",
        ] {
            let error = fixture
                .engine()
                .render_string(&format!("<?jhs include(\"{name}\") ?>"), &Map::new())
                .expect_err("must fail");
            assert!(
                error.to_string().contains("not a valid include name"),
                "{name}: {error}"
            );
        }
    }

    #[test]
    fn include_depth_is_bounded() {
        let fixture = IncludeFixture::create("depth");
        // A partial that includes itself: 8 levels deep, then the error.
        fixture.write("loop.jhs", "L<?jhs include(\"loop\") ?>");
        let error = fixture
            .engine()
            .render_string("<?jhs include(\"loop\") ?>", &Map::new())
            .expect_err("must fail");
        assert!(error.to_string().contains("nesting depth"), "{error}");
    }

    #[test]
    fn editing_a_partial_recompiles_its_includers() {
        let fixture = IncludeFixture::create("reload");
        fixture.write("main.jhs", "A<?jhs include(\"partials/header\") ?>B");

        let engine = JhsEngine::new(JhsOptions {
            views_path: fixture.dir.clone(),
            cache: true,
            ..JhsOptions::default()
        });

        let first = engine
            .render("main.jhs", &data(json!({ "user": "bo" })))
            .expect("renders");
        assert_eq!(first.html, "A[hola bo]B");

        // Ensure a distinct mtime so the invalidation triggers.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fixture.write("partials/header.jhs", "[cambiado]");

        let second = engine.render("main.jhs", &Map::new()).expect("renders");
        assert_eq!(second.html, "A[cambiado]B");
    }

    #[test]
    fn echo_raw_prints_trusted_markup() {
        // v0.8.0: the function form matches the documented `<?= raw() ?>`
        // semantics (the sentinel survives `echo`'s stringification).
        let out = engine()
            .render_string("<?jhs echo(raw(\"<b>negrita</b>\")); ?>", &Map::new())
            .expect("renders");
        assert_eq!(out.html, "<b>negrita</b>");
    }

    // ── Ported 1:1 from node-jhs2's test suite ──────────────────────────

    #[test]
    fn renders_static_html_unchanged() {
        assert_eq!(render("<h1>Hello</h1>"), "<h1>Hello</h1>");
    }

    #[test]
    fn renders_echo_tag_with_variable() {
        let out = engine()
            .render_string("<p><?= name ?></p>", &data(json!({"name": "World"})))
            .expect("renders");
        assert_eq!(out.html, "<p>World</p>");
    }

    #[test]
    fn auto_escapes_html_in_echo_tag() {
        let out = engine()
            .render_string(
                "<?= val ?>",
                &data(json!({"val": "<script>alert(1)</script>"})),
            )
            .expect("renders");
        assert_eq!(out.html, "&lt;script&gt;alert(1)&lt;/script&gt;");
    }

    #[test]
    fn raw_bypasses_auto_escape() {
        let out = engine()
            .render_string("<?= raw(val) ?>", &data(json!({"val": "<b>bold</b>"})))
            .expect("renders");
        assert_eq!(out.html, "<b>bold</b>");
    }

    #[test]
    fn executes_code_block_if_else() {
        let template = "<?jhs if (x > 0) { ?>positive<?jhs } else { ?>non-positive<?jhs } ?>";
        let positive = engine()
            .render_string(template, &data(json!({"x": 5})))
            .expect("renders");
        let negative = engine()
            .render_string(template, &data(json!({"x": -1})))
            .expect("renders");
        assert_eq!(positive.html.trim(), "positive");
        assert_eq!(negative.html.trim(), "non-positive");
    }

    #[test]
    fn executes_foreach_loop() {
        let template = "<?jhs items.forEach(i => { ?><?= i ?>,<?jhs }); ?>";
        let out = engine()
            .render_string(template, &data(json!({"items": ["a", "b", "c"]})))
            .expect("renders");
        assert_eq!(out.html, "a,b,c,");
    }

    #[test]
    fn echo_function_outputs_escaped_content() {
        let out = engine()
            .render_string("<?jhs echo(msg); ?>", &data(json!({"msg": "<b>test</b>"})))
            .expect("renders");
        assert_eq!(out.html, "&lt;b&gt;test&lt;/b&gt;");
    }

    #[test]
    fn handles_null_in_echo_tag_gracefully() {
        let out = engine()
            .render_string("<?= val ?>", &data(json!({"val": null})))
            .expect("renders");
        assert_eq!(out.html, "");
    }

    #[test]
    fn blocks_banned_module_vm() {
        // 'vm' ships in the default banner, so the call throws the
        // forbidden-module error like the original's filter did.
        let error = engine()
            .render_string("<?jhs require(\"vm\"); ?>", &Map::new())
            .expect_err("must fail");
        assert!(error.to_string().contains("forbidden"), "{error}");
    }

    #[test]
    fn blocks_banned_module_jhs() {
        let error = engine()
            .render_string("<?jhs require(\"jhs\"); ?>", &Map::new())
            .expect_err("must fail");
        assert!(error.to_string().contains("forbidden"), "{error}");
    }

    #[test]
    fn clear_cache_empties_the_cache() {
        let cached = JhsEngine::new(JhsOptions::default());
        cached.cache.lock().expect("mutex").insert(
            PathBuf::from("test-key"),
            CachedTemplate {
                program: String::from("compiled"),
                mtime: None,
                includes: Vec::new(),
            },
        );
        assert_eq!(cached.cache.lock().expect("mutex").len(), 1);
        cached.clear_cache();
        assert_eq!(cached.cache.lock().expect("mutex").len(), 0);
    }

    #[test]
    fn autoescape_false_does_not_escape_output() {
        let out = engine_no_escape()
            .render_string("<?= val ?>", &data(json!({"val": "<b>bold</b>"})))
            .expect("renders");
        assert_eq!(out.html, "<b>bold</b>");
    }

    // ── Extension tests: quirks, sandbox and robustness ─────────────────

    #[test]
    fn undeclared_variables_throw_reference_error() {
        let result = engine().render_string("<?= nope ?>", &Map::new());
        assert!(result.is_err());
    }

    #[test]
    fn runtime_errors_are_wrapped_with_template_name() {
        let error = engine()
            .render_string("<?jhs null.x; ?>", &Map::new())
            .expect_err("throws");
        assert!(error.to_string().contains("Template execution error"));
        assert!(error.to_string().contains("null"));
    }

    #[test]
    fn infinite_loops_throw_instead_of_hanging() {
        let limited = JhsEngine::new(JhsOptions {
            cache: false,
            loop_iteration_limit: 1_000,
            ..JhsOptions::default()
        });
        let result = limited.render_string("<?jhs while (true) { } ?>", &Map::new());
        assert!(result.is_err());
    }

    #[test]
    fn render_missing_file_reports_io_error() {
        let error = engine()
            .render("does-not-exist.jhs", &Map::new())
            .expect_err("missing file");
        assert!(matches!(error, JhsError::Io(_)));
    }

    #[test]
    fn render_reloads_when_the_mtime_changes() {
        let dir = std::env::temp_dir().join(format!(
            "wallermax-jhs-engine-{}-reload",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let file = dir.join("cached.jhs");
        std::fs::write(&file, "v1").expect("fixture write");

        let engine = JhsEngine::new(JhsOptions {
            views_path: dir.clone(),
            cache: true,
            ..JhsOptions::default()
        });

        let first = engine.render("cached.jhs", &Map::new()).expect("renders");
        assert_eq!(first.html, "v1");
        assert!(engine
            .cache
            .lock()
            .expect("mutex")
            .get(&dir.join("cached.jhs"))
            .is_some_and(|hit| hit.program.contains("__output += \"v1\";")));

        // Give the mtime time to move, then rewrite and render again.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&file, "v2 <?= 40 + 2 ?>").expect("fixture rewrite");
        let second = engine.render("cached.jhs", &Map::new()).expect("renders");
        assert_eq!(second.html, "v2 42");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn console_output_is_captured() {
        let out = engine()
            .render_string("<?jhs console.log(\"line\"); ?>html", &Map::new())
            .expect("renders");
        assert_eq!(out.html, "html");
        assert_eq!(
            out.console,
            vec![ConsoleLine {
                level: String::from("log"),
                message: String::from("line")
            }]
        );
    }

    #[test]
    fn echo_null_follows_the_original_quirk() {
        // echo() stringifies *before* escaping, so null prints as "null".
        let out = engine()
            .render_string("<?jhs echo(null); ?>", &Map::new())
            .expect("renders");
        assert_eq!(out.html, "null");
    }

    #[test]
    fn autoescape_off_concatenates_null_verbatim() {
        // __escape is the identity when auto-escape is off, so the +=
        // coercion prints "null".
        let out = engine_no_escape()
            .render_string("<?= val ?>", &data(json!({"val": null})))
            .expect("renders");
        assert_eq!(out.html, "null");
    }

    #[test]
    fn autoescape_off_undefined_echo_tag() {
        let template = "<?jhs var u; ?>a<?= u ?>b";
        let escaped = engine()
            .render_string(template, &Map::new())
            .expect("renders");
        assert_eq!(escaped.html, "ab");

        let raw = engine_no_escape()
            .render_string(template, &Map::new())
            .expect("renders");
        assert_eq!(raw.html, "aundefinedb");
    }

    #[test]
    fn numbers_render_as_javascript_numbers() {
        let out = engine()
            .render_string("<?= 40 + 2 ?> / <?= 0.1 + 0.2 ?>", &Map::new())
            .expect("renders");
        assert_eq!(out.html, "42 / 0.30000000000000004");
    }

    #[test]
    fn top_level_return_hijacks_the_output() {
        let out = engine()
            .render_string("head<?jhs return \"ignored\"; ?>tail", &Map::new())
            .expect("renders");
        assert_eq!(out.html, "ignored");
    }

    #[test]
    fn bare_top_level_return_renders_empty() {
        let out = engine()
            .render_string("head<?jhs return; ?>tail", &Map::new())
            .expect("renders");
        assert_eq!(out.html, "");
    }

    #[test]
    fn literal_text_is_never_escaped() {
        let out = engine()
            .render_string(
                "<b>literal</b><?= val ?>",
                &data(json!({"val": "<i>dyn</i>"})),
            )
            .expect("renders");
        assert_eq!(out.html, "<b>literal</b>&lt;i&gt;dyn&lt;/i&gt;");
    }

    #[test]
    fn nested_data_objects_and_arrays() {
        let out = engine()
            .render_string(
                "<?= user.name ?> / <?= user.tags[1] ?>",
                &data(json!({"user": {"name": "Ana", "tags": ["a", "b"]}})),
            )
            .expect("renders");
        assert_eq!(out.html, "Ana / b");
    }

    #[test]
    fn user_object_drives_conditional_rendering() {
        // The shape the template middleware injects: a `user` global with
        // id/username/role, or null for anonymous visitors.
        let template = "<?jhs if (user && user.role == 'admin') { ?>\
             <p>Bienvenido, administrador <?= user.username ?></p>\
             <?jhs } else if (user) { ?>\
             <p>Hola <?= user.username ?> (<?= user.role ?>)</p>\
             <?jhs } else { ?>\
             <p>por favor, inicia sesión</p>\
             <?jhs } ?>";

        let admin = engine()
            .render_string(
                template,
                &data(json!({"user": {"id": 1, "username": "justo", "role": "admin"}})),
            )
            .expect("renders");
        assert_eq!(admin.html, "<p>Bienvenido, administrador justo</p>");

        let member = engine()
            .render_string(
                template,
                &data(json!({"user": {"id": 2, "username": "ana", "role": "user"}})),
            )
            .expect("renders");
        assert_eq!(member.html, "<p>Hola ana (user)</p>");

        let anonymous = engine()
            .render_string(template, &data(json!({"user": null})))
            .expect("renders");
        assert_eq!(anonymous.html, "<p>por favor, inicia sesión</p>");
    }

    #[test]
    fn variadic_echo_joins_arguments() {
        let out = engine()
            .render_string("<?jhs echo(\"a\", \"b\", 3); ?>", &Map::new())
            .expect("renders");
        assert_eq!(out.html, "ab3");
    }

    #[test]
    fn unclosed_tags_render_literally() {
        assert_eq!(render("a<?jhs if (true) { b"), "a<?jhs if (true) { b");
        assert_eq!(render("a<?= name b"), "a<?= name b");
    }

    #[test]
    fn xml_prolog_passes_through() {
        assert_eq!(
            render("<?xml version=\"1.0\"?><p>ok</p>"),
            "<?xml version=\"1.0\"?><p>ok</p>"
        );
    }

    #[test]
    fn empty_echo_tag_renders_empty() {
        assert_eq!(render("a<?= ?>b"), "ab");
    }

    #[test]
    fn sandbox_exposes_no_host_apis() {
        // Buffer/include/process/fs/timers must all stay undefined (the
        // `require` bridge itself is covered by its own test module).
        let template = "<?jhs \
             var missing = []; \
             ['Buffer', 'include', 'process', 'fs', 'setTimeout'].forEach(function(name){ \
               if (typeof globalThis[name] !== 'undefined') missing.push(name); \
             }); \
             missing.join(',') ?>";
        let out = engine()
            .render_string(template, &Map::new())
            .expect("renders");
        assert_eq!(out.html, "", "leaked globals: {}", out.html);
    }

    #[test]
    fn require_is_installed_but_shadowable_by_neither_data_nor_code() {
        // With the default options the bridge is present and functional,
        // and data keys cannot replace it with something else.
        let out = engine()
            .render_string(
                "<?= typeof require ?>",
                &data(json!({"require": "overwritten"})),
            )
            .expect("renders");
        assert_eq!(out.html, "function");
    }

    #[test]
    fn require_disabled_leaves_require_undefined() {
        let disabled = JhsEngine::new(JhsOptions {
            cache: false,
            require: crate::template_engine::RequireOptions {
                enabled: false,
                ..crate::template_engine::RequireOptions::default()
            },
            ..JhsOptions::default()
        });
        let out = disabled
            .render_string("<?= typeof require ?>", &Map::new())
            .expect("renders");
        assert_eq!(out.html, "undefined");
    }

    // ── res.redirect() (v0.9.0) ─────────────────────────────────────

    #[test]
    fn res_redirect_records_a_default_302_intent() {
        let output = engine()
            .render_string(
                "<?jhs res.redirect('/login'); return; ?>secreto",
                &Map::new(),
            )
            .expect("renders");
        assert_eq!(
            output.redirect,
            Some(RedirectIntent {
                location: String::from("/login"),
                status: 302,
            })
        );
        // A bare top-level `return;` hijacks the output (the preserved
        // quirk), so the HTML is empty; the route layer answers the
        // redirect intent instead.
        assert_eq!(output.html, "");
    }

    #[test]
    fn res_redirect_accepts_explicit_statuses() {
        for (code, expected) in [(301, 301u16), (303, 303), (307, 307), (308, 308)] {
            let output = engine()
                .render_string(
                    &format!("<?jhs res.redirect('/x', {code}); ?>"),
                    &Map::new(),
                )
                .expect("renders");
            assert_eq!(output.redirect.map(|intent| intent.status), Some(expected));
        }
    }

    #[test]
    fn res_redirect_rejects_open_redirect_targets() {
        for target in [
            "https://evil.example",
            "http://evil.example",
            "//evil.example",
            "relative/path",
            "",
        ] {
            let error = engine()
                .render_string(&format!("<?jhs res.redirect('{target}'); ?>"), &Map::new())
                .expect_err("must fail");
            assert!(
                error.to_string().contains("local paths")
                    || error.to_string().contains("target path"),
                "{target}: {error}"
            );
        }
    }

    #[test]
    fn res_redirect_rejects_unknown_statuses() {
        let error = engine()
            .render_string("<?jhs res.redirect('/x', 200); ?>", &Map::new())
            .expect_err("must fail");
        assert!(error.to_string().contains("status"), "{error}");

        let error = engine()
            .render_string("<?jhs res.redirect('/x', 999); ?>", &Map::new())
            .expect_err("must fail");
        assert!(error.to_string().contains("status"), "{error}");
    }

    #[test]
    fn res_redirect_first_call_wins() {
        let output = engine()
            .render_string(
                "<?jhs res.redirect('/first'); res.redirect('/second', 301); ?>",
                &Map::new(),
            )
            .expect("renders");
        assert_eq!(
            output.redirect,
            Some(RedirectIntent {
                location: String::from("/first"),
                status: 302,
            })
        );
    }

    #[test]
    fn res_cannot_be_shadowed_by_data() {
        let out = engine()
            .render_string(
                "<?= typeof res.redirect ?>",
                &data(json!({"res": {"redirect": "fake"}})),
            )
            .expect("renders");
        assert_eq!(out.html, "function");
    }

    #[test]
    fn res_is_present_even_with_require_disabled() {
        let disabled = JhsEngine::new(JhsOptions {
            cache: false,
            require: crate::template_engine::RequireOptions {
                enabled: false,
                ..crate::template_engine::RequireOptions::default()
            },
            ..JhsOptions::default()
        });
        let output = disabled
            .render_string("<?jhs res.redirect('/login'); ?>", &Map::new())
            .expect("renders");
        assert_eq!(
            output.redirect,
            Some(RedirectIntent {
                location: String::from("/login"),
                status: 302,
            })
        );
    }

    #[test]
    fn data_keys_cannot_reach_the_global_prototype() {
        // A __proto__ data key becomes an own global variable, never the
        // prototype of the global object.
        let probe = engine()
            .render_string(
                "<?= ({}).polluted === undefined ?>",
                &data(json!({"__proto__": {"polluted": "yes"}})),
            )
            .expect("renders");
        assert_eq!(probe.html, "true");
    }

    #[test]
    fn data_keys_cannot_shadow_the_sandbox_helpers() {
        // The original engine assigns helpers after the data spread, so
        // data may never override them; the port skips those keys.
        let out = engine()
            .render_string(
                "<?= val ?>",
                &data(json!({"val": "<b>", "__escape": "overwritten", "console": null})),
            )
            .expect("renders");
        assert_eq!(out.html, "&lt;b&gt;");
    }

    #[test]
    fn fresh_context_per_render_shares_no_state() {
        let set = "<?jhs globalThis.leak = 42; ?>done";
        let read = "<?= typeof globalThis.leak ?>";

        assert_eq!(render(set), "done");
        assert_eq!(render(read), "undefined");
    }

    #[test]
    fn unicode_text_and_emoji_pass_through() {
        assert_eq!(render("café áéíóú 中文 😀"), "café áéíóú 中文 😀");
    }

    #[test]
    fn quoted_strings_inside_echo_tags() {
        let out = engine()
            .render_string("<?jhs var s = \"a\\\"b\"; ?><?= s ?>", &Map::new())
            .expect("renders");
        assert_eq!(out.html, "a&quot;b");
    }

    #[test]
    fn hidden_sentinel_is_not_reachable() {
        // RawString lives inside the prelude closure; typeof never throws.
        let out = engine()
            .render_string("<?= typeof RawString ?>", &Map::new())
            .expect("renders");
        assert_eq!(out.html, "undefined");
    }

    #[test]
    fn engine_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<JhsEngine>();
    }

    #[test]
    fn options_are_exposed() {
        let engine = JhsEngine::new(JhsOptions {
            loop_iteration_limit: 77,
            ..JhsOptions::default()
        });
        assert_eq!(engine.options().loop_iteration_limit, 77);
        assert!(engine.options().cache);
    }
}
