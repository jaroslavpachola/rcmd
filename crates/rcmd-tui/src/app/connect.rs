use super::*;

impl App {
    /// Start (or resume) a connection for the active panel. The scheme
    /// picks the protocol; everything after that - the password
    /// prompt, the cache, the panel switch - is the same either way.
    pub(super) fn connect_remote(&mut self, input: &str) {
        if self.connect.is_some() {
            self.status = Some(" a connection attempt is already running ".into());
            return;
        }
        let handle = if input.starts_with("rclone://") {
            let Some(url) = rcmd_core::rclone::RcloneUrl::parse(input) else {
                self.status = Some(" bad URL - rclone://remote[/path] ".into());
                return;
            };
            // no login and no host key: rclone's own config did that
            // already, so the connection is the first listing
            let fs = match self.connection(&url.prefix()) {
                Some(fs) => fs,
                None => std::sync::Arc::new(rcmd_core::rclone::RcloneFs::new(&url.remote)),
            };
            remote::spawn_reuse(fs, url.path, url.remote)
        } else if input.starts_with("ftp://") {
            let Some(url) = FtpUrl::parse(input).map(FtpUrl::with_netrc) else {
                self.status = Some(" bad URL - ftp://[user[:password]@]host[:port][/path] ".into());
                return;
            };
            match self.connection(&url.prefix()) {
                Some(fs) => remote::spawn_reuse(fs, url.path, url.host),
                None => ftp::spawn_connect(url),
            }
        } else {
            let fish = input.starts_with("fish://");
            let scheme = if fish { "fish" } else { "sftp" };
            let Some(url) = SftpUrl::parse_as(scheme, input).map(SftpUrl::with_ssh_config) else {
                self.status = Some(format!(" bad URL - {scheme}://[user@]host[:port][/path] "));
                return;
            };
            match (self.connection(&url.prefix()), fish) {
                (Some(fs), _) => remote::spawn_reuse(fs, url.path, url.host),
                (None, true) => fish::spawn_connect(url),
                (None, false) => sftp::spawn_connect(url),
            }
        };
        self.status = Some(format!(" connecting to {}… - Esc cancels ", handle.host));
        self.connect = Some(ConnectState {
            handle,
            panel: self.active,
            ask: None,
        });
    }

    /// What the panels are sitting on that is not the local filesystem,
    /// plus any SFTP connection still cached. An archive belongs to the
    /// panel that opened it and disappears with it; a connection is
    /// kept so that going back to the same host does not mean logging
    /// in again, which is why one can be listed with no panel on it.
    pub(super) fn vfs_rows(&mut self) -> Vec<VfsRow> {
        self.connections.retain(|(_, weak)| weak.strong_count() > 0);
        let mut rows: Vec<VfsRow> = Vec::new();
        for (prefix, _) in &self.connections {
            let used_by = (0..self.panels.len())
                .filter(|i| self.panels[*i].remote.as_deref() == Some(prefix.as_str()))
                .collect();
            rows.push(VfsRow {
                label: prefix.clone(),
                target: prefix.clone(),
                used_by,
                kind: VfsKind::Remote,
            });
        }
        for (index, panel) in self.panels.iter().enumerate() {
            let Some(archive) = &panel.archive else {
                continue;
            };
            let target = archive.display().to_string();
            if let Some(row) = rows
                .iter_mut()
                .find(|row| row.kind == VfsKind::Archive && row.target == target)
            {
                row.used_by.push(index);
                continue;
            }
            rows.push(VfsRow {
                label: format!("{target}://"),
                target,
                used_by: vec![index],
                kind: VfsKind::Archive,
            });
        }
        // ...and under rcmd's own doing, the machine's: what is
        // mounted, and how much room is left on it. Far keeps this on
        // M-F1 / M-F2 and calls it the drive menu; on a unix the drives
        // are mount points, and the archives and connections are the
        // rest of the same question - where can this panel go
        for mount in rcmd_core::mounts::mounts() {
            let room = match mount.total {
                0 => String::new(),
                total => format!(
                    "  {} / {} free",
                    crate::ui::human_size(mount.free),
                    crate::ui::human_size(total)
                ),
            };
            let used_by = (0..self.panels.len())
                .filter(|i| {
                    self.panels[*i].is_local()
                        && self.panels[*i].local_cwd().starts_with(&mount.point)
                })
                .collect();
            rows.push(VfsRow {
                label: format!("{:<24} {}{room}", mount.point, mount.source),
                target: mount.point,
                used_by,
                kind: VfsKind::Mount,
            });
        }
        rows
    }

    /// Send whichever panels are on this entry back to a local
    /// directory, and forget the connection if it was one. mc calls
    /// this "free"; what it frees is the panel as much as the handle.
    pub(super) fn free_vfs(&mut self, row: &VfsRow) {
        for index in row.used_by.iter().copied() {
            let home = self.panels[index].local_cwd();
            if let Err(err) = self.panels[index].to_local(home) {
                self.status = Some(format!(" {err} "));
                return;
            }
        }
        if row.kind == VfsKind::Remote {
            self.connections.retain(|(prefix, _)| prefix != &row.target);
        }
        self.status = Some(format!(" freed {} ", row.label));
    }

