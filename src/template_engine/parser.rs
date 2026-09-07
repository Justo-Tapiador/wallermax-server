//! `.jhs` → JavaScript compiler.
//!
//! A faithful port of the two-pass regex compilation of `node-jhs2`:
//!
//! 1. **Echo pass** — every `<?= expr ?>` is rewritten into a code block
//!    `<?jhs__output += __escape(expr);?>` (note the absence of spaces:
//!    the original concatenates the tags verbatim). Unclosed echo tags
//!    (no `?>` anywhere after them) stay literal text.
//! 2. **Code pass** — the source is scanned for `<?jhs ... ?>` blocks;
//!    text between blocks becomes `__output += <JSON string literal>;`
//!    lines and block bodies are emitted verbatim. Unclosed code tags
//!    stay literal text.
//!
//! The result is wrapped in the original's flat IIFE, which defines the
//! variadic `echo()` helper (arguments are `String()`-ified **before**
//! escaping, so `echo(null)` prints `null` while `<?= null ?>` prints
//! the empty string) and returns `__output`. Because the body is flat, a
//! top-level `return` inside a template hijacks the whole output — the
//! quirk is preserved on purpose.
//!
//! Known limitations shared with the original:
//!
//! - a `?>` appearing inside a JavaScript string in a code block closes
//!   the block early;
//! - pass one rewrites `<?= ... ?>` even when it sits inside a code
//!   block's string literal (the echo tag then splits the block).

/// Delimiter configuration (mirrors the original constructor options).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagOptions {
    /// Opening tag of code blocks (default `<?jhs`).
    pub open_tag: String,
    /// Closing tag of both block kinds (default `?>`).
    pub close_tag: String,
    /// Opening tag of output expressions (default `<?=`).
    pub echo_tag: String,
}

impl Default for TagOptions {
    fn default() -> Self {
        Self {
            open_tag: String::from("<?jhs"),
            close_tag: String::from("?>"),
            echo_tag: String::from("<?="),
        }
    }
}

/// Compiles a `.jhs` template into a JavaScript program.
///
/// The program is a single expression whose value is the rendered output;
/// `__escape`, `raw`, `escapeHtml`, `console`, `JSON` and the data
/// variables are supplied by the execution environment (see [`crate::template_engine::engine`]).
pub fn compile(source: &str, tags: &TagOptions) -> String {
    let rewritten = rewrite_echo_tags(source, tags);
    let body = compile_code_blocks(&rewritten, tags);
    wrap(&body)
}

/// Pass one: rewrite `<?= expr ?>` into `<?jhs__output += __escape(expr);?>`.
///
/// Equivalent to the original's global non-overlapping regex replace: the
/// expression spans lazily up to the **first** closing tag (even across
/// other open tags), and a missing closing tag leaves the rest untouched.
fn rewrite_echo_tags(source: &str, tags: &TagOptions) -> String {
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0;

    while let Some(open_offset) = source[cursor..].find(tags.echo_tag.as_str()) {
        let open_at = cursor + open_offset;
        out.push_str(&source[cursor..open_at]);

        let expression_start = open_at + tags.echo_tag.len();
        let Some(close_offset) = source[expression_start..].find(tags.close_tag.as_str()) else {
            // No closing tag exists after this echo tag, so no later echo
            // tag can match either: keep the remainder verbatim.
            out.push_str(&source[open_at..]);
            return out;
        };
        let close_at = expression_start + close_offset;

        out.push_str(&tags.open_tag);
        out.push_str("__output += __escape(");
        out.push_str(source[expression_start..close_at].trim());
        out.push_str(");");
        out.push_str(&tags.close_tag);

        cursor = close_at + tags.close_tag.len();
    }

    out.push_str(&source[cursor..]);
    out
}

/// Pass two: turn `<?jhs ... ?>` blocks into code and everything else
/// into `__output += "…";` lines.
///
/// Mirrors the original's regex loop: text between matches becomes one
/// output line per segment, and an unclosed open tag makes the whole
/// remainder — from the previous match end — a single literal segment.
fn compile_code_blocks(source: &str, tags: &TagOptions) -> String {
    let mut code = String::new();
    let mut cursor = 0;

    while let Some(open_offset) = source[cursor..].find(tags.open_tag.as_str()) {
        let open_at = cursor + open_offset;
        let body_start = open_at + tags.open_tag.len();

        let Some(close_offset) = source[body_start..].find(tags.close_tag.as_str()) else {
            // Unclosed code tag: no match here or later; the remainder
            // from `cursor` (including the opener) is one literal text
            // segment, exactly like the original's final append.
            push_text(&mut code, &source[cursor..]);
            return code;
        };

        if open_at > cursor {
            push_text(&mut code, &source[cursor..open_at]);
        }
        let close_at = body_start + close_offset;
        code.push_str(&source[body_start..close_at]);
        code.push('\n');
        cursor = close_at + tags.close_tag.len();
    }

    if cursor < source.len() {
        push_text(&mut code, &source[cursor..]);
    }
    code
}

/// Appends an output line for a literal text segment.
fn push_text(code: &mut String, text: &str) {
    code.push_str("__output += ");
    code.push_str(&json_stringify(text));
    code.push_str(";\n");
}

