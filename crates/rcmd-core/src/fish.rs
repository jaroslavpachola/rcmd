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
//!
//! The shell is reached through a [`ShellTransport`]: SSH is one, and a
//! local command is the other - `docker exec -i`, `podman exec -i`,
//! `kubectl exec -i`, `adb shell` and `sudo` are each a way of running a
//! command somewhere with a shell, so each is a panel: a container, a
//! pod, a phone, the machine as root. See [`ShellUrl`].

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
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

/// A way to run a command in a shell somewhere and read what it
/// prints: over SSH, or through a local command that reaches a
/// container, a pod, a phone or root.
pub trait ShellTransport: Send + Sync {
    /// Run one command with `stdin` fed to it, and collect everything.
    fn run(&self, command: &str, stdin: &[u8]) -> io::Result<Output>;
    /// Run one command and read its output as it arrives; a command
    /// that failed says so when the output ends.
    fn stream(&self, command: &str) -> io::Result<Box<dyn Read + Send>>;
    /// Run one command and write its input as it goes; flushing the
    /// writer ends the input and reports how the command ended.
    fn feed(&self, command: &str) -> io::Result<Box<dyn Write + Send>>;
    /// Keep an idle connection open; nothing to do for most.
    fn keepalive(&self) {}
}

/// What a remote command printed and what it exited with.
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub status: i32,
}

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
    let (session, redial) = sftp::ssh_session(url, tx, rx)?;
    let fs = Arc::new(ShellFs::new(
        Box::new(Ssh {
            session: Mutex::new(session),
            redial,
            dead: AtomicBool::new(false),
        }),
        url.prefix(),
    ));
    sftp::keep_alive(Arc::downgrade(&fs), |fs: &ShellFs| fs.transport.keepalive());
    first_listing(fs, &url.path)
}

/// The panel's first directory - the one asked for, or where the shell
/// starts - and what is in it.
fn first_listing(fs: Arc<ShellFs>, path: &Path) -> Result<Dialed, String> {
    let start = if path.as_os_str().is_empty() {
        fs.realpath(Path::new(".")).map_err(|err| err.to_string())?
    } else {
        path.to_path_buf()
    };
    let entries = fs
        .read_dir(&start)
        .map_err(|err| format!("{}: {err}", start.display()))?;
    Ok((fs, start, entries))
}

/// A panel on a shell: every operation a small script run through the
/// transport.
pub struct ShellFs {
    transport: Box<dyn ShellTransport>,
    prefix: String,
}

/// The name this had when SSH was the only way in.
pub type FishFs = ShellFs;

impl ShellFs {
    pub fn new(transport: Box<dyn ShellTransport>, prefix: String) -> Self {
        ShellFs { transport, prefix }
    }

