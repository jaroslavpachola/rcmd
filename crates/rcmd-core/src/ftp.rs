//! FTP, as much of it as a file manager needs: log in, list, fetch,
//! store, and the handful of commands that move things around.
//!
//! The protocol's shape drives the design here. A control connection
//! carries commands and replies; every transfer opens a *second*
//! connection for the data, and the control connection is unusable
//! until that transfer finishes. So a provider holds a small **pool**
//! of logged-in control connections: a transfer takes one for its whole
//! life and gives it back when the reader or writer is dropped, which
//! is what lets a panel list a directory while a copy is still running.
//!
//! Listings prefer `MLSD`, which is machine-readable and says what
//! everything is. Servers too old for it get the `LIST` path, whose
//! output is `ls -l` by convention and by nothing else.

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::entry::{Entry, EntryKind};
use crate::remote::{ConnectEvent, ConnectHandle, ConnectReply};
use crate::vfs::{FsProvider, FsWrite, RemoteFs};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// A reply line longer than this is a server misbehaving, not a reply.
const MAX_LINE: u64 = 64 * 1024;

/// `ftp://[user[:password]@]host[:port][/path]`. No user means the
/// anonymous login every public server still answers to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FtpUrl {
    pub user: String,
    pub password: Option<String>,
    pub host: String,
    pub port: u16,
    pub path: PathBuf,
}

impl FtpUrl {
    pub fn parse(s: &str) -> Option<FtpUrl> {
        let rest = s.strip_prefix("ftp://")?;
        let (hostpart, path) = match rest.find('/') {
            Some(i) => (&rest[..i], PathBuf::from(&rest[i..])),
            None => (rest, PathBuf::new()),
        };
        let (userinfo, hostport) = match hostpart.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, hostpart),
        };
        let (user, password) = match userinfo {
            Some(info) => match info.split_once(':') {
                Some((u, p)) => (u.to_string(), Some(p.to_string())),
                None => (info.to_string(), None),
            },
            None => ("anonymous".to_string(), None),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().ok()?),
            None => (hostport, 21),
        };
        if host.is_empty() || user.is_empty() {
            return None;
        }
        Some(FtpUrl {
            user,
            password,
            host: host.to_string(),
            port,
            path,
        })
    }

    /// `ftp://user@host[:port]` - the connection identity, the panel
    /// title prefix and the cache key. The password is never in it.
    pub fn prefix(&self) -> String {
        if self.port == 21 {
            format!("ftp://{}@{}", self.user, self.host)
        } else {
            format!("ftp://{}@{}:{}", self.user, self.host, self.port)
        }
    }

    pub fn display(&self) -> String {
        format!("{}{}", self.prefix(), self.path.display())
    }
}

/// Dial a server on a worker thread. A password the URL did not carry
/// is asked for, once; FTP has no host key to check, so that step of
/// the SFTP flow simply does not arise here.
pub fn spawn_connect(url: FtpUrl) -> ConnectHandle {
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
    url: &FtpUrl,
    tx: &std::sync::mpsc::Sender<ConnectEvent>,
    rx: &std::sync::mpsc::Receiver<ConnectReply>,
) -> Result<Dialed, String> {
    let _ = tx.send(ConnectEvent::Info(format!("connecting to {}", url.host)));
    // anonymous FTP wants an e-mail address as the password and does
    // not check it; anything else has to be asked for
    let password = match &url.password {
        Some(password) => password.clone(),
        None if url.user == "anonymous" || url.user == "ftp" => "rcmd@".to_string(),
        None => {
            let _ = tx.send(ConnectEvent::AskPassword {
                prompt: format!("{}@{}'s password: ", url.user, url.host),
                echo: false,
            });
            match rx.recv() {
                Ok(ConnectReply::Password(password)) => password,
                _ => return Err("cancelled".into()),
            }
        }
    };

    let fs = FtpFs::connect(url, &password).map_err(|err| err.to_string())?;
    let start = if url.path.as_os_str().is_empty() {
        fs.home.clone()
    } else {
        url.path.clone()
    };
    let entries = fs
        .read_dir(&start)
        .map_err(|err| format!("{}: {err}", start.display()))?;
    Ok((Arc::new(fs), start, entries))
}

/// One logged-in control connection.
struct Control {
    stream: BufReader<TcpStream>,
}

