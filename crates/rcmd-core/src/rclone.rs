//! Everything rclone can reach, as a panel: `cd rclone://remote/path`.
//!
//! One integration instead of forty. rclone speaks S3, Google Drive,
//! Dropbox, B2, WebDAV, Swift and the rest, it is already configured on
//! the machines where it is used, and its `lsf` and `cat` are a stable
//! command-line contract - which is a smaller thing to depend on than
//! forty protocols, and a much smaller thing to get wrong.
//!
//! Writing is rclone's too: `rcat` takes a file on its stdin as the
//! copy streams it, and `mkdir`, `deletefile`, `rmdir` and `moveto` are
//! the rest of a filesystem. What a cloud store has no notion of - a
//! mode, an owner, a symlink - says so.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime};

use crate::entry::{Entry, EntryKind};
use crate::vfs::{FsProvider, FsWrite, RemoteFs};

/// A configured rclone remote, and the panel on it.
pub struct RcloneFs {
    /// The remote's name in the user's rclone config, without the
    /// colon: `gdrive`, `s3`, `box`.
    remote: String,
    /// `rclone://<remote>`, built once because [`RemoteFs::prefix`]
    /// hands out a borrow of it.
    prefix: String,
    /// The rclone to run: `rclone` from the PATH, or a stand-in.
    program: String,
}

/// `rclone://remote/path` - the remote's name, and where in it to open.
pub struct RcloneUrl {
    pub remote: String,
    pub path: PathBuf,
}

impl RcloneUrl {
    pub fn parse(input: &str) -> Option<RcloneUrl> {
        let rest = input.strip_prefix("rclone://")?;
        let (remote, path) = match rest.split_once('/') {
            Some((remote, path)) => (remote, path),
            None => (rest, ""),
        };
        if remote.is_empty() {
            return None;
        }
        Some(RcloneUrl {
            remote: remote.to_string(),
            path: PathBuf::from(format!("/{}", path.trim_matches('/'))),
        })
    }

    pub fn prefix(&self) -> String {
        format!("rclone://{}", self.remote)
    }
}

impl RcloneFs {
    pub fn new(remote: &str) -> RcloneFs {
        RcloneFs {
            remote: remote.to_string(),
            prefix: format!("rclone://{remote}"),
            program: "rclone".into(),
        }
    }

    /// The same, run through another program - a test's stand-in.
    pub fn with_program(remote: &str, program: &str) -> RcloneFs {
        RcloneFs {
            program: program.into(),
            ..RcloneFs::new(remote)
        }
    }

    fn command(&self) -> Command {
        Command::new(&self.program)
    }

    /// `remote:path`, which is how rclone names a place. The panel's
    /// paths are absolute so they read like a filesystem; rclone wants
    /// them relative to the remote's root.
    fn target(&self, path: &Path) -> String {
        let path = path.to_string_lossy();
        format!("{}:{}", self.remote, path.trim_start_matches('/'))
    }

    fn run(&self, args: &[&str]) -> io::Result<String> {
        let out = self.command().args(args).output().map_err(|err| {
            io::Error::new(
                err.kind(),
                match err.kind() {
                    io::ErrorKind::NotFound => "rclone is not installed".to_string(),
                    _ => err.to_string(),
                },
            )
        })?;
        if !out.status.success() {
            let message = String::from_utf8_lossy(&out.stderr);
            let first = message.lines().last().unwrap_or("rclone failed").trim();
            return Err(io::Error::other(first.to_string()));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

impl FsProvider for RcloneFs {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        let text = self.run(&[
            "lsf",
            "--format",
            "pst",
            "--separator",
            "|",
            &self.target(dir),
        ])?;
        let mut entries = vec![Entry::parent()];
        entries.extend(text.lines().filter_map(parse_lsf));
        Ok(entries)
    }

    fn stat(&self, path: &Path) -> io::Result<Entry> {
        let Some(name) = path.file_name() else {
            // the root of a remote is a directory and nothing else
            return Ok(Entry {
                name: "/".into(),
                kind: EntryKind::Dir,
                ..Entry::parent()
            });
        };
        let parent = path.parent().unwrap_or(Path::new("/"));
        self.read_dir(parent)?
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file on the remote"))
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let mut child = self
            .command()
            .args(["cat", &self.target(path)])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| match err.kind() {
                io::ErrorKind::NotFound => {
                    io::Error::new(err.kind(), "rclone is not installed".to_string())
                }
                _ => err,
            })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("rclone gave nothing to read"))?;
        Ok(Box::new(Streamed { child, stdout }))
    }

    fn writer(&self) -> Option<&dyn FsWrite> {
        Some(self)
    }
}

fn no_such_thing(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("an rclone remote has no {what}"),
    )
}

impl FsWrite for RcloneFs {
    fn mkdir(&self, dir: &Path) -> io::Result<()> {
        self.run(&["mkdir", &self.target(dir)]).map(drop)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.run(&["deletefile", &self.target(path)]).map(drop)
    }

