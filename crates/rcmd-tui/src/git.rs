//! Git awareness (feature `git`): [`scan`] computes a branch name and
//! one-character status marks for one directory of a work tree. It runs
//! on a background thread per directory change - libgit2 status walks
//! can take a while in big repositories and must never block the UI.
//!
//! Without the feature the module still compiles and [`scan`] returns
//! `None`, keeping the call sites free of `#[cfg]`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub const ENABLED: bool = cfg!(feature = "git");

pub struct GitStatus {
    pub branch: String,
    /// Mark per directory entry: 'U' in conflict, 'M' changed in the
    /// work tree, 'S' changed and staged (nothing unstaged on top),
    /// 'A' added, '?' untracked, '!' ignored. Changes inside a
    /// subdirectory collapse onto it, the loudest winning.
    pub marks: HashMap<OsString, char>,
}

#[cfg(not(feature = "git"))]
pub fn scan(_dir: &Path) -> Option<GitStatus> {
    None
}

/// What the last commit has of a file - its bytes, and its path in the
/// repository - for a diff against HEAD. `None` outside a work tree,
/// or for a file the commit does not have.
#[cfg(not(feature = "git"))]
pub fn head_blob(_path: &Path) -> Option<(Vec<u8>, String)> {
    None
}

#[cfg(feature = "git")]
pub fn head_blob(path: &Path) -> Option<(Vec<u8>, String)> {
    let path = path.canonicalize().ok()?;
    let repo = git2::Repository::discover(path.parent()?).ok()?;
    let workdir = repo.workdir()?.canonicalize().ok()?;
    let rel = path.strip_prefix(&workdir).ok()?;
    let tree = repo.head().ok()?.peel_to_tree().ok()?;
    let blob = tree
        .get_path(rel)
        .ok()?
        .to_object(&repo)
        .ok()?
        .peel_to_blob()
        .ok()?;
    Some((blob.content().to_vec(), rel.to_string_lossy().into_owned()))
}

/// Stage `paths` - files and whole directories - as `git add -A` would:
/// what is there goes in, what is gone comes out. How many were taken.
#[cfg(not(feature = "git"))]
pub fn stage(_paths: &[PathBuf]) -> Result<usize, String> {
    Err("built without git".into())
}

/// Take `paths` back out of the index to what HEAD has, as
/// `git restore --staged` would. The work tree is not touched.
#[cfg(not(feature = "git"))]
pub fn unstage(_paths: &[PathBuf]) -> Result<usize, String> {
    Err("built without git".into())
}

/// The local branches of the repository `dir` is in, and which one is
/// checked out.
#[cfg(not(feature = "git"))]
pub fn branches(_dir: &Path) -> Result<(Vec<String>, Option<String>), String> {
    Err("built without git".into())
}

/// Check a branch out, safely: a change it would overwrite stops it.
#[cfg(not(feature = "git"))]
pub fn switch(_dir: &Path, _name: &str) -> Result<(), String> {
    Err("built without git".into())
}

/// The repository `paths` are in, and each one relative to its work
/// tree. A path that is gone (a deleted file to be staged) is found
/// through its directory.
#[cfg(feature = "git")]
fn in_repo(paths: &[PathBuf]) -> Result<(git2::Repository, Vec<PathBuf>), String> {
    let real = |path: &Path| -> Option<PathBuf> {
        path.canonicalize()
            .ok()
            .or_else(|| Some(path.parent()?.canonicalize().ok()?.join(path.file_name()?)))
    };
    let first = paths
        .first()
        .and_then(|p| real(p))
        .ok_or("nothing to act on")?;
    let repo = git2::Repository::discover(first.parent().unwrap_or(&first))
        .map_err(|_| "not in a git repository".to_string())?;
    let workdir = repo
        .workdir()
        .and_then(|w| w.canonicalize().ok())
        .ok_or("a bare repository has no work tree")?;
    let mut rels = Vec::new();
    for path in paths {
        let path = real(path).ok_or_else(|| format!("{}: not found", path.display()))?;
        let rel = path
            .strip_prefix(&workdir)
            .map_err(|_| format!("{}: outside the work tree", path.display()))?;
        rels.push(rel.to_path_buf());
    }
    Ok((repo, rels))
}

