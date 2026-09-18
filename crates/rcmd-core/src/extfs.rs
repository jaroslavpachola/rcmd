//! User-defined filesystems, mc's extfs: a file that matches a rule is
//! entered like an archive, and what is in it comes from two commands
//! the rule names - one that lists it, one that copies a member out.
//! mc's own helper scripts (`/usr/lib/mc/extfs.d/*`) work as they are:
//! `script = ".../uzip"` means `uzip list %f` and `uzip copyout %f %p %t`.
//!
//! The listing is `ls -l`-shaped, one member a line, its name the path
//! inside the file - which is what mc's scripts print. Read-only.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::entry::{Entry, EntryKind};
use crate::vfs::FsProvider;

/// How to read one kind of file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtRule {
    /// The command that lists it: `%f` is the file.
    pub list: String,
    /// The command that copies one member out: `%f` the file, `%p` the
    /// member's path inside it, `%t` the file to write.
    pub copyout: String,
}

impl ExtRule {
    /// A rule from one of mc's extfs helper scripts.
    pub fn script(script: &str) -> ExtRule {
        ExtRule {
            list: format!("{script} list %f"),
            copyout: format!("{script} copyout %f %p %t"),
        }
    }
}

/// A file opened through its rule: the listing read once, the members
/// copied out as they are read.
pub struct ExtFs {
    file: PathBuf,
    rule: ExtRule,
    /// Each directory inside, and what is in it.
    dirs: BTreeMap<PathBuf, Vec<Entry>>,
}

/// A word the shell takes literally.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// A command template with its `%f`, `%p` and `%t` filled in, quoted.
fn expand(template: &str, file: &Path, member: &str, target: &Path) -> String {
    template
        .replace("%f", &quote(&file.to_string_lossy()))
        .replace("%p", &quote(member))
        .replace("%t", &quote(&target.to_string_lossy()))
}

fn run(command: &str) -> io::Result<Vec<u8>> {
    let out = Command::new("sh").args(["-c", command]).output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let line = stderr
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("the command failed");
        return Err(io::Error::other(line.trim().to_string()));
    }
    Ok(out.stdout)
}

impl ExtFs {
    /// List `file` through `rule`.
    pub fn open(file: &Path, rule: &ExtRule) -> io::Result<ExtFs> {
        let text = run(&expand(&rule.list, file, "", Path::new("")))?;
        let mut dirs: BTreeMap<PathBuf, Vec<Entry>> = BTreeMap::new();
        let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
        dirs.insert(PathBuf::new(), Vec::new());
        for (path, mut entry) in parse(&String::from_utf8_lossy(&text)) {
            // every directory on the way down exists, listed or not
            let mut at = PathBuf::new();
            for part in path.parent().into_iter().flat_map(Path::components) {
                let Component::Normal(name) = part else {
                    continue;
                };
                let parent = at.clone();
                at.push(name);
                if seen.insert(at.clone()) {
                    dirs.entry(parent)
                        .or_default()
                        .push(dir_entry(name.to_os_string()));
                }
                dirs.entry(at.clone()).or_default();
            }
            let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
            if entry.kind == EntryKind::Dir {
                dirs.entry(path.clone()).or_default();
                if !seen.insert(path.clone()) {
                    continue;
                }
            }
            entry.name = path.file_name().unwrap_or_default().to_os_string();
            dirs.entry(parent).or_default().push(entry);
        }
        Ok(ExtFs {
            file: file.to_path_buf(),
            rule: rule.clone(),
            dirs,
        })
    }

    fn inside(path: &Path) -> PathBuf {
        path.components()
            .filter(|c| matches!(c, Component::Normal(_)))
            .collect()
    }
}

fn dir_entry(name: OsString) -> Entry {
    Entry {
        name,
        kind: EntryKind::Dir,
        size: 0,
        mtime: None,
        mode: 0o755,
        link_target: None,
        extra: Default::default(),
    }
}

