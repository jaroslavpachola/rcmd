use super::*;

impl App {
    /// Leave the TUI, run a command or an interactive shell in the active
    /// panel's directory, then restore the TUI and reload both panels.
    pub(super) fn execute(&mut self, terminal: &mut DefaultTerminal, exec: Exec) -> Result<()> {
        use std::io::Write as _;
        use std::os::unix::process::CommandExt as _;

        let cwd = self.panels[self.active].local_cwd();
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        if self.config.mouse {
            set_mouse_capture(false);
        }
        set_bracketed_paste(false);
        ratatui::restore();
        // Shell-style job control: the child runs in its own foreground
        // process group, so Ctrl+C/Ctrl+Z hit it and never rcmd. We ignore
        // the terminal signals meanwhile (SIGTTOU also lets us tcsetpgrp
        // back from the "background").
        let old_sigint = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
        let old_sigtstp = unsafe { libc::signal(libc::SIGTSTP, libc::SIG_IGN) };
        let old_sigttou = unsafe { libc::signal(libc::SIGTTOU, libc::SIG_IGN) };
        let mut command = std::process::Command::new(&shell);
        match &exec {
            Exec::Command(cmd) => {
                println!("{}$ {cmd}", cwd.display());
                command.arg("-c").arg(cmd);
            }
            Exec::Quiet(cmd) => {
                command.arg("-c").arg(cmd);
            }
            Exec::Shell => {}
        }
        command.current_dir(&cwd);
        if let Some(server) = &self.remote {
            command.env("RCMD_SOCKET", server.path());
        }
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGTSTP, libc::SIG_DFL);
                libc::signal(libc::SIGTTOU, libc::SIG_DFL);
                libc::signal(libc::SIGTTIN, libc::SIG_DFL);
                libc::setpgid(0, 0);
                Ok(())
            });
        }
        match command.spawn() {
            Ok(child) => {
                let pid = child.id() as libc::pid_t;
                unsafe {
                    libc::setpgid(pid, pid); // idempotent with pre_exec's
                    libc::tcsetpgrp(libc::STDIN_FILENO, pid);
                }
                loop {
                    let mut status: libc::c_int = 0;
                    let rc = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
                    if rc < 0 {
                        break;
                    }
                    if libc::WIFSTOPPED(status) {
                        // Ctrl+Z: we cannot park a stopped child (no jobs
                        // table), so take the tty, say so, and resume it.
                        unsafe {
                            libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp());
                        }
                        println!("[rcmd has no job control - resuming]");
                        unsafe {
                            libc::tcsetpgrp(libc::STDIN_FILENO, pid);
                            libc::kill(-pid, libc::SIGCONT);
                        }
                        continue;
                    }
                    if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) != 0 {
                        unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp()) };
                        println!("[exit code {}]", libc::WEXITSTATUS(status));
                    } else if libc::WIFSIGNALED(status) {
                        unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp()) };
                        println!("[killed by signal {}]", libc::WTERMSIG(status));
                    }
                    break;
                }
                unsafe {
                    libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp());
                }
            }
            Err(err) => println!("cannot run {shell}: {err}"),
        }
        if matches!(exec, Exec::Command(_)) {
            print!("Press Enter to return to rcmd...");
            let _ = std::io::stdout().flush();
            let mut sink = String::new();
            let _ = std::io::stdin().read_line(&mut sink);
        }
        unsafe {
            libc::signal(libc::SIGINT, old_sigint);
            libc::signal(libc::SIGTSTP, old_sigtstp);
            libc::signal(libc::SIGTTOU, old_sigttou);
        }
        *terminal = ratatui::init();
        if self.config.mouse {
            set_mouse_capture(true);
        }
        set_bracketed_paste(true);
        // whatever ran may have set a title of its own
        self.title_shown = None;
        let _ = terminal.clear();
        for panel in &mut self.panels {
            let _ = panel.refresh();
        }
        self.git_refresh();
        Ok(())
    }

    /// Pump the hidden subshell every loop tick: collect output for the
    /// next Ctrl+O, keep the hook channel drained, respawn on `exit`.
    pub(super) fn subshell_tick(&mut self) {
        let Some(sub) = self.subshell.as_mut() else {
            return;
        };
        sub.pump(false);
        let note = sub.note.take();
        let failed = sub.failed;
        if let Some(note) = note {
            self.status = Some(note);
            self.dirty = true;
        }
        if failed {
            self.subshell = None;
            self.dirty = true;
        }
    }
}

/// What one pass of a subshell session wants the front end to do.
#[must_use]
pub enum SubshellStep {
    /// Bytes the shell produced: to the terminal's stdout, or to
    /// the window's VT parser.
    Output(Vec<u8>),
    /// Nothing to say this pass; the session continues.
    Waiting,
    /// The session is over - the fed command finished, or the shell
    /// died. Ctrl+O ends it from the other side, by not stepping.
    Done,
}

