//! SFTP remote filesystem: an [`FsProvider`]+[`FsWrite`] over a blocking
//! `ssh2` session (decision D1: worker threads, no async runtime).
//!
//! Connecting is interactive (host-key confirmation, password prompts),
//! so it runs on a worker thread speaking [`ConnectEvent`]s to the UI -
//! the same ask/reply shape as the job engine. All session use is
//! serialized behind one mutex; SFTP round-trips dominate, the lock is
//! noise. One connection is shared by both panels and any jobs on it.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ssh2::{CheckResult, FileStat, HashType, KnownHostFileKind, OpenFlags, OpenType, Session};

use crate::entry::{Entry, EntryKind};
use crate::remote::{ConnectEvent, ConnectHandle, ConnectReply};
use crate::vfs::{FsProvider, FsWrite, RemoteFs};

/// Blocking-call timeout on the session; a dead link surfaces as an
/// error dialog instead of a hung worker.
const IO_TIMEOUT_MS: u32 = 30_000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// `sftp://[user@]host[:port][/path]`. An empty path means "the remote
/// home directory", resolved once connected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SftpUrl {
    /// "sftp" or "fish" - the same transport, a different thing done
    /// with it once the session is up.
    pub scheme: String,
    pub user: String,
    /// As typed: a name, an address, or an alias `~/.ssh/config` knows.
    pub host: String,
    pub port: u16,
    pub path: PathBuf,
    /// The address to dial, when `host` is an alias with a `HostName`.
    pub hostname: Option<String>,
    /// `IdentityFile`s for this host, tried before the default keys.
    pub identities: Vec<PathBuf>,
    /// The way there when it is not a straight connection.
    pub proxy: Option<Proxy>,
    user_given: bool,
    port_given: bool,
}

impl SftpUrl {
    pub fn parse(s: &str) -> Option<SftpUrl> {
        SftpUrl::parse_as("sftp", s)
    }

    /// Parse under a given scheme. `fish://` reaches the same servers
    /// over the same SSH transport, so it reaches the same type too.
    pub fn parse_as(scheme: &str, s: &str) -> Option<SftpUrl> {
        let rest = s.strip_prefix(&format!("{scheme}://"))?;
        let (hostpart, path) = match rest.find('/') {
            Some(i) => (&rest[..i], PathBuf::from(&rest[i..])),
            None => (rest, PathBuf::new()),
        };
        let (user, hostport) = match hostpart.rsplit_once('@') {
            Some((u, h)) => (Some(u.to_string()), h),
            None => (None, hostpart),
        };
        // a port is written after the host - after the brackets of an
        // IPv6 one, or after the only colon of anything else
        let port_given = match hostport.split_once(']') {
            Some((_, after)) => after.starts_with(':'),
            None => hostport.matches(':').count() == 1,
        };
        let (host, port) = crate::remote::split_host_port(hostport, 22)?;
        let user_given = user.is_some();
        let user = user.unwrap_or_else(default_user);
        if host.is_empty() || user.is_empty() {
            return None;
        }
        Some(SftpUrl {
            scheme: scheme.to_string(),
            user,
            host,
            port,
            path,
            hostname: None,
            identities: Vec::new(),
            proxy: None,
            user_given,
            port_given,
        })
    }

    /// What `~/.ssh/config` says about the host fills in what the URL
    /// left out: the user and the port, the address behind an alias,
    /// and the keys to offer. What the URL did say stands.
    pub fn with_ssh_config(self) -> SftpUrl {
        let config = crate::sshconfig::lookup(&self.host);
        self.with_host_config(&config)
    }

