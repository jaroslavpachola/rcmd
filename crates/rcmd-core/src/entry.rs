use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::Path;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    SymlinkDir,
    SymlinkFile,
    SymlinkBroken,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: OsString,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime: Option<SystemTime>,
}

impl Entry {
    pub fn parent() -> Self {
        Entry {
            name: OsString::from(".."),
            kind: EntryKind::Dir,
            size: 0,
            mtime: None,
        }
    }

    pub fn is_dir(&self) -> bool {
        matches!(self.kind, EntryKind::Dir | EntryKind::SymlinkDir)
    }

    pub fn is_parent(&self) -> bool {
        self.name == ".."
    }
}

/// List a directory without following symlinks for the entries themselves;
/// symlink targets are stat'ed only to classify the link as dir/file/broken.
/// Entries that vanish between readdir and stat are skipped.
pub fn read_dir(path: &Path) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for dent in fs::read_dir(path)? {
        let dent = dent?;
        let Ok(meta) = dent.metadata() else { continue };
        let kind = if meta.is_symlink() {
            match fs::metadata(dent.path()) {
                Ok(target) if target.is_dir() => EntryKind::SymlinkDir,
                Ok(_) => EntryKind::SymlinkFile,
                Err(_) => EntryKind::SymlinkBroken,
            }
        } else if meta.is_dir() {
            EntryKind::Dir
        } else {
            EntryKind::File
        };
        out.push(Entry {
            name: dent.file_name(),
            kind,
            size: meta.len(),
            mtime: meta.modified().ok(),
        });
    }
    Ok(out)
}
