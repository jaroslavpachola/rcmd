//! The disks: `disks://` lists one row a mounted filesystem - where it
//! is mounted, from what, as what, how big and how full - read from
//! `/proc/self/mountinfo` and a `statvfs` of each. F3 describes one, and
//! Enter on it opens the mount point in the other panel. Read-only
//! towards the disks themselves: nothing here mounts, unmounts or
//! writes to a volume.
//!
//! Everything PLAN8 builds that is not drawing lives in this module,
//! so that it can move out of rcmd into a crate of its own without the
//! TUI coming along.
//!
//! A volume is named by its mount point, with each `/` written `∕`
//! (U+2215) so the name stays one path component and still reads as
//! the path: the root is `∕`, `/home` is `∕home`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use crate::entry::{Entry, EntryKind, EntryStat};
use crate::vfs::{FsProvider, RemoteFs};

pub const PREFIX: &str = "disks://";

/// A `statvfs` that has not answered in this long is on a server that
/// is not answering either: the volume is listed without its sizes
/// rather than holding the listing up.
const STAT_WAIT: Duration = Duration::from_millis(800);

/// One mounted filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    pub point: String,
    /// What is mounted: a device, or what the filesystem calls itself.
    pub source: String,
    pub fstype: String,
    /// Bytes: in all, free to anyone who may write there, and used.
    /// All 0 where the filesystem would not say.
    pub total: u64,
    pub free: u64,
    pub used: u64,
    pub inodes_total: u64,
    pub inodes_free: u64,
    /// The kernel's own and the in-memory kinds - proc, sysfs, tmpfs,
    /// a snap's squashfs - and a bind mount, which is a directory of a
    /// volume listed already: no disk, and not listed.
    pub pseudo: bool,
    /// The sizes did not come back in time (a network mount that has
    /// gone away).
    pub stalled: bool,
}

impl Volume {
    /// How full, in whole percent of what is not reserved - what `df`
    /// says in its Use% column.
    pub fn used_percent(&self) -> Option<u64> {
        let usable = self.used + self.free;
        (usable > 0).then(|| (self.used * 100).div_ceil(usable))
    }
}

/// The filesystem types that hold no disk of their own.
const PSEUDO: &[&str] = &[
    "proc",
    "sysfs",
    "devtmpfs",
    "devpts",
    "tmpfs",
    "ramfs",
    "cgroup",
    "cgroup2",
    "securityfs",
    "pstore",
    "bpf",
    "debugfs",
    "tracefs",
    "mqueue",
    "hugetlbfs",
    "configfs",
    "fusectl",
    "autofs",
    "binfmt_misc",
    "efivarfs",
    "nsfs",
    "rpc_pipefs",
    "overlay",
    "squashfs",
    "fuse.portal",
    "fuse.gvfsd-fuse",
];

pub fn is_pseudo(fstype: &str) -> bool {
    PSEUDO.contains(&fstype)
}

/// One line of the mount table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mounted {
    pub source: String,
    pub point: String,
    pub fstype: String,
    /// A bind mount: a directory of a filesystem mounted somewhere
    /// else as well, where a mount of the whole of it has `/`.
    pub bind: bool,
}

/// `/proc/self/mountinfo`: `id parent major:minor root point options
/// [optional fields] - type source super-options`, the escapes (`\040`
/// for a space) undone. A point mounted over again is listed once, as
/// the last mount there - the one that is seen.
pub fn parse_mountinfo(text: &str) -> Vec<Mounted> {
    let mut out: Vec<Mounted> = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let Some(dash) = fields.iter().position(|f| *f == "-") else {
            continue;
        };
        let (Some(root), Some(point), Some(fstype), Some(source)) = (
            fields.get(3),
            fields.get(4),
            fields.get(dash + 1),
            fields.get(dash + 2),
        ) else {
            continue;
        };
        let point = crate::mounts::unescape(point);
        out.retain(|m| m.point != point);
        out.push(Mounted {
            source: crate::mounts::unescape(source),
            point,
            fstype: fstype.to_string(),
            bind: *root != "/",
        });
    }
    out
}

