//! SFTP remote filesystem: an [`FsProvider`]+[`FsWrite`] over a blocking
//! `ssh2` session (decision D1: worker threads, no async runtime).
//!
//! Connecting is interactive (host-key confirmation, password prompts),
//! so it runs on a worker thread speaking [`ConnectEvent`]s to the UI —
//! the same ask/reply shape as the job engine. All session use is
//! serialized behind one mutex; SFTP round-trips dominate, the lock is
//! noise. One connection is shared by both panels and any jobs on it.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ssh2::{CheckResult, FileStat, HashType, KnownHostFileKind, OpenFlags, OpenType, Session};

use crate::entry::{Entry, EntryKind};
use crate::vfs::{FsProvider, FsWrite};

/// Blocking-call timeout on the session; a dead link surfaces as an
/// error dialog instead of a hung worker.
const IO_TIMEOUT_MS: u32 = 30_000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// `sftp://[user@]host[:port][/path]`. An empty path means "the remote
/// home directory", resolved once connected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SftpUrl {
    pub user: String,
    pub host: String,
    pub port: u16,
    pub path: PathBuf,
}

impl SftpUrl {
    pub fn parse(s: &str) -> Option<SftpUrl> {
        let rest = s.strip_prefix("sftp://")?;
        let (hostpart, path) = match rest.find('/') {
            Some(i) => (&rest[..i], PathBuf::from(&rest[i..])),
            None => (rest, PathBuf::new()),
        };
        let (user, hostport) = match hostpart.rsplit_once('@') {
            Some((u, h)) => (u.to_string(), h),
            None => (default_user(), hostpart),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().ok()?),
            None => (hostport, 22),
        };
        if host.is_empty() || user.is_empty() {
            return None;
        }
        Some(SftpUrl {
            user,
            host: host.to_string(),
            port,
            path,
        })
    }

    /// `sftp://user@host[:port]` — the connection identity, also the
    /// panel title prefix and the connection-cache key.
    pub fn prefix(&self) -> String {
        if self.port == 22 {
            format!("sftp://{}@{}", self.user, self.host)
        } else {
            format!("sftp://{}@{}:{}", self.user, self.host, self.port)
        }
    }

    pub fn display(&self) -> String {
        format!("{}{}", self.prefix(), self.path.display())
    }
}

fn default_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "root".to_string())
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Streamed by the connect worker. `Ask*` events block the worker until
/// the UI answers on the reply channel.
pub enum ConnectEvent {
    Info(String),
    /// Unknown host: show the fingerprint, ask whether to trust and save.
    AskHostKey {
        fingerprint: String,
    },
    /// A secret to type: password, key passphrase, or a
    /// keyboard-interactive challenge. `echo` mirrors the server's wish
    /// for that prompt (false = mask the input).
    AskPassword {
        prompt: String,
        echo: bool,
    },
    /// Connected; `entries` is the listing of `start`, prefetched so the
    /// panel can switch over without blocking.
    Ok {
        fs: Arc<SftpFs>,
        start: PathBuf,
        entries: Vec<Entry>,
    },
    Err(String),
}

pub enum ConnectReply {
    Accept(bool),
    Password(String),
    Cancel,
}

pub struct ConnectHandle {
    pub events: Receiver<ConnectEvent>,
    pub replies: Sender<ConnectReply>,
    pub url: SftpUrl,
}

pub fn spawn_connect(url: SftpUrl) -> ConnectHandle {
    let (event_tx, event_rx) = mpsc::channel();
    let (reply_tx, reply_rx) = mpsc::channel();
    let worker_url = url.clone();
    thread::spawn(move || {
        let outcome = connect(&worker_url, &event_tx, &reply_rx);
        let _ = event_tx.send(match outcome {
            Ok((fs, start, entries)) => ConnectEvent::Ok { fs, start, entries },
            Err(msg) => ConnectEvent::Err(msg),
        });
    });
    ConnectHandle {
        events: event_rx,
        replies: reply_tx,
        url,
    }
}

