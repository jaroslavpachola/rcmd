use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
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
    /// Unix permission bits (0 where unavailable).
    pub mode: u32,
    pub link_target: Option<PathBuf>,
}

impl Entry {
    pub fn parent() -> Self {
        Entry {
            name: OsString::from(".."),
            kind: EntryKind::Dir,
            size: 0,
            mtime: None,
            mode: 0,
            link_target: None,
        }
    }

    pub fn is_dir(&self) -> bool {
        matches!(self.kind, EntryKind::Dir | EntryKind::SymlinkDir)
    }

    pub fn is_parent(&self) -> bool {
        self.name == ".."
    }

    pub fn is_executable(&self) -> bool {
        self.kind == EntryKind::File && self.mode & 0o111 != 0
    }

    pub fn is_hidden(&self) -> bool {
        !self.is_parent() && self.name.as_encoded_bytes().starts_with(b".")
    }

    /// ls-style permission string, e.g. "drwxr-xr-x".
    pub fn perm_string(&self) -> String {
        let type_ch = match self.kind {
            EntryKind::Dir => 'd',
            EntryKind::SymlinkDir | EntryKind::SymlinkFile | EntryKind::SymlinkBroken => 'l',
            EntryKind::File => '-',
        };
        let mut s = String::with_capacity(10);
        s.push(type_ch);
        for shift in [6, 3, 0] {
            let bits = (self.mode >> shift) & 7;
            s.push(if bits & 4 != 0 { 'r' } else { '-' });
            s.push(if bits & 2 != 0 { 'w' } else { '-' });
            s.push(if bits & 1 != 0 { 'x' } else { '-' });
        }
        s
    }

    /// Extension for sorting, lowercased; empty for dotfiles and no-ext names.
    pub fn ext(&self) -> String {
        Path::new(&self.name)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    }
}

#[cfg(unix)]
fn mode_of(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn mode_of(_meta: &fs::Metadata) -> u32 {
    0
}

/// List a directory without following symlinks for the entries themselves;
/// symlink targets are stat'ed only to classify the link as dir/file/broken.
/// Entries that vanish between readdir and stat are skipped.
pub fn read_dir(path: &Path) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for dent in fs::read_dir(path)? {
        let dent = dent?;
        let Ok(meta) = dent.metadata() else { continue };
        let (kind, link_target) = if meta.is_symlink() {
            let kind = match fs::metadata(dent.path()) {
                Ok(target) if target.is_dir() => EntryKind::SymlinkDir,
                Ok(_) => EntryKind::SymlinkFile,
                Err(_) => EntryKind::SymlinkBroken,
            };
            (kind, fs::read_link(dent.path()).ok())
        } else if meta.is_dir() {
            (EntryKind::Dir, None)
        } else {
            (EntryKind::File, None)
        };
        out.push(Entry {
            name: dent.file_name(),
            kind,
            size: meta.len(),
            mtime: meta.modified().ok(),
            mode: mode_of(&meta),
            link_target,
        });
    }
    Ok(out)
}