/// What a `statvfs` says, in bytes and inodes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Sizes {
    total: u64,
    free: u64,
    used: u64,
    inodes_total: u64,
    inodes_free: u64,
}

fn statvfs(point: &str) -> Option<Sizes> {
    let c = std::ffi::CString::new(point).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let unit = st.f_frsize as u64;
    let total = st.f_blocks as u64 * unit;
    Some(Sizes {
        total,
        free: st.f_bavail as u64 * unit,
        used: total.saturating_sub(st.f_bfree as u64 * unit),
        inodes_total: st.f_files as u64,
        inodes_free: st.f_favail as u64,
    })
}

/// The sizes of every point, each asked on a thread of its own and
/// waited for together: one server that does not answer costs
/// [`STAT_WAIT`] once, not once a mount. `None` for one that did not
/// answer in time; a thread still stuck is left to finish on its own.
fn sizes_of(points: &[String]) -> Vec<Option<Option<Sizes>>> {
    let (tx, rx) = mpsc::channel();
    for (i, point) in points.iter().enumerate() {
        let (tx, point) = (tx.clone(), point.clone());
        std::thread::spawn(move || {
            let _ = tx.send((i, statvfs(&point)));
        });
    }
    drop(tx);
    let mut out = vec![None; points.len()];
    let deadline = std::time::Instant::now() + STAT_WAIT;
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(left) {
            Ok((i, sizes)) => out[i] = Some(sizes),
            Err(_) => break,
        }
    }
    out
}

/// Every mounted filesystem, from `mountinfo` (the text of a
/// `/proc/self/mountinfo`) or, where there is none, from `df`.
pub fn volumes_from(mountinfo: Option<&str>) -> Vec<Volume> {
    let table: Vec<Mounted> = match mountinfo {
        Some(text) => parse_mountinfo(text),
        // no /proc: df names the mounts, if not their types
        None => crate::mounts::mounts()
            .into_iter()
            .map(|m| Mounted {
                source: m.source,
                point: m.point,
                fstype: String::new(),
                bind: false,
            })
            .collect(),
    };
    let points: Vec<String> = table.iter().map(|m| m.point.clone()).collect();
    let sizes = sizes_of(&points);
    table
        .into_iter()
        .zip(sizes)
        .map(|(m, sizes)| {
            let stalled = sizes.is_none();
            let s = sizes.flatten().unwrap_or_default();
            Volume {
                pseudo: m.bind || is_pseudo(&m.fstype) || (!stalled && s.total == 0),
                point: m.point,
                source: m.source,
                fstype: m.fstype,
                total: s.total,
                free: s.free,
                used: s.used,
                inodes_total: s.inodes_total,
                inodes_free: s.inodes_free,
                stalled,
            }
        })
        .collect()
}

pub fn volumes() -> Vec<Volume> {
    let table = std::fs::read_to_string("/proc/self/mountinfo").ok();
    volumes_from(table.as_deref())
}

/// A mount point as a name: `/` written `∕`.
pub fn name_of(point: &str) -> OsString {
    point.replace('/', "\u{2215}").into()
}

/// ...and back.
pub fn point_of(name: &std::ffi::OsStr) -> String {
    name.to_string_lossy().replace('\u{2215}', "/")
}

fn entry_of(volume: Volume) -> Entry {
    Entry {
        name: name_of(&volume.point),
        kind: EntryKind::File,
        size: volume.total,
        mtime: None,
        mode: 0o444,
        link_target: None,
        extra: EntryStat {
            volume: Some(Arc::new(volume)),
            ..Default::default()
        },
    }
}

/// `disks://` as a filesystem of one directory.
pub struct DisksFs {
    /// The mount table to read; `None` asks `df`.
    table: Option<PathBuf>,
    /// The volumes of the last listing, by name, for F3 and the line
    /// under the panel.
    seen: Mutex<HashMap<OsString, Arc<Volume>>>,
}

impl Default for DisksFs {
    fn default() -> Self {
        Self::new()
    }
}

impl DisksFs {
    pub fn new() -> Self {
        let table = PathBuf::from("/proc/self/mountinfo");
        Self::with_table(table.exists().then_some(table))
    }

