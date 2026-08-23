//! A patch, browsed as the files it touches. Each entry is that one
//! file's slice of the diff, so `src/main.rs` inside a 4000-line patch
//! opens as just the hunks that touch it - and because the names are
//! paths, the whole patch lists as the tree it would apply to.
//!
//! Unified diffs (`--- a/x` / `+++ b/x`), git's own (`diff --git`),
//! context diffs (`*** x` / `--- x`) and Subversion's `Index:` headers
//! all start a section. Nothing is applied or reversed: this is a way
//! of reading a patch, not of using one.

use std::path::PathBuf;

/// One file's part of the patch: where it starts in the text and how
/// far it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Piece {
    pub path: PathBuf,
    pub at: usize,
    pub len: usize,
    /// Lines added and removed, for a listing that says something.
    pub added: usize,
    pub removed: usize,
}

/// Split a patch into one piece per file it touches.
pub fn split(text: &str) -> Vec<Piece> {
    let mut starts: Vec<(usize, Option<String>)> = Vec::new();
    let lines: Vec<(usize, &str)> = line_offsets(text);

    for (i, (at, line)) in lines.iter().enumerate() {
        let next = lines.get(i + 1).map(|(_, l)| *l).unwrap_or("");
        // a header that names the file two lines down
        if line.starts_with("diff ") || line.starts_with("Index: ") {
            starts.push((*at, None));
            continue;
        }
        // "--- old" followed by "+++ new" is a unified diff's header,
        // and "*** old" followed by "--- new" is a context diff's
        let unified = line.starts_with("--- ") && next.starts_with("+++ ");
        let context = line.starts_with("*** ") && next.starts_with("--- ");
        if unified || context {
            let old = header_path(line);
            let new = header_path(next);
            let name = pick(old, new);
            // a "diff"/"Index:" line just above already opened this one
            match starts.last_mut() {
                Some((_, slot @ None)) if opened_just_above(&lines, i) => *slot = name,
                _ => starts.push((*at, name)),
            }
        }
    }

    let mut out = Vec::new();
    for (index, (at, name)) in starts.iter().enumerate() {
        let end = starts
            .get(index + 1)
            .map(|(at, _)| *at)
            .unwrap_or(text.len());
        let Some(name) = name else {
            continue; // a header whose file we never learned
        };
        let body = &text[*at..end];
        out.push(Piece {
            path: PathBuf::from(name),
            at: *at,
            len: end - *at,
            added: count(body, '+'),
            removed: count(body, '-'),
        });
    }
    out
}

/// A "diff"/"Index:" header opens a section that the `---`/`+++` pair
/// two or three lines later names. Anything further down is a new one.
fn opened_just_above(lines: &[(usize, &str)], at: usize) -> bool {
    lines[at.saturating_sub(6)..at]
        .iter()
        .any(|(_, l)| l.starts_with("diff ") || l.starts_with("Index: "))
}

/// "--- a/src/main.rs\t2026-08-23 10:00:00" → "src/main.rs".
fn header_path(line: &str) -> Option<String> {
    let rest = line.get(4..)?.trim_end();
    // the timestamp diff(1) appends is separated by a tab
    let rest = rest.split('\t').next()?.trim_end();
    if rest.is_empty() || rest == "/dev/null" {
        return None;
    }
    // git and diff -N write a/ and b/ prefixes
    let rest = rest
        .strip_prefix("a/")
        .or_else(|| rest.strip_prefix("b/"))
        .unwrap_or(rest);
    let rest = rest.trim_start_matches('/');
    (!rest.is_empty()).then(|| rest.to_string())
}

/// The new name is the file's name; a deletion has none, so the old one
/// stands in.
fn pick(old: Option<String>, new: Option<String>) -> Option<String> {
    new.or(old)
}

fn count(body: &str, sign: char) -> usize {
    body.lines()
        .filter(|line| {
            line.starts_with(sign)
                && !line.starts_with("+++ ")
                && !line.starts_with("--- ")
                && !line.starts_with("---")
        })
        .count()
}

