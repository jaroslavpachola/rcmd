//! `fish://` - mc's protocol for a server that has a shell but no SFTP
//! subsystem. There is no protocol here in the usual sense: the client
//! runs small shell commands over SSH and reads what they print.
//!
//! rcmd's version is the same idea with a different listing script.
//! mc parses an `ls -l` variant; this asks the remote shell for one
//! record per entry, NUL-separated, so a filename with a space, a
//! newline or a `->` in it survives - none of which `ls -l` can promise.
//! `stat(1)` is used where the server has it and a `ls`-based fallback
//! where it does not, which is the split between Linux/BSD boxes and
//! the busybox ones.
//!
//! Every operation is one `exec` on the shared session. That is more
//! round trips than mc's persistent helper shell, and simpler to be
//! sure of: nothing can be left half-said on a channel that the next
//! command then reads as its own output.

use std::ffi::OsString;
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ssh2::Session;

use crate::entry::{Entry, EntryKind};
use crate::remote::{ConnectEvent, ConnectHandle, ConnectReply};
use crate::sftp::{self, SftpUrl};
use crate::vfs::{FsProvider, FsWrite, RemoteFs};

/// One listing record per entry: name, type, size, mtime, mode, link
/// target - each field NUL-terminated, so nothing in a filename can be
/// mistaken for a separator.
const LIST_SCRIPT: &str = r#"
cd -- "$D" 2>/dev/null || exit 1
for f in * .*; do
  [ "$f" = "." ] && continue
  [ "$f" = ".." ] && continue
  [ -e "$f" ] || [ -L "$f" ] || continue
  if s=$(stat -c '%s|%Y|%f|%F' -- "$f" 2>/dev/null); then
    size=${s%%|*}; r=${s#*|}; mt=${r%%|*}; r=${r#*|}; hex=${r%%|*}; kind=${r#*|}
    case "$kind" in
      "symbolic link") t=l ;;
      directory) t=d ;;
      *) t=f ;;
    esac
    mode=$(printf '%o' $((0x$hex & 07777)))
  else
    size=$(wc -c < "$f" 2>/dev/null || echo 0)
    mt=0
    if [ -L "$f" ]; then t=l; elif [ -d "$f" ]; then t=d; else t=f; fi
    mode=644
  fi
  link=
  [ "$t" = l ] && link=$(readlink -- "$f" 2>/dev/null)
  printf '%s\0%s\0%s\0%s\0%s\0%s\0' "$f" "$t" "$size" "$mt" "$mode" "$link"
done
"#;

/// Dial a server and put a panel on its shell.
pub fn spawn_connect(url: SftpUrl) -> ConnectHandle {
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    let host = url.host.clone();
    std::thread::spawn(move || {
        let outcome = dial(&url, &event_tx, &reply_rx);
        let _ = event_tx.send(match outcome {
            Ok((fs, start, entries)) => ConnectEvent::Ok { fs, start, entries },
            Err(message) => ConnectEvent::Err(message),
        });
    });
    ConnectHandle {
        events: event_rx,
        replies: reply_tx,
        host,
    }
}

type Dialed = (Arc<dyn RemoteFs>, PathBuf, Vec<Entry>);

fn dial(
    url: &SftpUrl,
    tx: &std::sync::mpsc::Sender<ConnectEvent>,
    rx: &std::sync::mpsc::Receiver<ConnectReply>,
) -> Result<Dialed, String> {
    let session = sftp::ssh_session(url, tx, rx)?;
    let fs = Arc::new_cyclic(|me| FishFs {
        session: Mutex::new(session),
        prefix: url.prefix(),
        me: me.clone(),
    });
    let start = if url.path.as_os_str().is_empty() {
        fs.realpath(Path::new("."))
            .unwrap_or_else(|_| PathBuf::from("/"))
    } else {
        url.path.clone()
    };
    let entries = fs
        .read_dir(&start)
        .map_err(|err| format!("{}: {err}", start.display()))?;
    Ok((fs, start, entries))
}

