//! The filesystem seam: panels and jobs talk to a [`FsProvider`] (the
//! read half) and, when a provider is writable, its [`FsWrite`] (the
//! write half via [`FsProvider::writer`]). Archives are read-only;
//! [`LocalFs`] and `SftpFs` implement both halves.

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::entry::{self, Entry};

pub trait FsProvider: Send + Sync {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<Entry>>;
    /// lstat semantics: symlinks are reported, not followed.
    fn stat(&self, path: &Path) -> io::Result<Entry>;
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>>;
    /// The file from `offset` on: what a resumed copy still needs. The
    /// default reads and drops what comes before, which works anywhere
    /// and costs the skipped bytes; a provider that can seek does better.
    fn open_read_at(&self, path: &Path, offset: u64) -> io::Result<Box<dyn Read + Send>> {
        let mut reader = self.open_read(path)?;
        io::copy(&mut (&mut reader).take(offset), &mut io::sink())?;
        Ok(reader)
    }
    fn is_local(&self) -> bool {
        false
    }
    /// A word about an entry beyond what its stat says - where a
    /// trashed thing came from - for the line under the panel. Asked on
    /// every draw, so it answers from what it already knows.
    fn note(&self, _path: &Path) -> Option<String> {
        None
    }
    /// The write half, if this provider supports writing.
    fn writer(&self) -> Option<&dyn FsWrite> {
        None
    }
    /// The same thing read again from scratch - what a reload of a
    /// panel inside an archive does, so members added since show up.
    /// `None` for a provider that is always current.
    fn reopen(&self) -> Option<io::Result<std::sync::Arc<dyn FsProvider>>> {
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
    /// Whether [`FsWrite::open_append`] works here - what decides if the
    /// overwrite question offers Append and Reget.
    fn can_append(&self) -> bool {
        false
    }
    /// The file opened to add to its end, for Append and Reget.
    fn open_append(&self, _path: &Path) -> io::Result<Box<dyn Write + Send>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cannot append here",
        ))
    }
    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()>;
    /// Change owner and/or group (numeric ids; `None` = leave as is).
    /// Symlinks themselves are changed, not their targets, where the
    /// backend allows it.
    fn set_owner(&self, path: &Path, uid: Option<u32>, gid: Option<u32>) -> io::Result<()>;
    fn set_mtime(&self, path: &Path, mtime: SystemTime) -> io::Result<()>;
    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()>;

    /// A second name for the same file. Only local filesystems have
    /// one: the SFTP protocol's hardlink is an OpenSSH extension and
    /// an archive has nowhere to put it, so the default says so rather
    /// than pretending.
    fn hard_link(&self, _existing: &Path, _link: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "hard links are local only",
        ))
    }
}

/// A filesystem reached over the network. Beyond reading and writing it
/// has an **identity** - the URL prefix that names the connection, which
/// is what the panel title shows and what the connection cache is keyed
/// on - and it can say where a path really is, since a server's idea of
/// "." is its own.
pub trait RemoteFs: FsProvider {
    /// `sftp://user@host[:port]` or `ftp://user@host[:port]`.
    fn prefix(&self) -> &str;
    /// Resolve a path the way the server sees it; `.` is the login
    /// directory.
    fn realpath(&self, path: &Path) -> io::Result<PathBuf>;
}

pub struct LocalFs;

/// Open a file to read its bytes - and only a regular file. A FIFO
/// blocks the open until a writer turns up, a device hands out data
/// that has nothing to do with a copy (and opening a tape rewinds it),
/// so both are refused here rather than hanging a job or a viewer on
/// them. The type is checked before the open, and again on the handle,
/// in case the name was swapped in between.
pub fn open_regular(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let not_regular = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file (a FIFO, socket or device has no contents to copy)",
        )
    };
    if !std::fs::metadata(path)?.is_file() {
        return Err(not_regular());
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(not_regular());
    }
    Ok(file)
}

impl FsProvider for LocalFs {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        entry::read_dir(dir)
    }

    fn stat(&self, path: &Path) -> io::Result<Entry> {
        entry::stat(path)
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(open_regular(path)?))
    }

    fn open_read_at(&self, path: &Path, offset: u64) -> io::Result<Box<dyn Read + Send>> {
        use std::io::Seek;
        let mut file = open_regular(path)?;
        file.seek(io::SeekFrom::Start(offset))?;
        Ok(Box::new(file))
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

    fn can_append(&self) -> bool {
        true
    }

    fn open_append(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        Ok(Box::new(
            std::fs::OpenOptions::new().append(true).open(path)?,
        ))
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

    fn set_owner(&self, path: &Path, uid: Option<u32>, gid: Option<u32>) -> io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::lchown(path, uid, gid)
        }
        #[cfg(not(unix))]
        {
            let _ = (path, uid, gid);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "ownership is not supported on this platform",
            ))
        }
    }

    fn set_mtime(&self, path: &Path, mtime: SystemTime) -> io::Result<()> {
        let f = std::fs::File::open(path)?;
        f.set_times(std::fs::FileTimes::new().set_modified(mtime))
    }

    fn hard_link(&self, existing: &Path, link: &Path) -> io::Result<()> {
        std::fs::hard_link(existing, link)
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
        ".zip",
        ".tar",
        ".tar.gz",
        ".tgz",
        ".tar.xz",
        ".txz",
        ".tar.bz2",
        ".tbz2",
        ".tbz",
        ".cpio",
        ".cpio.gz",
        ".cpio.xz",
        ".cpio.bz2",
        ".cpio.zst",
        ".tar.zst",
        ".tzst",
        ".deb",
        ".udeb",
        ".rpm",
        ".iso",
        ".patch",
        ".patch.gz",
        ".patch.xz",
        ".patch.bz2",
        ".diff",
        ".diff.gz",
        ".diff.xz",
        ".diff.bz2",
        ".mbox",
        ".mbox.gz",
        ".mbox.xz",
        ".mbox.bz2",
        ".mbx",
        ".a",
        ".ar",
        ".rar",
        ".7z",
        ".lha",
        ".lzh",
        ".arj",
        ".cab",
    ]
    .iter()
    .any(|ext| name.ends_with(ext))
}
