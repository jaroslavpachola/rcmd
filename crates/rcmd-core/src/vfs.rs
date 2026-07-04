//! The filesystem seam: panels and extraction jobs talk to a
//! [`FsProvider`], so archives (and one day remote filesystems) plug in
//! without touching panel logic. [`LocalFs`] is the trivial passthrough.

use std::ffi::OsStr;
use std::io::{self, Read};
use std::path::Path;

use crate::entry::{self, Entry};

pub trait FsProvider: Send + Sync {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<Entry>>;
    /// lstat semantics: symlinks are reported, not followed.
    fn stat(&self, path: &Path) -> io::Result<Entry>;
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>>;
    fn is_local(&self) -> bool {
        false
    }
}

pub struct LocalFs;

impl FsProvider for LocalFs {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        entry::read_dir(dir)
    }

    fn stat(&self, path: &Path) -> io::Result<Entry> {
        entry::stat(path)
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(std::fs::File::open(path)?))
    }

    fn is_local(&self) -> bool {
        true
    }
}

/// Filenames Enter will try to open as an archive.
pub fn is_archive_name(name: &OsStr) -> bool {
    let name = name.to_string_lossy().to_lowercase();
    name.ends_with(".zip")
        || name.ends_with(".tar")
        || name.ends_with(".tar.gz")
        || name.ends_with(".tgz")
}