pub struct FishFs {
    /// One session, one command at a time - `exec` opens its own
    /// channel, but the library's session is not shared across threads.
    session: Mutex<Session>,
    prefix: String,
    /// A handle on itself, so an upload can outlive the `&self` call
    /// that made it. `open_write` hands back a writer, and the writer
    /// needs the connection when it is closed, not when it is made.
    me: std::sync::Weak<FishFs>,
}

/// What a remote command printed and what it exited with.
struct Output {
    stdout: Vec<u8>,
    stderr: String,
    status: i32,
}

impl FishFs {
    /// Run one command on the server. `stdin` is fed to it, which is
    /// how a file gets uploaded without a temporary anywhere.
    fn run(&self, command: &str, stdin: &[u8]) -> io::Result<Output> {
        let session = self.session.lock().unwrap_or_else(|p| p.into_inner());
        let mut channel = session.channel_session().map_err(ioerr)?;
        channel.exec(command).map_err(ioerr)?;
        if !stdin.is_empty() {
            channel.write_all(stdin)?;
        }
        channel.send_eof().map_err(ioerr)?;
        let mut stdout = Vec::new();
        channel.read_to_end(&mut stdout)?;
        let mut stderr = String::new();
        let _ = channel.stderr().read_to_string(&mut stderr);
        channel.wait_close().map_err(ioerr)?;
        let status = channel.exit_status().unwrap_or(-1);
        Ok(Output {
            stdout,
            stderr,
            status,
        })
    }

    /// Run a command that is expected to succeed and print nothing
    /// useful; a non-zero exit becomes the error the caller sees.
    fn check(&self, command: &str) -> io::Result<()> {
        let out = self.run(command, &[])?;
        if out.status == 0 {
            return Ok(());
        }
        Err(io::Error::other(first_line(&out.stderr)))
    }

    fn text(&self, command: &str) -> io::Result<String> {
        let out = self.run(command, &[])?;
        if out.status != 0 {
            return Err(io::Error::other(first_line(&out.stderr)));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    }
}

/// Wrap a path so the remote shell takes it as one literal word. Single
/// quotes stop everything a shell does except a single quote, which is
/// closed, escaped and reopened.
fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn first_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("the remote command failed")
        .trim()
        .to_string()
}

fn ioerr(err: ssh2::Error) -> io::Error {
    io::Error::other(err.to_string())
}

impl FsProvider for FishFs {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        let out = self.run(
            &format!("D={} sh -c {}", quote(dir), shell_quote(LIST_SCRIPT)),
            &[],
        )?;
        if out.status != 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                if out.stderr.trim().is_empty() {
                    "no such directory on the server".to_string()
                } else {
                    first_line(&out.stderr)
                },
            ));
        }
        Ok(parse_listing(&out.stdout))
    }

    fn stat(&self, path: &Path) -> io::Result<Entry> {
        // one listing of the parent, filtered - a per-file stat script
        // would be a second dialect to keep working
        let Some(name) = path.file_name() else {
            return Ok(Entry {
                name: OsString::from("/"),
                kind: EntryKind::Dir,
                size: 0,
                mtime: None,
                mode: 0o755,
                link_target: None,
                extra: Default::default(),
            });
        };
        let parent = path.parent().unwrap_or(Path::new("/"));
        self.read_dir(parent)?
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file on the server"))
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let out = self.run(&format!("cat -- {}", quote(path)), &[])?;
        if out.status != 0 {
            return Err(io::Error::other(first_line(&out.stderr)));
        }
        Ok(Box::new(Cursor::new(out.stdout)))
    }

    fn writer(&self) -> Option<&dyn FsWrite> {
        Some(self)
    }
}

impl RemoteFs for FishFs {
    fn prefix(&self) -> &str {
        &self.prefix
    }

    fn realpath(&self, path: &Path) -> io::Result<PathBuf> {
        if path == Path::new(".") {
            return Ok(PathBuf::from(self.text("pwd")?));
        }
        let command = format!("cd -- {} && pwd", quote(path));
        Ok(PathBuf::from(self.text(&command)?))
    }
}

