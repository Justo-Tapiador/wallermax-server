//! `require()` bridge: CommonJS-style module loading for the sandbox
//! (v0.9.0).
//!
//! The original `node-jhs2` templates imported modules with Node's own
//! `require`, guarded by a small **banner** (`banned_require`, default
//! `['vm', 'jhs']`) that rejected a configurable list of module names.
//! This port runs on boa_engine — there is no Node runtime behind the
//! sandbox — so the banner concept is kept and the module loader is
//! rebuilt on top of two safe sources:
//!
//! 1. **Native polyfills** implemented in Rust (currently `crypto` with
//!    `randomBytes().toString(enc)` and `randomUUID()`), registered as
//!    builtin modules.
//! 2. **Local JavaScript modules** under the configured modules
//!    directory (default `modules/`), loaded with a CommonJS wrapper
//!    (`exports`, `require`, `module`, `__filename`, `__dirname`) and
//!    evaluated in the same sandbox. Any pure-JS module works — its
//!    own files, or npm packages copied into the modules directory —
//!    as long as it only uses ECMAScript and the sandbox globals
//!    (`JSON`, `Math`, `Date`, `console`, …). Modules that need Node
//!    APIs (`fs`, `Buffer`, `process`, native addons) cannot work here
//!    and fail with descriptive errors.
//!
//! Resolution order for a specifier, mirroring Node:
//!
//! 1. the **banner** (`[templates] forbidden_modules`) — checked first,
//!    against the package name (first path segment), so `mv`,
//!    `mv/sub` and `node:mv` are all rejected. The banner wins over
//!    everything, including polyfills;
//! 2. **builtin modules**: polyfilled builtins (`crypto`) return the
//!    native module; every other Node builtin throws a descriptive
//!    "no Node runtime behind the sandbox" error;
//! 3. **local files**: bare names resolve under the modules directory
//!    (then under `modules/node_modules/`); `./` and `../` names
//!    resolve against the requiring file's directory. Every candidate
//!    is canonicalised and must stay **under the modules directory** —
//!    `require('../../etc/passwd')` is a sandbox error, not a file read.
//!
//! The loader is split in two halves on purpose:
//!
//! - the **native** `__jhsResolve(spec, baseDir)` does everything that
//!   needs Rust: banner, resolution, sandbox prefix check, source
//!   reading (mtime-cached at the engine level) and the size budget.
//!   It only ever returns plain data objects — no functions — so no
//!   GC-sensitive value crosses the native boundary.
//! - the **JavaScript** `__jhsModuleLoad` (installed by the sandbox
//!   prelude) owns the module registry and evaluates module bodies
//!   through `new Function('exports', 'require', 'module',
//!   '__filename', '__dirname', source)`. The body is a *parameter* of
//!   the Function constructor, never concatenated into evaluable
//!   source, so a module cannot break out of its wrapper; and the
//!   closure is created by the ordinary compiler path — a nested
//!   `Context::eval` inside a native call reenters boa's `run()` loop
//!   and leaves freshly created closures un-callable, which
//!   `new Function` avoids entirely.
//!
//! Module instances are cached **per render** on the sandbox's global
//! `__jhsModuleCache` (the module object registered *before* the body
//! runs, exactly like Node's circular-require semantics). Guards:
//! total embedded module bytes per render, plus boa's recursion limit
//! for the loader's own nesting.
//!
//! `res` and `require` are installed as protected globals: render data
//! can never shadow them.

// The crate denies `unsafe` everywhere; this module is the one
// contained exception. Every block below is a
// `NativeFunction::from_closure` call whose safety contract is that
// the closure captures **no traceable GC data** — only Rust-owned
// types — so the collector can never free something the closure
// still points at. Each call documents its captures next to it.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use boa_engine::{
    js_string, Context, JsError, JsNativeError, JsObject, JsResult, JsString, JsValue,
    NativeFunction,
};

use base64::Engine as _;
use rand_core::{OsRng, RngCore};

/// Default banner: the module names rejected by `require()` even when
/// nothing else would stop them. Mirrors the original engine's
/// `banned_require` (which shipped `vm` and `jhs`) hardened with the
/// Node builtins that can touch the host: file system, processes,
/// networking, threads and introspection. Extend or trim it through
/// `[templates] forbidden_modules` — it is the administrator's list.
pub const DEFAULT_FORBIDDEN_MODULES: [&str; 19] = [
    "child_process",
    "cluster",
    "dgram",
    "dns",
    "fs",
    "http",
    "https",
    "inspector",
    "jhs",
    "mv",
    "net",
    "os",
    "process",
    "repl",
    "tls",
    "tty",
    "v8",
    "vm",
    "worker_threads",
];

/// Node's builtin module names. Behind boa there is nothing to load
/// for them, so requiring one without a native polyfill throws the
/// descriptive "no Node runtime" error instead of a plain miss.
const BUILTIN_MODULES: &[&str] = &[
    "assert",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "domain",
    "events",
    "fs",
    "http",
    "http2",
    "https",
    "inspector",
    "module",
    "net",
    "os",
    "path",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "repl",
    "stream",
    "string_decoder",
    "sys",
    "test",
    "timers",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
];

