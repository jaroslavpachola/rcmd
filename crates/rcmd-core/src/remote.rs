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
