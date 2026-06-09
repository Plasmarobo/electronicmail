//! Broad-coverage fallback fonts.
//!
//! egui's bundled fonts cover Latin text and a subset of emoji, but several of
//! the UI symbols this app uses (✉ ⟳ ★ ⚠ ← ✓ ● and the emoji 📥 🚫 📍, plus
//! full-width forms ＋ ？) fall outside that set and render as empty boxes
//! ("tofu"). We append platform fonts that have wide symbol/emoji coverage as
//! *fallbacks*, so ordinary text keeps egui's default look while any missing
//! glyph is filled in from these.

use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily};

/// Install broad-coverage fallback fonts into the egui context.
///
/// Safe to call once at startup. If no extra fonts are found the egui defaults
/// are left untouched.
pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let mut added: Vec<String> = Vec::new();

    for (name, path) in candidate_fonts() {
        if added.contains(&name) {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&path) {
            fonts
                .font_data
                .insert(name.clone(), Arc::new(FontData::from_owned(bytes)));
            added.push(name);
        }
    }

    if added.is_empty() {
        return;
    }

    // Append as lowest-priority fallbacks for both families so the primary text
    // font is unchanged and only otherwise-missing glyphs use these.
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        let list = fonts.families.entry(family).or_default();
        for name in &added {
            if !list.contains(name) {
                list.push(name.clone());
            }
        }
    }

    ctx.set_fonts(fonts);
}

/// Candidate fallback font files, in priority order. Only the ones that exist
/// on the current machine are loaded.
fn candidate_fonts() -> Vec<(String, String)> {
    let mut out = Vec::new();

    #[cfg(target_os = "windows")]
    {
        let dir = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
        let dir = format!("{dir}\\Fonts");
        // Segoe UI Symbol covers arrows/stars/dingbats and monochrome emoji;
        // Segoe UI adds Latin/Greek/Cyrillic + full-width forms; Segoe UI Emoji
        // provides colour emoji; Arial is a last-ditch broad fallback.
        for file in ["seguisym.ttf", "segoeui.ttf", "seguiemj.ttf", "arial.ttf"] {
            out.push((file.to_string(), format!("{dir}\\{file}")));
        }
    }

    #[cfg(target_os = "macos")]
    {
        for path in [
            "/System/Library/Fonts/Apple Symbols.ttf",
            "/Library/Fonts/Arial Unicode.ttf",
            "/System/Library/Fonts/Apple Color Emoji.ttc",
        ] {
            out.push((path.to_string(), path.to_string()));
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for path in [
            "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf",
        ] {
            out.push((path.to_string(), path.to_string()));
        }
    }

    out
}
