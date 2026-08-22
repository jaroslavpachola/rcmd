//! Bulk rename (R3): the marked names become a numbered text buffer the
//! user edits in the built-in editor; the saved diff turns into renames
//! and deletes. The parsing and the two-phase apply live here where
//! they are unit-testable - the TUI owns only the editor session and
//! the preview dialog.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io;
use std::path::Path;

/// One line per name: `<index>\t<name>`. The index keys every line back
/// to its original, so deleted lines and renames stay unambiguous even
/// after heavy editing.
pub fn buffer_for(names: &[OsString]) -> String {
    let mut out = String::new();
    for (i, name) in names.iter().enumerate() {
        out.push_str(&format!("{i}\t{}\n", name.to_string_lossy()));
    }
    out
}

/// What the edited buffer asks for.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// (original name, new relative name).
    pub renames: Vec<(OsString, String)>,
    /// Names whose lines disappeared.
    pub deletes: Vec<OsString>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.renames.is_empty() && self.deletes.is_empty()
    }
}

/// Diff the edited buffer against the original names. Every kept line
/// must still carry its number; a removed line means "delete", a
/// changed name means "rename". Errors abort the whole operation -
/// nothing is ever applied from a buffer that doesn't parse.
pub fn parse(buffer: &str, names: &[OsString]) -> Result<Plan, String> {
    let mut seen = vec![false; names.len()];
    let mut renames = Vec::new();
    let mut targets = HashSet::new();
    for (i, line) in buffer.lines().enumerate() {
        let lineno = i + 1;
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim().is_empty() {
            continue;
        }
        let (idx, new_name) = line.split_once('\t').ok_or(format!(
            "line {lineno}: no tab - keep the number column, delete whole lines to delete files"
        ))?;
        let idx: usize = idx
            .trim()
            .parse()
            .map_err(|_| format!("line {lineno}: bad index '{}'", idx.trim()))?;
        if idx >= names.len() {
            return Err(format!("line {lineno}: unknown index {idx}"));
        }
        if seen[idx] {
            return Err(format!("line {lineno}: index {idx} appears twice"));
        }
        seen[idx] = true;
        if new_name.is_empty() {
            return Err(format!(
                "line {lineno}: empty name - delete the whole line to delete the file"
            ));
        }
        if !targets.insert(new_name.to_string()) {
            return Err(format!("line {lineno}: duplicate target '{new_name}'"));
        }
        if names[idx].to_string_lossy() == new_name {
            continue;
        }
        let path = Path::new(new_name);
        if new_name.starts_with('/')
            || new_name.ends_with('/')
            || path.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
        {
            return Err(format!("line {lineno}: '{new_name}' is not a valid name"));
        }
        renames.push((names[idx].clone(), new_name.to_string()));
    }
    let deletes = names
        .iter()
        .zip(&seen)
        .filter(|(_, seen)| !**seen)
        .map(|(name, _)| name.clone())
        .collect();
    Ok(Plan { renames, deletes })
}

/// Two-phase apply inside `dir`: every source first moves to a unique
/// temp name, then the temps move onto the final names - swaps and
/// chains need no special-casing. A target that still exists (it was
/// never part of the buffer, or its delete has not happened yet) is
/// refused and that item returns to its original name; phase-1 failures
/// roll everything back.
pub fn apply(dir: &Path, renames: &[(OsString, String)]) -> Result<(), String> {
    let pid = std::process::id();
    let temp_of = |i: usize| dir.join(format!(".rcmd-bulk-{pid}-{i}"));
    for (i, (old, _)) in renames.iter().enumerate() {
        if let Err(err) = std::fs::rename(dir.join(old), temp_of(i)) {
            for (j, (undo, _)) in renames.iter().enumerate().take(i) {
                let _ = std::fs::rename(temp_of(j), dir.join(undo));
            }
            return Err(format!("{}: {err}", Path::new(old).display()));
        }
    }
    let mut errors = Vec::new();
    for (i, (old, new)) in renames.iter().enumerate() {
        let target = dir.join(new);
        let result = if target.symlink_metadata().is_ok() {
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "target exists",
            ))
        } else {
            std::fs::rename(temp_of(i), &target)
        };
        if let Err(err) = result {
            let _ = std::fs::rename(temp_of(i), dir.join(old));
            errors.push(format!("{} → {new}: {err}", Path::new(old).display()));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn names(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn buffer_round_trips_unchanged() {
        let n = names(&["a.txt", "b.txt"]);
        let buf = buffer_for(&n);
        assert_eq!(buf, "0\ta.txt\n1\tb.txt\n");
        assert!(parse(&buf, &n).unwrap().is_empty());
    }

    #[test]
    fn renames_and_deletes_are_detected() {
        let n = names(&["a.txt", "b.txt", "c.txt"]);
        let plan = parse("0\tz.txt\n2\tc.txt\n", &n).unwrap();
        assert_eq!(plan.renames, [(OsString::from("a.txt"), "z.txt".into())]);
        assert_eq!(plan.deletes, [OsString::from("b.txt")]);
    }

    #[test]
    fn parse_rejects_bad_buffers() {
        let n = names(&["a", "b"]);
        assert!(parse("a-renamed\n", &n).is_err()); // number column gone
        assert!(parse("7\ta\n", &n).is_err()); // unknown index
        assert!(parse("0\ta\n0\tb\n", &n).is_err()); // index twice
        assert!(parse("0\tsame\n1\tsame\n", &n).is_err()); // duplicate target
        assert!(parse("0\t\n", &n).is_err()); // empty name
        assert!(parse("0\t/etc/passwd\n", &n).is_err()); // absolute
        assert!(parse("0\t../escape\n", &n).is_err()); // parent escape
        // an unchanged line never trips the name validation
        let odd = names(&["weird/kept"]);
        assert!(parse("0\tweird/kept\n", &odd).unwrap().is_empty());
    }

    #[test]
    fn apply_handles_swaps_and_chains() {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in [("a", "1"), ("b", "2"), ("c", "3")] {
            fs::write(dir.path().join(name), content).unwrap();
        }
        // swap a and b, chain c → d
        let renames = [
            (OsString::from("a"), "b".to_string()),
            (OsString::from("b"), "a".to_string()),
            (OsString::from("c"), "d".to_string()),
        ];
        apply(dir.path(), &renames).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("b")).unwrap(), "1");
        assert_eq!(fs::read_to_string(dir.path().join("a")).unwrap(), "2");
        assert_eq!(fs::read_to_string(dir.path().join("d")).unwrap(), "3");
        assert!(!dir.path().join("c").exists());
    }

    #[test]
    fn apply_refuses_occupied_targets_and_restores() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), "1").unwrap();
        fs::write(dir.path().join("bystander"), "x").unwrap();
        let renames = [(OsString::from("a"), "bystander".to_string())];
        let err = apply(dir.path(), &renames).unwrap_err();
        assert!(err.contains("target exists"), "{err}");
        // nothing lost: both files still there under their old names
        assert_eq!(fs::read_to_string(dir.path().join("a")).unwrap(), "1");
        assert_eq!(
            fs::read_to_string(dir.path().join("bystander")).unwrap(),
            "x"
        );
    }

    #[test]
    fn apply_rolls_back_when_a_source_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), "1").unwrap();
        let renames = [
            (OsString::from("a"), "x".to_string()),
            (OsString::from("ghost"), "y".to_string()),
        ];
        assert!(apply(dir.path(), &renames).is_err());
        assert_eq!(fs::read_to_string(dir.path().join("a")).unwrap(), "1");
        assert!(!dir.path().join("x").exists());
    }
}