#[cfg(feature = "git")]
pub fn stage(paths: &[PathBuf]) -> Result<usize, String> {
    let (repo, rels) = in_repo(paths)?;
    let mut index = repo.index().map_err(|e| e.message().to_string())?;
    let workdir = repo.workdir().ok_or("no work tree")?.to_path_buf();
    for rel in &rels {
        let result = if workdir.join(rel).is_dir() {
            // new files in it, then changed and deleted ones
            let spec = [rel.as_path()];
            index
                .add_all(spec, git2::IndexAddOption::DEFAULT, None)
                .and_then(|()| index.update_all(spec, None))
        } else if workdir.join(rel).symlink_metadata().is_ok() {
            index.add_path(rel)
        } else {
            index.remove_path(rel)
        };
        result.map_err(|e| format!("{}: {}", rel.display(), e.message()))?;
    }
    index.write().map_err(|e| e.message().to_string())?;
    Ok(rels.len())
}

#[cfg(feature = "git")]
pub fn unstage(paths: &[PathBuf]) -> Result<usize, String> {
    let (repo, rels) = in_repo(paths)?;
    match repo.head().and_then(|h| h.peel_to_commit()) {
        Ok(head) => repo
            .reset_default(Some(head.as_object()), rels.iter())
            .map_err(|e| e.message().to_string())?,
        // nothing committed yet: out of the index is all there is
        Err(_) => {
            let mut index = repo.index().map_err(|e| e.message().to_string())?;
            for rel in &rels {
                index
                    .remove_all([rel.as_path()], None)
                    .map_err(|e| e.message().to_string())?;
            }
            index.write().map_err(|e| e.message().to_string())?;
        }
    }
    Ok(rels.len())
}

#[cfg(feature = "git")]
pub fn branches(dir: &Path) -> Result<(Vec<String>, Option<String>), String> {
    let repo =
        git2::Repository::discover(dir).map_err(|_| "not in a git repository".to_string())?;
    let mut names: Vec<String> = repo
        .branches(Some(git2::BranchType::Local))
        .map_err(|e| e.message().to_string())?
        .flatten()
        .filter_map(|(branch, _)| branch.name().ok().flatten().map(str::to_string))
        .collect();
    names.sort();
    let current = repo
        .head()
        .ok()
        .filter(|h| h.is_branch())
        .and_then(|h| h.shorthand().map(str::to_string));
    Ok((names, current))
}

#[cfg(feature = "git")]
pub fn switch(dir: &Path, name: &str) -> Result<(), String> {
    let repo =
        git2::Repository::discover(dir).map_err(|_| "not in a git repository".to_string())?;
    let branch = repo
        .find_branch(name, git2::BranchType::Local)
        .map_err(|e| e.message().to_string())?;
    let target = branch
        .get()
        .peel(git2::ObjectType::Commit)
        .map_err(|e| e.message().to_string())?;
    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.safe();
    repo.checkout_tree(&target, Some(&mut checkout))
        .map_err(|e| e.message().to_string())?;
    let refname = branch.get().name().ok_or("the branch name is not UTF-8")?;
    repo.set_head(refname).map_err(|e| e.message().to_string())
}

/// A skip predicate for find (R3): true for gitignored paths and the
/// `.git` directory itself. `None` when `root` is not inside a work
/// tree (or the feature is off) - find then walks everything.
#[cfg(not(feature = "git"))]
pub fn ignore_filter(_root: &Path) -> Option<rcmd_core::find::SkipFn> {
    None
}

#[cfg(feature = "git")]
pub fn ignore_filter(root: &Path) -> Option<rcmd_core::find::SkipFn> {
    let repo = git2::Repository::discover(root).ok()?;
    repo.workdir()?;
    // find walks on several threads and a Repository is Send but not
    // Sync; the lock costs little next to the readdir it saves, and the
    // `.git` shortcut answers the hottest path without taking it
    let repo = std::sync::Mutex::new(repo);
    Some(Box::new(move |path: &Path| {
        if path.file_name().is_some_and(|n| n == ".git") {
            return true;
        }
        repo.lock()
            .is_ok_and(|repo| repo.is_path_ignored(path).unwrap_or(false))
    }))
}