/// Registry key of the `crypto` polyfill inside `__jhsModuleCache`.
/// Angle brackets cannot appear in a resolved file path, so builtin
/// entries can never collide with module files.
const CRYPTO_MODULE_KEY: &str = "<builtin>crypto";

/// Maximum total bytes of module source loaded during one render.
const REQUIRE_MAX_TOTAL_BYTES: usize = 8 * 1024 * 1024;

/// Maximum `crypto.randomBytes(size)`.
const MAX_RANDOM_BYTES: usize = 65_536;

/// Sandbox and policy options for the `require()` bridge (the
/// `[templates]` section).
#[derive(Debug, Clone)]
pub struct RequireOptions {
    /// Installs `require()` (and the module machinery) in the sandbox.
    /// While `false`, `require` stays undefined like in v0.8.x.
    pub enabled: bool,
    /// Directory local JavaScript modules resolve under (the original
    /// has no equivalent: Node's `require` resolved against the
    /// process). Relative paths resolve against the working directory
    /// at engine construction, exactly like `views_path`.
    pub modules_dir: PathBuf,
    /// The banner: module names (package names) `require()` rejects
    /// with a descriptive error. Checked before anything else, so an
    /// entry can also ban a polyfilled builtin or a local module.
    pub forbidden: Vec<String>,
}

impl Default for RequireOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            modules_dir: PathBuf::from("modules"),
            forbidden: DEFAULT_FORBIDDEN_MODULES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
        }
    }
}

/// A cached module source with its mtime (invalidation works exactly
/// like the compiled-template cache).
#[derive(Debug)]
pub(super) struct ModuleSource {
    pub(super) source: String,
    pub(super) mtime: Option<SystemTime>,
}

/// Per-render loader state captured by the native resolver.
struct RequireState {
    modules_root: PathBuf,
    forbidden: HashSet<String>,
    source_cache: Option<Arc<Mutex<HashMap<PathBuf, ModuleSource>>>>,
    budget: RefCell<usize>,
}

/// Installs the native `__jhsResolve(spec, baseDir)` function in the
/// context. The JavaScript half of the loader — `require` itself and
/// `__jhsModuleLoad` — is installed by the sandbox prelude (see
/// [`crate::template_engine::engine::prelude`]).
///
/// `source_cache` is the engine-level module source cache; pass `None`
/// to read from disk on every load (the `cache = false` mode).
pub(super) fn install(
    context: &mut Context,
    options: &RequireOptions,
    source_cache: Option<&Arc<Mutex<HashMap<PathBuf, ModuleSource>>>>,
) {
    let state = Rc::new(RequireState {
        modules_root: options.modules_dir.clone(),
        forbidden: options.forbidden.iter().cloned().collect(),
        source_cache: source_cache.cloned(),
        budget: RefCell::new(0),
    });

    // SAFETY: the closure captures an `Rc<RequireState>` holding only
    // non-traceable Rust data (paths, strings, mutexes, refcells) and
    // no GC references (`JsValue`, `JsObject`, `Gc`, …), so the
    // garbage collector can never collect something the closure still
    // points at.
    let native = unsafe {
        NativeFunction::from_closure({
            let state = Rc::clone(&state);
            move |this, args, context| resolve_impl(&state, this, args, context)
        })
    };

    context
        .register_global_builtin_callable(js_string!("__jhsResolve"), 2, native)
        .expect("registering __jhsResolve cannot fail");
}

/// The native `__jhsResolve(spec, baseDir)` implementation: everything
/// that needs Rust, returning a plain data descriptor — `{ crypto:
/// true, key }` for the polyfill, or `{ key, file, dir, base, source }`
/// for a local module.
fn resolve_impl(
    state: &Rc<RequireState>,
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let spec = match args.first().and_then(JsValue::as_string) {
        Some(spec) => spec.to_std_string_escaped(),
        None => {
            let kind = args.first().map(JsValue::type_of).unwrap_or("undefined");
            return Err(type_error(format!(
                "require() expects a module name string (got {kind})"
            )));
        }
    };

    if spec.contains('\0') {
        return Err(type_error(format!(
            "require() module name cannot contain NUL characters: {spec:?}"
        )));
    }
    if spec.contains('\\') {
        return Err(type_error(format!(
            "require('{spec}') is invalid: module names must use forward slashes"
        )));
    }
    let name = spec.trim().strip_prefix("node:").unwrap_or(spec.trim());
    if name.is_empty() {
        return Err(type_error(
            "require() expects a non-empty module name".to_owned(),
        ));
    }

    // 1. The banner wins over everything (polyfills and files).
    let package = name.split('/').next().unwrap_or_default();
    if state.forbidden.contains(package) {
        return Err(error(format!(
            "require('{spec}') is forbidden: the module '{package}' is listed in \
             [templates] forbidden_modules"
        )));
    }

    // 2. Builtin modules: polyfill or descriptive miss.
    if BUILTIN_MODULES.contains(&package) {
        if package == "crypto" {
            // The polyfill object is stored on the (rooted) module
            // registry; the descriptor tells the JS loader where.
            crypto_module(context)?;
            let descriptor = JsObject::with_object_proto(context.intrinsics());
            descriptor.set(js_string!("crypto"), JsValue::from(true), false, context)?;
            descriptor.set(
                js_string!("key"),
                JsValue::from(JsString::from(CRYPTO_MODULE_KEY)),
                false,
                context,
            )?;
            return Ok(descriptor.into());
        }
        return Err(error(format!(
            "Cannot find module '{spec}': '{package}' is a Node.js built-in and the JHS \
             sandbox runs on boa_engine, not Node — built-ins exist here only as native \
             polyfills (currently: crypto). Plain JS modules under the modules directory \
             can be required instead"
        )));
    }

    // 3. Local JavaScript modules: resolve, sandbox-check, read.
    resolve_local_module(state, &spec, name, args, context)
}