    fn run(&self, command: &str, stdin: &[u8]) -> io::Result<Output> {
        crate::vfslog::line(">", command);
        let out = self.transport.run(command, stdin)?;
        if crate::vfslog::is_on() {
            // the payload is a listing or a whole file; what is worth
            // reading back is how it went, and anything it complained about
            crate::vfslog::line(
                "<",
                &format!("exit {}, {} byte(s)", out.status, out.stdout.len()),
            );
            for line in out.stderr.lines() {
                crate::vfslog::line("<", line);
            }
        }
        Ok(out)
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

/// SSH: one session, one command per channel.
struct Ssh {
    /// The library's session is not shared across threads; a channel,
    /// once open, carries its own share of it.
    session: Mutex<Session>,
    /// How to get the connection back when it drops.
    redial: sftp::Redial,
    /// The keepalive found the connection gone.
    dead: AtomicBool,
}

impl Ssh {
    /// A channel running `command`. A connection found gone when the
    /// channel is opened is dialed again first: nothing has been sent
    /// yet, so the command runs once either way.
    fn channel(&self, command: &str) -> io::Result<ssh2::Channel> {
        let mut session = self.session.lock().unwrap_or_else(|p| p.into_inner());
        if self.dead.swap(false, Ordering::Relaxed) {
            self.revive(&mut session)?;
        }
        let mut channel = match session.channel_session() {
            Err(err) if sftp::is_dead(&err) => {
                self.revive(&mut session)?;
                session.channel_session().map_err(ioerr)?
            }
            opened => opened.map_err(ioerr)?,
        };
        channel.exec(command).map_err(ioerr)?;
        Ok(channel)
    }

    fn revive(&self, session: &mut Session) -> io::Result<()> {
        crate::vfslog::line("!", "connection lost, dialing again");
        let fresh = self
            .redial
            .session()
            .map_err(|why| io::Error::new(io::ErrorKind::NotConnected, why))?;
        let old = std::mem::replace(session, fresh);
        std::thread::spawn(move || drop(old));
        Ok(())
    }
}

impl ShellTransport for Ssh {
    fn run(&self, command: &str, stdin: &[u8]) -> io::Result<Output> {
        let mut channel = self.channel(command)?;
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

    fn stream(&self, command: &str) -> io::Result<Box<dyn Read + Send>> {
        let mut channel = self.channel(command)?;
        channel.send_eof().map_err(ioerr)?;
        Ok(Box::new(SshRead {
            channel,
            ended: false,
        }))
    }

    fn feed(&self, command: &str) -> io::Result<Box<dyn Write + Send>> {
        Ok(Box::new(SshWrite {
            channel: Some(self.channel(command)?),
        }))
    }

    fn keepalive(&self) {
        let session = self.session.lock().unwrap_or_else(|p| p.into_inner());
        if let Err(err) = session.keepalive_send()
            && sftp::is_dead(&err)
        {
            self.dead.store(true, Ordering::Relaxed);
        }
    }
}

/// A command's output on its way in over SSH.
struct SshRead {
    channel: ssh2::Channel,
    ended: bool,
}

impl Read for SshRead {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.ended {
            return Ok(0);
        }
        let n = self.channel.read(buf)?;
        if n == 0 && !buf.is_empty() {
            self.ended = true;
            let mut stderr = String::new();
            let _ = self.channel.stderr().read_to_string(&mut stderr);
            self.channel.wait_close().map_err(ioerr)?;
            if self.channel.exit_status().unwrap_or(-1) != 0 {
                return Err(io::Error::other(first_line(&stderr)));
            }
        }
        Ok(n)
    }
}

/// A command's input on its way out over SSH: written as it comes, and
/// ended - with the command's verdict - on flush.
struct SshWrite {
    channel: Option<ssh2::Channel>,
}

impl Write for SshWrite {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.channel.as_mut() {
            Some(channel) => channel.write(buf),
            None => Err(io::Error::other("the upload is already finished")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let Some(mut channel) = self.channel.take() else {
            return Ok(());
        };
        channel.send_eof().map_err(ioerr)?;
        let mut stderr = String::new();
        let _ = channel.stderr().read_to_string(&mut stderr);
        let _ = io::copy(&mut channel, &mut io::sink());
        channel.wait_close().map_err(ioerr)?;
        match channel.exit_status().unwrap_or(-1) {
            0 => Ok(()),
            _ => Err(io::Error::other(first_line(&stderr))),
        }
    }
}

impl Drop for SshWrite {
    fn drop(&mut self) {
        // a writer dropped without a flush still gets its bytes there;
        // the error, if any, has nowhere left to go
        let _ = self.flush();
    }
}

/// How a local command takes the command it is to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wrap {
    /// As `sh -c COMMAND` after its own arguments: `docker exec -i box
    /// sh -c ...`, which passes the arguments on untouched.
    ShellArgs,
    /// As one more argument, which the far side hands to its shell:
    /// `adb shell COMMAND`.
    OneString,
}

/// A shell reached by running a local command: each operation is one
/// process, as each is one channel over SSH.
pub struct CommandTransport {
    argv: Vec<String>,
    wrap: Wrap,
}

impl CommandTransport {
    pub fn new(argv: Vec<String>, wrap: Wrap) -> Self {
        CommandTransport { argv, wrap }
    }

    fn command(&self, command: &str) -> io::Result<Command> {
        let mut args: Vec<&str> = self.argv.iter().map(String::as_str).collect();
        match self.wrap {
            Wrap::ShellArgs => args.extend(["sh", "-c", command]),
            Wrap::OneString => args.push(command),
        }
        let (program, rest) = args
            .split_first()
            .ok_or_else(|| io::Error::other("no command to reach the shell with"))?;
        let mut cmd = Command::new(program);
        cmd.args(rest);
        Ok(cmd)
    }

    fn spawn(&self, command: &str, stdin: Stdio, stdout: Stdio) -> io::Result<Child> {
        self.command(command)?
            .stdin(stdin)
            .stdout(stdout)
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| match err.kind() {
                io::ErrorKind::NotFound => io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{} is not installed", self.argv.first().map_or("", |a| a)),
                ),
                _ => err,
            })
    }
}