/// Reuse an established connection for another `cd sftp://…` (same
/// user@host:port): only the start directory is resolved and listed.
pub fn spawn_reuse(fs: Arc<SftpFs>, url: SftpUrl) -> ConnectHandle {
    let (event_tx, event_rx) = mpsc::channel();
    let (reply_tx, _reply_rx) = mpsc::channel();
    let worker_url = url.clone();
    thread::spawn(move || {
        let start = if worker_url.path.as_os_str().is_empty() {
            fs.realpath(Path::new("."))
                .unwrap_or_else(|_| PathBuf::from("/"))
        } else {
            worker_url.path.clone()
        };
        let _ = event_tx.send(match fs.read_dir(&start) {
            Ok(entries) => ConnectEvent::Ok { fs, start, entries },
            Err(e) => ConnectEvent::Err(format!("{}: {e}", start.display())),
        });
    });
    ConnectHandle {
        events: event_rx,
        replies: reply_tx,
        url,
    }
}

fn connect(
    url: &SftpUrl,
    tx: &Sender<ConnectEvent>,
    rx: &Receiver<ConnectReply>,
) -> Result<(Arc<SftpFs>, PathBuf, Vec<Entry>), String> {
    let info = |msg: String| {
        let _ = tx.send(ConnectEvent::Info(msg));
    };
    info(format!("Connecting to {}:{}…", url.host, url.port));
    let addrs = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|e| format!("{}: {e}", url.host))?;
    let mut tcp = None;
    let mut last_err = format!("{}: no addresses", url.host);
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(s) => {
                tcp = Some(s);
                break;
            }
            Err(e) => last_err = format!("{addr}: {e}"),
        }
    }
    let tcp = tcp.ok_or(last_err)?;

    let mut sess = Session::new().map_err(|e| e.to_string())?;
    sess.set_timeout(IO_TIMEOUT_MS);
    sess.set_tcp_stream(tcp);
    sess.handshake().map_err(|e| format!("handshake: {e}"))?;

    check_host_key(url, &sess, tx, rx)?;

    info(format!("Authenticating as {}…", url.user));
    authenticate(url, &sess, tx, rx)?;

    let sftp = sess.sftp().map_err(|e| format!("sftp: {e}"))?;
    let start = if url.path.as_os_str().is_empty() {
        sftp.realpath(Path::new("."))
            .unwrap_or_else(|_| PathBuf::from("/"))
    } else {
        url.path.clone()
    };
    let fs = Arc::new(SftpFs {
        raw: Mutex::new(Raw {
            _session: sess,
            sftp,
        }),
        prefix: url.prefix(),
    });
    let entries = fs
        .read_dir(&start)
        .map_err(|e| format!("{}: {e}", start.display()))?;
    Ok((fs, start, entries))
}

fn check_host_key(
    url: &SftpUrl,
    sess: &Session,
    tx: &Sender<ConnectEvent>,
    rx: &Receiver<ConnectReply>,
) -> Result<(), String> {
    let mut kh = sess.known_hosts().map_err(|e| e.to_string())?;
    let file = home_dir().map(|h| h.join(".ssh/known_hosts"));
    if let Some(f) = &file {
        let _ = kh.read_file(f, KnownHostFileKind::OpenSSH); // may not exist yet
    }
    let (key, key_type) = sess.host_key().ok_or("server sent no host key")?;
    match kh.check_port(&url.host, url.port, key) {
        CheckResult::Match => Ok(()),
        CheckResult::Mismatch => Err(format!(
            "HOST KEY MISMATCH for {} — possible man-in-the-middle attack. \
             Remove the old key from ~/.ssh/known_hosts if the host really changed.",
            url.host
        )),
        CheckResult::NotFound | CheckResult::Failure => {
            let fingerprint = sess
                .host_key_hash(HashType::Sha256)
                .map(|h| format!("SHA256:{}", base64(h)))
                .unwrap_or_else(|| "(unavailable)".into());
            if tx.send(ConnectEvent::AskHostKey { fingerprint }).is_err() {
                return Err("cancelled".into());
            }
            match rx.recv() {
                Ok(ConnectReply::Accept(true)) => {
                    let name = if url.port == 22 {
                        url.host.clone()
                    } else {
                        format!("[{}]:{}", url.host, url.port)
                    };
                    let _ = kh.add(&name, key, "added by rcmd", key_type.into());
                    if let Some(f) = &file {
                        if let Some(dir) = f.parent() {
                            let _ = std::fs::create_dir_all(dir);
                        }
                        let _ = kh.write_file(f, KnownHostFileKind::OpenSSH);
                    }
                    Ok(())
                }
                _ => Err("host key rejected".into()),
            }
        }
    }
}

