//! The settings a file carries with it: how it is indented, guessed
//! from the file itself, and what `.editorconfig` says about it, which
//! wins - the project decided, and the editor should not argue.
//!
//! `.editorconfig` files are read from the file's directory upwards
//! until one says `root = true`; a nearer one overrides a further one,
//! and a later section overrides an earlier one. What rcmd's editor can
//! act on is read: `indent_style`, `indent_size`, `tab_width`,
//! `trim_trailing_whitespace` and `insert_final_newline`.

use std::collections::BTreeMap;
use std::path::Path;

use rcmd_edit::{Editor, Prefs};

/// The prefs for this file: the options in force, then its own indent,
/// then its `.editorconfig`.
pub fn for_file(ed: &Editor, mut prefs: Prefs) -> Prefs {
    if let Some((spaces, width)) = ed.guess_indent() {
        prefs.fill_tabs = spaces;
        if spaces {
            prefs.tab_size = width;
        }
    }
    let set = settings(&ed.path);
    let number = |key: &str| set.get(key).and_then(|v| v.parse::<usize>().ok());
    let flag = |key: &str| set.get(key).map(|v| v == "true");
    match set.get("indent_style").map(String::as_str) {
        Some("space") => prefs.fill_tabs = true,
        Some("tab") => prefs.fill_tabs = false,
        _ => {}
    }
    // indent_size = tab means "as wide as a tab", which is tab_width
    if let Some(size) = number("indent_size").or_else(|| number("tab_width")) {
        prefs.tab_size = size.clamp(1, 16);
    }
    if let Some(on) = flag("trim_trailing_whitespace") {
        prefs.trim_trailing = on;
    }
    if let Some(on) = flag("insert_final_newline") {
        prefs.final_newline = on;
    }
    prefs
}

/// Every `.editorconfig` key that applies to `path`, its value
/// lowercased, the nearest file's word the last.
pub fn settings(path: &Path) -> BTreeMap<String, String> {
    let mut files = Vec::new();
    for dir in path.ancestors().skip(1) {
        let config = dir.join(".editorconfig");
        let Ok(text) = std::fs::read_to_string(&config) else {
            continue;
        };
        let root = text.lines().any(|line| {
            let line = line.trim().to_ascii_lowercase().replace(' ', "");
            line == "root=true"
        });
        files.push((dir.to_path_buf(), text));
        if root {
            break;
        }
    }
    let mut out = BTreeMap::new();
    // the furthest first, so the nearest has the last word
    for (dir, text) in files.into_iter().rev() {
        let mut applies = false;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(glob) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                applies = section_matches(glob, path, &dir);
                continue;
            }
            if !applies {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                out.insert(
                    key.trim().to_ascii_lowercase(),
                    value.trim().to_ascii_lowercase(),
                );
            }
        }
    }
    out
}

/// A section header against a file: a glob with no `/` in it is about
/// the name, one with a `/` about the path under the `.editorconfig`'s
/// directory. `{a,b}` is either.
fn section_matches(glob: &str, path: &Path, dir: &Path) -> bool {
    let subject = match glob.contains('/') {
        true => path
            .strip_prefix(dir)
            .map(|rel| rel.to_string_lossy().into_owned())
            .unwrap_or_default(),
        false => path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    let glob = glob.trim_start_matches('/').replace("**", "*");
    expand_braces(&glob)
        .iter()
        .any(|g| rcmd_core::glob::glob_match(g, &subject))
}

/// `*.{js,ts}` as `*.js` and `*.ts`; nested and repeated groups too.
fn expand_braces(glob: &str) -> Vec<String> {
    let Some(open) = glob.find('{') else {
        return vec![glob.to_string()];
    };
    let Some(close) = glob[open..].find('}').map(|at| open + at) else {
        return vec![glob.to_string()];
    };
    let (head, body, tail) = (&glob[..open], &glob[open + 1..close], &glob[close + 1..]);
    body.split(',')
        .flat_map(|alt| expand_braces(&format!("{head}{alt}{tail}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nearest_editorconfig_wins_and_the_file_decides_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(
            tmp.path().join(".editorconfig"),
            "[*]\nindent_style = tab\ntrim_trailing_whitespace = true\n",
        )
        .unwrap();
        std::fs::write(
            project.join(".editorconfig"),
            "[*.{rs,toml}]\nindent_style = space\nindent_size = 4\n\n[src/**.rs]\ninsert_final_newline = true\n",
        )
        .unwrap();
        let file = project.join("src/main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let ed = Editor::open(&file).unwrap();
        let prefs = for_file(&ed, Prefs::default());
        assert!(prefs.fill_tabs);
        assert_eq!(prefs.tab_size, 4);
        assert!(prefs.trim_trailing, "the outer file still counts");
        assert!(prefs.final_newline);

        // a root stops the walk
        std::fs::write(
            project.join(".editorconfig"),
            "root = true\n[*.md]\nindent_size = 2\n",
        )
        .unwrap();
        let set = settings(&file);
        assert!(!set.contains_key("trim_trailing_whitespace"));
        assert_eq!(expand_braces("*.{a,b}"), ["*.a", "*.b"]);
    }
}