/// A reply: the three-digit code and the text after it.
struct Reply {
    code: u16,
    text: String,
}

impl Control {
    fn connect(url: &FtpUrl, password: &str) -> io::Result<Control> {
        let address = resolve(&url.host, url.port)?;
        let stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        let mut control = Control {
            stream: BufReader::new(stream),
        };
        control.read_reply()?.expect_ok(&[220])?;

        let reply = control.command(&format!("USER {}", url.user))?;
        // 230 = logged in already; 331/332 = the server wants more
        if reply.code != 230 {
            reply.expect_ok(&[331, 332])?;
            control
                .command(&format!("PASS {password}"))?
                .expect_ok(&[230, 202])?;
        }
        // binary, always: a file manager never wants line endings
        // rewritten under it
        control.command("TYPE I")?.expect_ok(&[200])?;
        Ok(control)
    }

    fn command(&mut self, line: &str) -> io::Result<Reply> {
        self.stream.get_mut().write_all(line.as_bytes())?;
        self.stream.get_mut().write_all(b"\r\n")?;
        self.stream.get_mut().flush()?;
        self.read_reply()
    }

    /// A reply is one line, or several when the first ends in `-` and
    /// the last repeats the code followed by a space.
    fn read_reply(&mut self) -> io::Result<Reply> {
        let first = self.read_line()?;
        let code: u16 = first
            .get(..3)
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad FTP reply"))?;
        let mut text = first[3..].trim().to_string();
        if first.as_bytes().get(3) != Some(&b'-') {
            return Ok(Reply { code, text });
        }
        let terminator = format!("{code} ");
        loop {
            let line = self.read_line()?;
            if line.starts_with(&terminator) {
                text = line[4..].trim().to_string();
                return Ok(Reply { code, text });
            }
        }
    }

    fn read_line(&mut self) -> io::Result<String> {
        let mut line = String::new();
        let read = (&mut self.stream)
            .take(MAX_LINE)
            .read_line(&mut line)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "FTP reply is not text"))?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the server closed the connection",
            ));
        }
        Ok(line)
    }

    /// Open a data connection for the next transfer. `EPSV` first: it
    /// works over IPv6 and behind NAT, where `PASV`'s address in the
    /// reply is often the server's own idea of where it lives.
    fn data(&mut self, peer: SocketAddr) -> io::Result<TcpStream> {
        let port = match self.command("EPSV") {
            Ok(reply) if reply.code == 229 => parse_epsv(&reply.text),
            _ => None,
        };
        let address = match port {
            Some(port) => SocketAddr::new(peer.ip(), port),
            None => {
                let reply = self.command("PASV")?;
                reply.expect_ok(&[227])?;
                let (ip, port) = parse_pasv(&reply.text)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad PASV reply"))?;
                // trust the port, not the address: a server behind NAT
                // reports the address it thinks it has
                SocketAddr::new(if ip.is_loopback() { ip } else { peer.ip() }, port)
            }
        };
        let stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        Ok(stream)
    }
}

impl Reply {
    fn expect_ok(&self, codes: &[u16]) -> io::Result<()> {
        if codes.contains(&self.code) {
            return Ok(());
        }
        Err(io::Error::other(format!("{} {}", self.code, self.text)))
    }
}

/// "229 Entering Extended Passive Mode (|||51234|)".
fn parse_epsv(text: &str) -> Option<u16> {
    let open = text.find('(')?;
    let close = text[open..].find(')')? + open;
    let inner = &text[open + 1..close];
    let delimiter = inner.chars().next()?;
    inner
        .trim_matches(delimiter)
        .split(delimiter)
        .next_back()?
        .parse()
        .ok()
}

/// "227 Entering Passive Mode (127,0,0,1,200,42)".
fn parse_pasv(text: &str) -> Option<(std::net::IpAddr, u16)> {
    let open = text.find('(')?;
    let close = text[open..].find(')')? + open;
    let numbers: Vec<u16> = text[open + 1..close]
        .split(',')
        .map(|n| n.trim().parse().ok())
        .collect::<Option<Vec<u16>>>()?;
    if numbers.len() != 6 || numbers.iter().any(|n| *n > 255) {
        return None;
    }
    let ip = std::net::IpAddr::from([
        numbers[0] as u8,
        numbers[1] as u8,
        numbers[2] as u8,
        numbers[3] as u8,
    ]);
    Some((ip, numbers[4] << 8 | numbers[5]))
}