fn authenticate(
    url: &SftpUrl,
    sess: &Session,
    tx: &Sender<ConnectEvent>,
    rx: &Receiver<ConnectReply>,
) -> Result<(), String> {
    // The "none" probe behind auth_methods() tells us what the server
    // accepts, so we only try (and only prompt for) methods that can
    // work — OpenSSH order: publickey, keyboard-interactive, password.
    let methods = sess
        .auth_methods(&url.user)
        .map(|m| m.to_string())
        .unwrap_or_default();
    if sess.authenticated() {
        return Ok(()); // the "none" probe itself was accepted
    }
    let has = |m: &str| methods.is_empty() || methods.split(',').any(|x| x.trim() == m);

    let mut last = String::from("authentication failed");
    if has("publickey") {
        // agent first, then default key files — encrypted ones prompt
        // for their passphrase instead of silently falling through
        let _ = sess.userauth_agent(&url.user);
        if sess.authenticated() {
            return Ok(());
        }
        if let Some(home) = home_dir() {
            for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
                let key = home.join(".ssh").join(name);
                if !key.exists() {
                    continue;
                }
                if key_needs_passphrase(&key) {
                    for _ in 0..3 {
                        let prompt = format!("Enter passphrase for ~/.ssh/{name}:");
                        let phrase = ask_secret(tx, rx, prompt, false)?;
                        if phrase.is_empty() {
                            break; // skip this key, try the next method
                        }
                        match sess.userauth_pubkey_file(&url.user, None, &key, Some(&phrase)) {
                            Ok(()) => return Ok(()),
                            Err(e) => last = e.to_string(),
                        }
                        if sess.authenticated() {
                            return Ok(());
                        }
                    }
                } else {
                    let _ = sess.userauth_pubkey_file(&url.user, None, &key, None);
                    if sess.authenticated() {
                        return Ok(());
                    }
                }
            }
        }
    }
    if has("keyboard-interactive") {
        for _ in 0..3 {
            let mut prompter = Prompter {
                tx,
                rx,
                cancelled: false,
            };
            let result = sess.userauth_keyboard_interactive(&url.user, &mut prompter);
            if prompter.cancelled {
                return Err("cancelled".into());
            }
            if sess.authenticated() {
                return Ok(());
            }
            if let Err(e) = result {
                last = e.to_string();
            }
        }
    }
    if has("password") {
        for _ in 0..3 {
            let prompt = format!("{}@{}'s password:", url.user, url.host);
            let password = ask_secret(tx, rx, prompt, false)?;
            match sess.userauth_password(&url.user, &password) {
                Ok(()) => return Ok(()),
                Err(e) => last = e.to_string(),
            }
        }
    }
    if methods.is_empty() {
        Err(format!("authentication failed: {last}"))
    } else {
        Err(format!(
            "authentication failed: {last} (server allows: {methods})"
        ))
    }
}