/// How a finished child went: its stderr's first line if it failed.
fn verdict(child: &mut Child) -> io::Result<()> {
    let mut stderr = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr);
    }
    match child.wait()?.success() {
        true => Ok(()),
        false => Err(io::Error::other(first_line(&stderr))),
    }
}

impl ShellTransport for CommandTransport {
    fn run(&self, command: &str, stdin: &[u8]) -> io::Result<Output> {
        let mut child = self.spawn(command, Stdio::piped(), Stdio::piped())?;
        // feed on a thread: a command that prints before it has read
        // all of its input would otherwise wait on us while we wait on it
        let feeder = child.stdin.take().map(|mut pipe| {
            let input = stdin.to_vec();
            std::thread::spawn(move || {
                let _ = pipe.write_all(&input);
            })
        });
        let out = child.wait_with_output()?;
        if let Some(feeder) = feeder {
            let _ = feeder.join();
        }
        Ok(Output {
            stdout: out.stdout,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            status: out.status.code().unwrap_or(-1),
        })
    }

    fn stream(&self, command: &str) -> io::Result<Box<dyn Read + Send>> {
        let mut child = self.spawn(command, Stdio::null(), Stdio::piped())?;
        let stdout = child.stdout.take().expect("piped");
        Ok(Box::new(ChildRead {
            child,
            stdout,
            ended: false,
        }))
    }

    fn feed(&self, command: &str) -> io::Result<Box<dyn Write + Send>> {
        let mut child = self.spawn(command, Stdio::piped(), Stdio::null())?;
        let stdin = child.stdin.take();
        Ok(Box::new(ChildWrite { child, stdin }))
    }
}

struct ChildRead {
    child: Child,
    stdout: ChildStdout,
    ended: bool,
}

impl Read for ChildRead {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.ended {
            return Ok(0);
        }
        let n = self.stdout.read(buf)?;
        if n == 0 && !buf.is_empty() {
            self.ended = true;
            verdict(&mut self.child)?;
        }
        Ok(n)
    }
}

