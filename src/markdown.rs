//! CMS Markdown rendering (F8): the safe body mode for editors.
//!
//! Pages stored with `body_format = "markdown"` render through this
//! module instead of the `.jhs` engine. [`render`] turns Markdown
//! source into an HTML fragment the public wrapper injects with
//! `raw()` — trusted **because it comes from here**, never because the
//! editor wrote it.
//!
//! The subset is deliberately small and safe by construction:
//!
//! * **Raw HTML is dropped, not rendered.** pulldown-cmark 0.13 has
//!   no switch to stop parsing raw HTML — it always emits `Html` /
//!   `InlineHtml` events — so the filter drops those events here.
//!   `<script>`, `<iframe>` or `onerror=` payloads vanish; the
//!   previsualización makes that visible immediately (the editor
//!   previews, sees the markup go, and knows). Editors who genuinely
//!   need markup use the `.jhs` body mode, whose power is already
//!   gated behind the editor role.
//! * **URL schemes are filtered.** Link and image destinations may be
//!   `http`, `https`, `mailto` or `ftp`, or any relative form (`/`,
//!   `#`, `?`, plain paths). Anything else — `javascript:`, `data:`,
//!   `vbscript:` — degrades to `#`, so a Markdown link can never
//!   become a script carrier.
//! * **Tables and strikethrough** are on (the useful, safe GFM
//!   extras). Footnotes, task lists and the other exotica stay off to
//!   keep the rendered surface small and predictable.
//!
//! The renderer is pure CPU work on editor-sized strings (the page
//! form caps bodies at 600 000 characters), so callers wrap it in
//! `spawn_blocking` exactly like the `.jhs` pipeline does.

use pulldown_cmark::{html, CowStr, Event, Options, Parser, Tag};

/// URL schemes a Markdown body may link to. Everything else — most
/// importantly `javascript:` and `data:` — degrades to `#`.
const ALLOWED_SCHEMES: [&str; 4] = ["http", "https", "mailto", "ftp"];

/// Renders a Markdown body into the HTML fragment the CMS page wrapper
/// injects. The input is untrusted editor content; the output is safe
/// by construction (see the module docs).
pub(crate) fn render(markdown: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);

    let parser = Parser::new_ext(markdown, options).map(sanitize_event);
    let mut output = String::with_capacity(markdown.len() + 64);
    html::push_html(&mut output, parser);
    output
}

/// Maps parser events into safe ones: raw HTML is dropped (there is
/// no parser option for that in 0.13, so the filter lives here) and
/// link/image destinations outside the scheme allowlist degrade to
/// `#` — the anchor still renders, the payload does not. Closings
/// (`TagEnd`) carry no URL, so only openings are rewritten.
fn sanitize_event(event: Event) -> Event {
    match event {
        Event::Start(tag) => Event::Start(sanitize_tag(tag)),
        Event::Html(_) | Event::InlineHtml(_) => Event::Text(CowStr::Borrowed("")),
        other => other,
    }
}

/// Applies the destination filter to the two tags that carry URLs.
fn sanitize_tag(tag: Tag) -> Tag {
    match tag {
        Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        } => Tag::Link {
            link_type,
            dest_url: sanitize_dest(dest_url),
            title,
            id,
        },
        Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        } => Tag::Image {
            link_type,
            dest_url: sanitize_dest(dest_url),
            title,
            id,
        },
        other => other,
    }
}

/// Passes safe destinations through, rewrites everything else to `#`.
fn sanitize_dest(dest_url: CowStr) -> CowStr {
    if destination_is_safe(&dest_url) {
        dest_url
    } else {
        CowStr::Borrowed("#")
    }
}

/// A destination is safe when it is relative (no scheme at all) or its
/// scheme is one of [`ALLOWED_SCHEMES`]. A scheme is letters, digits,
/// `+`, `-` or `.` starting with a letter, followed by `:` before any
/// `/`, `?` or `#` — the CommonMark reading of an absolute URI.
fn destination_is_safe(dest: &str) -> bool {
    match destination_scheme(dest) {
        Some(scheme) => ALLOWED_SCHEMES.contains(&scheme.to_ascii_lowercase().as_str()),
        None => true,
    }
}