/// Higher wins when several changed paths collapse onto one entry.
#[cfg(feature = "git")]
fn rank(mark: char) -> u8 {
    match mark {
        'U' => 5,
        'M' => 4,
        'S' => 3,
        'A' => 2,
        '?' => 1,
        _ => 0,
    }
}

/// None when `dir` is not inside a git work tree (or anything fails -
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

    const UNSTAGED: Status = Status::WT_MODIFIED
        .union(Status::WT_TYPECHANGE)
        .union(Status::WT_RENAMED)
        .union(Status::WT_DELETED);
    const STAGED: Status = Status::INDEX_MODIFIED
        .union(Status::INDEX_TYPECHANGE)
        .union(Status::INDEX_RENAMED)
        .union(Status::INDEX_DELETED);

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
        let mark = if status.contains(Status::CONFLICTED) {
            'U'
        } else if status.intersects(UNSTAGED) {
            'M'
        } else if status.intersects(STAGED) {
            'S'
        } else if status.contains(Status::INDEX_NEW) {
            if deep { 'M' } else { 'A' }
        } else if status.contains(Status::WT_NEW) {
            if deep { 'M' } else { '?' }
        } else if status.contains(Status::IGNORED) && !deep {
            // libgit2 reports an empty directory as ignored, where git
            // itself says nothing about it - it has no state at all; so
            // a directory is `!` only when an ignore rule actually names it
            let dir_ignored =
                !workdir.join(path).is_dir() || repo.is_path_ignored(path).unwrap_or(false);
            if !dir_ignored {
                continue;
            }
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
    // how far the branch is from what it tracks, as the prompt of
    // every git-aware shell says it
    let branch = match ahead_behind(&repo) {
        Some((0, 0)) | None => branch,
        Some((ahead, behind)) => {
            let mut out = branch;
            if ahead > 0 {
                out.push_str(&format!(" \u{2191}{ahead}"));
            }
            if behind > 0 {
                out.push_str(&format!(" \u{2193}{behind}"));
            }
            out
        }
    };
    Some(GitStatus { branch, marks })
}

/// Commits ahead of and behind the branch's upstream; `None` without one.
#[cfg(feature = "git")]
fn ahead_behind(repo: &git2::Repository) -> Option<(usize, usize)> {
    let head = repo.head().ok()?;
    let name = head.shorthand()?;
    let branch = repo.find_branch(name, git2::BranchType::Local).ok()?;
    let upstream = branch.upstream().ok()?;
    repo.graph_ahead_behind(head.target()?, upstream.get().target()?)
        .ok()
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
        fs::create_dir(root.join("empty")).unwrap();

        let st = scan(root).unwrap();
        // an empty directory has no git state - libgit2 calls it
        // ignored, git does not mention it, and neither does the column
        assert_eq!(st.marks.get(OsStr::new("empty")), None);
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
    fn staged_apart_from_unstaged_and_how_far_ahead() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let repo = git2::Repository::init(root).unwrap();
        fs::write(root.join("a.txt"), "one").unwrap();
        commit_all(&repo, &["a.txt"]);
        // a local branch standing where we are is the upstream
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("base", &head, false).unwrap();
        fs::write(root.join("a.txt"), "two").unwrap();
        commit_all(&repo, &["a.txt"]);
        let name = repo.head().unwrap().shorthand().unwrap().to_string();
        repo.find_branch(&name, git2::BranchType::Local)
            .unwrap()
            .set_upstream(Some("base"))
            .unwrap();

        fs::write(root.join("a.txt"), "three").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let st = scan(root).unwrap();
        assert_eq!(st.marks.get(OsStr::new("a.txt")), Some(&'S'));
        assert!(st.branch.ends_with("\u{2191}1"), "{}", st.branch);

        // changed again on top of what is staged: unstaged wins
        fs::write(root.join("a.txt"), "four").unwrap();
        let st = scan(root).unwrap();
        assert_eq!(st.marks.get(OsStr::new("a.txt")), Some(&'M'));
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
            return; // environment has a repo above tmp - nothing to assert
        }
        assert!(scan(dir.path()).is_none());
    }
}