impl Drop for ChildRead {
    fn drop(&mut self) {
        // read only part way: the rest is not wanted
        if !self.ended {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

struct ChildWrite {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl Write for ChildWrite {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.stdin.as_mut() {
            Some(stdin) => stdin.write(buf),
            None => Err(io::Error::other("the upload is already finished")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.stdin.take() {
            // closing its input is what tells `cat` it is done
            Some(stdin) => {
                drop(stdin);
                verdict(&mut self.child)
            }
            None => Ok(()),
        }
    }
}

impl Drop for ChildWrite {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// `docker://box/path`, `podman://box/path`, `k8s://[namespace:]pod/path`,
/// `adb://[serial]/path` and `sudo://[user]/path`: a shell somewhere
/// that a local command reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellUrl {
    pub scheme: String,
    /// The container, pod, device or user; empty where the scheme has
    /// a default (the one device, root).
    pub target: String,
    pub path: PathBuf,
}

/// The schemes [`ShellUrl`] takes.
pub const SHELL_SCHEMES: &[&str] = &["docker", "podman", "k8s", "adb", "sudo"];

impl ShellUrl {
    pub fn parse(input: &str) -> Option<ShellUrl> {
        let (scheme, rest) = input.split_once("://")?;
        if !SHELL_SCHEMES.contains(&scheme) {
            return None;
        }
        let (target, path) = match rest.split_once('/') {
            Some((target, path)) => (target, format!("/{path}")),
            None => (rest, String::new()),
        };
        // a container, pod or device has to be named; the one device
        // and root are what an empty name means for adb and sudo
        if target.is_empty() && !matches!(scheme, "adb" | "sudo") {
            return None;
        }
        Some(ShellUrl {
            scheme: scheme.to_string(),
            target: target.to_string(),
            path: PathBuf::from(path),
        })
    }

    pub fn prefix(&self) -> String {
        format!("{}://{}", self.scheme, self.target)
    }

    /// The command that reaches the shell, and how it takes a command.
    pub fn transport(&self) -> CommandTransport {
        let t = self.target.clone();
        let argv = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        match self.scheme.as_str() {
            "docker" | "podman" => {
                CommandTransport::new(argv(&[&self.scheme, "exec", "-i", &t]), Wrap::ShellArgs)
            }
            "k8s" => {
                let mut words = argv(&["kubectl", "exec", "-i"]);
                match t.split_once(':') {
                    Some((ns, pod)) => words.extend(argv(&["-n", ns, pod])),
                    None => words.push(t),
                }
                words.push("--".into());
                CommandTransport::new(words, Wrap::ShellArgs)
            }
            "adb" => {
                let mut words = argv(&["adb"]);
                if !t.is_empty() {
                    words.extend(argv(&["-s", &t]));
                }
                words.push("shell".into());
                CommandTransport::new(words, Wrap::OneString)
            }
            // sudo: -n never stops to ask for a password on a terminal
            // rcmd is drawing on - the password, when one is wanted, is
            // asked once in a dialog and given to `sudo -S -v`, and the
            // timestamp that leaves behind is what every -n runs on
            _ => {
                let mut words = argv(&["sudo", "-n"]);
                if !t.is_empty() {
                    words.extend(argv(&["-u", &t]));
                }
                CommandTransport::new(words, Wrap::ShellArgs)
            }
        }
    }
}

/// Put a panel on the shell a [`ShellUrl`] names. Nothing to log in to
/// but sudo: the local command did that, or needs no login at all.
pub fn spawn_shell(url: ShellUrl) -> ConnectHandle {
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    let host = url.prefix();
    std::thread::spawn(move || {
        let open = || Arc::new(ShellFs::new(Box::new(url.transport()), url.prefix()));
        let mut listed = first_listing(open(), &url.path);
        if url.scheme == "sudo"
            && let Err(message) = &listed
            && message.contains("password is required")
        {
            listed = match sudo_login(SUDO, &event_tx, &reply_rx) {
                Ok(()) => first_listing(open(), &url.path),
                Err(message) => Err(message),
            };
        }
        if url.scheme == "sudo"
            && let Ok((fs, _, _)) = &listed
        {
            // sudo forgets a password after a few idle minutes; a panel
            // left open on it would start failing, so it is reminded
            sudo_keep_alive(Arc::downgrade(fs));
        }
        let _ = event_tx.send(match listed {
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

/// The program `sudo://` runs.
const SUDO: &str = "sudo";

/// Ask for the password sudo wants, in the connect dialog, and hand it
/// to `sudo -S -v` - three tries, as sudo gives on a terminal. What sudo
/// keeps afterwards (its timestamp) is what the `sudo -n` of every
/// operation then runs on; the password itself is not kept.
fn sudo_login(
    program: &str,
    tx: &std::sync::mpsc::Sender<ConnectEvent>,
    rx: &std::sync::mpsc::Receiver<ConnectReply>,
) -> Result<(), String> {
    let user = std::env::var("USER").unwrap_or_default();
    let mut last = String::from("a password is required");
    for _ in 0..3 {
        let prompt = format!("[sudo] password for {user}:");
        if tx
            .send(ConnectEvent::AskPassword {
                prompt,
                echo: false,
            })
            .is_err()
        {
            return Err("cancelled".into());
        }
        let password = match rx.recv() {
            Ok(ConnectReply::Password(password)) => password,
            _ => return Err("cancelled".into()),
        };
        let mut child = Command::new(program)
            .args(["-S", "-p", "", "-v"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("{program}: {err}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(format!("{password}\n").as_bytes());
        }
        match verdict(&mut child) {
            Ok(()) => return Ok(()),
            Err(err) => last = err.to_string(),
        }
    }
    Err(format!("sudo: {last}"))
}

/// `sudo -n -v` every [`sftp::KEEPALIVE`] while the panel is open: it
/// asks nothing, and keeps the timestamp from running out.
fn sudo_keep_alive(fs: std::sync::Weak<dyn RemoteFs>) {
    sftp::keep_alive(fs, |_: &(dyn RemoteFs + 'static)| {
        let _ = Command::new(SUDO)
            .args(["-n", "-v"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
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

impl FsProvider for ShellFs {
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

    /// `cat` on the server, its output read as it arrives rather than
    /// collected whole first: a large file no longer has to fit in
    /// memory, and a view starts with the first bytes. A `cat` that
    /// failed says so at the end of its output.
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let command = format!("cat -- {}", quote(path));
        crate::vfslog::line(">", &command);
        self.transport.stream(&command)
    }

    fn writer(&self) -> Option<&dyn FsWrite> {
        Some(self)
    }
}

impl RemoteFs for ShellFs {
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

impl FsWrite for ShellFs {
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

    /// `cat` on the far side, fed as the copy goes: nothing is held
    /// here but the piece in hand.
    fn open_write(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        let command = format!("cat > {}", quote(path));
        crate::vfslog::line(">", &command);
        self.transport.feed(&command)
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

    /// A sudo that wants "hunter2", as a script.
    fn fake_sudo(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("sudo");
        std::fs::write(
            &path,
            "#!/bin/sh\nread pw\n[ \"$pw\" = hunter2 ] && exit 0\necho 'Sorry, try again.' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn sudo_asks_for_its_password_until_it_is_right_or_three_times() {
        let dir = tempfile::tempdir().unwrap();
        let sudo = fake_sudo(dir.path());
        let program = sudo.to_str().unwrap();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        for answer in ["wrong", "hunter2"] {
            reply_tx
                .send(ConnectReply::Password(answer.into()))
                .unwrap();
        }
        assert_eq!(sudo_login(program, &event_tx, &reply_rx), Ok(()));
        let asked = event_rx
            .try_iter()
            .filter(|e| matches!(e, ConnectEvent::AskPassword { echo: false, .. }))
            .count();
        assert_eq!(asked, 2);
        for _ in 0..3 {
            reply_tx
                .send(ConnectReply::Password("nope".into()))
                .unwrap();
        }
        assert_eq!(
            sudo_login(program, &event_tx, &reply_rx),
            Err("sudo: Sorry, try again.".to_string())
        );
        // a closed dialog is a cancel, not a fourth try
        drop(reply_tx);
        assert_eq!(
            sudo_login(program, &event_tx, &reply_rx),
            Err("cancelled".to_string())
        );
    }

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

    /// A panel on the local shell - the transport every scheme but
    /// SSH uses, with nothing in front of `sh -c`.
    fn local_shell() -> ShellFs {
        ShellFs::new(
            Box::new(CommandTransport::new(Vec::new(), Wrap::ShellArgs)),
            "test://".into(),
        )
    }

    #[test]
    fn a_shell_through_a_local_command_is_a_whole_filesystem() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("a file.txt"), "hello").unwrap();
        let fs = local_shell();
        let names: Vec<_> = fs
            .read_dir(dir)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["a file.txt"]);

        let mut text = String::new();
        fs.open_read(&dir.join("a file.txt"))
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "hello");

        // an upload streams: written in pieces, finished by the flush
        let target = dir.join("up.bin");
        let mut out = fs.writer().unwrap().open_write(&target).unwrap();
        for _ in 0..64 {
            out.write_all(&[7u8; 4096]).unwrap();
        }
        out.flush().unwrap();
        drop(out);
        assert_eq!(std::fs::metadata(&target).unwrap().len(), 64 * 4096);

        let w = fs.writer().unwrap();
        w.mkdir(&dir.join("sub")).unwrap();
        w.rename(&target, &dir.join("sub/moved.bin")).unwrap();
        w.set_mode(&dir.join("sub/moved.bin"), 0o600).unwrap();
        let moved = fs.stat(&dir.join("sub/moved.bin")).unwrap();
        assert_eq!((moved.size, moved.mode), (64 * 4096, 0o600));
        w.remove_file(&dir.join("sub/moved.bin")).unwrap();
        w.remove_dir(&dir.join("sub")).unwrap();
        assert!(!dir.join("sub").exists());

        // a failure says what the far side said
        let err = fs.open_read(&dir.join("missing")).and_then(|mut r| {
            let mut sink = Vec::new();
            r.read_to_end(&mut sink)
        });
        assert!(err.is_err());
    }

    #[test]
    fn shell_urls_name_the_command_that_reaches_the_shell() {
        let url = ShellUrl::parse("docker://web/var/www").unwrap();
        assert_eq!(
            (url.prefix().as_str(), url.path.as_path()),
            ("docker://web", Path::new("/var/www"))
        );
        assert_eq!(url.transport().argv, ["docker", "exec", "-i", "web"]);
        let k8s = ShellUrl::parse("k8s://prod:api-7f9/app").unwrap();
        assert_eq!(
            k8s.transport().argv,
            ["kubectl", "exec", "-i", "-n", "prod", "api-7f9", "--"]
        );
        let adb = ShellUrl::parse("adb:///sdcard").unwrap();
        assert_eq!(
            (adb.target.as_str(), adb.transport().wrap),
            ("", Wrap::OneString)
        );
        assert_eq!(
            ShellUrl::parse("sudo://postgres/")
                .unwrap()
                .transport()
                .argv,
            ["sudo", "-n", "-u", "postgres"]
        );
        // a container has to be named
        assert!(ShellUrl::parse("docker:///x").is_none());
        assert!(ShellUrl::parse("sftp://host/").is_none());
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
