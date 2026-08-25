//! Comparing two panel listings, mc's three ways. The first two are a
//! matter of what the listing already knows; the third has to read the
//! files, which is why it goes to a worker thread and reports as it
//! goes.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, SystemTime};

use crate::entry::Entry;
use crate::vfs::FsProvider;

/// mc's three answers to "which files differ".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Size and modification time - the cheap one, and the default.
    Quick,
    /// Size alone, for trees whose timestamps were never going to
    /// survive the trip (an unzip, an rsync without -t, a copy through
    /// a filesystem that rounds them).
    SizeOnly,
    /// The bytes themselves. Nothing else can tell you that two files
    /// with the same size and date are actually the same file.
    Thorough,
}

/// mtimes within this of each other count as the same time: filesystems
/// and protocols round differently, and a second either way is not a
/// difference anyone means.
const MTIME_SLACK: Duration = Duration::from_secs(2);

fn same_time(a: Option<SystemTime>, b: Option<SystemTime>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.duration_since(b).unwrap_or_else(|e| e.duration()) <= MTIME_SLACK,
        // an unknown time is no evidence of a difference
        _ => true,
    }
}

/// What the listings alone can say.
#[derive(Default, Debug)]
pub struct Diff {
    /// Names to mark on the left panel: missing on the right, or
    /// different.
    pub left: Vec<OsString>,
    pub right: Vec<OsString>,
    /// Same name, same size, same time - a thorough run still has to
    /// read these two to know.
    pub undecided: Vec<OsString>,
}

impl Diff {
    /// How many differences are known so far, counting a file that
    /// differs on both sides once.
    pub fn count(&self) -> usize {
        let both = self.left.iter().filter(|n| self.right.contains(n)).count();
        self.left.len() + self.right.len() - both
    }
}

/// Compare two listings by name. Directories are left out: mc compares
/// the files in the two directories, not the trees under them.
pub fn compare_listings(left: &[Entry], right: &[Entry], mode: Mode) -> Diff {
    let files = |entries: &[Entry]| -> Vec<(OsString, u64, Option<SystemTime>)> {
        entries
            .iter()
            .filter(|e| !e.is_dir() && !e.is_parent())
            .map(|e| (e.name.clone(), e.size, e.mtime))
            .collect()
    };
    let (left, right) = (files(left), files(right));
    let mut diff = Diff::default();
    for (name, size, mtime) in &left {
        match right.iter().find(|(other, ..)| other == name) {
            None => diff.left.push(name.clone()),
            Some((_, rsize, rmtime)) => {
                let same = match mode {
                    Mode::SizeOnly => size == rsize,
                    Mode::Quick => size == rsize && same_time(*mtime, *rmtime),
                    // a different size settles it without reading
                    Mode::Thorough => size == rsize,
                };
                if !same {
                    diff.left.push(name.clone());
                    diff.right.push(name.clone());
                } else if mode == Mode::Thorough {
                    diff.undecided.push(name.clone());
                }
            }
        }
    }
    for (name, ..) in &right {
        if !left.iter().any(|(other, ..)| other == name) {
            diff.right.push(name.clone());
        }
    }
    diff
}

pub enum CompareEvent {
    /// This name's contents differ after all.
    Differs(OsString),
    Done,
}

pub struct CompareHandle {
    pub events: Receiver<CompareEvent>,
    cancel: Arc<AtomicBool>,
}

impl CompareHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Read the pairs that the listings could not tell apart. Runs on a
/// worker thread and reports each difference as it finds it, so a
/// directory of large files marks the first one long before the last is
/// read.
pub fn spawn_content_compare(
    left: (Arc<dyn FsProvider>, PathBuf),
    right: (Arc<dyn FsProvider>, PathBuf),
    names: Vec<OsString>,
) -> CompareHandle {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    thread::spawn(move || {
        let (left_fs, left_dir) = left;
        let (right_fs, right_dir) = right;
        for name in names {
            if flag.load(Ordering::Relaxed) {
                break;
            }
            let differs = contents_differ(
                &*left_fs,
                &left_dir.join(&name),
                &*right_fs,
                &right_dir.join(&name),
            );
            // a file that cannot be read is reported as differing: what
            // is certain is that it has not been shown to be the same
            if differs.unwrap_or(true) && tx.send(CompareEvent::Differs(name)).is_err() {
                return;
            }
        }
        let _ = tx.send(CompareEvent::Done);
    });
    CompareHandle { events: rx, cancel }
}

/// Byte for byte, in chunks, stopping at the first difference.
fn contents_differ(
    left_fs: &dyn FsProvider,
    left: &Path,
    right_fs: &dyn FsProvider,
    right: &Path,
) -> std::io::Result<bool> {
    const CHUNK: usize = 64 * 1024;
    let mut a = left_fs.open_read(left)?;
    let mut b = right_fs.open_read(right)?;
    let (mut abuf, mut bbuf) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    loop {
        let an = read_full(&mut a, &mut abuf)?;
        let bn = read_full(&mut b, &mut bbuf)?;
        if an != bn {
            return Ok(true);
        }
        if an == 0 {
            return Ok(false);
        }
        if abuf[..an] != bbuf[..bn] {
            return Ok(true);
        }
    }
}

/// Fill the buffer unless the file ends first; a short read is not the
/// end of a stream, and two files read in different-sized pieces would
/// otherwise never line up.
fn read_full(source: &mut impl std::io::Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match source.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryKind;

    fn file(name: &str, size: u64, secs: u64) -> Entry {
        Entry {
            name: name.into(),
            kind: EntryKind::File,
            size,
            mtime: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
            mode: 0o644,
            link_target: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn the_three_modes_disagree_where_they_should() {
        let left = [
            file("same", 10, 100),
            file("newer", 10, 100),
            file("only", 1, 1),
        ];
        let right = [file("same", 10, 100), file("newer", 10, 900)];

        // quick: a different time is a difference
        let quick = compare_listings(&left, &right, Mode::Quick);
        assert!(quick.left.contains(&"newer".into()));
        assert!(quick.left.contains(&"only".into()));
        assert_eq!(quick.count(), 2);

        // size only: the same ten bytes are the same ten bytes
        let size = compare_listings(&left, &right, Mode::SizeOnly);
        assert!(!size.left.contains(&"newer".into()));
        assert_eq!(size.count(), 1);

        // thorough: same size, so nothing is decided by the listing -
        // both of them go to the reader
        let deep = compare_listings(&left, &right, Mode::Thorough);
        // in listing order, which is the order they will be read in
        assert_eq!(
            deep.undecided,
            vec![OsString::from("same"), OsString::from("newer")]
        );
        assert!(deep.left.contains(&"only".into()));
    }

    #[test]
    fn contents_settle_what_the_listing_cannot() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        let fs: Arc<dyn FsProvider> = Arc::new(crate::vfs::LocalFs);
        std::fs::write(&a, "hello world").unwrap();
        std::fs::write(&b, "hello world").unwrap();
        assert!(!contents_differ(&*fs, &a, &*fs, &b).unwrap());
        std::fs::write(&b, "hello worlD").unwrap();
        assert!(contents_differ(&*fs, &a, &*fs, &b).unwrap());
        // and across a chunk boundary, where a naive read-and-compare
        // pairs up differently sized reads
        let big: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        std::fs::write(&a, &big).unwrap();
        let mut other = big.clone();
        *other.last_mut().unwrap() ^= 1;
        std::fs::write(&b, &other).unwrap();
        assert!(contents_differ(&*fs, &a, &*fs, &b).unwrap());
    }
}