    /// A mount table somewhere else - a test's own.
    pub fn with_table(table: Option<PathBuf>) -> Self {
        DisksFs {
            table,
            seen: Mutex::new(HashMap::new()),
        }
    }

    fn seen(&self) -> std::sync::MutexGuard<'_, HashMap<OsString, Arc<Volume>>> {
        self.seen.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The disks: what is mounted with a disk of its own behind it. The
    /// kernel's filesystems, the ones in memory, snaps and bind mounts
    /// are a dozen rows of noise around the three that are the answer.
    fn list(&self) -> io::Result<Vec<Entry>> {
        let text = match &self.table {
            Some(path) => Some(std::fs::read_to_string(path)?),
            None => None,
        };
        let entries: Vec<Entry> = volumes_from(text.as_deref())
            .into_iter()
            .filter(|v| !v.pseudo)
            .map(entry_of)
            .collect();
        let mut seen = self.seen();
        seen.clear();
        for entry in &entries {
            if let Some(volume) = &entry.extra.volume {
                seen.insert(entry.name.clone(), volume.clone());
            }
        }
        Ok(entries)
    }

    fn volume(&self, path: &Path) -> io::Result<Arc<Volume>> {
        let name = path.file_name().unwrap_or_default();
        self.seen().get(name).cloned().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is not mounted now", point_of(name)),
            )
        })
    }
}

/// F3 on a volume: all there is to say about it, one fact a line.
pub fn describe(v: &Volume) -> String {
    let bytes = |n: u64| match n < 1024 {
        true => format!("{n} bytes"),
        false => format!("{n} bytes ({})", human(n)),
    };
    let percent = |part: u64, whole: u64| match whole {
        0 => String::new(),
        _ => format!(" ({}%)", (part * 100).div_ceil(whole)),
    };
    let mut out = format!(
        "{}\n\nmount point: {}\ndevice:      {}\ntype:        {}\n",
        v.point,
        v.point,
        v.source,
        if v.fstype.is_empty() { "?" } else { &v.fstype },
    );
    if v.stalled {
        out.push_str("\nsizes:       not known - the filesystem did not answer\n");
        return out;
    }
    let inodes_used = v.inodes_total.saturating_sub(v.inodes_free);
    out.push_str(&format!(
        "\nsize:        {}\nused:        {}{}\nfree:        {}\n\
         inodes:      {} in all, {} used{}\n",
        bytes(v.total),
        bytes(v.used),
        v.used_percent()
            .map(|p| format!(", {p}%"))
            .unwrap_or_default(),
        bytes(v.free),
        v.inodes_total,
        inodes_used,
        percent(inodes_used, v.inodes_total),
    ));
    if v.pseudo {
        out.push_str("\nno disk of its own: the kernel's, held in memory, or a directory\n");
        out.push_str("of a volume mounted here as well (a bind mount)\n");
    }
    out
}

/// A size to one decimal in the largest unit it fills: 913.8G, not
/// 935681M. Seven characters at most, a panel column's width.
pub fn human(n: u64) -> String {
    const UNITS: [&str; 5] = ["", "K", "M", "G", "T"];
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{n}"),
        _ => format!("{size:.1}{}", UNITS[unit]),
    }
}

impl FsProvider for DisksFs {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        match is_top(dir) {
            true => self.list(),
            false => Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "a volume is opened in the other panel: Enter",
            )),
        }
    }

    fn stat(&self, path: &Path) -> io::Result<Entry> {
        if is_top(path) {
            return Ok(Entry {
                name: OsString::from("/"),
                kind: EntryKind::Dir,
                size: 0,
                mtime: None,
                mode: 0o555,
                link_target: None,
                extra: Default::default(),
            });
        }
        Ok(entry_of((*self.volume(path)?).clone()))
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let volume = self.volume(path)?;
        let text = describe(&volume);
        Ok(Box::new(io::Cursor::new(text.into_bytes())))
    }

    /// The device and the type, for the line under the panel.
    fn note(&self, path: &Path) -> Option<String> {
        let v = self.seen().get(path.file_name()?).cloned()?;
        Some(format!("{} {}", v.source, v.fstype))
    }
}