    fn with_host_config(mut self, config: &crate::sshconfig::HostConfig) -> SftpUrl {
        if !self.user_given
            && let Some(user) = &config.user
        {
            self.user = user.clone();
        }
        if !self.port_given
            && let Some(port) = config.port
        {
            self.port = port;
        }
        self.hostname = config.hostname.clone();
        self.identities = config
            .identities
            .iter()
            .map(|template| crate::sshconfig::identity_path(template, &self.host, &self.user))
            .collect();
        // `none` is ssh's way of saying no proxy, whatever comes later
        let set =
            |value: &Option<String>| value.clone().filter(|v| !v.eq_ignore_ascii_case("none"));
        self.proxy = match (set(&config.proxy_jump), set(&config.proxy_command)) {
            (Some(hops), _) => Some(Proxy::Jump(hops)),
            (None, Some(command)) if config.proxy_jump.is_none() => {
                Some(Proxy::Command(self.proxy_tokens(&command)))
            }
            _ => None,
        };
        self
    }

    /// A `ProxyCommand` with its tokens filled in: `%h` the address to
    /// dial, `%p` the port, `%r` the remote user, `%n` the name as
    /// typed, `%%` a percent sign.
    fn proxy_tokens(&self, command: &str) -> String {
        let mut out = String::with_capacity(command.len());
        let mut chars = command.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('h') => out.push_str(self.dial_host()),
                Some('p') => out.push_str(&self.port.to_string()),
                Some('r') => out.push_str(&self.user),
                Some('n') => out.push_str(&self.host),
                Some('%') => out.push('%'),
                Some(other) => {
                    out.push('%');
                    out.push(other);
                }
                None => out.push('%'),
            }
        }
        out
    }

    /// The address to connect to: the alias's `HostName`, or the host.
    pub fn dial_host(&self) -> &str {
        self.hostname.as_deref().unwrap_or(&self.host)
    }

    /// `sftp://user@host[:port]` - the connection identity, also the
    /// panel title prefix and the connection-cache key. The scheme is
    /// part of it: the same host reached two ways is two connections.
    pub fn prefix(&self) -> String {
        let host = crate::remote::url_host(&self.host);
        if self.port == 22 {
            format!("{}://{}@{}", self.scheme, self.user, host)
        } else {
            format!("{}://{}@{}:{}", self.scheme, self.user, host, self.port)
        }
    }

    pub fn display(&self) -> String {
        format!("{}{}", self.prefix(), self.path.display())
    }
}

/// How a connection reaches a server it does not dial itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Proxy {
    /// `ProxyJump`: through these hosts, comma-separated, the way
    /// `ssh -J` takes them.
    Jump(String),
    /// `ProxyCommand`, its tokens filled in: a shell command whose
    /// standard input and output are the connection.
    Command(String),
}

impl Proxy {
    /// The program that carries the connection, and its arguments.
    fn argv(&self, url: &SftpUrl) -> Vec<String> {
        match self {
            Proxy::Jump(hops) => {
                let hops: Vec<&str> = hops.split(',').map(str::trim).collect();
                let (last, before) = hops.split_last().expect("split yields one at least");
                // BatchMode: ssh must not ask anything on the terminal
                // rcmd is drawing on - the jump host needs a key or the
                // agent, as it would in a script
                let mut argv: Vec<String> = ["ssh", "-o", "BatchMode=yes", "-o", "LogLevel=ERROR"]
                    .map(String::from)
                    .to_vec();
                if !before.is_empty() {
                    argv.push("-J".into());
                    argv.push(before.join(","));
                }
                argv.push("-W".into());
                argv.push(format!(
                    "{}:{}",
                    crate::remote::url_host(url.dial_host()),
                    url.port
                ));
                argv.push("--".into());
                argv.push(last.to_string());
                argv
            }
            Proxy::Command(command) => vec!["sh".into(), "-c".into(), command.clone()],
        }
    }
}

/// A connection that is a program's standard input and output: one end
/// of a socket pair, the other end being the program's. libssh2 talks
/// to a socket with send and recv, which a pipe does not take - hence
/// the pair. The program lives exactly as long as the session.
struct Proxied {
    socket: std::os::unix::net::UnixStream,
    child: std::process::Child,
}

impl std::os::fd::AsRawFd for Proxied {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.socket.as_raw_fd()
    }
}