/// Resolution step 3: find, validate and read a local module, then
/// hand a data descriptor to the JavaScript loader.
fn resolve_local_module(
    state: &Rc<RequireState>,
    spec: &str,
    name: &str,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let root = state.modules_root.clone();
    let root_canonical = root.canonicalize().map_err(|_| {
        error(format!(
            "Cannot find module '{spec}': the modules directory {} does not exist \
             (see [templates] modules_dir)",
            root.display()
        ))
    })?;

    let base_dir = args
        .get(1)
        .and_then(JsValue::as_string)
        .map(|base| base.to_std_string_escaped())
        .filter(|base| !base.is_empty())
        .map(|base| root.join(base))
        .unwrap_or_else(|| root.clone());

    let relative = name.starts_with("./") || name.starts_with("../");
    let search_roots: [PathBuf; 2] = if relative {
        [base_dir, root.join("node_modules")]
    } else {
        [root.clone(), root.join("node_modules")]
    };

    let mut tried: Vec<String> = Vec::new();
    for search_root in &search_roots {
        let target = search_root.join(name);
        if let Some(candidate) = resolve_module_file(&target) {
            let canonical = candidate.canonicalize().map_err(|_| {
                error(format!(
                    "require('{spec}'): the module file {} could not be resolved",
                    candidate.display()
                ))
            })?;
            if !canonical.starts_with(&root_canonical) {
                return Err(error(format!(
                    "require('{spec}') escapes the modules directory: modules must live \
                     under {} (see [templates] modules_dir)",
                    root.display()
                )));
            }
            return module_descriptor(state, spec, &canonical, &candidate, context);
        }
        tried.push(format!(
            "{}/{name}(, .js, /index.js)",
            search_root.display()
        ));
    }

    Err(error(format!(
        "Cannot find module '{spec}': looked under the modules directory {} (tried {})",
        root.display(),
        tried.join("; ")
    )))
}

/// Reads the module source (with the mtime budget) and builds the
/// plain-data descriptor the JS loader consumes.
fn module_descriptor(
    state: &Rc<RequireState>,
    spec: &str,
    canonical: &Path,
    display_path: &Path,
    context: &mut Context,
) -> JsResult<JsValue> {
    let source = read_source(state, canonical)?;
    *state.budget.borrow_mut() += source.len();
    if *state.budget.borrow() > REQUIRE_MAX_TOTAL_BYTES {
        return Err(error(format!(
            "module loading exceeds the total size budget ({REQUIRE_MAX_TOTAL_BYTES} bytes) — \
             reduce the modules the template requires (while requiring '{spec}')"
        )));
    }

    // The module's directory, relative to the modules root, is the
    // base for its own relative requires.
    let root_canonical = state
        .modules_root
        .canonicalize()
        .unwrap_or_else(|_| state.modules_root.clone());
    let base = canonical
        .parent()
        .and_then(|dir| dir.strip_prefix(&root_canonical).ok())
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();

    let key = canonical.to_string_lossy().into_owned();
    let file = display_path.display().to_string();
    let dir = display_path
        .parent()
        .map(|dir| dir.display().to_string())
        .unwrap_or_default();

    let descriptor = JsObject::with_object_proto(context.intrinsics());
    descriptor.set(
        js_string!("key"),
        JsValue::from(JsString::from(key.as_str())),
        false,
        context,
    )?;
    descriptor.set(
        js_string!("file"),
        JsValue::from(JsString::from(file.as_str())),
        false,
        context,
    )?;
    descriptor.set(
        js_string!("dir"),
        JsValue::from(JsString::from(dir.as_str())),
        false,
        context,
    )?;
    descriptor.set(
        js_string!("base"),
        JsValue::from(JsString::from(base.as_str())),
        false,
        context,
    )?;
    descriptor.set(
        js_string!("source"),
        JsValue::from(JsString::from(source)),
        false,
        context,
    )?;
    Ok(descriptor.into())
}