    /// Look up a live connection by URL prefix, dropping dead ones.
    pub(super) fn connection(&mut self, prefix: &str) -> Option<Arc<dyn RemoteFs>> {
        self.connections.retain(|(_, weak)| weak.strong_count() > 0);
        self.connections
            .iter()
            .find(|(p, _)| p == prefix)
            .and_then(|(_, weak)| weak.upgrade())
    }

    pub(super) fn drain_connect(&mut self) {
        let Some(connect) = self.connect.as_mut() else {
            return;
        };
        while let Ok(event) = connect.handle.events.try_recv() {
            match event {
                ConnectEvent::Info(msg) => self.status = Some(format!(" {msg} ")),
                ConnectEvent::AskHostKey { fingerprint } => {
                    connect.ask = Some(ConnectAsk::HostKey {
                        fingerprint,
                        yes: false, // safe default
                    });
                }
                ConnectEvent::AskPassword { prompt, echo } => {
                    connect.ask = Some(ConnectAsk::Password {
                        prompt,
                        value: String::new(),
                        cursor: 0,
                        echo,
                    });
                }
                ConnectEvent::Ok { fs, start, entries } => {
                    let connect = self.connect.take().expect("connect present");
                    let prefix = fs.prefix().to_string();
                    self.connections.retain(|(p, _)| p != &prefix);
                    self.connections.push((prefix.clone(), Arc::downgrade(&fs)));
                    self.panels[connect.panel].adopt_remote(fs, prefix.clone(), start, entries);
                    self.status = Some(format!(" connected to {prefix} "));
                    return;
                }
                ConnectEvent::Err(msg) => {
                    self.connect = None;
                    self.status = Some(format!(" sftp: {msg} "));
                    return;
                }
            }
        }
    }

    pub(super) fn on_connect_key(&mut self, key: KeyEvent) {
        let Some(connect) = self.connect.as_mut() else {
            return;
        };
        match connect.ask.as_mut() {
            None => {
                if key.code == KeyCode::Esc {
                    // dropping the handle closes the reply channel; the
                    // worker unblocks and gives up
                    self.connect = None;
                    self.status = Some(" connection cancelled ".into());
                }
            }
            Some(ConnectAsk::HostKey { yes, .. }) => {
                let reply = match key.code {
                    KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                        *yes = !*yes;
                        None
                    }
                    KeyCode::Enter => Some(*yes),
                    KeyCode::Char('y' | 'Y') => Some(true),
                    KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(false),
                    _ => None,
                };
                if let Some(accept) = reply {
                    let _ = connect.handle.replies.send(ConnectReply::Accept(accept));
                    connect.ask = None;
                    if !accept {
                        self.connect = None;
                        self.status = Some(" host key rejected ".into());
                    }
                }
            }
            Some(ConnectAsk::Password { value, cursor, .. }) => match key.code {
                KeyCode::Esc => {
                    let _ = connect.handle.replies.send(ConnectReply::Cancel);
                    self.connect = None;
                    self.status = Some(" connection cancelled ".into());
                }
                KeyCode::Enter => {
                    let password = std::mem::take(value);
                    let _ = connect
                        .handle
                        .replies
                        .send(ConnectReply::Password(password));
                    connect.ask = None;
                }
                code => {
                    edit_line(value, cursor, code, key.modifiers);
                }
            },
        }
    }

    /// After F4 on a remote file: upload the scratch copy back if the
    /// editor modified it, then clean up.
    /// The external editor's copy, once the child has exited.
    pub fn finish_remote_edit(&mut self) {
        let Some(edit) = self.remote_edit.take() else {
            return;
        };
        self.upload_remote_edit(edit);
    }

    /// Send a scratch copy back where it came from, if it changed.
    pub(super) fn upload_remote_edit(&mut self, edit: RemoteEdit) {
        let mtime_now = std::fs::metadata(&edit.temp)
            .and_then(|m| m.modified())
            .ok();
        if mtime_now != edit.mtime_before {
            let uploaded = (|| -> std::io::Result<()> {
                let writer = edit
                    .fs
                    .writer()
                    .ok_or_else(|| std::io::Error::other("read-only filesystem"))?;
                let mut input = std::fs::File::open(&edit.temp)?;
                let mut output = writer.open_write(&edit.remote_path)?;
                std::io::copy(&mut input, &mut output)?;
                Ok(())
            })();
            self.status = Some(match &uploaded {
                Ok(()) => format!(" uploaded {} ", edit.remote_path.display()),
                Err(err) => format!(
                    " upload failed: {err} - local copy kept at {} ",
                    edit.temp.display()
                ),
            });
            if uploaded.is_err() {
                return; // keep the scratch file for rescue
            }
        }
        let _ = std::fs::remove_file(&edit.temp);
    }
}

impl App {
    pub(super) fn remote_mkdir(&mut self, value: &str) {
        let panel = &mut self.panels[self.active];
        let path = {
            let raw = Path::new(value);
            if raw.is_absolute() {
                raw.to_path_buf()
            } else {
                panel.cwd.join(raw)
            }
        };
        let made = panel
            .fs
            .writer()
            .ok_or_else(|| std::io::Error::other("read-only filesystem"))
            .and_then(|w| w.mkdir(&normalize(&path)));
        match made {
            Ok(()) => self.fallible(|p| p.reload().map(|()| true)),
            Err(err) => self.status = Some(format!(" mkdir: {err} ")),
        }
    }
}
