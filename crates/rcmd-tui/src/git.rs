//! Git awareness (feature `git`): [`scan`] computes a branch name and
//! one-character status marks for one directory of a work tree. It runs
//! on a background thread per directory change — libgit2 status walks
//! can take a while in big repositories and must never block the UI.
//!
//! Without the feature the module still compiles and [`scan`] returns
//! `None`, keeping the call sites free of `#[cfg]`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;

pub const ENABLED: bool = cfg!(feature = "git");

pub struct GitStatus {
    pub branch: String,
    /// Mark per directory entry: 'M' modified, 'A' added, '?' untracked,
    /// '!' ignored. Changes inside a subdirectory collapse to 'M' on it.
    pub marks: HashMap<OsString, char>,
}

#[cfg(not(feature = "git"))]
pub fn scan(_dir: &Path) -> Option<GitStatus> {
    None
}

/// Higher wins when several changed paths collapse onto one entry.
#[cfg(feature = "git")]
fn rank(mark: char) -> u8 {
    match mark {
        'M' => 3,
        'A' => 2,
        '?' => 1,
        _ => 0,
    }
}

/// None when `dir` is not inside a git work tree (or anything fails —
/// the panel then simply shows no git column).
#[cfg(feature = "git")]
pub fn scan(dir: &Path) -> Option<GitStatus> {
    use git2::Status;

    let repo = git2::Repository::discover(dir).ok()?;
    let workdir = repo.workdir()?.canonicalize().ok()?;
    let rel = dir
        .canonicalize()
        .ok()?
        .strip_prefix(&workdir)
        .ok()?
        .to_path_buf();

    let branch = match repo.head() {
        // detached HEAD: a short commit id instead of a branch name
        Ok(head) => match head.shorthand() {
            Some("HEAD") | None => head
                .target()
                .map(|id| id.to_string()[..8].to_string())
                .unwrap_or_default(),
            Some(name) => name.to_string(),
        },
        // unborn branch: the name HEAD points at
        Err(_) => repo
            .find_reference("HEAD")
            .ok()
            .and_then(|r| r.symbolic_target().map(str::to_string))
            .map(|t| t.trim_start_matches("refs/heads/").to_string())
            .unwrap_or_default(),
    };

    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(false)
        .include_ignored(true)
        .recurse_ignored_dirs(false)
        .exclude_submodules(true);
    if !rel.as_os_str().is_empty() {
        opts.pathspec(&rel);
    }
    let statuses = repo.statuses(Some(&mut opts)).ok()?;

    const MODIFIED: Status = Status::WT_MODIFIED
        .union(Status::INDEX_MODIFIED)
        .union(Status::WT_TYPECHANGE)
        .union(Status::INDEX_TYPECHANGE)
        .union(Status::WT_RENAMED)
        .union(Status::INDEX_RENAMED)
        .union(Status::WT_DELETED)
        .union(Status::INDEX_DELETED)
        .union(Status::CONFLICTED);

    let mut marks: HashMap<OsString, char> = HashMap::new();
    for entry in statuses.iter() {
        let Some(path) = entry.path() else { continue };
        let path = Path::new(path.trim_end_matches('/'));
        let Ok(rest) = path.strip_prefix(&rel) else {
            continue;
        };
        let Some(first) = rest.components().next() else {
            continue;
        };
        // a change somewhere below a subdirectory marks the subdirectory
        let deep = rest.components().nth(1).is_some();
        let status = entry.status();
        let mark = if status.intersects(MODIFIED) {
            'M'
        } else if status.contains(Status::INDEX_NEW) {
            if deep { 'M' } else { 'A' }
        } else if status.contains(Status::WT_NEW) {
            if deep { 'M' } else { '?' }
        } else if status.contains(Status::IGNORED) && !deep {
            '!'
        } else {
            continue;
        };
        marks
            .entry(first.as_os_str().to_os_string())
            .and_modify(|m| {
                if rank(mark) > rank(*m) {
                    *m = mark;
                }
            })
            .or_insert(mark);
    }
    Some(GitStatus { branch, marks })
}

#[cfg(all(test, feature = "git"))]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::fs;

    fn commit_all(repo: &git2::Repository, paths: &[&str]) {
        let mut index = repo.index().unwrap();
        for path in paths {
            index.add_path(Path::new(path)).unwrap();
        }
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("test", "test@example.com").unwrap();
        let parent = repo
            .head()
            .ok()
            .and_then(|h| h.peel_to_commit().ok())
            .into_iter()
            .collect::<Vec<_>>();
        let parents: Vec<_> = parent.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, "commit", &tree, &parents)
            .unwrap();
    }

    #[test]
    fn scan_marks_and_branch() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let repo = git2::Repository::init(root).unwrap();
        fs::write(root.join("committed.txt"), "one").unwrap();
        fs::write(root.join(".gitignore"), "ignored.log\n").unwrap();
        commit_all(&repo, &["committed.txt", ".gitignore"]);

        fs::write(root.join("committed.txt"), "two").unwrap();
        fs::write(root.join("untracked.txt"), "x").unwrap();
        fs::write(root.join("ignored.log"), "x").unwrap();
        fs::write(root.join("added.txt"), "x").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("added.txt")).unwrap();
        index.write().unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/inner.txt"), "x").unwrap();

        let st = scan(root).unwrap();
        assert!(!st.branch.is_empty());
        assert_eq!(st.marks.get(OsStr::new("committed.txt")), Some(&'M'));
        assert_eq!(st.marks.get(OsStr::new("added.txt")), Some(&'A'));
        assert_eq!(st.marks.get(OsStr::new("untracked.txt")), Some(&'?'));
        assert_eq!(st.marks.get(OsStr::new("ignored.log")), Some(&'!'));
        // untracked directory shows as one untracked entry
        assert_eq!(st.marks.get(OsStr::new("sub")), Some(&'?'));
        assert_eq!(st.marks.get(OsStr::new(".gitignore")), None);
    }

    #[test]
    fn scan_inside_a_subdirectory_and_aggregation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let repo = git2::Repository::init(root).unwrap();
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::write(root.join("src/lib.txt"), "one").unwrap();
        fs::write(root.join("src/nested/deep.txt"), "one").unwrap();
        commit_all(&repo, &["src/lib.txt", "src/nested/deep.txt"]);

        fs::write(root.join("src/nested/deep.txt"), "two").unwrap();
        // scanning src/ sees the tracked change below nested/ as 'M' on it
        let st = scan(&root.join("src")).unwrap();
        assert_eq!(st.marks.get(OsStr::new("nested")), Some(&'M'));
        assert_eq!(st.marks.get(OsStr::new("lib.txt")), None);
        // scanning the root aggregates everything onto src/
        let st = scan(root).unwrap();
        assert_eq!(st.marks.get(OsStr::new("src")), Some(&'M'));
    }

    #[test]
    fn unborn_repo_still_names_its_branch() {
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        let st = scan(dir.path()).unwrap();
        assert!(!st.branch.is_empty());
    }

    #[test]
    fn outside_a_repo_is_none() {
        let dir = tempfile::tempdir().unwrap();
        // tempdirs can live under a repo-owned path in odd setups; a
        // subdir of / is the closest we get to "definitely no repo"
        if git2::Repository::discover(dir.path()).is_ok() {
            return; // environment has a repo above tmp — nothing to assert
        }
        assert!(scan(dir.path()).is_none());
    }
}
