//! The XDG trash as a place: `trash://` lists what F8 put there, from
//! the home trash and every volume's own, and can say where each came
//! from, put it back, or delete it for good. The `trash` crate does the
//! throwing away; this is the other half, which it does not have in a
//! form a panel can use - read here from the directories themselves,
//! so a test can hand it a trash of its own.
//!
//! The layout (freedesktop.org Trash spec 1.0): a trash directory holds
//! `files/NAME` and `info/NAME.trashinfo`, the second saying
//! `Path=` (percent-encoded; relative to the volume's top directory in
//! a volume trash) and `DeletionDate=` (local time, no zone).

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::entry::{self, Entry, EntryKind};
use crate::vfs::{FsProvider, FsWrite, RemoteFs};

/// One trash directory, and the top of the volume it belongs to - the
/// directory a volume trash's relative `Path=` is relative to. The home
/// trash's paths are absolute, and it has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashDir {
    pub path: PathBuf,
    pub top: Option<PathBuf>,
}

/// One thing in the trash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trashed {
    /// What the panel calls it: the name under `files/`, made unique
    /// across trash directories when two have the same one.
    pub name: OsString,
    /// Where it is: `files/NAME`.
    pub files: PathBuf,
    /// Its `info/NAME.trashinfo`.
    pub info: PathBuf,
    /// Where it came from.
    pub original: PathBuf,
    pub deleted: Option<SystemTime>,
}

/// The trash directories there are: the home one, then each volume's
/// `.Trash/$uid` and `.Trash-$uid` that exists.
pub fn trash_dirs() -> Vec<TrashDir> {
    let mut dirs = Vec::new();
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    if let Some(data) = data {
        dirs.push(TrashDir {
            path: data.join("Trash"),
            top: None,
        });
    }
    let uid = unsafe { libc::getuid() };
    for top in mount_points() {
        let top = PathBuf::from(top);
        for path in [
            top.join(".Trash").join(uid.to_string()),
            top.join(format!(".Trash-{uid}")),
        ] {
            if path.join("info").is_dir() && !dirs.iter().any(|d| d.path == path) {
                dirs.push(TrashDir {
                    path,
                    top: Some(top.clone()),
                });
            }
        }
    }
    dirs
}

/// Where things are mounted. `/proc/self/mounts` where there is one -
/// a read, where `df` is a process and a stat of every mount, and a
/// dead network mount can hang a stat - and `df` where there is not.
fn mount_points() -> Vec<String> {
    match fs::read_to_string("/proc/self/mounts") {
        Ok(text) => text
            .lines()
            .filter_map(|line| line.split(' ').nth(1))
            .map(unescape_mount)
            .collect(),
        Err(_) => crate::mounts::mounts()
            .into_iter()
            .map(|m| m.point)
            .collect(),
    }
}