/// Resolves the concrete module file for a joined path, mirroring
/// Node's lookup ladder: exact file (with a `.js`/`.json` extension),
/// `<name>.js`, `<name>/index.js`, then `<name>/package.json`'s
/// `main` field.
fn resolve_module_file(target: &Path) -> Option<PathBuf> {
    if target.is_file() {
        return Some(target.to_path_buf());
    }

    let file_name = target.file_name()?.to_string_lossy().into_owned();
    let as_js = target.with_file_name(format!("{file_name}.js"));
    if as_js.is_file() {
        return Some(as_js);
    }

    if target.is_dir() {
        let index = target.join("index.js");
        if index.is_file() {
            return Some(index);
        }
        let manifest = target.join("package.json");
        if manifest.is_file() {
            if let Some(main) = read_package_main(&manifest) {
                let main_path = target.join(main);
                if main_path.is_file() {
                    return Some(main_path);
                }
                let main_name = main_path.file_name()?.to_string_lossy().into_owned();
                let main_as_js = main_path.with_file_name(format!("{main_name}.js"));
                if main_as_js.is_file() {
                    return Some(main_as_js);
                }
                let main_index = main_path.join("index.js");
                if main_index.is_file() {
                    return Some(main_index);
                }
            }
        }
    }

    None
}

/// Reads the `main` field of a `package.json` manifest, when present
/// and sane.
fn read_package_main(manifest: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(manifest).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let main = value.get("main")?.as_str()?.to_owned();
    if main.is_empty() || main.contains("..") || main.contains('\\') {
        return None;
    }
    Some(main)
}

/// Reads a module source, honouring the engine-level mtime cache.
fn read_source(state: &Rc<RequireState>, path: &Path) -> Result<String, JsError> {
    let mtime = std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok();

    if let Some(cache) = &state.source_cache {
        let mut guard = cache.lock().expect("module source cache mutex");
        if let Some(hit) = guard.get(path) {
            if hit.mtime == mtime {
                return Ok(hit.source.clone());
            }
        }
        let source = read_file(path)?;
        guard.insert(
            path.to_path_buf(),
            ModuleSource {
                source: source.clone(),
                mtime,
            },
        );
        Ok(source)
    } else {
        read_file(path)
    }
}

fn read_file(path: &Path) -> Result<String, JsError> {
    std::fs::read_to_string(path).map_err(|failure| {
        error(format!(
            "module '{}' could not be read ({failure})",
            path.display()
        ))
    })
}

/// The `__jhsModuleCache` object on the global.
fn module_registry(context: &mut Context) -> JsResult<JsObject> {
    let registry = context
        .global_object()
        .get(js_string!("__jhsModuleCache"), context)?;
    registry
        .as_object()
        .ok_or_else(|| error("the module cache is unavailable".to_owned()))
}

/// The `crypto` polyfill: `randomBytes(size).toString(enc)` and
/// `randomUUID()`. Cached per render on the module registry.
fn crypto_module(context: &mut Context) -> JsResult<()> {
    let registry = module_registry(context)?;
    let cached = registry.get(JsString::from(CRYPTO_MODULE_KEY), context)?;
    if cached.is_object() {
        return Ok(());
    }

    let crypto = JsObject::with_object_proto(context.intrinsics());

    // SAFETY: the closure captures nothing.
    let random_bytes = unsafe {
        NativeFunction::from_closure(|_this, args, context| random_bytes_impl(args, context))
    }
    .to_js_function(context.realm());
    crypto.set(js_string!("randomBytes"), random_bytes, false, context)?;

    // SAFETY: the closure captures nothing.
    let random_uuid = unsafe {
        NativeFunction::from_closure(|_this, _args, _context| {
            Ok(JsValue::from(JsString::from(
                uuid::Uuid::new_v4().to_string(),
            )))
        })
    }
    .to_js_function(context.realm());
    crypto.set(js_string!("randomUUID"), random_uuid, false, context)?;

    registry.set(
        JsString::from(CRYPTO_MODULE_KEY),
        JsValue::from(crypto.clone()),
        false,
        context,
    )?;
    Ok(())
}

/// `crypto.randomBytes(size)` → a byte object with `length` and
/// `toString(encoding)`.
fn random_bytes_impl(args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let requested = args
        .first()
        .cloned()
        .unwrap_or(JsValue::undefined())
        .to_number(context)?;

    if !requested.is_finite()
        || requested.fract() != 0.0
        || !(0.0..=MAX_RANDOM_BYTES as f64).contains(&requested)
    {
        return Err(type_error(format!(
            "crypto.randomBytes(size) expects an integer between 0 and {MAX_RANDOM_BYTES} \
             (got {requested})"
        )));
    }

    let mut bytes = vec![0u8; requested as usize];
    OsRng.fill_bytes(&mut bytes);

    let object = JsObject::with_object_proto(context.intrinsics());
    object.set(
        js_string!("length"),
        JsValue::from(bytes.len() as f64),
        false,
        context,
    )?;

    // SAFETY: the closure captures only the `Vec<u8>` of random bytes
    // — non-traceable Rust data, no GC references.
    let to_string = unsafe {
        NativeFunction::from_closure(move |_this, args, _context| bytes_to_string(&bytes, args))
    }
    .to_js_function(context.realm());
    object.set(js_string!("toString"), to_string, false, context)?;

    Ok(object.into())
}

