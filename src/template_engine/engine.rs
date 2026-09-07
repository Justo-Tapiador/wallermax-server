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
//! `escapeHtml`, `raw`, `console`, `JSON`) are ignored, mirroring the
//! original engine where the helpers are assigned **after** the data
//! spread and therefore always win.
//!
//! Nothing else is exposed: no `require`, no `Buffer`, no `include`, no
//! file system, no timers, no network. A loop iteration limit (boa's
//! runtime limits) bounds runaway loops — the original Node engine
//! hangs forever on `<?jhs while(true){} ?>` because its `vm` timeout
//! cannot interrupt a tight loop.
//!
//! The cache keeps compiled programs keyed by path and invalidated by
//! mtime, so editing a template takes effect on the next request
//! without a restart (the original engine caches forever).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use boa_engine::{Context, JsValue, Source};
use serde_json::{Map, Value};

use super::parser::{self, TagOptions};

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
}

impl Default for JhsOptions {
    fn default() -> Self {
        Self {
            views_path: PathBuf::from("views"),
            cache: true,
            auto_escape: true,
            tags: TagOptions::default(),
            loop_iteration_limit: 10_000_000,
        }
    }
}

/// Data keys that may not shadow the sandbox helpers.
const PROTECTED_GLOBALS: [&str; 6] = [
    "__escape",
    "escapeHtml",
    "raw",
    "console",
    "JSON",
    "__jhsConsoleLines",
];

/// Error produced while rendering a template.
#[derive(Debug)]
pub enum JhsError {
    /// The template file could not be read.
    Io(std::io::Error),
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
            JhsError::Execution { message, .. } => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for JhsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            JhsError::Io(error) => Some(error),
            JhsError::Execution { .. } => None,
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
/// to the (captured) console.
#[derive(Debug, Clone, Default)]
pub struct RenderOutput {
    /// Rendered HTML.
    pub html: String,
    /// Console lines captured during execution.
    pub console: Vec<ConsoleLine>,
}

/// The `.jhs` template engine. Cheap to share: rendering takes `&self`,
/// the compiled-template cache sits behind a mutex and every render
/// builds a fresh sandbox.
#[derive(Debug)]
pub struct JhsEngine {
    options: JhsOptions,
    cache: Mutex<HashMap<PathBuf, CachedTemplate>>,
}

#[derive(Debug, Clone)]
struct CachedTemplate {
    program: String,
    mtime: Option<SystemTime>,
}

impl JhsEngine {
    /// Creates an engine from the given options.
    pub fn new(options: JhsOptions) -> Self {
        Self {
            options,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The configured options.
    pub fn options(&self) -> &JhsOptions {
        &self.options
    }

    /// Renders the template at `template_path` (absolute, or relative to
    /// the configured views path) with the given data.
    ///
    /// # Errors
    ///
    /// Returns [`JhsError::Io`] when the file cannot be read and
    /// [`JhsError::Execution`] when the template throws.
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

    /// Renders a template from a raw string (labelled `<string>`).
    ///
    /// # Errors
    ///
    /// Returns [`JhsError::Execution`] when the template throws.
    pub fn render_string(
        &self,
        template: &str,
        data: &Map<String, Value>,
    ) -> Result<RenderOutput, JhsError> {
        let program = parser::compile(template, &self.options.tags);
        self.execute(&program, data, "<string>")
    }

    /// Empties the compiled-template cache.
    pub fn clear_cache(&self) {
        self.cache.lock().expect("template cache mutex").clear();
    }

    /// Loads (and caches) the compiled program for `path`.
    fn load_program(&self, path: &Path) -> Result<String, JhsError> {
        let mtime = std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok();

        if self.options.cache {
            if let Some(hit) = self.cache.lock().expect("template cache mutex").get(path) {
                if hit.mtime == mtime {
                    return Ok(hit.program.clone());
                }
            }
        }

        let source = std::fs::read_to_string(path).map_err(JhsError::Io)?;
        let program = parser::compile(&source, &self.options.tags);

        if self.options.cache {
            self.cache.lock().expect("template cache mutex").insert(
                path.to_path_buf(),
                CachedTemplate {
                    program: program.clone(),
                    mtime,
                },
            );
        }

        Ok(program)
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

        eval(&mut context, &prelude(self.options.auto_escape), path)?;
        let injection = data_injection(data);
        if !injection.is_empty() {
            eval(&mut context, &injection, path)?;
        }
        let result = eval(&mut context, program, path)?;
        let console = console_lines(&mut context, path)?;
        let html = result_value(result, &mut context, path)?;

        Ok(RenderOutput { html, console })
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

/// Builds the sandbox prelude: hidden sentinel, escapers, captured console.
fn prelude(auto_escape: bool) -> String {
    let auto = if auto_escape { "true" } else { "false" };
    String::from(
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
     \x20 globalThis.console = {\n\
     \x20   log: capture('log'),\n\
     \x20   info: capture('info'),\n\
     \x20   warn: capture('warn'),\n\
     \x20   error: capture('error'),\n\
     \x20   debug: capture('debug'),\n\
     \x20   trace: capture('trace')\n\
     \x20 };\n\
     \x20 globalThis.__jhsConsoleLines = function() { return JSON.stringify(__lines); };\n\
     })();"
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
        // No `require` exists in the sandbox at all: the call throws, like
        // the original's banned-module filter.
        let result = engine().render_string("<?jhs require(\"vm\"); ?>", &Map::new());
        assert!(result.is_err());
    }

    #[test]
    fn blocks_banned_module_jhs() {
        let result = engine().render_string("<?jhs require(\"jhs\"); ?>", &Map::new());
        assert!(result.is_err());
    }

    #[test]
    fn clear_cache_empties_the_cache() {
        let cached = JhsEngine::new(JhsOptions::default());
        cached.cache.lock().expect("mutex").insert(
            PathBuf::from("test-key"),
            CachedTemplate {
                program: String::from("compiled"),
                mtime: None,
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
        // require/Buffer/include/process/fs/timers must all be undefined.
        let template = "<?jhs \
             var missing = []; \
             ['require', 'Buffer', 'include', 'process', 'fs', 'setTimeout'].forEach(function(name){ \
               if (typeof globalThis[name] !== 'undefined') missing.push(name); \
             }); \
             missing.join(',') ?>";
        let out = engine()
            .render_string(template, &Map::new())
            .expect("renders");
        assert_eq!(out.html, "", "leaked globals: {}", out.html);
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