/// An upload: bytes are buffered here and sent on flush, because the
/// remote `cat` wants one stream and a `Write` hands them over in
/// pieces. Flush is where it happens rather than drop, so a server
/// that refuses the write says so to the job that asked.
struct Upload {
    fs: Arc<FishFs>,
    path: PathBuf,
    buffer: Vec<u8>,
    sent: bool,
}

impl Write for Upload {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.sent {
            return Ok(());
        }
        self.sent = true;
        let out = self
            .fs
            .run(&format!("cat > {}", quote(&self.path)), &self.buffer)?;
        self.buffer = Vec::new();
        if out.status != 0 {
            return Err(io::Error::other(first_line(&out.stderr)));
        }
        Ok(())
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        // a writer dropped without a flush still gets its bytes there;
        // the error, if any, has nowhere left to go
        let _ = self.flush();
    }
}

impl FsWrite for FishFs {
    fn mkdir(&self, dir: &Path) -> io::Result<()> {
        self.check(&format!("mkdir -- {}", quote(dir)))
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.check(&format!("rm -f -- {}", quote(path)))
    }

    fn remove_dir(&self, dir: &Path) -> io::Result<()> {
        self.check(&format!("rmdir -- {}", quote(dir)))
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.check(&format!("mv -- {} {}", quote(from), quote(to)))
    }

    fn open_write(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        let fs = self
            .me
            .upgrade()
            .ok_or_else(|| io::Error::other("the connection went away"))?;
        Ok(Box::new(Upload {
            fs,
            path: path.to_path_buf(),
            buffer: Vec::new(),
            sent: false,
        }))
    }

    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        self.check(&format!("chmod {mode:o} -- {}", quote(path)))
    }

    fn set_owner(&self, path: &Path, uid: Option<u32>, gid: Option<u32>) -> io::Result<()> {
        let who = match (uid, gid) {
            (Some(uid), Some(gid)) => format!("{uid}:{gid}"),
            (Some(uid), None) => uid.to_string(),
            (None, Some(gid)) => format!(":{gid}"),
            (None, None) => return Ok(()),
        };
        self.check(&format!("chown -h {who} -- {}", quote(path)))
    }

    fn set_mtime(&self, path: &Path, mtime: SystemTime) -> io::Result<()> {
        let secs = mtime
            .duration_since(UNIX_EPOCH)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "timestamp out of range"))?
            .as_secs();
        // -d @seconds is GNU; -t needs a formatted stamp, so try both
        self.check(&format!(
            "touch -h -d @{secs} -- {p} 2>/dev/null || touch -h -t \
             $(date -u -d @{secs} +%Y%m%d%H%M.%S 2>/dev/null || echo 197001010000.00) -- {p}",
            p = quote(path)
        ))
    }

    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
        self.check(&format!("ln -s -- {} {}", quote(target), quote(link)))
    }

    fn hard_link(&self, existing: &Path, link: &Path) -> io::Result<()> {
        self.check(&format!("ln -- {} {}", quote(existing), quote(link)))
    }
}

/// Quote a whole script as one shell word.
fn shell_quote(script: &str) -> String {
    format!("'{}'", script.replace('\'', "'\\''"))
}

/// Six NUL-terminated fields per entry, in the order the script prints
/// them: name, type, size, mtime, mode, link target.
fn parse_listing(bytes: &[u8]) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut fields = bytes.split(|b| *b == 0);
    while let Some(name) = fields.next() {
        if name.is_empty() {
            break; // the trailing NUL leaves one empty field behind
        }
        let (Some(kind), Some(size), Some(mtime), Some(mode), Some(link)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            break;
        };
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let kind = match kind.first() {
            Some(b'd') => EntryKind::Dir,
            Some(b'l') => EntryKind::SymlinkFile,
            _ => EntryKind::File,
        };
        let secs: u64 = text(mtime).parse().unwrap_or(0);
        out.push(Entry {
            name: os_string(name.to_vec()),
            kind,
            size: text(size).parse().unwrap_or(0),
            mtime: (secs > 0).then(|| UNIX_EPOCH + Duration::from_secs(secs)),
            mode: u32::from_str_radix(text(mode).trim(), 8).unwrap_or(0o644),
            link_target: (!link.is_empty()).then(|| PathBuf::from(text(link))),
            extra: Default::default(),
        });
    }
    out
}

