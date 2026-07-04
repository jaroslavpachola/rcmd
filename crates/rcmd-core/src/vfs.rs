//! The filesystem seam: panels and jobs talk to a [`FsProvider`] (the
//! read half) and, when a provider is writable, its [`FsWrite`] (the
//! write half via [`FsProvider::writer`]). Archives are read-only;
//! [`LocalFs`] and `SftpFs` implement both halves.

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::SystemTime;

use crate::entry::{self, Entry};

pub trait FsProvider: Send + Sync {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<Entry>>;
    /// lstat semantics: symlinks are reported, not followed.
    fn stat(&self, path: &Path) -> io::Result<Entry>;
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>>;
    fn is_local(&self) -> bool {
        false
    }
    /// The write half, if this provider supports writing.
    fn writer(&self) -> Option<&dyn FsWrite> {
        None
    }
}

/// Write operations. Failures are ordinary `io::Error`s so the job
/// engine's retry/skip dialogs apply unchanged.
pub trait FsWrite: Send + Sync {
    fn mkdir(&self, dir: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn remove_dir(&self, dir: &Path) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn open_write(&self, path: &Path) -> io::Result<Box<dyn Write + Send>>;
    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()>;
    fn set_mtime(&self, path: &Path, mtime: SystemTime) -> io::Result<()>;
    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()>;
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

    fn writer(&self) -> Option<&dyn FsWrite> {
        Some(self)
    }
}

impl FsWrite for LocalFs {
    fn mkdir(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir(dir)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn remove_dir(&self, dir: &Path) -> io::Result<()> {
        std::fs::remove_dir(dir)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn open_write(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        Ok(Box::new(std::fs::File::create(path)?))
    }

    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        }
        #[cfg(not(unix))]
        {
            let _ = (path, mode);
            Ok(())
        }
    }

    fn set_mtime(&self, path: &Path, mtime: SystemTime) -> io::Result<()> {
        let f = std::fs::File::open(path)?;
        f.set_times(std::fs::FileTimes::new().set_modified(mtime))
    }

    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link)
        }
        #[cfg(not(unix))]
        {
            let _ = (target, link);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "symlinks are not supported on this platform",
            ))
        }
    }
}

/// Filenames Enter will try to open as an archive.
pub fn is_archive_name(name: &OsStr) -> bool {
    let name = name.to_string_lossy().to_lowercase();
    [
        ".zip", ".tar", ".tar.gz", ".tgz", ".tar.xz", ".txz", ".tar.bz2", ".tbz2", ".tbz",
    ]
    .iter()
    .any(|ext| name.ends_with(ext))
}
