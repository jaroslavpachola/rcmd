//! Synchronize's comparison: two trees walked side by side, through
//! whatever [`FsProvider`] each is on - so one of them can be a server
//! that has never heard of rsync - and every place they disagree.
//!
//! A directory only one side has is one difference, not one per file in
//! it: copying it is one step, and a plan of ten thousand rows for one
//! new directory is a plan nobody reads. A directory both have is walked
//! into.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};

use crate::compare::{self, Mode};
use crate::entry::Entry;
use crate::vfs::FsProvider;

/// One place the trees disagree: `rel` is the path under both roots,
/// and each side's entry there, if it has one.
#[derive(Debug, Clone)]
pub struct Difference {
    pub rel: PathBuf,
    pub left: Option<Entry>,
    pub right: Option<Entry>,
}

impl Difference {
    /// A file on one side and a directory on the other: no copy makes
    /// those agree without deleting something first.
    pub fn clash(&self) -> bool {
        matches!((&self.left, &self.right), (Some(l), Some(r)) if l.is_dir() != r.is_dir())
    }
}

pub enum ScanEvent {
    /// The directory being read, for the status line.
    At(PathBuf),
    Done(io::Result<Vec<Difference>>),
}

pub struct ScanHandle {
    pub events: Receiver<ScanEvent>,
    cancel: Arc<AtomicBool>,
}

impl ScanHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Compare the trees under two roots on a thread of their own.
pub fn spawn_scan(
    left: (Arc<dyn FsProvider>, PathBuf),
    right: (Arc<dyn FsProvider>, PathBuf),
    mode: Mode,
) -> ScanHandle {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    std::thread::spawn(move || {
        let mut progress = |rel: &Path| {
            let _ = tx.send(ScanEvent::At(rel.to_path_buf()));
        };
        let result = scan(
            (&*left.0, &left.1),
            (&*right.0, &right.1),
            mode,
            &flag,
            &mut progress,
        );
        let _ = tx.send(ScanEvent::Done(result));
    });
    ScanHandle { events: rx, cancel }
}

/// Every difference between the trees, in path order. The roots have
/// to be readable; a directory below them that is not is a
/// difference of its own rather than the end of the walk.
pub fn scan(
    left: (&dyn FsProvider, &Path),
    right: (&dyn FsProvider, &Path),
    mode: Mode,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&Path),
) -> io::Result<Vec<Difference>> {
    let mut out = Vec::new();
    let lroot = left.0.read_dir(left.1)?;
    let rroot = right.0.read_dir(right.1)?;
    walk(
        left,
        right,
        PathBuf::new(),
        (lroot, rroot),
        mode,
        cancel,
        progress,
        &mut out,
    )?;
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn walk(
    left: (&dyn FsProvider, &Path),
    right: (&dyn FsProvider, &Path),
    rel: PathBuf,
    (lentries, rentries): (Vec<Entry>, Vec<Entry>),
    mode: Mode,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&Path),
    out: &mut Vec<Difference>,
) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    }
    progress(&rel);
    let mut names: BTreeMap<OsString, (Option<Entry>, Option<Entry>)> = BTreeMap::new();
    for entry in lentries.into_iter().filter(|e| !e.is_parent()) {
        let name = entry.name.clone();
        names.entry(name).or_default().0 = Some(entry);
    }
    for entry in rentries.into_iter().filter(|e| !e.is_parent()) {
        let name = entry.name.clone();
        names.entry(name).or_default().1 = Some(entry);
    }
    for (name, (l, r)) in names {
        let path = rel.join(&name);
        let difference = |left, right| Difference {
            rel: path.clone(),
            left,
            right,
        };
        match (l, r) {
            (Some(l), Some(r)) if l.is_dir() && r.is_dir() => {
                let listed = (
                    left.0.read_dir(&left.1.join(&path)),
                    right.0.read_dir(&right.1.join(&path)),
                );
                match listed {
                    (Ok(ls), Ok(rs)) => walk(
                        left,
                        right,
                        path.clone(),
                        (ls, rs),
                        mode,
                        cancel,
                        progress,
                        out,
                    )?,
                    // unreadable on a side: shown, and left to the user
                    _ => out.push(difference(Some(l), Some(r))),
                }
            }
            (Some(l), Some(r)) if l.is_dir() != r.is_dir() => {
                out.push(difference(Some(l), Some(r)))
            }
            (Some(l), Some(r)) => {
                let same = match mode {
                    Mode::SizeOnly => l.size == r.size,
                    Mode::Quick => l.size == r.size && compare::same_time(l.mtime, r.mtime),
                    Mode::Thorough => {
                        l.size == r.size
                            && !compare::contents_differ(
                                left.0,
                                &left.1.join(&path),
                                right.0,
                                &right.1.join(&path),
                            )
                            .unwrap_or(true)
                    }
                };
                if !same {
                    out.push(difference(Some(l), Some(r)));
                }
            }
            (l, r) => out.push(difference(l, r)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::LocalFs;
    use std::fs;

    #[test]
    fn two_trees_are_walked_and_a_lone_directory_is_one_difference() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        for root in [&a, &b] {
            fs::create_dir_all(root.join("sub/deep")).unwrap();
            fs::write(root.join("same.txt"), "same").unwrap();
            fs::write(root.join("sub/deep/same.txt"), "same").unwrap();
        }
        fs::write(a.join("sub/deep/changed.txt"), "one").unwrap();
        fs::write(b.join("sub/deep/changed.txt"), "three").unwrap();
        fs::write(a.join("sub/only_left.txt"), "l").unwrap();
        fs::create_dir_all(b.join("new/lots/of/it")).unwrap();
        fs::write(b.join("new/lots/of/it/x"), "x").unwrap();
        fs::write(a.join("clash"), "a file").unwrap();
        fs::create_dir(b.join("clash")).unwrap();

        let local: Arc<dyn FsProvider> = Arc::new(LocalFs);
        let handle = spawn_scan((local.clone(), a), (local, b), Mode::Quick);
        let found = loop {
            if let ScanEvent::Done(result) = handle.events.recv().unwrap() {
                break result.unwrap();
            }
        };
        let rels: Vec<String> = found
            .iter()
            .map(|d| d.rel.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            rels,
            ["clash", "new", "sub/deep/changed.txt", "sub/only_left.txt"]
        );
        assert!(found[0].clash());
        assert!(found[1].left.is_none() && found[1].right.as_ref().unwrap().is_dir());
    }
}