    fn remove_dir(&self, dir: &Path) -> io::Result<()> {
        self.run(&["rmdir", &self.target(dir)]).map(drop)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.run(&["moveto", &self.target(from), &self.target(to)])
            .map(drop)
    }

    /// `rclone rcat`, fed as the copy goes; the flush closes its input
    /// and says how the upload went.
    fn open_write(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        let mut child = self
            .command()
            .args(["rcat", &self.target(path)])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| match err.kind() {
                io::ErrorKind::NotFound => {
                    io::Error::new(err.kind(), "rclone is not installed".to_string())
                }
                _ => err,
            })?;
        let stdin = child.stdin.take();
        Ok(Box::new(Rcat { child, stdin }))
    }

    fn set_mode(&self, _path: &Path, _mode: u32) -> io::Result<()> {
        Err(no_such_thing("permission bits"))
    }

    fn set_owner(&self, _path: &Path, _uid: Option<u32>, _gid: Option<u32>) -> io::Result<()> {
        Err(no_such_thing("owners"))
    }

    /// The upload's own time is what most remotes keep, and rclone has
    /// no command that sets another on a file already there: a copy
    /// that preserves times keeps them on the way down, not up.
    fn set_mtime(&self, _path: &Path, _mtime: SystemTime) -> io::Result<()> {
        Ok(())
    }

    fn symlink(&self, _target: &Path, _link: &Path) -> io::Result<()> {
        Err(no_such_thing("symlinks"))
    }
}

/// A running `rclone rcat`: written to as the copy goes, finished - and
/// judged - when flushed.
struct Rcat {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
}

impl Write for Rcat {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.stdin.as_mut() {
            Some(stdin) => stdin.write(buf),
            None => Err(io::Error::other("the upload is already finished")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let Some(stdin) = self.stdin.take() else {
            return Ok(());
        };
        drop(stdin);
        let mut stderr = String::new();
        if let Some(mut err) = self.child.stderr.take() {
            let _ = err.read_to_string(&mut stderr);
        }
        match self.child.wait()?.success() {
            true => Ok(()),
            false => Err(io::Error::other(
                stderr
                    .lines()
                    .last()
                    .unwrap_or("rclone failed")
                    .trim()
                    .to_string(),
            )),
        }
    }
}

impl Drop for Rcat {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

impl RemoteFs for RcloneFs {
    fn prefix(&self) -> &str {
        &self.prefix
    }

    fn realpath(&self, path: &Path) -> io::Result<PathBuf> {
        // a remote has no working directory of its own: "." is its root
        Ok(match path == Path::new(".") {
            true => PathBuf::from("/"),
            false => path.to_path_buf(),
        })
    }
}

/// A running `rclone cat`, read from its stdout and reaped when the
/// reader is dropped - a half-read file must not leave a process
/// behind.
struct Streamed {
    child: Child,
    stdout: std::process::ChildStdout,
}

impl Read for Streamed {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stdout.read(buf)
    }
}

impl Drop for Streamed {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One line of `rclone lsf --format pst --separator |`:
/// `name|size|2026-08-27 10:11:12`, with a trailing slash on the name
/// where it is a directory.
fn parse_lsf(line: &str) -> Option<Entry> {
    let mut fields = line.split('|');
    let name = fields.next()?;
    if name.is_empty() {
        return None;
    }
    let size = fields.next().unwrap_or("").trim();
    let time = fields.next().unwrap_or("").trim();
    let is_dir = name.ends_with('/');
    Some(Entry {
        name: name.trim_end_matches('/').into(),
        kind: match is_dir {
            true => EntryKind::Dir,
            false => EntryKind::File,
        },
        size: size.parse().unwrap_or(0),
        mtime: parse_time(time),
        // a cloud remote has no unix mode worth showing; 0o644 / 0o755
        // is what every one of them means by "a file" and "a folder"
        mode: if is_dir { 0o755 } else { 0o644 },
        link_target: None,
        extra: Default::default(),
    })
}

/// `2026-08-27 10:11:12` (rclone's default) or the same with a `T` and
/// a zone, which is what `--format t` gives on some backends. Anything
/// else is no time at all rather than a wrong one.
fn parse_time(text: &str) -> Option<SystemTime> {
    let text = text.trim();
    if text.len() < 19 {
        return None;
    }
    let bytes = text.as_bytes();
    let num = |from: usize, to: usize| text.get(from..to)?.parse::<i64>().ok();
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let (hour, minute, second) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second;
    match seconds >= 0 {
        true => SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64)),
        false => SystemTime::UNIX_EPOCH.checked_sub(Duration::from_secs(-seconds as u64)),
    }
}

