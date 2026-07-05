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

/// Extended stat data for the info panel and the long listing; `None`
/// where the provider cannot supply it (archives, partly sftp).
#[derive(Debug, Clone, Copy, Default)]
pub struct EntryStat {
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub atime: Option<SystemTime>,
    pub ctime: Option<SystemTime>,
    pub nlink: Option<u64>,
    pub inode: Option<u64>,
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
    pub extra: EntryStat,
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
            extra: EntryStat::default(),
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

#[cfg(unix)]
fn extra_of(meta: &fs::Metadata) -> EntryStat {
    use std::os::unix::fs::MetadataExt;
    let ctime = u64::try_from(meta.ctime()).ok().and_then(|secs| {
        SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::new(secs, meta.ctime_nsec() as u32))
    });
    EntryStat {
        uid: Some(meta.uid()),
        gid: Some(meta.gid()),
        atime: meta.accessed().ok(),
        ctime,
        nlink: Some(meta.nlink()),
        inode: Some(meta.ino()),
    }
}

#[cfg(not(unix))]
fn extra_of(meta: &fs::Metadata) -> EntryStat {
    EntryStat {
        atime: meta.accessed().ok(),
        ..EntryStat::default()
    }
}

fn classify(meta: &fs::Metadata, path: &Path) -> (EntryKind, Option<PathBuf>) {
    if meta.is_symlink() {
        let kind = match fs::metadata(path) {
            Ok(target) if target.is_dir() => EntryKind::SymlinkDir,
            Ok(_) => EntryKind::SymlinkFile,
            Err(_) => EntryKind::SymlinkBroken,
        };
        (kind, fs::read_link(path).ok())
    } else if meta.is_dir() {
        (EntryKind::Dir, None)
    } else {
        (EntryKind::File, None)
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
        let (kind, link_target) = classify(&meta, &dent.path());
        out.push(Entry {
            name: dent.file_name(),
            kind,
            size: meta.len(),
            mtime: meta.modified().ok(),
            mode: mode_of(&meta),
            link_target,
            extra: extra_of(&meta),
        });
    }
    Ok(out)
}

/// Entry for one path (lstat semantics), classified like [`read_dir`].
pub fn stat(path: &Path) -> io::Result<Entry> {
    let meta = fs::symlink_metadata(path)?;
    let (kind, link_target) = classify(&meta, path);
    Ok(Entry {
        name: path.file_name().unwrap_or_default().to_os_string(),
        kind,
        size: meta.len(),
        mtime: meta.modified().ok(),
        mode: mode_of(&meta),
        link_target,
        extra: extra_of(&meta),
    })
}
