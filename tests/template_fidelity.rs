//! Fidelity battery: the Rust engine against the original
//! `node-jhs2` engine running on Node 24.
//!
//! Every expected value below was **recorded from the original
//! engine** (`scripts/gen_battery.mjs` drives
//! `upload/node-jhs2/index.js` and freezes its outputs into the
//! table). Error cases assert that both engines error out;
//! message wording is engine-specific (V8 vs boa) and therefore
//! not compared.

use serde_json::Value;
use wallermax_server::template_engine::{JhsEngine, JhsOptions};

/// (name, template, data, auto-escape off, mode, expected).
///
/// Modes: `exact` compares the whole output, `trim` compares
/// trimmed output, `error` expects a failure whose message keeps
/// the original `Template execution error` wrapping.
struct Case {
    name: &'static str,
    template: &'static str,
    data: &'static str,
    auto_escape_off: bool,
    mode: Mode,
    expected: &'static str,
}

enum Mode {
    Exact,
    Trim,
    Error,
}

const CASES: &[Case] = &[
    Case {
        name: "static_html",
        template: "<h1>Hello</h1>",
        data: "{}",
        auto_escape_off: false,
        mode: Mode::Exact,
        expected: "<h1>Hello</h1>",
    },
    Case {
        name: "echo_var",
        template: "<p><?= name ?></p>",
        data: "{\"name\":\"World\"}",
        auto_escape_off: false,
        mode: Mode::Exact,
        expected: "<p>World</p>",
    },
    Case {
        name: "auto_escape",
        template: "<?= val ?>",
        data: "{\"val\":\"<script>alert(1)</script>\"}",
        auto_escape_off: false,
        mode: Mode::Exact,
        expected: "&lt;script&gt;alert(1)&lt;/script&gt;",
    },
    Case {
        name: "raw_bypass",
        template: "<?= raw(val) ?>",
        data: "{\"val\":\"<b>bold</b>\"}",
        auto_escape_off: false,
        mode: Mode::Exact,
        expected: "<b>bold</b>",
    },
    Case {
        name: "if_else_pos",
        template: "<?jhs if (x > 0) { ?>positive<?jhs } else { ?>non-positive<?jhs } ?>",
        data: "{\"x\":5}",
        auto_escape_off: false,
        mode: Mode::Trim,
        expected: "positive",
    },
    Case {
        name: "if_else_neg",
        template: "<?jhs if (x > 0) { ?>positive<?jhs } else { ?>non-positive<?jhs } ?>",
        data: "{\"x\":-1}",
        auto_escape_off: false,
        mode: Mode::Trim,
        expected: "non-positive",
    },
    Case {
        name: "for_each",
        template: "<?jhs items.forEach(i => { ?><?= i ?>,<?jhs }); ?>",
        data: "{\"items\":[\"a\",\"b\",\"c\"]}",
        auto_escape_off: false,
        mode: Mode::Exact,
        expected: "a,b,c,",
    },
    Case {
        name: "echo_fn_escaped",
        template: "<?jhs echo(msg); ?>",
        data: "{\"msg\":\"<b>test</b>\"}",
        auto_escape_off: false,
        mode: Mode::Exact,
        expected: "&lt;b&gt;test&lt;/b&gt;",
    },
    Case {
        name: "null_graceful",
        template: "<?= val ?>",
        data: "{\"val\":null}",
        auto_escape_off: false,
        mode: Mode::Exact,
        expected: "",
    },
    Case {
        name: "top_level_return",
        template: "head<?jhs return \"ignored\"; ?>tail",
        data: "{}",
        auto_escape_off: false,
        mode: Mode::Exact,
        expected: "ignored",
    },
    Case {
        name: "echo_null_quirk",
        template: "<?= \"a\" ?><?jhs echo(null); ?>",
        data: "{}",
        auto_escape_off: false,
        mode: Mode::Exact,
        expected: "anull",
    },
    Case {
        name: "autoescape_off",
        template: "<?= val ?>",
        data: "{\"val\":\"<b>bold</b>\"}",
        auto_escape_off: true,
        mode: Mode::Exact,
        expected: "<b>bold</b>",
    },
    Case {
        name: "autoescape_off_null",
        template: "<?= \"a\" ?><?jhs __output += null; ?>",
        data: "{}",
        auto_escape_off: true,
        mode: Mode::Exact,
        expected: "anull",
    },
    Case {
        name: "autoescape_on_undefined",
        template: "<?jhs var u; ?>a<?= u ?>b",
        data: "{}",
        auto_escape_off: false,
        mode: Mode::Exact,
        expected: "ab",
    },
    Case {
        name: "autoescape_off_undefined",
        template: "<?jhs var u; ?>a<?= u ?>b",
        data: "{}",
        auto_escape_off: true,
        mode: Mode::Exact,
        expected: "aundefinedb",
    },
    Case {
        name: "unclosed_code",
        template: "a<?jhs if (true) { b",
        data: "{}",
        auto_escape_off: false,
        mode: Mode::Exact,
        expected: "a<?jhs if (true) { b",
    },
    Case {
        name: "syntax_error_case",
        template: "a<?jhs if (true) { ?>b",
        data: "{}",
        auto_escape_off: false,
        mode: Mode::Error,
        expected: "",
    },
    Case {
        name: "unclosed_echo",
        template: "a<?= name b",
        data: "{}",
        auto_escape_off: false,
        mode: Mode::Exact,
        expected: "a<?= name b",
    },
    Case {
        name: "xml_prolog",
        template: "<?xml version=\"1.0\" encoding=\"UTF-8\"?><p>ok</p>",
        data: "{}",
        auto_escape_off: false,
        mode: Mode::Exact,
        expected: "<?xml version=\"1.0\" encoding=\"UTF-8\"?><p>ok</p>",
    },
    Case {
        name: "runtime_error",
        template: "<?jhs null.x; ?>",
        data: "{}",
        auto_escape_off: false,
        mode: Mode::Error,
        expected: "",
    },
    Case {
        name: "require_vm",
        template: "<?jhs require(\"vm\"); ?>",
        data: "{}",
        auto_escape_off: false,
        mode: Mode::Error,
        expected: "",
    },
    Case {
        name: "require_fs_banned",
        template: "<?jhs require(\"fs\"); ?>",
        data: "{}",
        auto_escape_off: false,
        mode: Mode::Error,
        expected: "",
    },
    Case {
        name: "nested_data",
        template: "<?= user.name ?> / <?= user.tags[1] ?>",
        data: "{\"user\":{\"name\":\"Ana\",\"tags\":[\"a\",\"b\"]}}",
        auto_escape_off: false,
        mode: Mode::Exact,
        expected: "Ana / b",
    },
    Case {
        name: "number_expr",
        template: "<?= 40 + 2 ?> / <?= 0.1 + 0.2 ?>",
        data: "{}",
        auto_escape_off: false,
        mode: Mode::Exact,
        expected: "42 / 0.30000000000000004",
    },
    Case {
        name: "escape_dynamic_only",
        template: "<b>literal</b><?= val ?>",
        data: "{\"val\":\"<i>dyn</i>\"}",
        auto_escape_off: false,
        mode: Mode::Exact,
        expected: "<b>literal</b>&lt;i&gt;dyn&lt;/i&gt;",
    },
    Case {
        name: "variadic_echo",
        template: "<?jhs echo(\"a\", \"b\", 3); ?>",
        data: "{}",
        auto_escape_off: false,
        mode: Mode::Exact,
        expected: "ab3",
    },
    Case {
        name: "empty_echo",
        template: "a<?= ?>b",
        data: "{}",
        auto_escape_off: false,
        mode: Mode::Exact,
        expected: "ab",
    },
    Case {
        name: "undeclared",
        template: "<?= nope ?>",
        data: "{}",
        auto_escape_off: false,
        mode: Mode::Error,
        expected: "",
    },
    Case {
        name: "echo_number",
        template: "<?= 7 ?>",
        data: "{}",
        auto_escape_off: false,
        mode: Mode::Exact,
        expected: "7",
    },
    Case {
        name: "string_escape",
        template: "<?= \"a\\\"b\" ?>",
        data: "{}",
        auto_escape_off: false,
        mode: Mode::Exact,
        expected: "a&quot;b",
    },
    Case {
        name: "unicode_text",
        template: "café áéíóú 中文 😀",
        data: "{}",
        auto_escape_off: false,
        mode: Mode::Exact,
        expected: "café áéíóú 中文 😀",
    },
    Case {
        name: "console_case",
        template: "<?jhs console.log(\"dbg line\"); ?>html",
        data: "{}",
        auto_escape_off: false,
        mode: Mode::Exact,
        expected: "html",
    },
];