fn line_offsets(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        out.push((at, line.trim_end_matches(['\n', '\r'])));
        at += line.len();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIT: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 1234567..89abcde 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,4 @@
 fn main() {
-    println!(\"old\");
+    println!(\"new\");
+    println!(\"and more\");
 }
diff --git a/docs/readme.md b/docs/readme.md
--- a/docs/readme.md
+++ b/docs/readme.md
@@ -1 +1 @@
-old title
+new title
";

    #[test]
    fn a_git_patch_lists_the_files_it_touches() {
        let pieces = split(GIT);
        let names: Vec<_> = pieces
            .iter()
            .map(|p| p.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["src/main.rs", "docs/readme.md"]);

        // each piece must be exactly its own file's part of the patch
        let first = &GIT[pieces[0].at..pieces[0].at + pieces[0].len];
        assert!(first.starts_with("diff --git a/src/main.rs"));
        assert!(first.contains("and more"));
        assert!(!first.contains("readme"));

        assert_eq!(pieces[0].added, 2);
        assert_eq!(pieces[0].removed, 1);
        assert_eq!(pieces[1].added, 1);
        assert_eq!(pieces[1].removed, 1);
        // the pieces cover the patch with nothing left over
        assert_eq!(pieces[1].at + pieces[1].len, GIT.len());
    }

    #[test]
    fn a_plain_unified_diff_needs_no_git_header() {
        let text = "\
--- old/hello.c\t2026-08-01 12:00:00.000000000 +0200
+++ new/hello.c\t2026-08-02 12:00:00.000000000 +0200
@@ -1 +1 @@
-int main(void) { return 1; }
+int main(void) { return 0; }
";
        let pieces = split(text);
        assert_eq!(pieces.len(), 1);
        // the tab-separated timestamp is not part of the name
        assert_eq!(pieces[0].path, PathBuf::from("new/hello.c"));
    }

    #[test]
    fn a_deleted_file_is_named_by_its_old_path() {
        let text = "\
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-it was here
";
        let pieces = split(text);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].path, PathBuf::from("gone.txt"));
        assert_eq!(pieces[0].removed, 1);
    }

    #[test]
    fn a_context_diff_starts_a_section_too() {
        let text = "\
*** old.c\t2026-08-01
--- new.c\t2026-08-02
***************
*** 1,3 ****
! one
--- 1,3 ----
! two
";
        let pieces = split(text);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].path, PathBuf::from("new.c"));
    }

    #[test]
    fn subversion_index_headers_open_a_section() {
        let text = "\
Index: trunk/thing.c
===================================================================
--- trunk/thing.c\t(revision 12)
+++ trunk/thing.c\t(working copy)
@@ -1 +1 @@
-a
+b
";
        let pieces = split(text);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].path, PathBuf::from("trunk/thing.c"));
    }

    #[test]
    fn text_that_is_not_a_patch_yields_nothing() {
        assert!(split("just some notes\nand a second line\n").is_empty());
        assert!(split("").is_empty());
    }

    #[test]
    fn a_real_git_diff_splits_the_way_git_shows_it() {
        // built by git itself rather than typed out, so the shape is
        // whatever git actually writes today
        let tmp = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(tmp.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example")
                .output()
        };
        if run(&["init", "-q"])
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            eprintln!("skipping: no usable git");
            return;
        }
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/a.txt"), "one\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "two\n").unwrap();
        run(&["add", "-A"]).unwrap();
        run(&["commit", "-qm", "first"]).unwrap();
        std::fs::write(tmp.path().join("src/a.txt"), "one\nmore\n").unwrap();
        std::fs::remove_file(tmp.path().join("b.txt")).unwrap();
        std::fs::write(tmp.path().join("c.txt"), "three\n").unwrap();
        run(&["add", "-A"]).unwrap();

        let out = run(&["diff", "--cached"]).unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let mut names: Vec<_> = split(&text)
            .iter()
            .map(|p| p.path.to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["b.txt", "c.txt", "src/a.txt"], "{text}");
    }
}
