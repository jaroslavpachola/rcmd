//! Duplicate files under a directory: grouped by size, then by a hash
//! of their first 64 KiB, then by a hash of the whole file - each step
//! reading only what the one before could not tell apart. A group comes
//! back as soon as it is certain, biggest files first, every file in it
//! but the first flagged for marking: F8 after it keeps one of each.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;

use sha2::{Digest, Sha256};

use crate::entry;
use crate::find::{FindEvent, FindHandle, Found};

/// How much of each file the first hash reads.
const HEAD: u64 = 64 * 1024;

/// Look for duplicates under `root` on a worker thread. Empty files are
/// all alike and not worth a group; hidden ones are left out when
/// `skip_hidden` says so.
pub fn spawn_duplicates(root: PathBuf, skip_hidden: bool) -> FindHandle {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let thread = thread::spawn(move || {
        let (mut matches, mut scanned) = (0u64, 0u64);
        let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
        let walk = jwalk::WalkDir::new(&root)
            .min_depth(1)
            .skip_hidden(skip_hidden)
            .follow_links(false)
            .parallelism(jwalk::Parallelism::RayonNewPool(0));
        for entry in walk {
            if flag.load(Ordering::Relaxed) {
                break;
            }
            let Ok(entry) = entry else { continue };
            scanned += 1;
            if !entry.file_type.is_file() {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() > 0 {
                by_size.entry(meta.len()).or_default().push(entry.path());
            }
        }
        let mut sizes: Vec<u64> = by_size
            .iter()
            .filter(|(_, paths)| paths.len() > 1)
            .map(|(&size, _)| size)
            .collect();
        sizes.sort_unstable_by(|a, b| b.cmp(a));
        'sizes: for size in sizes {
            let paths = by_size.remove(&size).unwrap_or_default();
            for group in same_content(paths, size, &flag) {
                for (at, path) in group.iter().enumerate() {
                    let Ok(mut found) = entry::stat(path) else {
                        continue;
                    };
                    if let Ok(rel) = path.strip_prefix(&root) {
                        found.name = rel.as_os_str().to_os_string();
                    }
                    matches += 1;
                    let sent = tx.send(FindEvent::Match(Box::new(Found {
                        entry: found,
                        hit: None,
                        inside: None,
                        mark: at > 0,
                    })));
                    if sent.is_err() || flag.load(Ordering::Relaxed) {
                        break 'sizes;
                    }
                }
            }
        }
        let _ = tx.send(FindEvent::Done { matches, scanned });
    });
    FindHandle::new(rx, cancel, thread)
}

/// Files of one size split into groups of identical content, each at
/// least two long, in the order their first member was met.
fn same_content(paths: Vec<PathBuf>, size: u64, cancel: &AtomicBool) -> Vec<Vec<PathBuf>> {
    let mut groups = split_by(paths, |path| hash(path, HEAD), cancel);
    if size > HEAD {
        groups = groups
            .into_iter()
            .flat_map(|group| split_by(group, |path| hash(path, u64::MAX), cancel))
            .collect();
    }
    for group in &mut groups {
        group.sort();
    }
    groups
}

/// `paths` split by what `key` says of each, groups of one dropped - a
/// file that cannot be read is in no group.
fn split_by(
    paths: Vec<PathBuf>,
    key: impl Fn(&Path) -> io::Result<[u8; 32]>,
    cancel: &AtomicBool,
) -> Vec<Vec<PathBuf>> {
    let mut order: Vec<[u8; 32]> = Vec::new();
    let mut groups: HashMap<[u8; 32], Vec<PathBuf>> = HashMap::new();
    for path in paths {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Ok(digest) = key(&path) else { continue };
        if !groups.contains_key(&digest) {
            order.push(digest);
        }
        groups.entry(digest).or_default().push(path);
    }
    order
        .into_iter()
        .filter_map(|digest| groups.remove(&digest))
        .filter(|group| group.len() > 1)
        .collect()
}

/// SHA-256 of the first `limit` bytes of a file.
fn hash(path: &Path, limit: u64) -> io::Result<[u8; 32]> {
    let mut file = File::open(path)?.take(limit);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn duplicates_come_in_groups_with_all_but_one_marked() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        // two alike, big enough to need the whole-file hash, and one of
        // the same size that differs only past the first 64 KiB
        let mut big = vec![7u8; 100_000];
        fs::write(root.join("big1.bin"), &big).unwrap();
        fs::write(root.join("a/b/big2.bin"), &big).unwrap();
        big[90_000] = 8;
        fs::write(root.join("a/near.bin"), &big).unwrap();
        // three small alike
        for name in ["s1.txt", "a/s2.txt", "a/b/s3.txt"] {
            fs::write(root.join(name), "same\n").unwrap();
        }
        fs::write(root.join("other.txt"), "else\n").unwrap();
        fs::write(root.join("empty1"), "").unwrap();
        fs::write(root.join("empty2"), "").unwrap();
        let handle = spawn_duplicates(root.to_path_buf(), false);
        let mut got = Vec::new();
        while let Ok(event) = handle.events.recv() {
            match event {
                FindEvent::Match(found) => {
                    got.push((found.entry.name.to_string_lossy().into_owned(), found.mark))
                }
                FindEvent::Done { .. } => break,
            }
        }
        let expect: Vec<(String, bool)> = [
            ("a/b/big2.bin", false),
            ("big1.bin", true),
            ("a/b/s3.txt", false),
            ("a/s2.txt", true),
            ("s1.txt", true),
        ]
        .iter()
        .map(|&(n, m)| (n.to_string(), m))
        .collect();
        assert_eq!(got, expect);
    }
}
