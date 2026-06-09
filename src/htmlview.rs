//! Minimal, security-conscious rich-HTML renderer for egui.
//!
//! Email HTML is rendered as styled text: headings, bold/italic, lists,
//! blockquotes and clickable links. Nothing is ever fetched from the network —
//! images, scripts, styles, iframes and other remote/active content are
//! stripped entirely. This keeps tracking pixels and remote exploits out while
//! still giving a readable, formatted message.

use egui::{RichText, Ui};
use scraper::{ElementRef, Html, Node};

const BASE_SIZE: f32 = 14.0;

#[derive(Clone, Copy)]
struct Style {
    bold: bool,
    italic: bool,
    mono: bool,
    size: f32,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            bold: false,
            italic: false,
            mono: false,
            size: BASE_SIZE,
        }
    }
}

/// A piece of inline content accumulated within the current block.
enum Run {
    Text(String, Style),
    Link(String, String),
}

/// Render sanitized rich HTML into the current `ui`.
///
/// Should be called inside a scrolling container; it emits block elements with
/// vertical spacing and wraps long lines horizontally.
pub fn render(ui: &mut Ui, html: &str) {
    let doc = Html::parse_document(html);
    let mut runs: Vec<Run> = Vec::new();
    walk(doc.root_element(), Style::default(), &mut runs, ui);
    flush(ui, &mut runs);
}

fn walk(el: ElementRef, style: Style, runs: &mut Vec<Run>, ui: &mut Ui) {
    for child in el.children() {
        match child.value() {
            Node::Text(text) => {
                let s = collapse_ws(text);
                if s.chars().any(|c| !c.is_whitespace()) {
                    runs.push(Run::Text(s, style));
                }
            }
            Node::Element(elem) => {
                let Some(cel) = ElementRef::wrap(child) else {
                    continue;
                };
                match elem.name() {
                    // Drop anything that could load remote content, run code,
                    // or capture input. We never recurse into these.
                    "script" | "style" | "head" | "title" | "meta" | "link" | "noscript"
                    | "template" | "img" | "image" | "svg" | "picture" | "iframe" | "object"
                    | "embed" | "audio" | "video" | "canvas" | "map" | "form" | "input"
                    | "button" | "select" | "textarea" | "label" | "applet" | "base" => {}

                    "br" => flush(ui, runs),
                    "hr" => {
                        flush(ui, runs);
                        ui.separator();
                    }

                    "a" => {
                        let href = elem.attr("href").unwrap_or("").trim().to_string();
                        let text = collapse_ws(&cel.text().collect::<String>());
                        let text = text.trim().to_string();
                        if text.is_empty() {
                            continue;
                        }
                        if is_safe_link(&href) {
                            runs.push(Run::Link(text, href));
                        } else {
                            // Unsupported/unsafe scheme: show the text only.
                            runs.push(Run::Text(text, style));
                        }
                    }

                    "b" | "strong" => walk(
                        cel,
                        Style {
                            bold: true,
                            ..style
                        },
                        runs,
                        ui,
                    ),
                    "i" | "em" | "cite" | "var" => walk(
                        cel,
                        Style {
                            italic: true,
                            ..style
                        },
                        runs,
                        ui,
                    ),
                    "code" | "tt" | "kbd" | "samp" => walk(
                        cel,
                        Style {
                            mono: true,
                            ..style
                        },
                        runs,
                        ui,
                    ),
                    "pre" => {
                        flush(ui, runs);
                        walk(
                            cel,
                            Style {
                                mono: true,
                                ..style
                            },
                            runs,
                            ui,
                        );
                        flush(ui, runs);
                    }

                    "h1" => heading(cel, 22.0, runs, ui),
                    "h2" => heading(cel, 19.0, runs, ui),
                    "h3" => heading(cel, 17.0, runs, ui),
                    "h4" => heading(cel, 16.0, runs, ui),
                    "h5" | "h6" => heading(cel, 15.0, runs, ui),

                    "li" => {
                        flush(ui, runs);
                        runs.push(Run::Text("•  ".to_string(), style));
                        walk(cel, style, runs, ui);
                        flush(ui, runs);
                    }

                    // Block-level containers: break before and after.
                    "p" | "div" | "ul" | "ol" | "table" | "thead" | "tbody" | "tr" | "section"
                    | "article" | "header" | "footer" | "blockquote" | "figure" | "figcaption"
                    | "dl" | "dt" | "dd" | "nav" | "main" | "aside" | "address" => {
                        flush(ui, runs);
                        walk(cel, style, runs, ui);
                        flush(ui, runs);
                    }

                    // Table cells: keep inline but separate with spaces.
                    "td" | "th" => {
                        walk(cel, style, runs, ui);
                        runs.push(Run::Text("   ".to_string(), style));
                    }

                    // span, font, small, sup, sub, abbr, mark, etc.: keep style.
                    _ => walk(cel, style, runs, ui),
                }
            }
            _ => {}
        }
    }
}

fn heading(cel: ElementRef, size: f32, runs: &mut Vec<Run>, ui: &mut Ui) {
    flush(ui, runs);
    let style = Style {
        bold: true,
        size,
        ..Style::default()
    };
    walk(cel, style, runs, ui);
    flush(ui, runs);
}

/// Render the accumulated inline runs as one wrapped paragraph and clear them.
fn flush(ui: &mut Ui, runs: &mut Vec<Run>) {
    if runs.is_empty() {
        return;
    }
    let mut emitted = false;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for run in runs.iter() {
            match run {
                Run::Text(s, style) => {
                    for word in s.split_whitespace() {
                        ui.label(styled(format!("{word} "), *style));
                        emitted = true;
                    }
                }
                Run::Link(text, url) => {
                    ui.hyperlink_to(RichText::new(text), url);
                    ui.label(" ");
                    emitted = true;
                }
            }
        }
    });
    if emitted {
        ui.add_space(4.0);
    }
    runs.clear();
}

fn styled(text: String, style: Style) -> RichText {
    let mut rt = RichText::new(text);
    if style.bold {
        rt = rt.strong();
    }
    if style.italic {
        rt = rt.italics();
    }
    if style.mono {
        rt = rt.monospace();
    }
    if (style.size - BASE_SIZE).abs() > f32::EPSILON {
        rt = rt.size(style.size);
    }
    rt
}

/// Collapse any run of ASCII/Unicode whitespace into single spaces.
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    out
}

/// Only allow link schemes we can safely hand to the OS browser.
fn is_safe_link(href: &str) -> bool {
    let lower = href.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:")
}