impl Drop for Proxied {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Proxied {
    /// Start the proxy, and a thread keeping what it says on stderr -
    /// the reason, when it fails.
    fn spawn(proxy: &Proxy, url: &SftpUrl) -> Result<(Proxied, Arc<Mutex<String>>), String> {
        use std::os::unix::process::CommandExt;
        let argv = proxy.argv(url);
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().map_err(|e| e.to_string())?;
        let input = theirs.try_clone().map_err(|e| e.to_string())?;
        let mut command = std::process::Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .stdin(std::process::Stdio::from(std::os::fd::OwnedFd::from(input)))
            .stdout(std::process::Stdio::from(std::os::fd::OwnedFd::from(
                theirs,
            )))
            .stderr(std::process::Stdio::piped());
        // a session of its own: no controlling terminal, so nothing it
        // runs can read rcmd's keys or write over its screen
        // SAFETY: setsid is async-signal-safe and touches no memory
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|e| format!("{}: {e}", argv[0]))?;
        drop(command); // closes the parent's copies of the child's end
        let said = Arc::new(Mutex::new(String::new()));
        if let Some(mut stderr) = child.stderr.take() {
            let said = Arc::clone(&said);
            thread::spawn(move || {
                let mut buf = [0u8; 1024];
                while let Ok(n) = stderr.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let mut said = said.lock().unwrap_or_else(|p| p.into_inner());
                    if said.len() < 4096 {
                        said.push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                }
            });
        }
        Ok((
            Proxied {
                socket: ours,
                child,
            },
            said,
        ))
    }
}

fn default_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "root".to_string())
}

/// A path under the home directory written from `~`, as a prompt
/// should: the dialog shows a prompt's tail, and a long absolute path
/// pushes the question itself off the front.
fn tilde(path: &Path) -> String {
    match home_dir().and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
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
        host: url.host,
    }
}

/// Dial a host and authenticate. What is done with the session after
/// that - the SFTP subsystem, or a shell - is the caller's business,
/// which is what lets `fish://` reuse every question asked here. The
/// answers given are kept, so the connection can be dialed again later
/// without asking them twice.
pub fn ssh_session(
    url: &SftpUrl,
    tx: &Sender<ConnectEvent>,
    rx: &Receiver<ConnectReply>,
) -> Result<(Session, Redial), String> {
    let mut ask = Ui {
        tx,
        rx,
        told: Login::default(),
    };
    let session = dial(url, &mut ask)?;
    Ok((
        session,
        Redial {
            url: url.clone(),
            login: ask.told,
        },
    ))
}

/// How a session is dialed again once it has died: where to, and what
/// the first login was told - the secrets typed and a host key taken
/// on trust - so the same answers can be given without anyone asked.
pub struct Redial {
    url: SftpUrl,
    login: Login,
}

#[derive(Default)]
struct Login {
    answers: Vec<String>,
    fingerprint: Option<String>,
}

impl Redial {
    /// A fresh session, or why not. Nothing is asked: an answer the
    /// first login did not give - a new passphrase, a one-time code, a
    /// host key it did not accept - fails this the way a refusal would.
    pub fn session(&self) -> Result<Session, String> {
        let mut ask = Replay {
            login: &self.login,
            next: 0,
        };
        dial(&self.url, &mut ask)
    }
}

/// The questions a login asks, and who answers them.
trait Ask {
    fn info(&mut self, _message: String) {}
    /// A secret, or `Err` to stop the whole login.
    fn secret(&mut self, prompt: String, echo: bool) -> Result<String, String>;
    /// Whether to trust a host key known_hosts does not have.
    fn host_key(&mut self, fingerprint: &str) -> bool;
}

/// The person at the dialogs, with what they said written down.
struct Ui<'a> {
    tx: &'a Sender<ConnectEvent>,
    rx: &'a Receiver<ConnectReply>,
    told: Login,
}