#[cfg(all(test, feature = "git"))]
mod index_tests {
    use super::*;

    /// A repository with one commit of `a.txt` and `dir/b.txt` on its
    /// first branch, and a second branch `other` that adds `c.txt`.
    fn repo() -> (tempfile::TempDir, git2::Repository) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        std::fs::create_dir(tmp.path().join("dir")).unwrap();
        std::fs::write(tmp.path().join("a.txt"), "a\n").unwrap();
        std::fs::write(tmp.path().join("dir/b.txt"), "b\n").unwrap();
        let sig = git2::Signature::now("t", "t@example.com").unwrap();
        let commit = |repo: &git2::Repository, message: &str| {
            let mut index = repo.index().unwrap();
            index
                .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
                .unwrap();
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            let parents: Vec<git2::Commit> = repo
                .head()
                .ok()
                .and_then(|h| h.peel_to_commit().ok())
                .into_iter()
                .collect();
            let parents: Vec<&git2::Commit> = parents.iter().collect();
            repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
                .unwrap()
        };
        commit(&repo, "first");
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        let main = repo.head().unwrap().shorthand().unwrap().to_string();
        repo.branch("other", &head, false).unwrap();
        repo.set_head("refs/heads/other").unwrap();
        std::fs::write(tmp.path().join("c.txt"), "c\n").unwrap();
        commit(&repo, "second");
        repo.set_head(&format!("refs/heads/{main}")).unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        drop(head);
        (tmp, repo)
    }

    fn staged(repo: &git2::Repository) -> Vec<String> {
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(true);
        repo.statuses(Some(&mut opts))
            .unwrap()
            .iter()
            .filter(|s| {
                s.status().intersects(
                    git2::Status::INDEX_NEW
                        | git2::Status::INDEX_MODIFIED
                        | git2::Status::INDEX_DELETED,
                )
            })
            .map(|s| s.path().unwrap().to_string())
            .collect()
    }

    #[test]
    fn stage_and_unstage_files_directories_and_deletions() {
        let (tmp, repo) = repo();
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), "changed\n").unwrap();
        std::fs::write(root.join("dir/new.txt"), "new\n").unwrap();
        std::fs::remove_file(root.join("dir/b.txt")).unwrap();
        assert_eq!(stage(&[root.join("a.txt"), root.join("dir")]), Ok(2));
        let mut got = staged(&repo);
        got.sort();
        assert_eq!(got, ["a.txt", "dir/b.txt", "dir/new.txt"]);
        assert_eq!(unstage(&[root.join("dir")]), Ok(1));
        assert_eq!(staged(&repo), ["a.txt"]);
        // a deleted file named on its own
        assert_eq!(stage(&[root.join("dir/b.txt")]), Ok(1));
        assert!(staged(&repo).contains(&"dir/b.txt".to_string()));
        // the work tree is left as it was
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "changed\n"
        );
        assert!(stage(&[PathBuf::from("/")]).is_err());
    }

    #[test]
    fn branches_are_listed_and_switched_to_safely() {
        let (tmp, _repo) = repo();
        let root = tmp.path();
        let (names, current) = branches(root).unwrap();
        assert!(names.contains(&"other".to_string()), "{names:?}");
        let home = current.unwrap();
        assert!(!root.join("c.txt").exists());
        switch(root, "other").unwrap();
        assert!(root.join("c.txt").exists());
        assert_eq!(branches(root).unwrap().1.as_deref(), Some("other"));
        // a change the checkout would overwrite stops it
        std::fs::write(root.join("c.txt"), "mine\n").unwrap();
        assert!(switch(root, &home).is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("c.txt")).unwrap(),
            "mine\n"
        );
    }
}
