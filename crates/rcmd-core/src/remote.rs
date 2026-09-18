//! What a connection attempt looks like from the outside, whatever the
//! protocol underneath. A worker thread dials, and streams what it
//! needs along the way - a host key to trust, a password to type -
//! back to the UI, which answers on the reply channel. The worker
//! blocks on those answers, so the interface never has to.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use crate::entry::Entry;
use crate::vfs::RemoteFs;

/// Streamed by a connect worker. `Ask*` events block it until the UI
/// answers on the reply channel.
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
        fs: Arc<dyn RemoteFs>,
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
    /// Just for the "connecting to …" line: the handle is protocol
    /// agnostic and the URL it came from is not.
    pub host: String,
}

/// Reuse an established connection for another `cd` to the same host:
/// only the start directory is resolved and listed. The protocol does
/// not come into it - whatever dialled the connection, going back to it
/// is the same two steps.
pub fn spawn_reuse(fs: Arc<dyn RemoteFs>, path: PathBuf, host: String) -> ConnectHandle {
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let (reply_tx, _reply_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let start = if path.as_os_str().is_empty() {
            fs.realpath(std::path::Path::new("."))
                .unwrap_or_else(|_| PathBuf::from("/"))
        } else {
            path
        };
        let _ = event_tx.send(match fs.read_dir(&start) {
            Ok(entries) => ConnectEvent::Ok { fs, start, entries },
            Err(err) => ConnectEvent::Err(format!("{}: {err}", start.display())),
        });
    });
    ConnectHandle {
        events: event_rx,
        replies: reply_tx,
        host,
    }
}

/// Split a URL's `host[:port]`, an IPv6 literal written in brackets as
/// URLs write it (`[::1]:2222`). A bare address with more than one colon
/// is IPv6 with no port. `None` for a port that is not a number.
pub fn split_host_port(hostport: &str, default: u16) -> Option<(String, u16)> {
    if let Some(rest) = hostport.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(port) => port.parse().ok()?,
            None if after.is_empty() => default,
            None => return None,
        };
        return Some((host.to_string(), port));
    }
    if hostport.matches(':').count() > 1 {
        return Some((hostport.to_string(), default));
    }
    match hostport.rsplit_once(':') {
        Some((host, port)) => Some((host.to_string(), port.parse().ok()?)),
        None => Some((hostport.to_string(), default)),
    }
}

/// A host as a URL writes it: an IPv6 address in brackets.
pub fn url_host(host: &str) -> String {
    match host.contains(':') {
        true => format!("[{host}]"),
        false => host.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_and_port_including_ipv6() {
        assert_eq!(split_host_port("box", 22), Some(("box".into(), 22)));
        assert_eq!(split_host_port("box:2222", 22), Some(("box".into(), 2222)));
        assert_eq!(
            split_host_port("[::1]:2222", 22),
            Some(("::1".into(), 2222))
        );
        assert_eq!(
            split_host_port("[fe80::1]", 21),
            Some(("fe80::1".into(), 21))
        );
        assert_eq!(split_host_port("::1", 22), Some(("::1".into(), 22)));
        assert_eq!(split_host_port("box:x", 22), None);
        assert_eq!(url_host("::1"), "[::1]");
        assert_eq!(url_host("box"), "box");
    }
}