impl Ask for Ui<'_> {
    fn info(&mut self, message: String) {
        let _ = self.tx.send(ConnectEvent::Info(message));
    }

    fn secret(&mut self, prompt: String, echo: bool) -> Result<String, String> {
        let answer = ask_secret(self.tx, self.rx, prompt, echo)?;
        self.told.answers.push(answer.clone());
        Ok(answer)
    }

    fn host_key(&mut self, fingerprint: &str) -> bool {
        let fingerprint = fingerprint.to_string();
        if self
            .tx
            .send(ConnectEvent::AskHostKey {
                fingerprint: fingerprint.clone(),
            })
            .is_err()
        {
            return false;
        }
        let yes = matches!(self.rx.recv(), Ok(ConnectReply::Accept(true)));
        if yes {
            self.told.fingerprint = Some(fingerprint);
        }
        yes
    }
}

/// A login's answers said again, in order.
struct Replay<'a> {
    login: &'a Login,
    next: usize,
}

impl Ask for Replay<'_> {
    fn secret(&mut self, _prompt: String, _echo: bool) -> Result<String, String> {
        let answer = self.login.answers.get(self.next).cloned();
        self.next += 1;
        answer.ok_or_else(|| "cannot log in again without asking".to_string())
    }

    fn host_key(&mut self, fingerprint: &str) -> bool {
        self.login.fingerprint.as_deref() == Some(fingerprint)
    }
}

fn dial(url: &SftpUrl, ask: &mut dyn Ask) -> Result<Session, String> {
    let host = url.dial_host();
    let mut sess = Session::new().map_err(|e| e.to_string())?;
    sess.set_timeout(IO_TIMEOUT_MS);
    let proxy_said = match &url.proxy {
        None => {
            ask.info(format!("Connecting to {host}:{}…", url.port));
            sess.set_tcp_stream(tcp_to(host, url.port)?);
            None
        }
        Some(proxy) => {
            let via = match proxy {
                Proxy::Jump(hops) => format!("via {hops}"),
                Proxy::Command(_) => "via its ProxyCommand".to_string(),
            };
            ask.info(format!("Connecting to {host}:{} {via}…", url.port));
            let (stream, said) = Proxied::spawn(proxy, url)?;
            sess.set_tcp_stream(stream);
            Some(said)
        }
    };
    if let Err(err) = sess.handshake() {
        // the proxy's own words say more than "handshake failed": they
        // are complete once it is gone, which dropping the session sees to
        drop(sess);
        let said = proxy_said
            .map(|said| {
                std::thread::sleep(Duration::from_millis(100));
                let said = said.lock().unwrap_or_else(|p| p.into_inner());
                said.lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_default();
        return Err(match said {
            Some(said) => format!("proxy: {said}"),
            None => format!("handshake: {err}"),
        });
    }

    check_host_key(url, &sess, ask)?;

    ask.info(format!("Authenticating as {}…", url.user));
    authenticate(url, &sess, ask)?;
    // keepalives are sent only when asked for; `keep_alive` asks
    sess.set_keepalive(false, KEEPALIVE.as_secs() as u32);
    Ok(sess)
}

/// A TCP connection to the first of the host's addresses that answers.
fn tcp_to(host: &str, port: u16) -> Result<TcpStream, String> {
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("{host}: {e}"))?;
    let mut last_err = format!("{host}: no addresses");
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(s) => return Ok(s),
            Err(e) => last_err = format!("{addr}: {e}"),
        }
    }
    Err(last_err)
}

/// Whether an error says the connection itself is gone - the socket
/// closed, reset or silent past the timeout - rather than that the
/// server refused one request on a connection that is fine.
pub(crate) fn is_dead(err: &ssh2::Error) -> bool {
    // libssh2's SOCKET_NONE, SOCKET_SEND, TIMEOUT, SOCKET_DISCONNECT,
    // SOCKET_TIMEOUT and SOCKET_RECV
    matches!(
        err.code(),
        ssh2::ErrorCode::Session(-1 | -7 | -9 | -13 | -30 | -43)
    )
}