/// Send one masked (or echoed) prompt to the UI and wait for the answer.
/// A closed channel or an explicit cancel aborts the whole connect.
fn ask_secret(
    tx: &Sender<ConnectEvent>,
    rx: &Receiver<ConnectReply>,
    prompt: String,
    echo: bool,
) -> Result<String, String> {
    if tx.send(ConnectEvent::AskPassword { prompt, echo }).is_err() {
        return Err("cancelled".into());
    }
    match rx.recv() {
        Ok(ConnectReply::Password(p)) => Ok(p),
        _ => Err("cancelled".into()),
    }
}

/// Routes keyboard-interactive challenges through the connect dialogs.
/// Servers may send several prompts per round — each becomes its own
/// dialog, in order.
struct Prompter<'a> {
    tx: &'a Sender<ConnectEvent>,
    rx: &'a Receiver<ConnectReply>,
    cancelled: bool,
}

impl ssh2::KeyboardInteractivePrompt for Prompter<'_> {
    fn prompt<'b>(
        &mut self,
        _username: &str,
        instructions: &str,
        prompts: &[ssh2::Prompt<'b>],
    ) -> Vec<String> {
        let mut out = Vec::with_capacity(prompts.len());
        for p in prompts {
            if self.cancelled {
                out.push(String::new());
                continue;
            }
            let text = if instructions.trim().is_empty() {
                p.text.to_string()
            } else {
                format!("{} — {}", instructions.trim(), p.text)
            };
            match ask_secret(self.tx, self.rx, text, p.echo) {
                Ok(answer) => out.push(answer),
                Err(_) => {
                    self.cancelled = true;
                    out.push(String::new());
                }
            }
        }
        out
    }
}

/// Is this private key file encrypted? PEM keys carry an explicit
/// header; the OpenSSH v1 format names its cipher ("none" = clear)
/// right after the magic inside the base64 blob.
fn key_needs_passphrase(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    if text.contains("Proc-Type: 4,ENCRYPTED") || text.contains("BEGIN ENCRYPTED PRIVATE KEY") {
        return true;
    }
    let Some(body) = text
        .split_once("-----BEGIN OPENSSH PRIVATE KEY-----")
        .and_then(|(_, rest)| rest.split("-----END").next())
    else {
        return false;
    };
    const MAGIC: &[u8] = b"openssh-key-v1\0";
    let blob = base64_decode(body);
    let Some(rest) = blob.strip_prefix(MAGIC) else {
        return false;
    };
    if rest.len() < 4 {
        return false;
    }
    let n = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
    rest.get(4..4 + n).is_some_and(|cipher| cipher != b"none")
}

/// Lenient base64 decoder (whitespace skipped, stops at padding) — just
/// enough to peek inside OpenSSH-format key files.
fn base64_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => break, // '=' padding or junk ends the data
        };
        acc = acc << 6 | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// Unpadded base64, as OpenSSH prints SHA256 fingerprints.
fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = u32::from_be_bytes([
            0,
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ]);
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(T[(n >> 6 & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(T[(n & 63) as usize] as char);
        }
    }
    out
}

struct Raw {
    /// Owns the connection; dropped last, closing the transport.
    _session: Session,
    sftp: ssh2::Sftp,
}

pub struct SftpFs {
    raw: Mutex<Raw>,
    prefix: String,
}

impl SftpFs {
    /// `sftp://user@host[:port]` — panel title prefix / cache key.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    pub fn realpath(&self, path: &Path) -> io::Result<PathBuf> {
        self.lock().sftp.realpath(path).map_err(ioerr)
    }

    fn lock(&self) -> MutexGuard<'_, Raw> {
        self.raw.lock().unwrap_or_else(|p| p.into_inner())
    }
}

const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;