/// Days since 1970-01-01 for a civil date - Howard Hinnant's algorithm,
/// which is the short one that is also right.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in rclone over a local directory: enough of `lsf`,
    /// `cat`, `rcat`, `mkdir`, `deletefile`, `rmdir` and `moveto` to hold
    /// rcmd to its half of the contract.
    fn fake_rclone(dir: &Path, root: &Path) -> PathBuf {
        let script = dir.join("rclone");
        std::fs::write(
            &script,
            format!(
                r#"#!/bin/sh
root='{}'
p() {{ echo "$root/${{1#demo:}}"; }}
case "$1" in
  lsf) d=$(p "$6"); [ -d "$d" ] || exit 1
       for f in "$d"/*; do [ -e "$f" ] || continue; n=$(basename "$f")
         if [ -d "$f" ]; then printf '%s/|-1|2026-08-26 09:00:00\n' "$n"
         else printf '%s|%s|2026-08-26 09:00:00\n' "$n" $(wc -c < "$f"); fi; done ;;
  cat) cat "$(p "$2")" ;;
  rcat) cat > "$(p "$2")" ;;
  mkdir) mkdir -p "$(p "$2")" ;;
  deletefile) rm "$(p "$2")" ;;
  rmdir) rmdir "$(p "$2")" ;;
  moveto) mv "$(p "$2")" "$(p "$3")" ;;
  *) echo "unknown $1" >&2; exit 1 ;;
esac
"#,
                root.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[test]
    fn a_remote_is_written_through_rclone() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("remote");
        std::fs::create_dir(&root).unwrap();
        let script = fake_rclone(tmp.path(), &root);
        let fs = RcloneFs::with_program("demo", &script.to_string_lossy());
        let w = fs.writer().unwrap();
        w.mkdir(Path::new("/docs")).unwrap();
        let mut out = w.open_write(Path::new("/docs/a.txt")).unwrap();
        out.write_all(b"up it goes").unwrap();
        out.flush().unwrap();
        drop(out);
        assert_eq!(
            std::fs::read_to_string(root.join("docs/a.txt")).unwrap(),
            "up it goes"
        );
        let names: Vec<_> = fs
            .read_dir(Path::new("/docs"))
            .unwrap()
            .into_iter()
            .filter(|e| !e.is_parent())
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["a.txt"]);
        w.rename(Path::new("/docs/a.txt"), Path::new("/docs/b.txt"))
            .unwrap();
        w.remove_file(Path::new("/docs/b.txt")).unwrap();
        w.remove_dir(Path::new("/docs")).unwrap();
        assert!(!root.join("docs").exists());
        assert_eq!(
            w.set_mode(Path::new("/x"), 0o644).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[test]
    fn lsf_lines_become_entries() {
        let file = parse_lsf("notes.txt|1234|2026-08-27 10:11:12").unwrap();
        assert_eq!(file.name, "notes.txt");
        assert_eq!(file.size, 1234);
        assert!(!file.is_dir());
        let dir = parse_lsf("photos/|-1|2026-08-27 10:11:12").unwrap();
        assert_eq!(
            dir.name, "photos",
            "the slash says what it is, not what it is called"
        );
        assert!(dir.is_dir());
        assert_eq!(dir.size, 0, "a size that is not a number is not a size");
        // a name with a space, and one with no time at all
        let spaced = parse_lsf("my file.txt|10|").unwrap();
        assert_eq!(spaced.name, "my file.txt");
        assert_eq!(spaced.mtime, None);
        assert!(parse_lsf("").is_none());
    }

    #[test]
    fn times_come_back_as_instants() {
        // 2026-08-27 10:11:12 UTC
        let at = parse_time("2026-08-27 10:11:12").unwrap();
        let secs = at.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(secs, 1_787_825_472);
        // the epoch itself, and a date before it
        assert_eq!(
            parse_time("1970-01-01 00:00:00").unwrap(),
            SystemTime::UNIX_EPOCH
        );
        assert!(parse_time("1969-12-31 23:59:59").unwrap() < SystemTime::UNIX_EPOCH);
        // rclone's other spelling, and nonsense
        assert!(parse_time("2026-08-27T10:11:12.000000000+00:00").is_some());
        assert!(parse_time("").is_none());
        assert!(parse_time("yesterday afternoon").is_none());
        assert!(parse_time("2026/08/27 10:11:12").is_none());
    }

    #[test]
    fn a_url_names_a_remote_and_a_place_in_it() {
        let url = RcloneUrl::parse("rclone://gdrive/backups/2026").unwrap();
        assert_eq!(url.remote, "gdrive");
        assert_eq!(url.path, PathBuf::from("/backups/2026"));
        assert_eq!(url.prefix(), "rclone://gdrive");
        // the root of a remote, spelled both ways
        assert_eq!(
            RcloneUrl::parse("rclone://s3").unwrap().path,
            PathBuf::from("/")
        );
        assert_eq!(
            RcloneUrl::parse("rclone://s3/").unwrap().path,
            PathBuf::from("/")
        );
        assert!(RcloneUrl::parse("rclone:///no-remote").is_none());
        assert!(RcloneUrl::parse("sftp://host/path").is_none());
    }

    #[test]
    fn paths_are_handed_to_rclone_the_way_it_wants_them() {
        let fs = RcloneFs::new("gdrive");
        assert_eq!(fs.target(Path::new("/backups/2026")), "gdrive:backups/2026");
        assert_eq!(fs.target(Path::new("/")), "gdrive:");
    }
}