/// `/proc/mounts` writes a space as `\040`, and a tab, a newline and a
/// backslash the same way.
fn unescape_mount(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes.get(i + 1..i + 4).and_then(|d| {
            std::str::from_utf8(d)
                .ok()
                .and_then(|d| u8::from_str_radix(d, 8).ok())
        });
        match (bytes[i], octal) {
            (b'\\', Some(byte)) => {
                out.push(byte);
                i += 4;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Everything in `dirs`, in no particular order - the panel sorts.
pub fn list(dirs: &[TrashDir]) -> Vec<Trashed> {
    let mut out: Vec<Trashed> = Vec::new();
    for dir in dirs {
        let Ok(infos) = fs::read_dir(dir.path.join("info")) else {
            continue;
        };
        for info in infos.flatten() {
            let info = info.path();
            let Some(stem) = info
                .file_name()
                .and_then(|n| n.as_bytes().strip_suffix(b".trashinfo"))
                .map(|s| OsStr::from_bytes(s).to_os_string())
            else {
                continue;
            };
            let files = dir.path.join("files").join(&stem);
            // an info file whose payload has gone is a leftover, not
            // something in the trash
            if fs::symlink_metadata(&files).is_err() {
                continue;
            }
            let Ok(text) = read_small(&info) else {
                continue;
            };
            let Some((original, deleted)) = parse_info(&text, dir.top.as_deref()) else {
                continue;
            };
            let name = unique(&out, stem);
            out.push(Trashed {
                name,
                files,
                info,
                original,
                deleted,
            });
        }
    }
    out
}

fn read_small(path: &Path) -> io::Result<String> {
    let mut text = String::new();
    fs::File::open(path)?
        .take(64 * 1024)
        .read_to_string(&mut text)?;
    Ok(text)
}

/// `stem`, or `stem (2)`, `stem (3)`... whichever no earlier item has.
fn unique(items: &[Trashed], stem: OsString) -> OsString {
    let taken = |name: &OsStr| items.iter().any(|i| i.name == name);
    if !taken(&stem) {
        return stem;
    }
    (2..)
        .map(|n| {
            let mut name = stem.clone();
            name.push(format!(" ({n})"));
            name
        })
        .find(|name| !taken(name))
        .unwrap_or(stem)
}

/// A `.trashinfo`'s original path and deletion time.
pub fn parse_info(text: &str, top: Option<&Path>) -> Option<(PathBuf, Option<SystemTime>)> {
    let mut in_section = false;
    let (mut path, mut date) = (None, None);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line == "[Trash Info]";
            continue;
        }
        if !in_section {
            continue;
        }
        match line.split_once('=') {
            Some(("Path", value)) if path.is_none() => path = Some(percent_decode(value.trim())),
            Some(("DeletionDate", value)) if date.is_none() => date = parse_date(value.trim()),
            _ => {}
        }
    }
    let path = PathBuf::from(OsString::from_vec(path?));
    let path = match (path.is_absolute(), top) {
        (true, _) => path,
        (false, Some(top)) => top.join(path),
        // a relative path with nothing to be relative to
        (false, None) => return None,
    };
    Some((path, date))
}

fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes.get(i + 1..i + 3).and_then(|h| {
            std::str::from_utf8(h)
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        });
        match (bytes[i], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    out
}

/// `2026-09-18T12:30:05`, in local time, as the spec has it.
fn parse_date(text: &str) -> Option<SystemTime> {
    let (date, time) = text.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i32>());
    let mut t = time.split(':').map(|p| p.parse::<i32>());
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = d.next()?.ok()? - 1900;
    tm.tm_mon = d.next()?.ok()? - 1;
    tm.tm_mday = d.next()?.ok()?;
    tm.tm_hour = t.next()?.ok()?;
    tm.tm_min = t.next()?.ok()?;
    // seconds may carry a fraction or a zone some writers add
    tm.tm_sec = t
        .next()?
        .ok()
        .or_else(|| time.get(6..8).and_then(|s| s.parse().ok()))?;
    tm.tm_isdst = -1;
    let secs = unsafe { libc::mktime(&mut tm) };
    (secs >= 0).then(|| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64))
}

/// Put an item back where it came from. Something already there is not
/// overwritten - that is the user's to decide, and the error says so. A
/// directory it lived in that has gone since is made again.
pub fn restore(item: &Trashed) -> io::Result<()> {
    if fs::symlink_metadata(&item.original).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} is there already", item.original.display()),
        ));
    }
    if let Some(parent) = item.original.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(&item.files, &item.original) {
        Ok(()) => {}
        // the home trash takes things from other volumes too
        Err(err) if err.raw_os_error() == Some(libc::EXDEV) => {
            copy_all(&item.files, &item.original)?;
            remove_all(&item.files)?;
        }
        Err(err) => return Err(err),
    }
    forget_size(item);
    fs::remove_file(&item.info).or_else(ignore_missing)
}