/// How often an idle connection says something. NAT boxes and
/// firewalls forget a TCP connection that has been quiet for a few
/// minutes, and the next listing then hangs until it times out.
pub(crate) const KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(30);

/// A thread that sends a keepalive over `session` every [`KEEPALIVE`]
/// for as long as the filesystem holding it lives - it has only a weak
/// reference, so the connection is not kept open by it.
pub(crate) fn keep_alive<T: Send + Sync + 'static>(fs: std::sync::Weak<T>, ping: fn(&T)) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(KEEPALIVE);
            let Some(fs) = fs.upgrade() else { break };
            ping(&fs);
        }
    });
}

fn connect(
    url: &SftpUrl,
    tx: &Sender<ConnectEvent>,
    rx: &Receiver<ConnectReply>,
) -> Result<(Arc<SftpFs>, PathBuf, Vec<Entry>), String> {
    let (sess, redial) = ssh_session(url, tx, rx)?;
    let sftp = sess.sftp().map_err(|e| format!("sftp: {e}"))?;
    let start = if url.path.as_os_str().is_empty() {
        sftp.realpath(Path::new("."))
            .unwrap_or_else(|_| PathBuf::from("/"))
    } else {
        url.path.clone()
    };
    let fs = Arc::new(SftpFs {
        raw: Mutex::new(Raw {
            session: sess,
            sftp,
        }),
        prefix: url.prefix(),
        redial,
        dead: AtomicBool::new(false),
    });
    keep_alive(Arc::downgrade(&fs), |fs: &SftpFs| {
        // a keepalive that cannot be sent: the next operation dials
        // again at once instead of waiting out the timeout first
        if let Err(err) = fs.lock().session.keepalive_send()
            && is_dead(&err)
        {
            fs.dead.store(true, Ordering::Relaxed);
        }
    });
    let entries = fs
        .read_dir(&start)
        .map_err(|e| format!("{}: {e}", start.display()))?;
    Ok((fs, start, entries))
}