fn resolve(host: &str, port: u16) -> io::Result<SocketAddr> {
    (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no address for {host}")))
}

pub struct FtpFs {
    url: FtpUrl,
    password: String,
    prefix: String,
    /// Logged-in control connections not currently carrying a transfer.
    pool: Mutex<Vec<Control>>,
    /// Where a finished transfer hands its connection back. A channel
    /// rather than a reference to the pool, because a transfer outlives
    /// the call that made it and cannot borrow the provider.
    returns: std::sync::mpsc::Sender<Control>,
    returned: Mutex<std::sync::mpsc::Receiver<Control>>,
    /// Where the server put us at login, so `.` means something.
    home: PathBuf,
    /// Whether the server answered MLSD the first time it was asked.
    mlsd: Mutex<Option<bool>>,
    /// How many times this provider has logged in. One is the happy
    /// number: it means every transfer handed its connection back and
    /// the next one reused it.
    logins: std::sync::atomic::AtomicUsize,
}

impl FtpFs {
    /// Log in and note where the server dropped us.
    pub fn connect(url: &FtpUrl, password: &str) -> io::Result<FtpFs> {
        let mut control = Control::connect(url, password)?;
        let home = match control.command("PWD") {
            Ok(reply) if reply.code == 257 => parse_pwd(&reply.text),
            _ => None,
        }
        .unwrap_or_else(|| PathBuf::from("/"));
        let (returns, returned) = std::sync::mpsc::channel();
        Ok(FtpFs {
            prefix: url.prefix(),
            url: url.clone(),
            password: password.to_string(),
            pool: Mutex::new(vec![control]),
            returns,
            returned: Mutex::new(returned),
            home,
            mlsd: Mutex::new(None),
            logins: std::sync::atomic::AtomicUsize::new(1),
        })
    }

    /// Take a logged-in connection, opening a new one if the pool is
    /// empty - which happens exactly when another transfer holds them.
    fn take(&self) -> io::Result<Control> {
        let mut pool = self.pool.lock().unwrap_or_else(|p| p.into_inner());
        // collect whatever finished transfers have handed back
        let returned = self.returned.lock().unwrap_or_else(|p| p.into_inner());
        while let Ok(control) = returned.try_recv() {
            pool.push(control);
        }
        drop(returned);
        if let Some(control) = pool.pop() {
            return Ok(control);
        }
        drop(pool);
        self.logins
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Control::connect(&self.url, &self.password)
    }

    /// How many control connections this provider has had to open.
    pub fn logins(&self) -> usize {
        self.logins.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn give_back(&self, control: Control) {
        self.pool
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(control);
    }

    /// Run something with a connection, returning it afterwards. A
    /// connection that failed is dropped rather than pooled: the next
    /// call logs in again instead of inheriting a broken session.
    fn with<T>(&self, f: impl FnOnce(&mut Control) -> io::Result<T>) -> io::Result<T> {
        let mut control = self.take()?;
        match f(&mut control) {
            Ok(value) => {
                self.give_back(control);
                Ok(value)
            }
            Err(err) => Err(err),
        }
    }

    fn absolute(&self, path: &Path) -> PathBuf {
        if path.as_os_str().is_empty() || path == Path::new(".") {
            self.home.clone()
        } else if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.home.join(path)
        }
    }

    /// A whole data transfer: send the command, drain the connection,
    /// then read the completion reply the server owes us.
    fn download(&self, control: &mut Control, command: &str) -> io::Result<Vec<u8>> {
        let peer = control.stream.get_ref().peer_addr()?;
        let mut data = control.data(peer)?;
        control.command(command)?.expect_ok(&[125, 150])?;
        let mut buf = Vec::new();
        data.read_to_end(&mut buf)?;
        drop(data);
        control.read_reply()?.expect_ok(&[226, 250])?;
        Ok(buf)
    }

    fn list_raw(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        let dir = self.absolute(dir);
        let quoted = dir.to_string_lossy().into_owned();
        self.with(|control| {
            let prefer_mlsd = *self.mlsd.lock().unwrap_or_else(|p| p.into_inner()) != Some(false);
            if prefer_mlsd {
                match self.download(control, &format!("MLSD {quoted}")) {
                    Ok(bytes) => {
                        *self.mlsd.lock().unwrap_or_else(|p| p.into_inner()) = Some(true);
                        return Ok(parse_mlsd(&String::from_utf8_lossy(&bytes)));
                    }
                    Err(_) => {
                        // note it and fall through, so the next listing
                        // does not pay for the refusal again
                        *self.mlsd.lock().unwrap_or_else(|p| p.into_inner()) = Some(false);
                    }
                }
            }
            let bytes = self.download(control, &format!("LIST {quoted}"))?;
            Ok(parse_list(&String::from_utf8_lossy(&bytes)))
        })
    }
}

impl FsProvider for FtpFs {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        self.list_raw(dir)
    }

    fn stat(&self, path: &Path) -> io::Result<Entry> {
        let path = self.absolute(path);
        // the root has no parent to list it out of, and no name in one
        // - but it is a directory, and callers ask whether it is
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
        self.list_raw(parent)?
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file on the server"))
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let path = self.absolute(path);
        let mut control = self.take()?;
        let peer = control.stream.get_ref().peer_addr()?;
        let data = control.data(peer)?;
        control
            .command(&format!("RETR {}", path.to_string_lossy()))?
            .expect_ok(&[125, 150])?;
        Ok(Box::new(Transfer {
            data: Some(data),
            control: Some(control),
            returns: Some(self.returns.clone()),
        }))
    }

    fn writer(&self) -> Option<&dyn FsWrite> {
        Some(self)
    }
}