/// Delete an item for good: its contents and its info file.
pub fn purge(item: &Trashed) -> io::Result<()> {
    remove_all(&item.files).or_else(ignore_missing)?;
    forget_size(item);
    fs::remove_file(&item.info).or_else(ignore_missing)
}

/// Take a directory that has left the trash out of the trash's
/// `directorysizes`, the cache a desktop keeps of how big each trashed
/// directory is: a line left behind is a directory it still counts.
/// Best effort - the cache is the desktop's, and a stale line only
/// costs it a recount.
fn forget_size(item: &Trashed) {
    let Some(trash) = item.files.parent().and_then(Path::parent) else {
        return;
    };
    let cache = trash.join("directorysizes");
    let Ok(text) = fs::read_to_string(&cache) else {
        return;
    };
    let name = item
        .files
        .file_name()
        .unwrap_or_default()
        .as_encoded_bytes();
    // `SIZE MTIME NAME`, the name percent-encoded
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| {
            line.splitn(3, ' ')
                .nth(2)
                .is_none_or(|encoded| percent_decode(encoded.trim_end()) != name)
        })
        .collect();
    if kept.len() == text.lines().count() {
        return;
    }
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    let temp = trash.join(format!(".directorysizes.rcmd-{}", std::process::id()));
    if fs::write(&temp, out)
        .and_then(|()| fs::rename(&temp, &cache))
        .is_err()
    {
        let _ = fs::remove_file(&temp);
    }
}

fn ignore_missing(err: io::Error) -> io::Result<()> {
    match err.kind() {
        io::ErrorKind::NotFound => Ok(()),
        _ => Err(err),
    }
}

fn remove_all(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path)?.is_dir() {
        true => fs::remove_dir_all(path),
        false => fs::remove_file(path),
    }
}

/// A plain copy of a tree, for a restore across volumes: modes and
/// times come along, links stay links.
fn copy_all(from: &Path, to: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(from)?, to)?;
        return Ok(());
    }
    if meta.is_dir() {
        fs::create_dir(to)?;
        for child in fs::read_dir(from)? {
            let child = child?;
            copy_all(&child.path(), &to.join(child.file_name()))?;
        }
    } else {
        fs::copy(from, to)?;
    }
    fs::set_permissions(to, meta.permissions())?;
    if let Ok(modified) = meta.modified() {
        let _ = fs::File::open(to).and_then(|f| f.set_modified(modified));
    }
    Ok(())
}

/// The trash as a panel. The top level is what was thrown away, each
/// under its own name and dated when it was; below that, a trashed
/// directory reads as the directory it was. F5 copies out of it, F8
/// deletes for good; restoring is [`TrashFs::restore`], which the
/// panel's F6 calls, since "where it came from" is no directory a
/// panel can be moved to.
pub struct TrashFs {
    dirs: Vec<TrashDir>,
    items: Mutex<Vec<Trashed>>,
}

pub const PREFIX: &str = "trash://";

impl TrashFs {
    /// Every trash directory there is.
    pub fn new() -> Self {
        Self::with_dirs(trash_dirs())
    }

    pub fn with_dirs(dirs: Vec<TrashDir>) -> Self {
        TrashFs {
            dirs,
            items: Mutex::new(Vec::new()),
        }
    }

    fn items(&self) -> std::sync::MutexGuard<'_, Vec<Trashed>> {
        self.items.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Read the trash directories again.
    pub fn refresh(&self) -> Vec<Trashed> {
        let items = list(&self.dirs);
        *self.items() = items.clone();
        items
    }

    /// The item a top-level name in the panel stands for.
    pub fn item(&self, name: &OsStr) -> Option<Trashed> {
        let found = self.items().iter().find(|i| i.name == name).cloned();
        found.or_else(|| self.refresh().into_iter().find(|i| i.name == name))
    }