impl RemoteFs for DisksFs {
    fn prefix(&self) -> &str {
        PREFIX
    }

    /// There is one directory, and everything is in it.
    fn realpath(&self, _path: &Path) -> io::Result<PathBuf> {
        Ok(PathBuf::from("/"))
    }
}

fn is_top(path: &Path) -> bool {
    !path.components().any(|c| matches!(c, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
        26 1 253:1 / / rw,relatime shared:1 - ext4 /dev/sda2 rw\n\
        22 26 0:21 / /proc rw,nosuid shared:12 - proc proc rw\n\
        40 26 8:17 / /media/My\\040Disk rw shared:30 - vfat /dev/sdb1 rw\n\
        41 26 0:40 / /mnt rw - tmpfs tmpfs rw\n\
        42 41 8:33 / /mnt rw master:3 - ext4 /dev/sdc1 rw\n\
        50 26 253:1 /usr/share/hunspell /var/snap/ff/hunspell ro - ext4 /dev/sda2 rw\n";

    #[test]
    fn the_mount_table_is_read_with_its_escapes_binds_and_overmounts() {
        let rows = parse_mountinfo(MOUNTINFO);
        assert_eq!(rows.len(), 5, "/mnt once, as mounted last");
        assert_eq!(rows[2].point, "/media/My Disk");
        assert_eq!(
            rows[3],
            Mounted {
                source: "/dev/sdc1".into(),
                point: "/mnt".into(),
                fstype: "ext4".into(),
                bind: false,
            }
        );
        assert!(rows[4].bind, "a directory of / mounted again");
        assert!(!rows[0].bind);
        assert!(is_pseudo("proc") && is_pseudo("squashfs") && !is_pseudo("ext4"));
    }

    #[test]
    fn a_mount_point_is_one_name_and_back() {
        assert_eq!(name_of("/"), "\u{2215}");
        assert_eq!(point_of(&name_of("/media/My Disk")), "/media/My Disk");
        assert!(!name_of("/home/x").to_string_lossy().contains('/'));
    }

    #[test]
    fn used_is_counted_against_what_is_not_reserved() {
        let v = Volume {
            point: "/".into(),
            source: "/dev/x".into(),
            fstype: "ext4".into(),
            total: 1000,
            free: 150,
            used: 800,
            inodes_total: 10,
            inodes_free: 5,
            pseudo: false,
            stalled: false,
        };
        // 800 of 950 usable, rounded up as df does
        assert_eq!(v.used_percent(), Some(85));
        let text = describe(&v);
        assert!(text.contains("device:      /dev/x"), "{text}");
        assert!(text.contains("used:        800 bytes, 85%"), "{text}");
        assert_eq!(human(981_132_795_904), "913.8G");
        assert!(text.contains("5 used (50%)"), "{text}");
    }

    /// The root is always there, with sizes, and a listing names every
    /// volume the table has.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_table_is_listed_with_the_sizes_of_its_volumes() {
        let dir = std::env::temp_dir().join(format!("rcmd-disks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let table = dir.join("mounts");
        std::fs::write(
            &table,
            "1 0 8:1 / / rw - ext4 /dev/root rw\n2 1 0:4 / /proc rw - proc proc rw\n",
        )
        .unwrap();
        let fs = DisksFs::with_table(Some(table));
        let entries = fs.read_dir(Path::new("/")).unwrap();
        assert_eq!(entries.len(), 1, "proc is no disk, and not listed");
        let root = entries[0].extra.volume.clone().unwrap();
        assert_eq!(root.point, "/");
        assert!(root.total > 0 && !root.pseudo && !root.stalled);
        let path = Path::new("/").join(&entries[0].name);
        assert_eq!(fs.note(&path).as_deref(), Some("/dev/root ext4"));
        let mut text = String::new();
        fs.open_read(&path)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert!(text.contains("mount point: /"), "{text}");
        assert!(fs.read_dir(&path).is_err(), "a volume is not entered");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