impl RemoteFs for FtpFs {
    fn prefix(&self) -> &str {
        &self.prefix
    }

    fn realpath(&self, path: &Path) -> io::Result<PathBuf> {
        Ok(self.absolute(path))
    }
}

/// A transfer in flight. The data connection is read (or written) to
/// completion, then closed, and only then does the server send the
/// reply that ends the transfer - so that happens on drop, and the
/// control connection is not usable before it.
struct Transfer {
    data: Option<TcpStream>,
    control: Option<Control>,
    /// Where to hand the connection back when the transfer ends. A
    /// connection whose transfer went wrong is dropped instead.
    returns: Option<std::sync::mpsc::Sender<Control>>,
}

impl Read for Transfer {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.data.as_mut() {
            Some(data) => data.read(buf),
            None => Ok(0),
        }
    }
}

impl Write for Transfer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.data.as_mut() {
            Some(data) => data.write(buf),
            None => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the transfer is over",
            )),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.data.as_mut() {
            Some(data) => data.flush(),
            None => Ok(()),
        }
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        // closing the data connection is what tells the server the
        // transfer is done; the reply comes after it
        self.data.take();
        let Some(mut control) = self.control.take() else {
            return;
        };
        let completed = control
            .read_reply()
            .map(|reply| reply.expect_ok(&[226, 250]).is_ok())
            .unwrap_or(false);
        if let Some(returns) = self.returns.take()
            && completed
        {
            let _ = returns.send(control);
        }
    }
}

impl FsWrite for FtpFs {
    fn mkdir(&self, dir: &Path) -> io::Result<()> {
        let dir = self.absolute(dir);
        self.with(|control| {
            control
                .command(&format!("MKD {}", dir.to_string_lossy()))?
                .expect_ok(&[257, 250])
        })
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        let path = self.absolute(path);
        self.with(|control| {
            control
                .command(&format!("DELE {}", path.to_string_lossy()))?
                .expect_ok(&[250, 200])
        })
    }

    fn remove_dir(&self, dir: &Path) -> io::Result<()> {
        let dir = self.absolute(dir);
        self.with(|control| {
            control
                .command(&format!("RMD {}", dir.to_string_lossy()))?
                .expect_ok(&[250, 200])
        })
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let (from, to) = (self.absolute(from), self.absolute(to));
        self.with(|control| {
            control
                .command(&format!("RNFR {}", from.to_string_lossy()))?
                .expect_ok(&[350])?;
            control
                .command(&format!("RNTO {}", to.to_string_lossy()))?
                .expect_ok(&[250])
        })
    }