    /// The top-level item `path` is, or is inside, and the rest of it.
    fn split(&self, path: &Path) -> Option<(Trashed, PathBuf)> {
        let mut parts = path
            .components()
            .filter(|c| matches!(c, Component::Normal(_)));
        let Some(Component::Normal(name)) = parts.next() else {
            return None;
        };
        let item = self.item(name)?;
        Some((item, parts.collect()))
    }

    /// Where a path in the panel is on disk; `None` for the top level,
    /// which is no one directory.
    fn resolve(&self, path: &Path) -> io::Result<Option<PathBuf>> {
        if is_top(path) {
            return Ok(None);
        }
        match self.split(path) {
            Some((item, rest)) if rest.as_os_str().is_empty() => Ok(Some(item.files)),
            Some((item, rest)) => Ok(Some(item.files.join(rest))),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is not in the trash", path.display()),
            )),
        }
    }

    /// Put the top-level item `path` names back where it came from.
    pub fn restore(&self, path: &Path) -> io::Result<PathBuf> {
        match self.split(path) {
            Some((item, rest)) if rest.as_os_str().is_empty() => {
                restore(&item)?;
                self.items().retain(|i| i.name != item.name);
                Ok(item.original)
            }
            Some(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "only what was thrown away is put back, not a part of it",
            )),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is not in the trash", path.display()),
            )),
        }
    }

    /// The newest item that came from `original`: what an undo of an
    /// F8 puts back. A path spelled through a symlinked directory
    /// matches the real one the trash recorded.
    pub fn find(&self, original: &Path) -> Option<Trashed> {
        let real = original
            .parent()
            .and_then(|p| fs::canonicalize(p).ok())
            .zip(original.file_name())
            .map(|(p, n)| p.join(n));
        self.refresh()
            .into_iter()
            .filter(|i| i.original == original || Some(&i.original) == real.as_ref())
            .max_by_key(|i| i.deleted)
    }
}

impl Default for TrashFs {
    fn default() -> Self {
        Self::new()
    }
}

fn is_top(path: &Path) -> bool {
    !path.components().any(|c| matches!(c, Component::Normal(_)))
}

fn top_entry() -> Entry {
    Entry {
        name: OsString::from("/"),
        kind: EntryKind::Dir,
        size: 0,
        mtime: None,
        mode: 0o700,
        link_target: None,
        extra: Default::default(),
    }
}

fn read_only() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "the trash is not written to: F6 puts things back, F8 deletes them",
    )
}

impl FsProvider for TrashFs {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        let Some(real) = self.resolve(dir)? else {
            return Ok(self
                .refresh()
                .into_iter()
                .filter_map(|item| {
                    let mut entry = entry::stat(&item.files).ok()?;
                    entry.name = item.name;
                    // the date that matters here is when it was thrown
                    // away: sorted by time, the last F8 is on top
                    entry.mtime = item.deleted.or(entry.mtime);
                    Some(entry)
                })
                .collect());
        };
        entry::read_dir(&real)
    }

    fn stat(&self, path: &Path) -> io::Result<Entry> {
        let Some(real) = self.resolve(path)? else {
            return Ok(top_entry());
        };
        let mut entry = entry::stat(&real)?;
        if let Some(name) = path.file_name() {
            entry.name = name.to_os_string();
        }
        Ok(entry)
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        match self.resolve(path)? {
            Some(real) => Ok(Box::new(crate::vfs::open_regular(&real)?)),
            None => Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                "the trash is a directory",
            )),
        }
    }

    fn note(&self, path: &Path) -> Option<String> {
        let mut parts = path
            .components()
            .filter(|c| matches!(c, Component::Normal(_)));
        let (Some(Component::Normal(name)), None) = (parts.next(), parts.next()) else {
            return None;
        };
        let items = self.items();
        let item = items.iter().find(|i| i.name == name)?;
        Some(format!("from {}", item.original.display()))
    }

    fn writer(&self) -> Option<&dyn FsWrite> {
        Some(self)
    }
}