/// The URI scheme of a destination, or `None` when the destination is
/// relative (the shape every site-internal link takes).
fn destination_scheme(dest: &str) -> Option<&str> {
    let dest = dest.trim();
    for (index, char) in dest.char_indices() {
        match char {
            // A delimiter first: the rest cannot be a scheme.
            '/' | '?' | '#' => return None,
            ':' => {
                let candidate = &dest[..index];
                let valid = !candidate.is_empty()
                    && candidate.starts_with(|first: char| first.is_ascii_alphabetic())
                    && candidate.chars().all(
                        |char| matches!(char, 'a'..='z' | 'A'..='Z' | '0'..='9' | '+' | '-' | '.'),
                    );
                return valid.then_some(candidate);
            }
            _ => continue,
        }
    }
    // No colon at all: a plain relative destination.
    None
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn paragraphs_emphasis_and_code() {
        let html = render("# Título\n\nUn **negrita**, una *cursiva* y `código`.\n");
        assert!(html.contains("<h1>Título</h1>"), "{html}");
        assert!(html.contains("<strong>negrita</strong>"), "{html}");
        assert!(html.contains("<em>cursiva</em>"), "{html}");
        assert!(html.contains("<code>código</code>"), "{html}");
    }

    #[test]
    fn lists_and_blockquotes() {
        let html = render("- uno\n- dos\n\n> cita\n");
        assert!(
            html.contains("<ul>") && html.contains("<li>uno</li>"),
            "{html}"
        );
        assert!(html.contains("<blockquote>"), "{html}");
    }

    #[test]
    fn fenced_code_blocks_escape_their_contents() {
        let html = render("```html\n<b>raw</b>\n```\n");
        assert!(html.contains("<pre><code"), "{html}");
        assert!(html.contains("&lt;b&gt;"), "{html}");
    }

    #[test]
    fn tables_render() {
        let html = render("| a | b |\n|---|---|\n| 1 | 2 |\n");
        assert!(html.contains("<table>"), "{html}");
        assert!(html.contains("<td>1</td>"), "{html}");
    }

    #[test]
    fn strikethrough_renders() {
        let html = render("~~tachado~~\n");
        assert!(html.contains("<del>tachado</del>"), "{html}");
    }

    #[test]
    fn raw_html_is_dropped_wholesale() {
        let html = render("antes <script>alert(1)</script> después\n");
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("</script"), "{html}");
        // Inline tags drop; their inner text stays as inert prose.
        assert!(html.contains("antes"), "{html}");
        assert!(html.contains("después"), "{html}");

        // HTML blocks drop entirely; the surrounding Markdown stays.
        let html = render("párrafo\n\n<img src=x onerror=alert(1)>\n\notro\n");
        assert!(!html.contains("<img"), "{html}");
        assert!(!html.contains("onerror"), "{html}");
        assert!(html.contains("otro"), "{html}");

        // An iframe with a trusted-looking src is still raw HTML.
        let html = render("<iframe src=\"https://example.com\"></iframe>\n");
        assert!(!html.contains("<iframe"), "{html}");
    }

    #[test]
    fn dangerous_link_schemes_degrade_to_a_fragment() {
        for markdown in [
            "[pincha](javascript:alert(1))",
            "[](JAVASCRIPT:alert(1))",
            "[x](data:text/html,<b>)",
            "[x](vbscript:msgbox)",
        ] {
            let html = render(markdown);
            assert!(html.contains("href=\"#\""), "{markdown} → {html}");
            assert!(!html.contains("javascript:"), "{markdown} → {html}");
            assert!(!markdown
                .split(':')
                .next()
                .unwrap_or_default()
                .contains("mailto"));
        }
    }

    #[test]
    fn image_destinations_are_filtered_too() {
        let html = render("![pwn](javascript:alert(1))\n");
        assert!(!html.contains("javascript:"), "{html}");
        assert!(html.contains("src=\"#\""), "{html}");
    }

    #[test]
    fn safe_destinations_survive() {
        for markdown in [
            "[web](https://example.com)",
            "[interno](/p/otra)",
            "[ancla](#seccion)",
            "[correo](mailto:hola@example.com)",
            "[ftp](ftp://ejemplo.com/fichero)",
            "[relativo](docs/otro.html)",
            "[consulta](?page=2)",
        ] {
            let html = render(markdown);
            assert!(!html.contains("href=\"#\""), "{markdown} → {html}");
        }
    }

    #[test]
    fn autolinks_keep_their_scheme() {
        let html = render("<https://example.com>\n");
        assert!(html.contains("href=\"https://example.com\""), "{html}");
    }

    #[test]
    fn text_content_is_escaped() {
        let html = render("a < b & c\n");
        assert!(html.contains("&lt;"), "{html}");
        assert!(html.contains("&amp;"), "{html}");
    }
}