/// A subshell session in progress: Ctrl+O's output screen, or a
/// typed command running in it.
///
/// The protocol is the delicate part - wait for a prompt before
/// feeding anything, sync the directory in, feed the command, know
/// when it has finished - so it lives here once and both front ends
/// drive it: the terminal loops over [`App::step_subshell`] while
/// passing keys through raw, and the window calls it once per frame.
pub struct SubshellSession {
    /// The command still to be fed, once the shell is at a prompt.
    pending_cmd: Option<String>,
    /// A `cd` still to be fed, when the panels moved since the last
    /// time the two agreed on a directory.
    inject: Option<PathBuf>,
    /// Waiting for the shell's first prompt before feeding.
    start_wait: Option<Instant>,
    /// Waiting for the injected `cd` to land before the command.
    cd_wait: Option<Instant>,
    /// A command was fed and has not finished.
    awaiting: bool,
    /// The session is over - but whatever the shell printed last still
    /// has to be handed over before [`SubshellStep::Done`] is.
    finished: bool,
}

/// What a step hands back: the shell's last words first, `Done` only
/// once there are none left. A session that ended with output still
/// on the wire would lose a command's final line.
fn finish(session: &SubshellSession, bytes: Vec<u8>) -> SubshellStep {
    match (bytes.is_empty(), session.finished) {
        (false, _) => SubshellStep::Output(bytes),
        (true, true) => SubshellStep::Done,
        (true, false) => SubshellStep::Waiting,
    }
}

impl App {
    /// Open a subshell session for `exec`. `None` means there is none to
    /// open - no subshell, or a shell already busy with something else,
    /// which is what the status line then says (MC says the same).
    pub fn begin_subshell(&mut self, exec: Exec) -> Option<SubshellSession> {
        let pending_cmd = match exec {
            Exec::Command(cmd) => Some(cmd),
            _ => None,
        };
        let panel = &self.panels[self.active];
        let panel_dir = panel.is_local().then(|| panel.cwd.clone());
        let sub = self.subshell.as_mut()?;
        // a busy shell can't take a command (MC says the same); a shell
        // still starting up (slow rc files, compinit) is worth waiting
        // for - the session shows it booting
        if pending_cmd.is_some() && !sub.ready() && !sub.starting() {
            self.status = Some(" the shell is already running a command - Ctrl+O shows it ".into());
            return None;
        }
        sub.debug(&format!(
            "session start: cmd={pending_cmd:?} panel_dir={panel_dir:?} starting={}",
            sub.starting()
        ));

        // cd sync first if the panels moved since the last agreement
        // (the reverse sync happens on the way out)
        let mut inject = panel_dir.filter(|dir| *dir != sub.agreed && *dir != sub.cwd());
        if pending_cmd.is_none() && sub.ready() {
            // plain Ctrl+O at a prompt: sync the directory right away
            if let Some(dir) = inject.take() {
                sub.feed_line(&format!("cd {}", shell_quote(&dir.to_string_lossy())));
            }
        }
        Some(SubshellSession {
            start_wait: pending_cmd.is_some().then(Instant::now),
            pending_cmd,
            inject,
            cd_wait: None,
            awaiting: false,
            finished: false,
        })
    }

    /// One pass of a session: drain the pty, advance the waiting, and
    /// hand back whatever the shell wrote. The caller supplies the
    /// waiting - a `poll` in the terminal, egui's frame timer in the
    /// window - and passes keystrokes on with [`Subshell::feed`].
    pub fn step_subshell(&mut self, session: &mut SubshellSession) -> SubshellStep {
        let Some(sub) = self.subshell.as_mut() else {
            return SubshellStep::Done;
        };
        // a terminal answers the shell's queries itself once the
        // session is on its screen; a window's parser never will
        sub.pump(!self.subshell_headless);
        let mut bytes = sub.take_output();
        if sub.failed {
            session.finished = true;
            return finish(session, bytes);
        }
        if let Some(since) = session.start_wait {
            if sub.ready() {
                session.start_wait = None;
                if let Some(dir) = session.inject.take() {
                    sub.feed_line(&format!("cd {}", shell_quote(&dir.to_string_lossy())));
                    session.cd_wait = Some(Instant::now());
                } else if let Some(cmd) = session.pending_cmd.take() {
                    sub.feed_line(&cmd);
                    session.awaiting = true;
                }
            } else if since.elapsed() > Duration::from_secs(30) {
                // cold-start compinit can take a long while; the
                // user watches the shell boot and can Ctrl+O out
                session.start_wait = None;
                session.pending_cmd = None;
                bytes.extend_from_slice(
                    b"\r\n[rcmd: the shell never reached a prompt - command not sent]\r\n",
                );
                return SubshellStep::Output(bytes);
            }
        }
        if let Some(since) = session.cd_wait
            && (sub.ready() || since.elapsed() > Duration::from_secs(2))
        {
            session.cd_wait = None;
            if let Some(cmd) = session.pending_cmd.take() {
                sub.feed_line(&cmd);
                session.awaiting = true;
            }
        }
        if session.awaiting && session.cd_wait.is_none() && sub.ready() {
            // the command finished - back to the panels, like MC
            session.finished = true;
        }
        finish(session, bytes)
    }