fn engine(auto_escape_off: bool) -> JhsEngine {
    JhsEngine::new(JhsOptions {
        cache: false,
        auto_escape: !auto_escape_off,
        ..JhsOptions::default()
    })
}

#[test]
fn fidelity_battery_matches_the_original_node_engine() {
    let mut failures: Vec<String> = Vec::new();

    for case in CASES {
        let data: serde_json::Map<String, Value> =
            serde_json::from_str(case.data).expect("valid data json");
        let result = engine(case.auto_escape_off).render_string(case.template, &data);

        match (&case.mode, &result) {
            (Mode::Error, Err(error)) => {
                let message = error.to_string();
                if !message.contains("Template execution error") {
                    failures.push(format!(
                        "{}: error without the original wrapping: {message}",
                        case.name
                    ));
                }
            }
            (Mode::Error, Ok(output)) => failures.push(format!(
                "{}: expected an error, got {:?}",
                case.name, output.html
            )),
            (_, Err(error)) => failures.push(format!(
                "{}: expected output {:?}, got error {error}",
                case.name, case.expected
            )),
            (Mode::Exact, Ok(output)) => {
                if output.html != case.expected {
                    failures.push(format!(
                        "{}: expected {:?}, got {:?}",
                        case.name, case.expected, output.html
                    ));
                }
            }
            (Mode::Trim, Ok(output)) => {
                if output.html.trim() != case.expected {
                    failures.push(format!(
                        "{}: expected {:?}, got {:?}",
                        case.name, case.expected, output.html
                    ));
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} battery cases failed:\n{}",
        failures.len(),
        CASES.len(),
        failures.join("\n")
    );
}