fn entry_from(name: std::ffi::OsString, st: &FileStat, sftp: &ssh2::Sftp, path: &Path) -> Entry {
    let perm = st.perm.unwrap_or(0);
    let mut link_target = None;
    let kind = match perm & S_IFMT {
        S_IFDIR => EntryKind::Dir,
        S_IFLNK => {
            link_target = sftp.readlink(path).ok();
            match sftp.stat(path) {
                Ok(t) if t.perm.unwrap_or(0) & S_IFMT == S_IFDIR => EntryKind::SymlinkDir,
                Ok(_) => EntryKind::SymlinkFile,
                Err(_) => EntryKind::SymlinkBroken,
            }
        }
        _ => EntryKind::File,
    };
    Entry {
        name,
        kind,
        size: st.size.unwrap_or(0),
        mtime: st.mtime.map(|s| UNIX_EPOCH + Duration::from_secs(s)),
        mode: perm & 0o7777,
        link_target,
        extra: rcmd_entry_stat(st),
    }
}

/// What the SFTP protocol exposes: uid/gid/atime; no ctime/links/inode.
fn rcmd_entry_stat(st: &FileStat) -> crate::entry::EntryStat {
    crate::entry::EntryStat {
        uid: st.uid,
        gid: st.gid,
        atime: st.atime.map(|s| UNIX_EPOCH + Duration::from_secs(s)),
        ..Default::default()
    }
}

fn ioerr(e: ssh2::Error) -> io::Error {
    io::Error::other(e)
}

impl FsProvider for SftpFs {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        let raw = self.lock();
        let listed = raw.sftp.readdir(dir).map_err(ioerr)?;
        let mut entries = Vec::with_capacity(listed.len());
        for (path, st) in listed {
            let Some(name) = path.file_name() else {
                continue;
            };
            if name == "." || name == ".." {
                continue;
            }
            entries.push(entry_from(name.to_os_string(), &st, &raw.sftp, &path));
        }
        Ok(entries)
    }

    fn stat(&self, path: &Path) -> io::Result<Entry> {
        let raw = self.lock();
        let st = raw.sftp.lstat(path).map_err(ioerr)?;
        let name = path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_else(|| "/".into());
        Ok(entry_from(name, &st, &raw.sftp, path))
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let file = self.lock().sftp.open(path).map_err(ioerr)?;
        Ok(Box::new(SftpFile { file }))
    }

    fn writer(&self) -> Option<&dyn FsWrite> {
        Some(self)
    }
}

impl FsWrite for SftpFs {
    fn mkdir(&self, dir: &Path) -> io::Result<()> {
        self.lock().sftp.mkdir(dir, 0o755).map_err(ioerr)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.lock().sftp.unlink(path).map_err(ioerr)
    }

    fn remove_dir(&self, dir: &Path) -> io::Result<()> {
        self.lock().sftp.rmdir(dir).map_err(ioerr)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        use ssh2::RenameFlags;
        let raw = self.lock();
        let flags = RenameFlags::ATOMIC | RenameFlags::OVERWRITE | RenameFlags::NATIVE;
        raw.sftp.rename(from, to, Some(flags)).or_else(|_| {
            // servers without POSIX rename refuse to overwrite
            let _ = raw.sftp.unlink(to);
            raw.sftp.rename(from, to, None).map_err(ioerr)
        })
    }

    fn open_write(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        let file = self
            .lock()
            .sftp
            .open_mode(
                path,
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
                0o644,
                OpenType::File,
            )
            .map_err(ioerr)?;
        Ok(Box::new(SftpFile { file }))
    }

    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        self.lock()
            .sftp
            .setstat(path, stat_with(|st| st.perm = Some(mode)))
            .map_err(ioerr)
    }

    fn set_mtime(&self, path: &Path, mtime: SystemTime) -> io::Result<()> {
        let secs = mtime
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // the SFTP ACMODTIME attribute always carries both stamps
        self.lock()
            .sftp
            .setstat(
                path,
                stat_with(|st| {
                    st.mtime = Some(secs);
                    st.atime = Some(secs);
                }),
            )
            .map_err(ioerr)
    }

    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
        self.lock().sftp.symlink(target, link).map_err(ioerr)
    }
}