    fn open_write(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        let path = self.absolute(path);
        let mut control = self.take()?;
        let peer = control.stream.get_ref().peer_addr()?;
        let data = control.data(peer)?;
        control
            .command(&format!("STOR {}", path.to_string_lossy()))?
            .expect_ok(&[125, 150])?;
        Ok(Box::new(Transfer {
            data: Some(data),
            control: Some(control),
            returns: Some(self.returns.clone()),
        }))
    }

    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        let path = self.absolute(path);
        // SITE CHMOD is a convention, not part of the protocol; a
        // server without it says so and the caller sees why
        self.with(|control| {
            control
                .command(&format!("SITE CHMOD {mode:03o} {}", path.to_string_lossy()))?
                .expect_ok(&[200, 250])
        })
    }

    fn set_owner(&self, _path: &Path, _uid: Option<u32>, _gid: Option<u32>) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "FTP has no way to change ownership",
        ))
    }

    fn set_mtime(&self, path: &Path, mtime: SystemTime) -> io::Result<()> {
        let path = self.absolute(path);
        let stamp = format_mfmt(mtime)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "timestamp out of range"))?;
        self.with(|control| {
            control
                .command(&format!("MFMT {stamp} {}", path.to_string_lossy()))?
                .expect_ok(&[213])
        })
    }

    fn symlink(&self, _target: &Path, _link: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "FTP has no way to create a symlink",
        ))
    }
}

/// "257 \"/pub/dir\" is the current directory" - the quotes are the
/// only reliable delimiters, and a doubled quote is a literal one.
fn parse_pwd(text: &str) -> Option<PathBuf> {
    let rest = text.strip_prefix('"')?;
    let mut path = String::new();
    let mut chars = rest.chars();
    let mut closed = false;
    while let Some(c) = chars.next() {
        if c != '"' {
            path.push(c);
            continue;
        }
        // a doubled quote is one literal quote; a lone one ends the path
        match chars.clone().next() {
            Some('"') => {
                chars.next();
                path.push('"');
            }
            _ => {
                closed = true;
                break;
            }
        }
    }
    closed.then(|| PathBuf::from(path))
}

/// MLSD: `fact=value;fact=value; name`, one per line, and the facts say
/// what everything is instead of leaving it to be guessed.
fn parse_mlsd(text: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        let Some((facts, name)) = line.split_once(' ') else {
            continue;
        };
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        let mut kind = EntryKind::File;
        let mut size = 0u64;
        let mut mtime = None;
        let mut mode = 0o644;
        for fact in facts.split(';') {
            let Some((key, value)) = fact.split_once('=') else {
                continue;
            };
            match key.to_ascii_lowercase().as_str() {
                "type" => match value.to_ascii_lowercase().as_str() {
                    "dir" => kind = EntryKind::Dir,
                    "cdir" | "pdir" => kind = EntryKind::Dir,
                    v if v.starts_with("os.unix=slink") => kind = EntryKind::SymlinkFile,
                    _ => {}
                },
                "size" | "sizd" => size = value.parse().unwrap_or(0),
                "modify" => mtime = parse_mdtm(value),
                "unix.mode" => mode = u32::from_str_radix(value, 8).unwrap_or(0o644),
                _ => {}
            }
        }
        if kind == EntryKind::Dir && mode == 0o644 {
            mode = 0o755;
        }
        out.push(Entry {
            name: OsString::from(name),
            kind,
            size,
            mtime,
            mode,
            link_target: None,
            extra: Default::default(),
        });
    }
    out
}

/// `LIST` output, which by long convention looks like `ls -l` and by
/// specification looks like nothing at all.
fn parse_list(text: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() || line.starts_with("total ") {
            continue;
        }
        // permissions, links, owner, group, size, three date columns,
        // then the name - which may hold spaces, so it is taken by
        // offset rather than by splitting
        let columns: Vec<&str> = line.split_whitespace().collect();
        if columns.len() < 9 || columns[0].len() < 10 {
            continue;
        }
        let permissions = columns[0];
        let size: u64 = columns[4].parse().unwrap_or(0);
        // the name starts after the eighth column, and may hold spaces
        let name = match nth_field_start(line, 8) {
            Some(at) => line[at..].to_string(),
            None => continue,
        };
        if name == "." || name == ".." {
            continue;
        }
        let (name, link_target) = match (permissions.starts_with('l'), name.split_once(" -> ")) {
            (true, Some((name, target))) => (name.to_string(), Some(PathBuf::from(target))),
            _ => (name, None),
        };
        let kind = match permissions.as_bytes()[0] {
            b'd' => EntryKind::Dir,
            b'l' => EntryKind::SymlinkFile,
            _ => EntryKind::File,
        };
        out.push(Entry {
            name: OsString::from(name),
            kind,
            size,
            mtime: None,
            mode: mode_from_permissions(permissions),
            link_target,
            extra: Default::default(),
        });
    }
    out
}

