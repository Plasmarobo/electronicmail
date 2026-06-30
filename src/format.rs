//! Minimal Markdown-subset → HTML for outgoing mail.
//!
//! This converts just the "basic formatting" the compose toolbar can produce
//! into a small, safe HTML fragment for the `text/html` alternative of a sent
//! message:
//!
//! * `**bold**` → `<strong>`,
//! * `*italic*` → `<em>`,
//! * `- ` / `* ` lines → `<ul><li>…</li></ul>`,
//! * `> ` lines → `<blockquote>…</blockquote>`,
//! * blank-line-separated paragraphs, single newlines as `<br>`,
//! * bare `http://` / `https://` URLs auto-linked.
//!
//! Everything is HTML-escaped *before* any markup is added, so the user's
//! literal text can never inject tags or break the surrounding document.

/// Render a Markdown-subset string as an HTML fragment (no `<html>`/`<body>`
/// wrapper — just block-level elements ready to drop into a message body).
pub fn to_html(src: &str) -> String {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        if is_bullet(trimmed) {
            out.push_str("<ul>");
            while i < lines.len() && is_bullet(lines[i].trim_start()) {
                out.push_str("<li>");
                out.push_str(&inline(bullet_content(lines[i].trim_start())));
                out.push_str("</li>");
                i += 1;
            }
            out.push_str("</ul>");
        } else if trimmed.starts_with('>') {
            out.push_str("<blockquote>");
            let mut first = true;
            while i < lines.len() && lines[i].trim_start().starts_with('>') {
                let content = lines[i].trim_start()[1..].trim_start();
                if !first {
                    out.push_str("<br>");
                }
                out.push_str(&inline(content));
                first = false;
                i += 1;
            }
            out.push_str("</blockquote>");
        } else {
            out.push_str("<p>");
            let mut first = true;
            while i < lines.len() {
                let t = lines[i].trim_start();
                if t.is_empty() || is_bullet(t) || t.starts_with('>') {
                    break;
                }
                if !first {
                    out.push_str("<br>");
                }
                out.push_str(&inline(lines[i]));
                first = false;
                i += 1;
            }
            out.push_str("</p>");
        }
    }
    out
}

/// A line that introduces a bullet list item (`- foo`, `* foo`, or a bare
/// marker). The trailing space requirement keeps `*italic*` from being mistaken
/// for a list.
fn is_bullet(s: &str) -> bool {
    s == "-" || s == "*" || s.starts_with("- ") || s.starts_with("* ")
}

/// The text of a bullet line with its leading marker removed.
fn bullet_content(s: &str) -> &str {
    s.strip_prefix('-')
        .or_else(|| s.strip_prefix('*'))
        .unwrap_or(s)
        .trim_start()
}

/// Inline formatting for a single run of text: escape, then emphasis, then
/// auto-link.
fn inline(s: &str) -> String {
    let escaped = escape(s);
    let emphasized = emphasis(&escaped);
    autolink(&emphasized)
}

/// HTML-escape the characters that would otherwise be interpreted as markup.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Apply `**bold**` then `*italic*`, leaving unbalanced markers as literal text.
fn emphasis(s: &str) -> String {
    let bold = wrap_pairs(s, "**", "strong");
    wrap_pairs(&bold, "*", "em")
}

/// Wrap the text between balanced `marker` pairs in `<tag>…</tag>`. Empty or
/// whitespace-only spans are left untouched so stray markers survive verbatim.
fn wrap_pairs(s: &str, marker: &str, tag: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(open) = rest.find(marker) {
        let after_open = open + marker.len();
        let Some(rel) = rest[after_open..].find(marker) else {
            break;
        };
        let close = after_open + rel;
        let inner = &rest[after_open..close];
        if inner.trim().is_empty() {
            // Keep scanning past this marker without consuming it as emphasis.
            out.push_str(&rest[..after_open]);
            rest = &rest[after_open..];
            continue;
        }
        out.push_str(&rest[..open]);
        out.push('<');
        out.push_str(tag);
        out.push('>');
        out.push_str(inner);
        out.push_str("</");
        out.push_str(tag);
        out.push('>');
        rest = &rest[close + marker.len()..];
    }
    out.push_str(rest);
    out
}

/// Turn bare `http(s)://` words into anchor tags. Operates on already-escaped
/// text, so any `&` in a URL is the correct `&amp;` for an HTML attribute.
fn autolink(s: &str) -> String {
    let mut out = String::new();
    let mut word = String::new();
    for c in s.chars() {
        if c.is_whitespace() {
            if !word.is_empty() {
                out.push_str(&linkify(&word));
                word.clear();
            }
            out.push(c);
        } else {
            word.push(c);
        }
    }
    if !word.is_empty() {
        out.push_str(&linkify(&word));
    }
    out
}

/// Wrap a single whitespace-delimited word in an anchor when it looks like a
/// URL, keeping any trailing sentence punctuation outside the link.
fn linkify(word: &str) -> String {
    if !(word.starts_with("http://") || word.starts_with("https://")) {
        return word.to_string();
    }
    let url = word.trim_end_matches(['.', ',', ')', ']', '!', '?', ';', ':']);
    let tail = &word[url.len()..];
    format!("<a href=\"{url}\">{url}</a>{tail}")
}

#[cfg(test)]
mod tests {
    use super::to_html;

    #[test]
    fn bold_and_italic() {
        assert_eq!(to_html("**hi**"), "<p><strong>hi</strong></p>");
        assert_eq!(to_html("*hi*"), "<p><em>hi</em></p>");
        assert_eq!(
            to_html("a **b** and *c*"),
            "<p>a <strong>b</strong> and <em>c</em></p>"
        );
    }

    #[test]
    fn escapes_literal_markup() {
        // A literal "<b>" must never become a real tag.
        assert_eq!(to_html("<b>x</b>"), "<p>&lt;b&gt;x&lt;/b&gt;</p>");
        assert_eq!(to_html("a & b"), "<p>a &amp; b</p>");
    }

    #[test]
    fn unordered_list() {
        assert_eq!(to_html("- one\n- two"), "<ul><li>one</li><li>two</li></ul>");
        assert_eq!(to_html("* solo"), "<ul><li>solo</li></ul>");
    }

    #[test]
    fn blockquote() {
        assert_eq!(to_html("> hello"), "<blockquote>hello</blockquote>");
        assert_eq!(to_html("> a\n> b"), "<blockquote>a<br>b</blockquote>");
    }

    #[test]
    fn paragraphs_and_breaks() {
        assert_eq!(to_html("a\nb"), "<p>a<br>b</p>");
        assert_eq!(to_html("a\n\nb"), "<p>a</p><p>b</p>");
    }

    #[test]
    fn autolinks_urls() {
        assert_eq!(
            to_html("see https://example.com now"),
            "<p>see <a href=\"https://example.com\">https://example.com</a> now</p>"
        );
        // Trailing punctuation stays outside the link.
        assert_eq!(
            to_html("go to http://a.test."),
            "<p>go to <a href=\"http://a.test\">http://a.test</a>.</p>"
        );
    }

    #[test]
    fn stray_marker_is_literal() {
        assert_eq!(to_html("2 * 3 = 6 and done"), "<p>2 * 3 = 6 and done</p>");
    }
}
