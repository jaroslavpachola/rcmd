//! The monospace face the grid is drawn in.
//!
//! egui bundles Hack, which is a fine programming font and misses most
//! of the box-drawing block. rcmd frames every panel and every dialog
//! in U+2500s, so a face without them is not cosmetic - it is a screen
//! full of tofu. A system monospace font is looked for first and the
//! bundled one kept as the fallback beneath it, so a machine with no
//! fonts installed still starts.
//!
//! The face is the user's to choose: `font` in the window's settings
//! is a family name looked up among the system fonts, or a path to a
//! file, and the Font dialog lists the monospaced families it finds.

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

/// A face found and read: what to call it, its bytes, and which face
/// of a collection it is.
pub struct Face {
    pub name: String,
    pub bytes: Vec<u8>,
    pub index: u32,
}

/// Put a system monospace face at the head of the monospace family, if
/// one can be found. The bundled font stays behind it as the fallback,
/// so a glyph the system face lacks is still drawn. `spec` is a family
/// name or a file path; empty or `None` is the built-in search.
/// Returns what was installed, or `None` for "the bundled font".
pub fn install(ctx: &egui::Context, spec: Option<&str>) -> Option<String> {
    let face = load(spec)?;
    let name = face.name.clone();
    let mut fonts = FontDefinitions::default();
    let data = FontData {
        index: face.index,
        ..FontData::from_owned(face.bytes)
    };
    fonts.font_data.insert(name.clone(), data.into());
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .insert(0, name.clone());
    // the proportional family gets it too: the menu bar is drawn in
    // it, and a bar in another face than the grid would look borrowed
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, name.clone());
    ctx.set_fonts(fonts);
    Some(name)
}

/// `spec` if it names a readable file, else a system family of that
/// name, else the first candidate that exists. `None` means "keep the
/// bundled font".
pub fn load(spec: Option<&str>) -> Option<Face> {
    let spec = spec.map(str::trim).filter(|s| !s.is_empty());
    if let Some(spec) = spec {
        if let Some(face) = from_file(Path::new(spec)) {
            return Some(face);
        }
        if let Some(face) = by_family(spec) {
            return Some(face);
        }
        // a name that is nothing on this machine falls through to the
        // search: better the usual face than the bundled one
    }
    CANDIDATES
        .iter()
        .find_map(|path| from_file(Path::new(path)))
}

fn from_file(path: &Path) -> Option<Face> {
    let bytes = std::fs::read(path).ok()?;
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "system-mono".to_string());
    Some(Face {
        name,
        bytes,
        index: 0,
    })
}

/// The regular face of a family, by the name the system knows it under.
fn by_family(family: &str) -> Option<Face> {
    let db = system_fonts();
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        weight: fontdb::Weight::NORMAL,
        stretch: fontdb::Stretch::Normal,
        style: fontdb::Style::Normal,
    };
    let id = db.query(&query)?;
    let info = db.face(id)?;
    // the query matches a family name case-insensitively, but a query
    // that found nothing by name falls back to a default face, which
    // would quietly draw the grid in the wrong font
    if !info
        .families
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case(family))
    {
        return None;
    }
    let name = info.post_script_name.clone();
    let index = info.index;
    let bytes = db.with_face_data(id, |data, _| data.to_vec())?;
    Some(Face { name, bytes, index })
}

/// Every monospaced family on the system, each once, in order. What
/// the Font dialog lists.
pub fn families() -> Vec<String> {
    let db = system_fonts();
    let mut names: Vec<String> = db
        .faces()
        .filter(|face| face.monospaced)
        .filter_map(|face| face.families.first().map(|(name, _)| name.clone()))
        .collect();
    names.sort_by_key(|name| name.to_lowercase());
    names.dedup();
    names
}

fn system_fonts() -> fontdb::Database {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    db
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_override_that_is_not_there_falls_through() {
        // a path that cannot exist, and a family nobody has, must not
        // stop the real candidates from being tried, and must not panic
        let _ = load(Some("/nonexistent/font/that/is/not/here.ttf"));
        let _ = load(Some("No Such Family 3f9a"));
        assert!(by_family("No Such Family 3f9a").is_none());
    }

    /// A family the list offers is one `load` can find again: the
    /// dialog's choice has to be a name the lookup honours.
    #[test]
    fn a_listed_family_loads() {
        let names = families();
        let Some(name) = names.first() else {
            return; // a machine with no monospace font at all
        };
        let face = by_family(name).expect("listed family loads");
        assert!(!face.bytes.is_empty());
        assert!(names.windows(2).all(|w| w[0] != w[1]), "deduped");
    }
}