    /// Close a session: the shell moved, so the active panel follows,
    /// and both panels are reloaded because a command may have changed
    /// anything.
    pub fn end_subshell(&mut self) {
        if let Some(sub) = self.subshell.as_mut() {
            if sub.failed {
                self.status = sub.note.take();
                self.subshell = None;
            } else {
                let _ = sub.note.take(); // it was visible on the output screen
                let cwd = sub.cwd();
                sub.debug("session exit");
                sub.agreed = cwd.clone();
                // the shell moved -> the active panel follows
                let panel = &mut self.panels[self.active];
                if panel.is_local() && panel.cwd != cwd && cwd.is_dir() {
                    let _ = panel.cd(cwd);
                }
            }
        }
        for panel in &mut self.panels {
            let _ = panel.refresh();
        }
        self.git_refresh();
        self.dirty = true;
    }

    /// Whatever the shell has written and nobody has taken yet. The
    /// terminal build replays this onto the output screen when Ctrl+O
    /// leaves the alternate one; a window feeds it to its parser, so
    /// the pane opens on a screen that already has the prompt on it.
    pub fn take_subshell_output(&mut self) -> Vec<u8> {
        self.subshell
            .as_mut()
            .map(Subshell::take_output)
            .unwrap_or_default()
    }

    /// There is a subshell and it has not given up.
    pub fn subshell_alive(&self) -> bool {
        self.subshell.as_ref().is_some_and(|sub| !sub.failed)
    }

    /// Hand keystrokes to the shell.
    pub fn feed_subshell(&mut self, bytes: &[u8]) {
        if let Some(sub) = self.subshell.as_mut() {
            sub.feed(bytes);
        }
    }

    /// The pty's read end and the prompt-hook pipe, for a front end that
    /// wants to `poll` on them rather than spin.
    pub fn subshell_fds(&self) -> Option<(libc::c_int, libc::c_int)> {
        self.subshell
            .as_ref()
            .map(|sub| (sub.master_fd(), sub.pipe_fd()))
    }

    /// Ctrl+O or a typed command while the persistent subshell lives:
    /// leave the alternate screen (the shell owns the primary one - its
    /// scrollback IS MC's "output screen") and pass keys through raw
    /// until Ctrl+O comes back or the fed command finishes.
    pub(super) fn subshell_session(
        &mut self,
        terminal: &mut DefaultTerminal,
        exec: Exec,
    ) -> Result<()> {
        use std::io::Write as _;

        let Some(mut session) = self.begin_subshell(exec) else {
            return Ok(());
        };

        if self.config.mouse {
            set_mouse_capture(false);
        }
        set_bracketed_paste(false);
        let mut out = std::io::stdout();
        ratatui::crossterm::execute!(out, LeaveAlternateScreen, cursor::Show)?;
        // whatever the shell wrote while it was hidden, replayed
        if let Some(sub) = self.subshell.as_mut() {
            out.write_all(&sub.take_output())?;
            out.flush()?;
        }

        let mut size = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
        self.resize_subshell(size.0, size.1); // no-op unless it drifted
        loop {
            match self.step_subshell(&mut session) {
                SubshellStep::Output(bytes) => {
                    out.write_all(&bytes)?;
                    out.flush()?;
                }
                SubshellStep::Waiting => {}
                SubshellStep::Done => break,
            }
            let Some((master, pipe)) = self.subshell_fds() else {
                break;
            };
            let mut fds = [
                libc::pollfd {
                    fd: 0,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: master,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: pipe,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            unsafe { libc::poll(fds.as_mut_ptr(), 3, 100) };
            if fds[0].revents & libc::POLLIN != 0 {
                let mut keys = [0u8; 1024];
                let n = unsafe { libc::read(0, keys.as_mut_ptr().cast(), keys.len()) };
                if n > 0 {
                    let keys = &keys[..n as usize];
                    // Ctrl+O never reaches the shell (MC-compatible -
                    // yes, that shadows nano's save inside the subshell)
                    if let Some(pos) = keys.iter().position(|&b| b == 0x0F) {
                        self.feed_subshell(&keys[..pos]);
                        break;
                    }
                    self.feed_subshell(keys);
                }
            }
            let now = ratatui::crossterm::terminal::size().unwrap_or(size);
            if now != size {
                size = now;
                self.resize_subshell(now.0, now.1);
            }
        }

        ratatui::crossterm::execute!(out, EnterAlternateScreen)?;
        if self.config.mouse {
            set_mouse_capture(true);
        }
        set_bracketed_paste(true);
        // whatever ran may have set a title of its own
        self.title_shown = None;
        let _ = terminal.clear();
        self.end_subshell();
        Ok(())
    }
}