/// Byte offset where the nth whitespace-separated field begins.
fn nth_field_start(line: &str, n: usize) -> Option<usize> {
    let mut index = 0;
    let mut at = 0;
    let bytes = line.as_bytes();
    while at < bytes.len() {
        while at < bytes.len() && bytes[at] == b' ' {
            at += 1;
        }
        if index == n {
            return (at < bytes.len()).then_some(at);
        }
        while at < bytes.len() && bytes[at] != b' ' {
            at += 1;
        }
        index += 1;
    }
    None
}

fn mode_from_permissions(s: &str) -> u32 {
    let mut mode = 0u32;
    for (i, c) in s.chars().skip(1).take(9).enumerate() {
        if c != '-' {
            mode |= 1 << (8 - i);
        }
    }
    mode
}

/// `YYYYMMDDHHMMSS`, which is what MDTM and MLSD's `modify` both use.
fn parse_mdtm(text: &str) -> Option<SystemTime> {
    let text = text.split('.').next()?;
    if text.len() < 14 {
        return None;
    }
    let num = |from: usize, len: usize| text.get(from..from + len)?.parse::<i64>().ok();
    let (year, month, day) = (num(0, 4)?, num(4, 2)? as u32, num(6, 2)? as u32);
    let (hour, minute, second) = (num(8, 2)? as u64, num(10, 2)? as u64, num(12, 2)? as u64);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day)?;
    Some(UNIX_EPOCH + Duration::from_secs(days * 86_400 + hour * 3_600 + minute * 60 + second))
}

