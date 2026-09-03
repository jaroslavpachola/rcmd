//! The monospace face the grid is drawn in.
//!
//! egui bundles Hack, which is a fine programming font and misses most
//! of the box-drawing block. rcmd frames every panel and every dialog
//! in U+2500s, so a face without them is not cosmetic - it is a screen
//! full of tofu. A system monospace font is looked for first and the
//! bundled one kept as the fallback beneath it, so a machine with no
//! fonts installed still starts.

use std::path::Path;

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};

/// Where a full-coverage monospace face usually lives, best first.
/// DejaVu leads because it carries the whole box-drawing and block
/// range and is on essentially every Linux install.
const CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/dejavu-sans-mono-fonts/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
    "/usr/share/fonts/liberation-mono/LiberationMono-Regular.ttf",
    "/Library/Fonts/Menlo.ttc",
    "/System/Library/Fonts/Menlo.ttc",
    "C:\\Windows\\Fonts\\consola.ttf",
];

/// Put a system monospace face at the head of the monospace family, if
/// one can be found. The bundled font stays behind it as the fallback,
/// so a glyph the system face lacks is still drawn.
pub fn install(ctx: &egui::Context) {
    let Some((name, bytes)) = load(std::env::var("RCMD_EGUI_FONT").ok().as_deref()) else {
        return;
    };
    let mut fonts = FontDefinitions::default();
    fonts
        .font_data
        .insert(name.clone(), FontData::from_owned(bytes).into());
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .insert(0, name.clone());
    // the proportional family gets it too: nothing here is drawn
    // proportionally, but a stray tooltip should match
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, name);
    ctx.set_fonts(fonts);
}

/// `$RCMD_EGUI_FONT` if it names a readable file, else the first candidate
/// that exists. `None` means "keep the bundled font".
fn load(override_path: Option<&str>) -> Option<(String, Vec<u8>)> {
    let mut paths: Vec<&str> = Vec::new();
    if let Some(path) = override_path {
        paths.push(path);
    }
    paths.extend(CANDIDATES);
    for path in paths {
        let path = Path::new(path);
        if let Ok(bytes) = std::fs::read(path) {
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "system-mono".to_string());
            return Some((name, bytes));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_override_that_is_not_there_falls_through() {
        // a path that cannot exist must not stop the real candidates
        // from being tried, and must not panic
        let _ = load(Some("/nonexistent/font/that/is/not/here.ttf"));
    }
}