/// `ls -l`-shaped lines, as mc's scripts print them: permissions, link
/// count, owner, group, size, a date in two or three columns, then the
/// member's path - which may hold spaces.
pub fn parse(text: &str) -> Vec<(PathBuf, Entry)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 || fields[0].len() < 10 {
            continue;
        }
        // `2024-01-31 12:00` or `01-31-2024 12:00` is two columns;
        // `Jan 31 12:00` or `Jan 31 2024` is three
        let date_columns = if fields[5].contains('-') { 2 } else { 3 };
        let Some(at) = field_start(line, 5 + date_columns) else {
            continue;
        };
        let perms = fields[0];
        let name = &line[at..];
        let (name, link) = match (perms.starts_with('l'), name.split_once(" -> ")) {
            (true, Some((name, target))) => (name, Some(PathBuf::from(target))),
            _ => (name, None),
        };
        let path: PathBuf = Path::new(name.trim_start_matches("./"))
            .components()
            .filter(|c| matches!(c, Component::Normal(_)))
            .collect();
        if path.as_os_str().is_empty() {
            continue;
        }
        let kind = match perms.as_bytes()[0] {
            b'd' => EntryKind::Dir,
            b'l' => EntryKind::SymlinkFile,
            _ => EntryKind::File,
        };
        let mut mode = 0u32;
        for (i, c) in perms.chars().skip(1).take(9).enumerate() {
            if c != '-' {
                mode |= 1 << (8 - i);
            }
        }
        out.push((
            path,
            Entry {
                name: OsString::new(),
                kind,
                size: fields[4].parse().unwrap_or(0),
                mtime: None,
                mode,
                link_target: link,
                extra: Default::default(),
            },
        ));
    }
    out
}

/// Byte offset where the nth whitespace-separated field begins.
fn field_start(line: &str, n: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let (mut index, mut at) = (0, 0);
    while at < bytes.len() {
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if index == n {
            return (at < bytes.len()).then_some(at);
        }
        while at < bytes.len() && !bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        index += 1;
    }
    None
}

impl FsProvider for ExtFs {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        self.dirs
            .get(&Self::inside(dir))
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such directory in it"))
    }

    fn stat(&self, path: &Path) -> io::Result<Entry> {
        let inside = Self::inside(path);
        let Some(name) = inside.file_name() else {
            return Ok(dir_entry(OsString::from("/")));
        };
        let parent = inside.parent().map(Path::to_path_buf).unwrap_or_default();
        self.dirs
            .get(&parent)
            .and_then(|entries| entries.iter().find(|e| e.name == name))
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such member"))
    }

    /// The member copied out to a file of its own, which is opened and
    /// unlinked at once: the reader keeps it while it reads, and nothing
    /// is left behind after.
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let target = std::env::temp_dir().join(format!(
            ".rcmd-extfs-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let member = Self::inside(path).to_string_lossy().into_owned();
        let copied = run(&expand(&self.rule.copyout, &self.file, &member, &target));
        let file = copied.and_then(|_| std::fs::File::open(&target));
        let _ = std::fs::remove_file(&target);
        Ok(Box::new(file?))
    }

    fn reopen(&self) -> Option<io::Result<Arc<dyn FsProvider>>> {
        Some(ExtFs::open(&self.file, &self.rule).map(|fs| Arc::new(fs) as Arc<dyn FsProvider>))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listings_in_both_date_shapes_become_a_tree() {
        let text = "\
-rw-r--r-- 1 root root 12 2024-01-31 12:00 docs/read me.txt
drwxr-xr-x 1 root root 0 Jan 31 12:00 empty
lrwxrwxrwx 1 root root 3 Jan 31 2024 docs/link -> read me.txt
-rw-r--r-- 1 root root 5 01-31-2024 12:00 ./top.txt
";
        let parsed = parse(text);
        let names: Vec<_> = parsed
            .iter()
            .map(|(p, _)| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["docs/read me.txt", "empty", "docs/link", "top.txt"]);
        assert_eq!(parsed[0].1.size, 12);
        assert_eq!(parsed[2].1.link_target, Some(PathBuf::from("read me.txt")));
    }

    #[test]
    fn a_file_is_read_through_its_two_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("thing.box");
        std::fs::write(&file, "x").unwrap();
        // a "format" whose listing is fixed and whose members all say
        // what they are
        let rule = ExtRule {
            list: "echo '-rw-r--r-- 1 u g 6 2024-01-01 00:00 inner/one.txt'".into(),
            copyout: "printf 'from %p' > %t".into(),
        };
        let fs = ExtFs::open(&file, &rule).unwrap();
        let top: Vec<_> = fs
            .read_dir(Path::new("/"))
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(top, ["inner"]);
        assert!(fs.stat(Path::new("/inner")).unwrap().is_dir());
        let mut text = String::new();
        fs.open_read(Path::new("/inner/one.txt"))
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "from inner/one.txt");
        // mc's scripts, by their name alone
        assert_eq!(
            ExtRule::script("/x/uzip").copyout,
            "/x/uzip copyout %f %p %t"
        );
    }
}