fn format_mfmt(time: SystemTime) -> Option<String> {
    let secs = time.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let (days, rest) = (secs / 86_400, secs % 86_400);
    let (year, month, day) = civil_from_days(days as i64);
    Some(format!(
        "{year:04}{month:02}{day:02}{:02}{:02}{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    ))
}

/// Howard Hinnant's days-from-civil, as used elsewhere in the tree.
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<u64> {
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = u64::from((month + 9) % 12);
    let doy = (153 * mp + 2) / 5 + u64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe as i64 - 719_468;
    (days >= 0).then_some(days as u64)
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (y + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls_the_way_people_write_them() {
        let url = FtpUrl::parse("ftp://ftp.example.org/pub/linux").unwrap();
        assert_eq!(url.user, "anonymous"); // no user named: the old default
        assert_eq!(url.password, None);
        assert_eq!(url.host, "ftp.example.org");
        assert_eq!(url.port, 21);
        assert_eq!(url.path, PathBuf::from("/pub/linux"));
        assert_eq!(url.prefix(), "ftp://anonymous@ftp.example.org");

        let url = FtpUrl::parse("ftp://jarda:hunter2@example.org:2121").unwrap();
        assert_eq!(url.user, "jarda");
        assert_eq!(url.password.as_deref(), Some("hunter2"));
        assert_eq!(url.port, 2121);
        assert_eq!(url.path, PathBuf::new());
        // the password is never part of the identity, or of the title
        assert_eq!(url.prefix(), "ftp://jarda@example.org:2121");
        assert!(!url.display().contains("hunter2"));

        assert!(FtpUrl::parse("sftp://example.org").is_none());
        assert!(FtpUrl::parse("ftp:///path").is_none());
        assert!(FtpUrl::parse("ftp://host:notaport").is_none());
    }

    #[test]
    fn reads_the_two_passive_mode_replies() {
        assert_eq!(
            parse_epsv("Entering Extended Passive Mode (|||51234|)"),
            Some(51234)
        );
        assert_eq!(parse_epsv("no parentheses here"), None);
        let (ip, port) = parse_pasv("Entering Passive Mode (127,0,0,1,200,42)").unwrap();
        assert_eq!(ip.to_string(), "127.0.0.1");
        assert_eq!(port, 200 * 256 + 42);
        assert!(parse_pasv("Entering Passive Mode (1,2,3)").is_none());
        assert!(parse_pasv("Entering Passive Mode (1,2,3,4,5,999)").is_none());
    }

    #[test]
    fn reads_the_current_directory_out_of_its_quotes() {
        assert_eq!(
            parse_pwd("\"/pub/dir\" is the current directory"),
            Some(PathBuf::from("/pub/dir"))
        );
        // a doubled quote inside the path is one literal quote
        assert_eq!(
            parse_pwd("\"/od\"\"d\" is current"),
            Some(PathBuf::from("/od\"d"))
        );
        assert_eq!(parse_pwd("no quotes"), None);
    }

    #[test]
    fn mlsd_says_what_everything_is() {
        let text = "type=cdir;sizd=4096; .\r\n\
                    type=pdir;sizd=4096; ..\r\n\
                    type=dir;sizd=4096;modify=20260823100000;UNIX.mode=0750; docs\r\n\
                    type=file;size=1234;modify=20231114221320;UNIX.mode=0644; readme.txt\r\n\
                    type=OS.unix=slink:/etc/passwd;size=0; point\r\n";
        let entries = parse_mlsd(text);
        let names: Vec<_> = entries
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        // "." and ".." are the server describing itself, not contents
        assert_eq!(names, ["docs", "readme.txt", "point"]);
        assert_eq!(entries[0].kind, EntryKind::Dir);
        assert_eq!(entries[0].mode, 0o750);
        assert_eq!(entries[1].size, 1234);
        assert_eq!(
            entries[1].mtime,
            Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
        );
        assert_eq!(entries[2].kind, EntryKind::SymlinkFile);
    }

    #[test]
    fn list_output_is_read_as_the_ls_it_imitates() {
        let text = "total 12\r\n\
                    drwxr-xr-x 2 ftp ftp 4096 Aug 23 10:00 docs\r\n\
                    -rw-r--r-- 1 ftp ftp 1234 Aug 23 10:00 readme.txt\r\n\
                    -rw-r--r-- 1 ftp ftp   42 Aug 23 10:00 two words.txt\r\n\
                    lrwxrwxrwx 1 ftp ftp    7 Aug 23 10:00 point -> readme.txt\r\n\
                    drwxr-xr-x 2 ftp ftp 4096 Aug 23 10:00 .\r\n";
        let entries = parse_list(text);
        let names: Vec<_> = entries
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        // the "total" header and "." are not files
        assert_eq!(names, ["docs", "readme.txt", "two words.txt", "point"]);
        assert_eq!(entries[0].kind, EntryKind::Dir);
        assert_eq!(entries[0].mode, 0o755);
        assert_eq!(entries[1].size, 1234);
        assert_eq!(entries[1].mode, 0o644);
        // a name with a space in it survives, which splitting would not
        assert_eq!(entries[2].size, 42);
        assert_eq!(entries[3].kind, EntryKind::SymlinkFile);
        assert_eq!(entries[3].link_target, Some(PathBuf::from("readme.txt")));
    }

    /// Start the suite's own FTP server on a free port; `None` when
    /// python is not around to run it.
    fn server(root: &Path) -> Option<(std::process::Child, u16)> {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()?
            .parent()?
            .join("tests/e2e/ftp_server.py");
        if !script.exists() {
            eprintln!("skipping: no ftp_server.py");
            return None;
        }
        let mut child = std::process::Command::new("python3")
            .arg(&script)
            .arg(root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let stdout = child.stdout.take()?;
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).ok()?;
        let port = line.split_whitespace().nth(1)?.parse().ok()?;
        Some((child, port))
    }

    fn connect(port: u16, path: &str) -> FtpFs {
        let url = FtpUrl::parse(&format!("ftp://tester@127.0.0.1:{port}{path}")).unwrap();
        FtpFs::connect(&url, "secret").expect("login")
    }

    #[test]
    fn talks_to_a_real_server() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("docs")).unwrap();
        std::fs::write(tmp.path().join("readme.txt"), b"on the server\n").unwrap();
        std::fs::write(tmp.path().join("docs/deep.txt"), b"further in\n").unwrap();
        let Some((mut child, port)) = server(tmp.path()) else {
            return;
        };
        let fs = connect(port, "/");

        let mut names: Vec<_> = fs
            .read_dir(Path::new("/"))
            .unwrap()
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["docs", "readme.txt"]);
        assert!(fs.stat(Path::new("/docs")).unwrap().is_dir());
        assert_eq!(fs.stat(Path::new("/readme.txt")).unwrap().size, 14);

        // the root has no parent to be listed out of, and callers do
        // ask whether it is a directory before writing into it
        assert!(fs.stat(Path::new("/")).unwrap().is_dir());

        let mut text = String::new();
        fs.open_read(Path::new("/docs/deep.txt"))
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "further in\n");

        // a second read must reuse the pooled connection rather than
        // log in again, which is the whole reason the pool exists
        text.clear();
        fs.open_read(Path::new("/readme.txt"))
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "on the server\n");
        // one login for the whole session: each transfer handed its
        // connection back and the next one picked it up
        assert_eq!(fs.logins(), 1);

        let _ = child.kill();
    }

    #[test]
    fn writes_to_a_real_server() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("old.txt"), b"before\n").unwrap();
        let Some((mut child, port)) = server(tmp.path()) else {
            return;
        };
        let fs = connect(port, "/");
        let writer = fs.writer().unwrap();

        writer.mkdir(Path::new("/made")).unwrap();
        assert!(tmp.path().join("made").is_dir());

        {
            let mut out = writer.open_write(Path::new("/made/new.txt")).unwrap();
            out.write_all(b"uploaded\n").unwrap();
        }
        // the transfer ends when the writer is dropped, so wait for the
        // bytes rather than assuming they beat us here
        for _ in 0..50 {
            if tmp.path().join("made/new.txt").exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("made/new.txt")).unwrap(),
            "uploaded\n"
        );

        writer
            .rename(Path::new("/old.txt"), Path::new("/renamed.txt"))
            .unwrap();
        assert!(tmp.path().join("renamed.txt").exists());
        assert!(!tmp.path().join("old.txt").exists());

        writer.set_mode(Path::new("/renamed.txt"), 0o600).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(tmp.path().join("renamed.txt"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let stamp = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        writer.set_mtime(Path::new("/renamed.txt"), stamp).unwrap();
        assert_eq!(
            fs.stat(Path::new("/renamed.txt")).unwrap().mtime,
            Some(stamp)
        );

        writer.remove_file(Path::new("/renamed.txt")).unwrap();
        assert!(!tmp.path().join("renamed.txt").exists());
        writer.remove_file(Path::new("/made/new.txt")).unwrap();
        writer.remove_dir(Path::new("/made")).unwrap();
        assert!(!tmp.path().join("made").exists());

        // things FTP simply cannot do say so rather than pretending
        assert!(writer.symlink(Path::new("a"), Path::new("b")).is_err());
        assert!(writer.set_owner(Path::new("/x"), Some(0), None).is_err());

        let _ = child.kill();
    }

    #[test]
    fn falls_back_to_list_when_the_server_refuses_mlsd() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("readme.txt"), b"listed\n").unwrap();
        let Some((mut child, port)) = server(tmp.path()) else {
            return;
        };
        let fs = connect(port, "/");
        // pretend MLSD was already refused once
        *fs.mlsd.lock().unwrap() = Some(false);
        let entries = fs.read_dir(Path::new("/")).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name.to_string_lossy(), "readme.txt");
        assert_eq!(entries[0].size, 7);
        let _ = child.kill();
    }

    #[test]
    fn a_wrong_password_is_refused_and_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let Some((mut child, port)) = server(tmp.path()) else {
            return;
        };
        let url = FtpUrl::parse(&format!("ftp://tester@127.0.0.1:{port}/")).unwrap();
        let err = match FtpFs::connect(&url, "wrong") {
            Ok(_) => panic!("the wrong password logged in"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("530"), "{err}");
        let _ = child.kill();
    }

    #[test]
    fn timestamps_round_trip_through_the_wire_format() {
        let stamp = parse_mdtm("20231114221320").unwrap();
        assert_eq!(stamp, UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        assert_eq!(format_mfmt(stamp).as_deref(), Some("20231114221320"));
        // fractional seconds are allowed and ignored
        assert_eq!(parse_mdtm("20231114221320.500"), Some(stamp));
        assert!(parse_mdtm("2023").is_none());
        assert!(parse_mdtm("20231314221320").is_none());
    }
}
