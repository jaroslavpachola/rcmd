//! Driving a running rcmd from outside it.
//!
//! Every instance listens on a unix socket of its own; `rcmd --remote`
//! connects to one and hands it a line. That one mechanism is also the
//! plugin story: a shell script that can `cd` the panel, mark files and
//! run any action by name does not need an ABI, a runtime, or a
//! versioned interface to be a plugin - it needs a socket and the name
//! of a command, both of which are in the environment as `RCMD_SOCKET`
//! while the command runs.
//!
//! The socket lives under the user's runtime directory, 0700: anyone
//! who can reach it can already run commands as the user.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};

use anyhow::{Context, Result, bail};

/// One line from a client, and where to send the answer.
pub struct Request {
    pub line: String,
    pub reply: Sender<String>,
}

/// The listener, alive as long as this instance is.
pub struct Server {
    pub requests: Receiver<Request>,
    path: PathBuf,
}

impl Server {
    /// Where to reach this instance. Handed to every command rcmd runs
    /// as `RCMD_SOCKET`, which is how a script knows which of several
    /// instances asked for it.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// `$XDG_RUNTIME_DIR/rcmd`, or `/tmp/rcmd-<uid>` where there is no
/// runtime directory (a bare ssh, a cron job). Created 0700.
pub fn socket_dir() -> Result<PathBuf> {
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(runtime) => PathBuf::from(runtime).join("rcmd"),
        // SAFETY: getuid cannot fail and touches no memory
        None => PathBuf::from(format!("/tmp/rcmd-{}", unsafe { libc::getuid() })),
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot make {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// Where this instance's socket is, whether or not it is listening
/// yet: the name is only the process id, so it can be handed to a
/// child spawned before the listener starts.
pub fn socket_path() -> Result<PathBuf> {
    Ok(socket_dir()?.join(format!("{}.sock", std::process::id())))
}

/// Start listening. Every accepted line goes to the channel; the answer
/// the app puts back goes to the client and the connection closes, so a
/// client is one line and one answer.
pub fn serve() -> Result<Server> {
    let path = socket_path()?;
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("cannot listen on {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let (tx, requests) = channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            if handle(stream, &tx).is_err() {
                // the app is gone; so is any point in listening
                break;
            }
        }
    });
    Ok(Server { requests, path })
}

fn handle(mut stream: UnixStream, tx: &Sender<Request>) -> Result<()> {
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    let (reply, answer) = channel();
    tx.send(Request {
        line: line.trim().to_string(),
        reply,
    })
    .map_err(|_| anyhow::anyhow!("the app has stopped listening"))?;
    // a command that never answers must not wedge the client for ever
    let answer = answer
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap_or_else(|_| "error: no answer".to_string());
    let _ = writeln!(stream, "{answer}");
    Ok(())
}

/// The client half: `rcmd --remote [--to PID] LINE`. Prints whatever
/// the instance answers.
pub fn send(to: Option<u32>, line: &str) -> Result<()> {
    let dir = socket_dir()?;
    let path = match to {
        Some(pid) => dir.join(format!("{pid}.sock")),
        // a command rcmd itself started was told which instance asked
        // for it, and that is the one it means
        None if std::env::var_os("RCMD_SOCKET").is_some() => {
            PathBuf::from(std::env::var_os("RCMD_SOCKET").unwrap_or_default())
        }
        None => match live_sockets(&dir)?.as_slice() {
            [] => bail!("no rcmd is running (no socket in {})", dir.display()),
            [one] => one.clone(),
            many => bail!(
                "{} instances are running - name one with --to PID: {}",
                many.len(),
                many.iter()
                    .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
    };
    let mut stream =
        UnixStream::connect(&path).with_context(|| format!("cannot reach {}", path.display()))?;
    writeln!(stream, "{line}")?;
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer)?;
    let answer = answer.trim();
    if let Some(err) = answer.strip_prefix("error: ") {
        bail!("{err}");
    }
    if !answer.is_empty() && answer != "ok" {
        println!("{answer}");
    }
    Ok(())
}

/// Sockets in the directory that something is actually listening on;
/// the rest are what a crash left behind, and are cleaned up.
fn live_sockets(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "sock") {
            continue;
        }
        match UnixStream::connect(&path) {
            Ok(_) => out.push(path),
            Err(_) => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    out.sort();
    Ok(out)
}