/// `randomBytes().toString(encoding)` — supported encodings: `hex`,
/// `base64`, `utf8` (the Node default), `latin1`/`binary`.
fn bytes_to_string(bytes: &[u8], args: &[JsValue]) -> JsResult<JsValue> {
    let encoding = match args.first() {
        None => String::from("utf8"),
        Some(value) if value.is_undefined() || value.is_null() => String::from("utf8"),
        Some(value) => value
            .as_string()
            .map(|encoding| encoding.to_std_string_escaped())
            .ok_or_else(|| {
                type_error("randomBytes().toString(encoding) expects a string encoding".to_owned())
            })?,
    };

    let encoded = match encoding.as_str() {
        "hex" => bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
        "base64" => base64::engine::general_purpose::STANDARD.encode(bytes),
        "utf8" | "utf-8" => String::from_utf8_lossy(bytes).into_owned(),
        "latin1" | "binary" => bytes.iter().map(|&byte| byte as char).collect(),
        other => {
            return Err(type_error(format!(
                "randomBytes().toString(): unsupported encoding '{other}' (supported: hex, \
                 base64, utf8, latin1)"
            )));
        }
    };
    Ok(JsValue::from(JsString::from(encoded)))
}

/// A plain `Error` with `message`.
fn error(message: String) -> JsError {
    JsError::from_native(JsNativeError::error().with_message(message))
}

