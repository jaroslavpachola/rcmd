//! Find file: walk a tree on a worker thread, stream matches back to the
//! UI as they are found. Matches carry their path relative to the search
//! root in `Entry::name`, ready for a panelized listing.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use crate::entry::{self, Entry};
use crate::glob::glob_match;

pub enum FindEvent {
    Match(Box<Entry>),
    Done { matches: u64, scanned: u64 },
}

pub struct FindHandle {
    pub events: Receiver<FindEvent>,
    cancel: Arc<AtomicBool>,
    pub thread: Option<JoinHandle<()>>,
}

impl FindHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// "Skip this path?" — supplied by the caller (e.g. a gitignore check);
/// a skipped directory is not descended into.
pub type SkipFn = Box<dyn Fn(&Path) -> bool + Send>;

/// `pattern` globs against file names (not paths); `content`, when set,
/// additionally requires a case-insensitive substring match in the file.
pub fn spawn_find(
    root: PathBuf,
    pattern: String,
    content: Option<String>,
    skip: Option<SkipFn>,
) -> FindHandle {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let thread = thread::spawn(move || {
        let needle = content.map(|c| c.to_lowercase().into_bytes());
        let mut matches = 0u64;
        let mut scanned = 0u64;
        walk(
            &root,
            &root,
            &pattern,
            needle.as_deref(),
            skip.as_deref(),
            &tx,
            &flag,
            &mut matches,
            &mut scanned,
        );
        let _ = tx.send(FindEvent::Done { matches, scanned });
    });
    FindHandle {
        events: rx,
        cancel,
        thread: Some(thread),
    }
}

#[allow(clippy::too_many_arguments)]
fn walk(
    root: &Path,
    dir: &Path,
    pattern: &str,
    needle: Option<&[u8]>,
    skip: Option<&(dyn Fn(&Path) -> bool + Send)>,
    tx: &Sender<FindEvent>,
    cancel: &AtomicBool,
    matches: &mut u64,
    scanned: &mut u64,
) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return; // unreadable dirs are silently skipped, like find's -readable
    };
    for dent in read.flatten() {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let path = dent.path();
        if skip.is_some_and(|f| f(&path)) {
            continue;
        }
        let Ok(meta) = dent.metadata() else { continue }; // lstat
        *scanned += 1;
        // symlinked dirs are not followed (no loops)
        let is_dir = meta.is_dir();
        let name_matches = glob_match(pattern, &dent.file_name().to_string_lossy());
        if name_matches {
            let hit = match needle {
                None => true,
                Some(n) => meta.is_file() && file_contains(&path, n),
            };
            if hit && let Ok(mut entry) = entry::stat(&path) {
                if let Ok(rel) = path.strip_prefix(root) {
                    entry.name = rel.as_os_str().to_os_string();
                }
                *matches += 1;
                if tx.send(FindEvent::Match(Box::new(entry))).is_err() {
                    cancel.store(true, Ordering::Relaxed);
                    return;
                }
            }
        }
        if is_dir {
            walk(
                root, &path, pattern, needle, skip, tx, cancel, matches, scanned,
            );
        }
    }
}

/// Chunked, ASCII-case-insensitive substring search; never loads the
/// whole file. `needle` must already be lowercase.
fn file_contains(path: &Path, needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let overlap = needle.len() - 1;
    let mut buf = vec![0u8; 64 * 1024 + overlap];
    let mut carry = 0usize;
    loop {
        let n = match file.read(&mut buf[carry..]) {
            Ok(0) | Err(_) => return false,
            Ok(n) => n,
        };
        let hay = &buf[..carry + n];
        let lower: Vec<u8> = hay.iter().map(|b| b.to_ascii_lowercase()).collect();
        if lower.windows(needle.len()).any(|w| w == needle) {
            return true;
        }
        carry = overlap.min(hay.len());
        let start = hay.len() - carry;
        buf.copy_within(start..start + carry, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn collect(handle: FindHandle) -> (Vec<String>, u64) {
        let mut names = Vec::new();
        loop {
            match handle.events.recv().expect("find died without Done") {
                FindEvent::Match(entry) => names.push(entry.name.to_string_lossy().into_owned()),
                FindEvent::Done { matches, .. } => {
                    names.sort();
                    return (names, matches);
                }
            }
        }
    }

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/deep")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() { magic() }").unwrap();
        fs::write(dir.path().join("src/deep/util.rs"), "pub fn util() {}").unwrap();
        fs::write(dir.path().join("notes.txt"), "the MAGIC word").unwrap();
        dir
    }

    #[test]
    fn finds_by_name_glob_with_relative_paths() {
        let t = tree();
        let (names, matches) = collect(spawn_find(
            t.path().to_path_buf(),
            "*.rs".into(),
            None,
            None,
        ));
        assert_eq!(names, ["src/deep/util.rs", "src/main.rs"]);
        assert_eq!(matches, 2);
    }

    #[test]
    fn content_filter_is_case_insensitive() {
        let t = tree();
        let (names, _) = collect(spawn_find(
            t.path().to_path_buf(),
            "*".into(),
            Some("Magic".into()),
            None,
        ));
        assert_eq!(names, ["notes.txt", "src/main.rs"]);
    }

    #[test]
    fn skip_prunes_files_and_whole_trees() {
        let t = tree();
        let (names, _) = collect(spawn_find(
            t.path().to_path_buf(),
            "*".into(),
            None,
            Some(Box::new(|p: &Path| {
                p.file_name()
                    .is_some_and(|n| n == "deep" || n == "notes.txt")
            })),
        ));
        assert_eq!(names, ["src", "src/main.rs"]);
    }

    #[test]
    fn content_straddling_chunk_boundary_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let mut data = vec![b'x'; 64 * 1024 - 3];
        data.extend_from_slice(b"needle");
        fs::write(dir.path().join("big.bin"), &data).unwrap();
        let (names, _) = collect(spawn_find(
            dir.path().to_path_buf(),
            "*".into(),
            Some("needle".into()),
            None,
        ));
        assert_eq!(names, ["big.bin"]);
    }
}