/// Only deleting is writing here: a top-level item goes for good, info
/// file and all; a part of a trashed directory goes on its own.
impl FsWrite for TrashFs {
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.remove(path, false)
    }

    fn remove_dir(&self, dir: &Path) -> io::Result<()> {
        self.remove(dir, true)
    }

    fn mkdir(&self, _dir: &Path) -> io::Result<()> {
        Err(read_only())
    }
    fn rename(&self, _from: &Path, _to: &Path) -> io::Result<()> {
        Err(read_only())
    }
    fn open_write(&self, _path: &Path) -> io::Result<Box<dyn io::Write + Send>> {
        Err(read_only())
    }
    fn set_mode(&self, _path: &Path, _mode: u32) -> io::Result<()> {
        Err(read_only())
    }
    fn set_owner(&self, _path: &Path, _uid: Option<u32>, _gid: Option<u32>) -> io::Result<()> {
        Err(read_only())
    }
    fn set_mtime(&self, _path: &Path, _mtime: SystemTime) -> io::Result<()> {
        Err(read_only())
    }
    fn symlink(&self, _target: &Path, _link: &Path) -> io::Result<()> {
        Err(read_only())
    }
}

impl TrashFs {
    fn remove(&self, path: &Path, dir: bool) -> io::Result<()> {
        let Some((item, rest)) = self.split(path) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is not in the trash", path.display()),
            ));
        };
        if rest.as_os_str().is_empty() {
            purge(&item)?;
            self.items().retain(|i| i.name != item.name);
            return Ok(());
        }
        let real = item.files.join(rest);
        match dir {
            true => fs::remove_dir(real),
            false => fs::remove_file(real),
        }
    }
}

impl RemoteFs for TrashFs {
    fn prefix(&self) -> &str {
        PREFIX
    }