fn stat_with(f: impl FnOnce(&mut FileStat)) -> FileStat {
    let mut st = FileStat {
        size: None,
        uid: None,
        gid: None,
        perm: None,
        atime: None,
        mtime: None,
    };
    f(&mut st);
    st
}

/// An open remote file. `ssh2` serializes session access internally, so
/// reads/writes here run concurrently with other ops on the same
/// connection without extra ceremony.
struct SftpFile {
    file: ssh2::File,
}

impl Read for SftpFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Write for SftpFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parse_full() {
        let u = SftpUrl::parse("sftp://alice@example.com:2222/srv/data").unwrap();
        assert_eq!(u.user, "alice");
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, 2222);
        assert_eq!(u.path, PathBuf::from("/srv/data"));
        assert_eq!(u.prefix(), "sftp://alice@example.com:2222");
        assert_eq!(u.display(), "sftp://alice@example.com:2222/srv/data");
    }

    #[test]
    fn url_parse_defaults() {
        let u = SftpUrl::parse("sftp://example.com").unwrap();
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, 22);
        assert!(u.path.as_os_str().is_empty());
        assert!(!u.user.is_empty()); // current user
        let u = SftpUrl::parse("sftp://bob@box/").unwrap();
        assert_eq!(u.user, "bob");
        assert_eq!(u.path, PathBuf::from("/"));
        assert_eq!(u.prefix(), "sftp://bob@box");
    }

    #[test]
    fn url_parse_rejects_junk() {
        assert!(SftpUrl::parse("ftp://x").is_none());
        assert!(SftpUrl::parse("sftp://").is_none());
        assert!(SftpUrl::parse("sftp://user@host:notaport/x").is_none());
    }

    #[test]
    fn base64_matches_openssh_style() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg");
        assert_eq!(base64(b"fo"), "Zm8");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_decode_roundtrip() {
        for data in [&b""[..], b"f", b"fo", b"foo", b"openssh-key-v1\0stuff"] {
            assert_eq!(base64_decode(&base64(data)), data);
        }
        // whitespace is skipped, padding stops the data
        assert_eq!(base64_decode("Zm 9\nv"), b"foo");
        assert_eq!(base64_decode("Zm8="), b"fo");
    }

    fn openssh_key_file(dir: &Path, cipher: &str) -> PathBuf {
        let mut blob = b"openssh-key-v1\0".to_vec();
        blob.extend_from_slice(&(cipher.len() as u32).to_be_bytes());
        blob.extend_from_slice(cipher.as_bytes());
        blob.extend_from_slice(b"\0\0\0\x04none"); // kdfname, truncated rest
        let path = dir.join(format!("key-{cipher}"));
        std::fs::write(
            &path,
            format!(
                "-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n-----END OPENSSH PRIVATE KEY-----\n",
                base64(&blob)
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn passphrase_detection() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!key_needs_passphrase(&openssh_key_file(dir.path(), "none")));
        assert!(key_needs_passphrase(&openssh_key_file(
            dir.path(),
            "aes256-ctr"
        )));
        let pem = dir.path().join("pem");
        std::fs::write(
            &pem,
            "-----BEGIN EC PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,ABCD\n\nZm9v\n-----END EC PRIVATE KEY-----\n",
        )
        .unwrap();
        assert!(key_needs_passphrase(&pem));
        let clear = dir.path().join("clear");
        std::fs::write(
            &clear,
            "-----BEGIN EC PRIVATE KEY-----\nZm9v\n-----END EC PRIVATE KEY-----\n",
        )
        .unwrap();
        assert!(!key_needs_passphrase(&clear));
        assert!(!key_needs_passphrase(&dir.path().join("missing")));
    }
}