fn check_host_key(url: &SftpUrl, sess: &Session, ask: &mut dyn Ask) -> Result<(), String> {
    let mut kh = sess.known_hosts().map_err(|e| e.to_string())?;
    let file = home_dir().map(|h| h.join(".ssh/known_hosts"));
    if let Some(f) = &file {
        let _ = kh.read_file(f, KnownHostFileKind::OpenSSH); // may not exist yet
    }
    let (key, key_type) = sess.host_key().ok_or("server sent no host key")?;
    // known_hosts knows the machine by the name it was dialled by, as
    // OpenSSH records it - the HostName behind an alias
    let host = url.dial_host();
    match kh.check_port(host, url.port, key) {
        CheckResult::Match => Ok(()),
        CheckResult::Mismatch => Err(format!(
            "HOST KEY MISMATCH for {} - possible man-in-the-middle attack. \
             Remove the old key from ~/.ssh/known_hosts if the host really changed.",
            url.host
        )),
        CheckResult::NotFound | CheckResult::Failure => {
            let fingerprint = sess
                .host_key_hash(HashType::Sha256)
                .map(|h| format!("SHA256:{}", base64(h)))
                .unwrap_or_else(|| "(unavailable)".into());
            match ask.host_key(&fingerprint) {
                true => {
                    let name = if url.port == 22 {
                        host.to_string()
                    } else {
                        format!("[{host}]:{}", url.port)
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
                false => Err("host key rejected".into()),
            }
        }
    }
}

fn authenticate(url: &SftpUrl, sess: &Session, ask: &mut dyn Ask) -> Result<(), String> {
    // The "none" probe behind auth_methods() tells us what the server
    // accepts, so we only try (and only prompt for) methods that can
    // work - OpenSSH order: publickey, keyboard-interactive, password.
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
        // agent first, then default key files - encrypted ones prompt
        // for their passphrase instead of silently falling through
        let _ = sess.userauth_agent(&url.user);
        if sess.authenticated() {
            return Ok(());
        }
        // the host's own keys from ~/.ssh/config first, then the usual
        let defaults = home_dir().into_iter().flat_map(|home| {
            ["id_ed25519", "id_ecdsa", "id_rsa"].map(|name| home.join(".ssh").join(name))
        });
        let mut keys: Vec<PathBuf> = url.identities.clone();
        keys.extend(defaults.filter(|key| !url.identities.contains(key)));
        for key in keys {
            if !key.exists() {
                continue;
            }
            if key_needs_passphrase(&key) {
                for _ in 0..3 {
                    let prompt = format!("Enter passphrase for {}:", tilde(&key));
                    let phrase = ask.secret(prompt, false)?;
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
    if has("keyboard-interactive") {
        for _ in 0..3 {
            let mut prompter = Prompter {
                ask: &mut *ask,
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
            let password = ask.secret(prompt, false)?;
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
/// Servers may send several prompts per round - each becomes its own
/// dialog, in order.
struct Prompter<'a> {
    ask: &'a mut dyn Ask,
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
                format!("{} - {}", instructions.trim(), p.text)
            };
            match self.ask.secret(text, p.echo) {
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

/// Lenient base64 decoder (whitespace skipped, stops at padding) - just
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
    session: Session,
    sftp: ssh2::Sftp,
}

pub struct SftpFs {
    raw: Mutex<Raw>,
    prefix: String,
    /// How to get the connection back when it drops.
    redial: Redial,
    /// The keepalive found the connection gone.
    dead: AtomicBool,
}

impl RemoteFs for SftpFs {
    /// `sftp://user@host[:port]` - panel title prefix / cache key.
    fn prefix(&self) -> &str {
        &self.prefix
    }

    fn realpath(&self, path: &Path) -> io::Result<PathBuf> {
        self.with(|raw| raw.sftp.realpath(path))
    }
}

impl SftpFs {
    fn lock(&self) -> MutexGuard<'_, Raw> {
        self.raw.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Run one request, and if the connection turns out to be gone -
    /// a server restarted, a laptop that slept - dial again and run it
    /// once more. A request that failed at the socket never reached
    /// the server whole, so saying it again is what was meant.
    fn with<T>(&self, op: impl Fn(&Raw) -> Result<T, ssh2::Error>) -> io::Result<T> {
        let mut raw = self.lock();
        if self.dead.swap(false, Ordering::Relaxed) {
            self.revive(&mut raw)
                .map_err(|why| io::Error::new(io::ErrorKind::NotConnected, why))?;
        }
        match op(&raw) {
            Err(err) if is_dead(&err) => {
                self.revive(&mut raw).map_err(|why| {
                    io::Error::new(
                        io::ErrorKind::NotConnected,
                        format!("{err} - and dialing again: {why}"),
                    )
                })?;
                op(&raw).map_err(ioerr)
            }
            done => done.map_err(ioerr),
        }
    }

    fn revive(&self, raw: &mut Raw) -> Result<(), String> {
        crate::vfslog::line(
            "!",
            &format!("{}: connection lost, dialing again", self.prefix),
        );
        let session = self.redial.session()?;
        let sftp = session.sftp().map_err(|e| format!("sftp: {e}"))?;
        let old = std::mem::replace(raw, Raw { session, sftp });
        // closing a connection that went quiet rather than away can
        // wait out the whole timeout: let it, somewhere else
        thread::spawn(move || drop(old));
        Ok(())
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
        self.with(|raw| {
            let listed = raw.sftp.readdir(dir)?;
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
        })
    }

    fn stat(&self, path: &Path) -> io::Result<Entry> {
        let name = path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_else(|| "/".into());
        self.with(|raw| {
            let st = raw.sftp.lstat(path)?;
            Ok(entry_from(name.clone(), &st, &raw.sftp, path))
        })
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let file = self.with(|raw| raw.sftp.open(path))?;
        Ok(Box::new(SftpFile { file }))
    }

    fn open_read_at(&self, path: &Path, offset: u64) -> io::Result<Box<dyn Read + Send>> {
        use std::io::Seek;
        let mut file = self.with(|raw| raw.sftp.open(path))?;
        file.seek(io::SeekFrom::Start(offset))?;
        Ok(Box::new(SftpFile { file }))
    }

    fn writer(&self) -> Option<&dyn FsWrite> {
        Some(self)
    }
}

impl FsWrite for SftpFs {
    fn mkdir(&self, dir: &Path) -> io::Result<()> {
        self.with(|raw| raw.sftp.mkdir(dir, 0o755))
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.with(|raw| raw.sftp.unlink(path))
    }

    fn remove_dir(&self, dir: &Path) -> io::Result<()> {
        self.with(|raw| raw.sftp.rmdir(dir))
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        use ssh2::RenameFlags;
        let flags = RenameFlags::ATOMIC | RenameFlags::OVERWRITE | RenameFlags::NATIVE;
        self.with(|raw| {
            raw.sftp.rename(from, to, Some(flags)).or_else(|err| {
                if is_dead(&err) {
                    return Err(err);
                }
                // servers without POSIX rename refuse to overwrite
                let _ = raw.sftp.unlink(to);
                raw.sftp.rename(from, to, None)
            })
        })
    }

    fn open_write(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        let file = self.with(|raw| {
            raw.sftp.open_mode(
                path,
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
                0o644,
                OpenType::File,
            )
        })?;
        Ok(Box::new(SftpFile { file }))
    }

    fn can_append(&self) -> bool {
        true
    }

    fn open_append(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        use std::io::Seek;
        let mut file = self.with(|raw| {
            raw.sftp.open_mode(
                path,
                OpenFlags::WRITE | OpenFlags::APPEND,
                0o644,
                OpenType::File,
            )
        })?;
        // not every server honours APPEND; writing from the end does
        // the same on any of them
        file.seek(io::SeekFrom::End(0))?;
        Ok(Box::new(SftpFile { file }))
    }

    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        self.with(|raw| raw.sftp.setstat(path, stat_with(|st| st.perm = Some(mode))))
    }

    fn set_owner(&self, path: &Path, uid: Option<u32>, gid: Option<u32>) -> io::Result<()> {
        // SFTP's UIDGID attribute carries both ids; fill the missing
        // half from the current stat so it stays unchanged
        self.with(|raw| {
            let (cur_uid, cur_gid) = match raw.sftp.lstat(path) {
                Ok(st) => (st.uid, st.gid),
                Err(err) if is_dead(&err) => return Err(err),
                Err(_) => (None, None),
            };
            raw.sftp.setstat(
                path,
                stat_with(|st| {
                    st.uid = uid.or(cur_uid);
                    st.gid = gid.or(cur_gid);
                }),
            )
        })
    }

    fn set_mtime(&self, path: &Path, mtime: SystemTime) -> io::Result<()> {
        let secs = mtime
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // the SFTP ACMODTIME attribute always carries both stamps
        self.with(|raw| {
            raw.sftp.setstat(
                path,
                stat_with(|st| {
                    st.mtime = Some(secs);
                    st.atime = Some(secs);
                }),
            )
        })
    }

    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
        self.with(|raw| raw.sftp.symlink(target, link))
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
    fn a_proxy_that_fails_says_why_in_its_own_words() {
        let mut url = SftpUrl::parse("sftp://bob@box/x").unwrap();
        url.proxy = Some(Proxy::Command(
            "echo 'gate: no route to box' >&2; exit 1".into(),
        ));
        let login = Login::default();
        let Err(err) = dial(
            &url,
            &mut Replay {
                login: &login,
                next: 0,
            },
        ) else {
            panic!("a failing proxy connected");
        };
        assert_eq!(err, "proxy: gate: no route to box");
    }

    #[test]
    fn a_proxy_from_the_config_becomes_the_command_that_carries_it() {
        let url = SftpUrl::parse("sftp://bob@box:2200/x").unwrap();
        let jump = crate::sshconfig::HostConfig {
            hostname: Some("10.0.0.5".into()),
            proxy_jump: Some("alice@gate, inner:2022".into()),
            ..Default::default()
        };
        let via = url.clone().with_host_config(&jump);
        assert_eq!(
            via.proxy,
            Some(Proxy::Jump("alice@gate, inner:2022".into()))
        );
        let argv = via.proxy.as_ref().unwrap().argv(&via);
        assert_eq!(
            argv[5..],
            [
                "-J",
                "alice@gate",
                "-W",
                "10.0.0.5:2200",
                "--",
                "inner:2022"
            ]
        );
        let command = crate::sshconfig::HostConfig {
            proxy_command: Some("nc %h %p # %r %n 100%%".into()),
            ..Default::default()
        };
        let via = url.clone().with_host_config(&command);
        assert_eq!(
            via.proxy,
            Some(Proxy::Command("nc box 2200 # bob box 100%".into()))
        );
        // none is none, even with the other kind set
        let off = crate::sshconfig::HostConfig {
            proxy_jump: Some("none".into()),
            proxy_command: Some("nc %h %p".into()),
            ..Default::default()
        };
        assert_eq!(url.with_host_config(&off).proxy, None);
    }

    #[test]
    fn a_redial_says_what_the_login_was_told_and_nothing_more() {
        let login = Login {
            answers: vec!["wrong".into(), "secret".into()],
            fingerprint: Some("SHA256:abc".into()),
        };
        let mut replay = Replay {
            login: &login,
            next: 0,
        };
        assert_eq!(replay.secret("password:".into(), false).unwrap(), "wrong");
        assert_eq!(replay.secret("password:".into(), false).unwrap(), "secret");
        // a third question was never answered: nobody is asked it now
        assert!(replay.secret("one-time code:".into(), true).is_err());
        assert!(replay.host_key("SHA256:abc"));
        assert!(!replay.host_key("SHA256:other"));
        let never = Login::default();
        assert!(
            !Replay {
                login: &never,
                next: 0
            }
            .host_key("SHA256:abc")
        );
    }

    #[test]
    fn only_the_socket_going_counts_as_a_dead_connection() {
        use ssh2::{Error, ErrorCode};
        for code in [-1, -7, -9, -13, -30, -43] {
            assert!(
                is_dead(&Error::new(ErrorCode::Session(code), "gone")),
                "{code}"
            );
        }
        // an SFTP "no such file" (2) or a denied request is an answer
        assert!(!is_dead(&Error::new(ErrorCode::SFTP(2), "no such file")));
        assert!(!is_dead(&Error::new(ErrorCode::Session(-18), "auth")));
    }

    #[test]
    fn ssh_config_fills_in_only_what_the_url_left_out() {
        let config = crate::sshconfig::HostConfig {
            hostname: Some("box.example.com".into()),
            user: Some("alice".into()),
            port: Some(2222),
            identities: vec!["/keys/%r_%h".into()],
            ..Default::default()
        };
        let url = SftpUrl::parse("sftp://box/srv")
            .unwrap()
            .with_host_config(&config);
        assert_eq!((url.user.as_str(), url.port), ("alice", 2222));
        assert_eq!(url.dial_host(), "box.example.com");
        assert_eq!(
            url.prefix(),
            "sftp://alice@box:2222",
            "the alias names the connection"
        );
        assert_eq!(url.identities, [PathBuf::from("/keys/alice_box")]);
        let url = SftpUrl::parse("sftp://bob@box:22")
            .unwrap()
            .with_host_config(&config);
        assert_eq!((url.user.as_str(), url.port), ("bob", 22));
    }

    #[test]
    fn an_ipv6_literal_parses() {
        let url = SftpUrl::parse("sftp://me@[::1]:2222/tmp").unwrap();
        assert_eq!((url.host.as_str(), url.port), ("::1", 2222));
        assert_eq!(url.prefix(), "sftp://me@[::1]:2222");
    }

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