    fn realpath(&self, path: &Path) -> io::Result<PathBuf> {
        let mut out = PathBuf::from("/");
        for part in path.components() {
            match part {
                Component::Normal(name) => out.push(name),
                Component::ParentDir => {
                    out.pop();
                }
                _ => {}
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trash directory of the test's own with one item thrown into it
    /// per `(name, original, date)`.
    fn trash(root: &Path, items: &[(&str, &Path, &str)]) -> TrashDir {
        let dir = root.join("Trash");
        fs::create_dir_all(dir.join("files")).unwrap();
        fs::create_dir_all(dir.join("info")).unwrap();
        for (name, original, date) in items {
            fs::write(dir.join("files").join(name), format!("{name} body")).unwrap();
            let path = original
                .to_str()
                .unwrap()
                .replace('%', "%25")
                .replace(' ', "%20");
            fs::write(
                dir.join("info").join(format!("{name}.trashinfo")),
                format!("[Trash Info]\nPath={path}\nDeletionDate={date}\n"),
            )
            .unwrap();
        }
        TrashDir {
            path: dir,
            top: None,
        }
    }

    #[test]
    fn a_directory_leaving_the_trash_leaves_its_size_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = tmp.path().join("Trash");
        fs::create_dir_all(trash.join("files/my dir")).unwrap();
        fs::create_dir_all(trash.join("files/other")).unwrap();
        fs::create_dir_all(trash.join("info")).unwrap();
        fs::write(
            trash.join("directorysizes"),
            "4096 1726000000 my%20dir\n8192 1726000001 other\n",
        )
        .unwrap();
        let item = |name: &str| Trashed {
            name: name.into(),
            files: trash.join("files").join(name),
            info: trash.join("info").join(format!("{name}.trashinfo")),
            original: tmp.path().join("back").join(name),
            deleted: None,
        };
        restore(&item("my dir")).unwrap();
        assert_eq!(
            fs::read_to_string(trash.join("directorysizes")).unwrap(),
            "8192 1726000001 other\n"
        );
        purge(&item("other")).unwrap();
        assert_eq!(
            fs::read_to_string(trash.join("directorysizes")).unwrap(),
            ""
        );
    }

    #[test]
    fn info_files_say_where_and_when() {
        let (path, date) = parse_info(
            "[Trash Info]\nPath=/home/me/My%20Notes.txt\nDeletionDate=2026-09-18T12:30:05\n",
            None,
        )
        .unwrap();
        assert_eq!(path, Path::new("/home/me/My Notes.txt"));
        assert!(date.is_some());
        // a volume trash's paths are relative to the volume
        let (path, _) = parse_info(
            "[Trash Info]\nPath=docs/a.txt\nDeletionDate=2026-09-18T12:30:05\n",
            Some(Path::new("/mnt/usb")),
        )
        .unwrap();
        assert_eq!(path, Path::new("/mnt/usb/docs/a.txt"));
        assert!(parse_info("[Trash Info]\nPath=rel\n", None).is_none());
        assert_eq!(unescape_mount("/media/my\\040disk"), "/media/my disk");
    }

    #[test]
    fn the_panel_lists_reads_and_restores() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let original = home.join("gone dir").join("a.txt");
        let dir = trash(
            tmp.path(),
            &[
                ("a.txt", &original, "2026-09-18T12:00:00"),
                ("b.txt", &home.join("b.txt"), "2026-09-18T13:00:00"),
            ],
        );
        let fs_ = TrashFs::with_dirs(vec![dir.clone()]);
        let mut names: Vec<_> = fs_
            .read_dir(Path::new("/"))
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        names.sort();
        assert_eq!(names, ["a.txt", "b.txt"]);
        let mut body = String::new();
        fs_.open_read(Path::new("/b.txt"))
            .unwrap()
            .read_to_string(&mut body)
            .unwrap();
        assert_eq!(body, "b.txt body");

        // back where it came from, the directory it was in made again,
        // and gone from the trash, info file and all
        assert_eq!(fs_.restore(Path::new("/a.txt")).unwrap(), original);
        assert_eq!(fs::read_to_string(&original).unwrap(), "a.txt body");
        assert!(!dir.path.join("info/a.txt.trashinfo").exists());
        assert_eq!(fs_.read_dir(Path::new("/")).unwrap().len(), 1);

        // something there already is not overwritten
        fs::write(home.join("b.txt"), "new").unwrap();
        let err = fs_.restore(Path::new("/b.txt")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(home.join("b.txt")).unwrap(), "new");
    }

    #[test]
    fn delete_is_for_good_and_find_takes_the_newest() {
        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("x.txt");
        let dir = trash(
            tmp.path(),
            &[
                ("x.txt", &original, "2026-09-18T10:00:00"),
                ("x.2.txt", &original, "2026-09-18T11:00:00"),
            ],
        );
        let fs_ = TrashFs::with_dirs(vec![dir.clone()]);
        assert_eq!(fs_.find(&original).unwrap().name, "x.2.txt");
        fs_.writer()
            .unwrap()
            .remove_file(Path::new("/x.2.txt"))
            .unwrap();
        assert!(!dir.path.join("files/x.2.txt").exists());
        assert!(!dir.path.join("info/x.2.txt.trashinfo").exists());
        assert_eq!(fs_.find(&original).unwrap().name, "x.txt");
        // and nothing but deleting is writing
        assert!(fs_.writer().unwrap().mkdir(Path::new("/new")).is_err());
    }

    #[test]
    fn two_trashes_with_one_name_keep_both() {
        let tmp = tempfile::tempdir().unwrap();
        let a = trash(
            &tmp.path().join("a"),
            &[("n.txt", Path::new("/a/n.txt"), "")],
        );
        let b = trash(
            &tmp.path().join("b"),
            &[("n.txt", Path::new("/b/n.txt"), "")],
        );
        let mut names: Vec<_> = list(&[a, b]).into_iter().map(|i| i.name).collect();
        names.sort();
        assert_eq!(names, ["n.txt", "n.txt (2)"]);
    }
}