/// A `TypeError` with `message`.
fn type_error(message: String) -> JsError {
    JsError::from_native(JsNativeError::typ().with_message(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template_engine::{JhsEngine, JhsOptions};
    use serde_json::Map;

    /// A fixture modules directory, rebuilt per test.
    struct ModulesFixture {
        dir: PathBuf,
    }

    impl ModulesFixture {
        fn create(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "wallermax-jhs-require-{tag}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("fixture dir");
            Self { dir }
        }

        fn engine(&self) -> JhsEngine {
            self.engine_with(RequireOptions::default())
        }

        fn engine_with(&self, require: RequireOptions) -> JhsEngine {
            JhsEngine::new(JhsOptions {
                cache: false,
                require: RequireOptions {
                    modules_dir: self.dir.clone(),
                    ..require
                },
                ..JhsOptions::default()
            })
        }

        fn write(&self, relative: &str, source: &str) {
            let path = self.dir.join(relative);
            std::fs::create_dir_all(path.parent().expect("parent dir")).expect("fixture dirs");
            std::fs::write(path, source).expect("fixture write");
        }

        fn render(&self, template: &str) -> String {
            self.engine()
                .render_string(template, &Map::new())
                .expect("renders")
                .html
        }

        fn render_error(&self, template: &str) -> String {
            self.engine()
                .render_string(template, &Map::new())
                .expect_err("must fail")
                .to_string()
        }
    }

    impl Drop for ModulesFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    // ── The crypto polyfill ──────────────────────────────────────────

    #[test]
    fn random_bytes_hex_and_uniqueness() {
        let fixture = ModulesFixture::create("hex");
        let first = fixture.render(
            "<?jhs var c = require('crypto'); \
             var s = c.randomBytes(16).toString('hex'); \
             echo(s.length + ':' + /^[0-9a-f]+$/.test(s)); ?>",
        );
        assert_eq!(first, "32:true");

        let second = fixture.render("<?= require('crypto').randomBytes(16).toString('hex') ?>");
        assert_eq!(second.len(), 32);
        assert_ne!(first, second, "two CSPRNG draws must differ");
    }

    #[test]
    fn random_bytes_encodings_and_length() {
        let fixture = ModulesFixture::create("enc");
        assert_eq!(
            fixture.render("<?= require('crypto').randomBytes(0).toString('hex') ?>"),
            ""
        );
        assert_eq!(
            fixture.render("<?= require('crypto').randomBytes(0).toString('base64') ?>"),
            ""
        );

        // 8 bytes → 12 base64 characters (padding included).
        let base64 = fixture.render("<?= require('crypto').randomBytes(8).toString('base64') ?>");
        assert_eq!(base64.len(), 12, "base64: {base64}");

        let latin1 = fixture.render(
            "<?= require('crypto').randomBytes(8).toString('latin1').length === 8 ? 'ok' : 'bad' ?>",
        );
        assert_eq!(latin1, "ok");

        let length = fixture.render("<?= require('crypto').randomBytes(8).length ?>");
        assert_eq!(length, "8");

        // Node's default encoding is utf8.
        let utf8 = fixture.render("<?= require('crypto').randomBytes(4).toString() ?>");
        assert!(!utf8.is_empty());
        let utf8_alias =
            fixture.render("<?= require('crypto').randomBytes(4).toString('utf-8') ?>");
        assert!(!utf8_alias.is_empty());
    }

    #[test]
    fn random_bytes_rejects_bad_sizes_and_encodings() {
        let fixture = ModulesFixture::create("sizes");
        for template in [
            "<?= require('crypto').randomBytes(-1) ?>",
            "<?= require('crypto').randomBytes(1.5) ?>",
            "<?= require('crypto').randomBytes(65537) ?>",
            "<?= require('crypto').randomBytes('x') ?>",
            "<?= require('crypto').randomBytes(4).toString('rot13') ?>",
        ] {
            let error = fixture.render_error(template);
            assert!(error.contains("randomBytes"), "{template}: {error}");
        }
    }

    #[test]
    fn random_uuid_shape() {
        let fixture = ModulesFixture::create("uuid");
        let uuid = fixture.render("<?= require('crypto').randomUUID() ?>");
        assert_eq!(uuid.len(), 36, "uuid: {uuid}");
        let chars: Vec<char> = uuid.chars().collect();
        for dash_at in [8, 13, 18, 23] {
            assert_eq!(chars[dash_at], '-', "uuid: {uuid}");
        }
        assert_eq!(chars[14], '4', "uuid version nibble: {uuid}");
        assert!(
            ['8', '9', 'a', 'b'].contains(&chars[19]),
            "uuid variant nibble: {uuid}"
        );

        let other = fixture.render("<?= require('crypto').randomUUID() ?>");
        assert_ne!(uuid, other, "two v4 UUIDs must differ");
    }

    // ── The banner ───────────────────────────────────────────────────

    #[test]
    fn default_banner_rejects_dangerous_modules() {
        let fixture = ModulesFixture::create("banner");
        for spec in [
            "fs",
            "mv",
            "vm",
            "jhs",
            "child_process",
            "net",
            "os",
            "http",
        ] {
            let error = fixture.render_error(&format!("<?jhs require('{spec}'); ?>"));
            assert!(
                error.contains("forbidden") && error.contains(spec),
                "{spec}: {error}"
            );
        }
    }

    #[test]
    fn banner_covers_node_prefix_and_subpaths() {
        let fixture = ModulesFixture::create("banner-prefix");
        let error = fixture.render_error("<?jhs require('node:fs'); ?>");
        assert!(error.contains("forbidden"), "{error}");

        let error = fixture.render_error("<?jhs require('mv/sub'); ?>");
        assert!(error.contains("forbidden"), "{error}");
    }

    #[test]
    fn banner_can_be_trimmed_and_blocks_even_polyfills() {
        let fixture = ModulesFixture::create("banner-custom");
        // No banner at all: 'net' is still a builtin without a polyfill,
        // so the error becomes the descriptive "no Node runtime" one.
        let engine = fixture.engine_with(RequireOptions {
            forbidden: Vec::new(),
            ..RequireOptions::default()
        });
        let error = engine
            .render_string("<?jhs require('net'); ?>", &Map::new())
            .expect_err("must fail")
            .to_string();
        assert!(
            error.contains("Node.js built-in") && error.contains("boa_engine"),
            "{error}"
        );

        // Banning 'crypto' beats the polyfill.
        let engine = fixture.engine_with(RequireOptions {
            forbidden: vec![String::from("crypto")],
            ..RequireOptions::default()
        });
        let error = engine
            .render_string("<?jhs require('crypto'); ?>", &Map::new())
            .expect_err("must fail")
            .to_string();
        assert!(error.contains("forbidden"), "{error}");
    }

    #[test]
    fn unknown_builtins_fail_descriptively() {
        let fixture = ModulesFixture::create("builtin");
        let engine = fixture.engine_with(RequireOptions {
            forbidden: Vec::new(),
            ..RequireOptions::default()
        });
        for spec in ["buffer", "path", "events", "zlib"] {
            let error = engine
                .render_string(&format!("<?jhs require('{spec}'); ?>"), &Map::new())
                .expect_err("must fail")
                .to_string();
            assert!(error.contains("Node.js built-in"), "{spec}: {error}");
        }
    }

    // ── Local modules ────────────────────────────────────────────────

    #[test]
    fn local_module_resolves_by_bare_name_and_dot_slash() {
        let fixture = ModulesFixture::create("local");
        fixture.write(
            "greeting.js",
            "module.exports = { hello: function (who) { return 'hola ' + who; } };",
        );
        assert_eq!(
            fixture.render("<?= require('greeting').hello('jhs') ?>"),
            "hola jhs"
        );
        assert_eq!(
            fixture.render("<?= require('./greeting').hello('rust') ?>"),
            "hola rust"
        );
    }

    #[test]
    fn exported_closures_keep_working_after_the_load() {
        // The regression that shaped the loader's design: functions
        // closing over module-level state, called after require()
        // returns. A nested Context::eval inside the native call left
        // these un-callable; `new Function` keeps them ordinary.
        let fixture = ModulesFixture::create("closures");
        fixture.write(
            "counter.js",
            "var count = 0;\nmodule.exports = function () { count += 1; return 'n' + count; };",
        );
        assert_eq!(
            fixture.render(
                "<?jhs var next = require('counter'); \
                 echo(next() + next() + next()); ?>"
            ),
            "n1n2n3"
        );
    }

    #[test]
    fn local_module_resolves_index_js_and_package_main() {
        let fixture = ModulesFixture::create("pkg");
        fixture.write("pkg/index.js", "module.exports = 'desde index';");
        assert_eq!(fixture.render("<?= require('pkg') ?>"), "desde index");

        fixture.write(
            "boxed/package.json",
            r#"{ "name": "boxed", "main": "./lib/entry.js" }"#,
        );
        fixture.write("boxed/lib/entry.js", "module.exports = 'desde main';");
        assert_eq!(fixture.render("<?= require('boxed') ?>"), "desde main");
    }

    #[test]
    fn modules_can_require_each_other_relatively() {
        let fixture = ModulesFixture::create("relative");
        fixture.write(
            "lib/a.js",
            "var b = require('./b');\nmodule.exports = 'a(' + b + ')';",
        );
        fixture.write("lib/b.js", "module.exports = 'b';");
        assert_eq!(fixture.render("<?= require('lib/a') ?>"), "a(b)");
    }

    #[test]
    fn module_scope_receives_the_commonjs_arguments() {
        let fixture = ModulesFixture::create("scope");
        fixture.write(
            "scope.js",
            "module.exports = [typeof exports, typeof require, \
             typeof module, __filename, __dirname].join('|');",
        );
        let out = fixture.render("<?= require('scope') ?>");
        assert!(out.starts_with("object|function|object|"), "scope: {out}");
        assert!(out.contains("scope.js"), "filename: {out}");
    }

    #[test]
    fn circular_requires_return_partial_exports() {
        let fixture = ModulesFixture::create("circular");
        fixture.write(
            "a.js",
            "exports.name = 'a';\nvar b = require('./b');\nexports.saw = b.name;",
        );
        fixture.write(
            "b.js",
            "exports.name = 'b';\nvar a = require('./a');\nexports.saw = a.name;",
        );
        // b sees a's partial exports: `saw` is still undefined there.
        assert_eq!(fixture.render("<?= require('a').saw ?>"), "b");
        assert_eq!(fixture.render("<?= require('b').saw ?>"), "a");
    }

    #[test]
    fn module_state_is_per_render() {
        let fixture = ModulesFixture::create("state");
        fixture.write(
            "counter.js",
            "globalThis.__n = (globalThis.__n || 0) + 1;\nmodule.exports = globalThis.__n;",
        );
        assert_eq!(fixture.render("<?= require('counter') ?>"), "1");
        assert_eq!(fixture.render("<?= require('counter') ?>"), "1");
    }

    #[test]
    fn repeated_requires_load_the_module_once_per_render() {
        let fixture = ModulesFixture::create("once");
        fixture.write(
            "once.js",
            "globalThis.__loads = (globalThis.__loads || 0) + 1;\n\
             module.exports = globalThis.__loads;",
        );
        assert_eq!(
            fixture.render("<?= require('once') ?><?= require('once') ?><?= require('./once') ?>"),
            "111"
        );
    }

    #[test]
    fn module_sources_reload_on_mtime_change() {
        let fixture = ModulesFixture::create("reload");
        fixture.write("value.js", "module.exports = 'v1';");

        let engine = JhsEngine::new(JhsOptions {
            cache: true,
            require: RequireOptions {
                modules_dir: fixture.dir.clone(),
                ..RequireOptions::default()
            },
            ..JhsOptions::default()
        });
        let first = engine
            .render_string("<?= require('value') ?>", &Map::new())
            .expect("renders");
        assert_eq!(first.html, "v1");

        std::thread::sleep(std::time::Duration::from_millis(20));
        fixture.write("value.js", "module.exports = 'v2';");
        let second = engine
            .render_string("<?= require('value') ?>", &Map::new())
            .expect("renders");
        assert_eq!(second.html, "v2");
    }

    // ── Sandbox and diagnostics ──────────────────────────────────────

    #[test]
    fn traversal_and_absolute_specs_are_rejected() {
        let fixture = ModulesFixture::create("traversal");
        fixture.write("keep.js", "module.exports = 'ok';");
        for template in [
            "<?jhs require('../outside'); ?>",
            "<?jhs require('../../etc/passwd'); ?>",
            "<?jhs require('/etc/passwd'); ?>",
            "<?jhs require('.\\\\..\\\\secret'); ?>",
        ] {
            let error = fixture.render_error(template);
            // Three safe outcomes: a descriptive miss, the backslash
            // rejection, or the sandbox-escape rejection when the target
            // exists outside the modules directory.
            assert!(
                error.contains("Cannot find module")
                    || error.contains("forward slashes")
                    || error.contains("escapes the modules directory"),
                "{template}: {error}"
            );
        }
    }

    #[test]
    fn modules_cannot_escape_through_relative_requires() {
        // modules root at <dir>/modules; the sneaky module climbs out
        // towards <dir>/secrets.js, which EXISTS — so the resolution
        // finds it and the sandbox prefix check must reject it.
        let dir = std::env::temp_dir().join(format!(
            "wallermax-jhs-require-escape-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("modules").join("deep")).expect("fixture dirs");
        std::fs::write(dir.join("secrets.js"), "module.exports = 'stolen';")
            .expect("fixture write");
        std::fs::write(
            dir.join("modules").join("deep").join("sneaky.js"),
            "module.exports = require('../../secrets');",
        )
        .expect("fixture write");

        let engine = JhsEngine::new(JhsOptions {
            cache: false,
            require: RequireOptions {
                modules_dir: dir.join("modules"),
                ..RequireOptions::default()
            },
            ..JhsOptions::default()
        });
        let error = engine
            .render_string("<?jhs require('deep/sneaky'); ?>", &Map::new())
            .expect_err("must fail")
            .to_string();
        assert!(error.contains("escapes the modules directory"), "{error}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn module_bodies_cannot_break_out_of_the_wrapper() {
        // A crafted source trying to close the wrapper early and run
        // in global scope: with `new Function` the body is a parameter,
        // never concatenated source, so the escape is impossible —
        // whatever the parse outcome, nothing reaches global scope.
        let fixture = ModulesFixture::create("inject");
        fixture.write(
            "sneaky.js",
            "}); globalThis.__escaped = true; (function () {",
        );
        let _ = fixture
            .engine()
            .render_string("<?jhs require('sneaky'); ?>", &Map::new());
        let probe = fixture.render("<?= typeof globalThis.__escaped ?>");
        assert_eq!(probe, "undefined");
    }

    #[test]
    fn missing_modules_list_the_searched_candidates() {
        let fixture = ModulesFixture::create("missing");
        let error = fixture.render_error("<?jhs require('nope'); ?>");
        assert!(error.contains("Cannot find module 'nope'"), "{error}");
        assert!(error.contains("index.js"), "{error}");
    }

    #[test]
    fn non_string_specifiers_throw_type_errors() {
        let fixture = ModulesFixture::create("types");
        let error = fixture.render_error("<?jhs require(123); ?>");
        assert!(error.contains("module name string"), "{error}");
        let error = fixture.render_error("<?jhs require(); ?>");
        assert!(error.contains("module name string"), "{error}");
    }

    #[test]
    fn module_syntax_errors_carry_the_file_name() {
        let fixture = ModulesFixture::create("syntax");
        fixture.write("broken.js", "this is not ] valid javascript");
        let error = fixture.render_error("<?jhs require('broken'); ?>");
        assert!(error.contains("broken.js"), "{error}");
        assert!(error.contains("failed to load"), "{error}");
    }

    #[test]
    fn deep_nesting_is_bounded_by_the_recursion_limit() {
        // The loader recurses through JS (`__jhsModuleLoad` → Wrapper →
        // `__jhsModuleLoad` → …), so boa's runtime recursion limit is
        // the nesting bound. 300 levels of two frames each comfortably
        // exceed it.
        let fixture = ModulesFixture::create("depth");
        for level in 1..=300 {
            fixture.write(
                &format!("chain{level}.js"),
                &format!("module.exports = require('./chain{}');", level + 1),
            );
        }
        fixture.write("chain301.js", "module.exports = 'end';");
        let error = fixture.render_error("<?jhs require('chain1'); ?>");
        assert!(
            error.contains("recursive") || error.contains("call stack"),
            "{error}"
        );
    }

    #[test]
    fn self_require_returns_partial_exports_like_node() {
        let fixture = ModulesFixture::create("self");
        fixture.write(
            "self.js",
            "exports.tag = 'partial';\nvar again = require('./self');\n\
             exports.confirmed = again === module.exports;",
        );
        assert_eq!(fixture.render("<?= require('self').tag ?>"), "partial");
        assert_eq!(fixture.render("<?= require('self').confirmed ?>"), "true");
    }

    #[test]
    fn require_works_from_cms_page_bodies() {
        // render_string is the CMS seam: stored page bodies see require
        // exactly like the views do.
        let fixture = ModulesFixture::create("cms");
        fixture.write(
            "slug.js",
            "module.exports = function (s) { return s.toUpperCase(); };",
        );
        let output = fixture
            .engine()
            .render_string("<?= require('slug')('cms') ?>", &Map::new())
            .expect("renders");
        assert_eq!(output.html, "CMS");
        assert!(output.redirect.is_none());
    }
}