#[cfg(unix)]
fn os_string(bytes: Vec<u8>) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(bytes)
}

#[cfg(not(unix))]
fn os_string(bytes: Vec<u8>) -> OsString {
    OsString::from(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the record stream the listing script prints, so the parser
    /// is tested against the format rather than against a server.
    fn records(rows: &[(&str, &str, &str, &str, &str, &str)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, kind, size, mtime, mode, link) in rows {
            for field in [name, kind, size, mtime, mode, link] {
                out.extend_from_slice(field.as_bytes());
                out.push(0);
            }
        }
        out
    }

    #[test]
    fn reads_the_listing_the_script_prints() {
        let bytes = records(&[
            ("docs", "d", "4096", "1700000000", "755", ""),
            ("readme.txt", "f", "1234", "1700000000", "644", ""),
            ("point", "l", "10", "0", "777", "readme.txt"),
        ]);
        let entries = parse_listing(&bytes);
        let names: Vec<_> = entries
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["docs", "readme.txt", "point"]);
        assert_eq!(entries[0].kind, EntryKind::Dir);
        assert_eq!(entries[0].mode, 0o755);
        assert_eq!(
            entries[0].mtime,
            Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
        );
        assert_eq!(entries[1].size, 1234);
        assert_eq!(entries[2].kind, EntryKind::SymlinkFile);
        assert_eq!(entries[2].link_target, Some(PathBuf::from("readme.txt")));
        // a zero mtime is "the server could not say", not 1970
        assert_eq!(entries[2].mtime, None);
    }

    #[test]
    fn names_that_ls_could_not_survive_come_through() {
        // a space, a newline and the "->" that a symlink listing uses
        // as its own separator - none of which NUL-separated records
        // can be confused by
        let bytes = records(&[
            ("two words.txt", "f", "1", "0", "644", ""),
            ("line\nbreak", "f", "2", "0", "644", ""),
            ("odd -> name", "f", "3", "0", "644", ""),
        ]);
        let entries = parse_listing(&bytes);
        let names: Vec<_> = entries
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["two words.txt", "line\nbreak", "odd -> name"]);
    }

    #[test]
    fn a_truncated_record_is_dropped_rather_than_guessed_at() {
        let mut bytes = records(&[("good.txt", "f", "1", "0", "644", "")]);
        bytes.extend_from_slice(b"half\0f\0");
        let entries = parse_listing(&bytes);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name.to_string_lossy(), "good.txt");
    }

    #[test]
    fn empty_output_is_an_empty_directory() {
        assert!(parse_listing(b"").is_empty());
    }

    #[test]
    fn paths_reach_the_shell_as_one_word() {
        assert_eq!(quote(Path::new("/tmp/plain")), "'/tmp/plain'");
        assert_eq!(quote(Path::new("two words")), "'two words'");
        // the one character single quotes cannot hold
        assert_eq!(quote(Path::new("it's")), r#"'it'\''s'"#);
        // and the ones that would otherwise be the shell's
        assert_eq!(quote(Path::new("$(rm -rf /)")), "'$(rm -rf /)'");
        assert_eq!(quote(Path::new("a;b|c&d")), "'a;b|c&d'");
    }

    #[test]
    fn an_error_is_reported_by_its_first_useful_line() {
        assert_eq!(
            first_line("\n\n  cat: nope: No such file\n"),
            "cat: nope: No such file"
        );
        assert_eq!(first_line("   "), "the remote command failed");
    }
}