/// Wraps the compiled body in the original's flat IIFE.
///
/// `echo` stringifies each argument before escaping (the original's
/// `args.map(arg => __escape(String(arg)))`), and the flat body lets a
/// top-level `return` replace the whole output.
fn wrap(code: &str) -> String {
    let mut program = String::with_capacity(code.len() + 256);
    program.push_str("\n(function() {\n");
    program.push_str("  let __output = \"\";\n\n");
    program.push_str("  // echo() — writes escaped output directly from code blocks\n");
    program.push_str("  function echo(...args) {\n");
    // v0.8.0: arguments go through the prelude's `__jhsEchoPart`, which
    // keeps the original's String()-before-escaping semantics
    // (`echo(null)` prints `null`) while letting `raw()` sentinel values
    // through untouched — `echo(raw(markup))` prints trusted markup
    // exactly like the `<?= raw(markup) ?>` form, and the sentinel
    // itself stays hidden inside the prelude's closure.
    program.push_str("    __output += args.map(__jhsEchoPart).join('');\n");
    program.push_str("  }\n\n");
    program.push_str(code);
    program.push_str("\n  return __output;\n");
    program.push_str("})()");
    program
}

/// JavaScript-compatible string literal for a text segment, matching
/// `JSON.stringify` (used verbatim by the original engine).
///
/// Quotes, backslashes and control characters are escaped; non-ASCII
/// characters pass through as raw UTF-8.
fn json_stringify(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
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

    #[test]
    fn tiny_template_compiles_to_the_original_shape() {
        let program = compile("a<?= x ?>b<?jhs y; ?>c", &TagOptions::default());

        assert!(program.contains("__output += \"a\";"));
        assert!(program.contains("__output += __escape(x);"));
        assert!(program.contains("__output += \"b\";"));
        assert!(program.contains(" y; "));
        assert!(program.contains("__output += \"c\";"));
        assert!(program.starts_with("\n(function() {"));
        assert!(program.ends_with("})()"));
    }

    #[test]
    fn text_between_tags_is_json_stringified() {
        let program = compile("line1\n\"quoted\"\n\\slash", &TagOptions::default());

        assert!(program.contains("__output += \"line1\\n\\\"quoted\\\"\\n\\\\slash\";"));
    }

    #[test]
    fn control_characters_are_escaped() {
        let program = compile("a\u{08}b\u{0c}c\u{1f}d", &TagOptions::default());

        assert!(program.contains("__output += \"a\\bb\\fc\\u001fd\";"));
    }

    #[test]
    fn echo_expression_is_trimmed() {
        let program = compile("<?=   name   ?>", &TagOptions::default());

        assert!(program.contains("__output += __escape(name);"));
        assert!(!program.contains("   name   "));
    }

    #[test]
    fn echo_without_spaces_is_recognized() {
        let program = compile("<?=x?>", &TagOptions::default());

        assert!(program.contains("__output += __escape(x);"));
    }

    #[test]
    fn empty_echo_tag_calls_escape_with_no_arguments() {
        let program = compile("a<?= ?>b", &TagOptions::default());

        assert!(program.contains("__output += __escape();"));
    }

    #[test]
    fn echo_tag_spans_lazily_to_the_first_close_tag() {
        // One echo match spanning the second echo tag, like the regex.
        let program = compile("<?= a <?= b ?>", &TagOptions::default());

        assert!(program.contains("__output += __escape(a <?= b);"));
    }

    #[test]
    fn unclosed_code_tag_stays_literal_text() {
        let program = compile("a<?jhs if (true) { b", &TagOptions::default());

        assert!(program.contains("__output += \"a<?jhs if (true) { b\";"));
    }

    #[test]
    fn unclosed_echo_tag_stays_literal_text() {
        let program = compile("a<?= name b", &TagOptions::default());

        assert!(program.contains("__output += \"a<?= name b\";"));
    }

    #[test]
    fn unknown_open_tags_stay_literal_text() {
        let program = compile("<?xml v?><?php echo 1; ?>", &TagOptions::default());

        assert!(program.contains("<?xml v?>"));
        assert!(program.contains("<?php echo 1; ?>"));
        assert!(!program.contains("__output += __escape"));
    }

    #[test]
    fn close_tag_inside_js_string_closes_the_block_early() {
        let program = compile("<?jhs var s = \"?>\"; echo(s); ?>", &TagOptions::default());

        // The block ends at the first `?>` — inside the string literal.
        assert!(program.contains("var s = \""));
        assert!(program.ends_with("})()"));
    }

    #[test]
    fn echo_nested_inside_code_block_matches_pass_one_first() {
        // Pass one rewrites the inner echo first, then pass two splits the
        // (now broken) code block at the first closing tag.
        let program = compile("<?jhs var s = \"<?= x ?>\"; ?>", &TagOptions::default());

        assert!(program.contains("__output += __escape(x);"));
    }

    #[test]
    fn custom_tags_are_supported() {
        let tags = TagOptions {
            open_tag: String::from("<%"),
            close_tag: String::from("%>"),
            echo_tag: String::from("<%="),
        };
        let program = compile("a<%= name %>b<% if (x) { %>c<% } %>", &tags);

        assert!(program.contains("__output += __escape(name);"));
        assert!(program.contains("if (x) {"));
        assert!(program.contains("__output += \"c\";"));
    }

    #[test]
    fn unicode_text_passes_through_raw() {
        let program = compile("café áéíóú 中文 😀", &TagOptions::default());

        assert!(program.contains("__output += \"café áéíóú 中文 😀\";"));
    }

    #[test]
    fn echo_replacement_has_no_spaces_around_the_tags() {
        // The original concatenates the tags without separators in the
        // intermediate rewrite (pass one output).
        let rewritten = rewrite_echo_tags("<?= x ?>", &TagOptions::default());

        assert_eq!(rewritten, "<?jhs__output += __escape(x);?>");
    }
}
