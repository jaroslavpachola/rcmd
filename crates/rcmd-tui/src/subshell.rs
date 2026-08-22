//! The persistent subshell (PLAN3 R1): a long-lived `$SHELL` on its own
//! pty, spawned at startup. Ctrl+O toggles to its screen, typed commands
//! run inside it, and the panels follow its working directory.
//!
//! Mechanics (decision E2): bash, zsh and fish get a prompt hook that
//! writes `pwd` to an inherited pipe on fd 27 - each message doubles as
//! "the prompt is idle" detection. Shells without a precmd mechanism
//! (sh, dash, …) fall back to `/proc/<pid>/cwd` plus the pty's
//! foreground process group and output quiescence. The pty layer is
//! hand-rolled over `libc::openpty` (decision E1) - Linux-only, like
//! the rest of the terminal handling.
//!
//! While the subshell is hidden its output is buffered (replayed on the
//! next Ctrl+O) and a tiny shim answers the terminal queries that every
//! real terminal must answer (DA1, DSR) - fish blocks at startup
//! waiting for those.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// The fd number the prompt hook writes to inside the shell. High
/// enough not to collide with scripted redirections (0–9).
const CTL_FD: libc::c_int = 27;
/// Keep at most this much hidden output for replay.
const BUF_CAP: usize = 1024 * 1024;
/// Plain shells: how long the pty must stay quiet to count as idle.
const QUIET: Duration = Duration::from_millis(100);
/// Plain shells: minimum time after a feed before "idle" is believed
/// (covers the window before the command's process group takes over).
const FED_GUARD: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Bash,
    Zsh,
    Fish,
    /// No precmd mechanism - /proc + foreground-pgroup fallback.
    Plain,
}

impl Kind {
    fn of(shell: &str) -> Kind {
        let base = Path::new(shell)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match base.as_str() {
            "bash" => Kind::Bash,
            "zsh" => Kind::Zsh,
            "fish" => Kind::Fish,
            _ => Kind::Plain,
        }
    }
}

pub struct Subshell {
    kind: Kind,
    shell: String,
    master: OwnedFd,
    child: Child,
    pipe_r: OwnedFd,
    /// Temp dir holding the injected rc files (bash/zsh), removed on drop.
    rcdir: Option<PathBuf>,
    /// Last cwd the shell reported (hook shells) or was seen at.
    cwd: PathBuf,
    /// The directory panels and subshell last agreed on; whoever moved
    /// away from it since is the one to sync from.
    pub agreed: PathBuf,
    /// A prompt message arrived since the last feed (hook shells).
    prompt_seen: bool,
    /// The shell reached a prompt at least once - distinguishes "still
    /// starting up" (worth waiting for) from "busy with a command".
    ever_ready: bool,
    last_output: Instant,
    fed_at: Option<Instant>,
    /// Output collected while the subshell screen is hidden.
    buf: Vec<u8>,
    /// Partial hook message (a cwd line split across reads).
    pipe_acc: Vec<u8>,
    /// Tail of the previous chunk, for query patterns split across reads.
    carry: Vec<u8>,
    /// One-shot status-line note (respawn and the like).
    pub note: Option<String>,
    /// The shell died and could not be respawned; fall back to plain exec.
    pub failed: bool,
    size: (u16, u16),
}

impl Subshell {
    /// Spawn `$SHELL` on a fresh pty in `dir`. Any failure here means
    /// "no subshell this session" - the caller falls back to plain exec.
    pub fn spawn(dir: &Path, cols: u16, rows: u16) -> Result<Subshell> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        Subshell::spawn_shell(&shell, dir, cols, rows)
    }

    fn spawn_shell(shell: &str, dir: &Path, cols: u16, rows: u16) -> Result<Subshell> {
        // a 0×0 terminal (size not yet negotiated) breaks readline
        let (cols, rows) = if cols == 0 || rows == 0 {
            (80, 24)
        } else {
            (cols, rows)
        };
        let kind = Kind::of(shell);
        let (rcdir, mut command) = build_command(shell, kind)?;
        let (master, slave) = open_pty(cols, rows)?;
        let (pipe_r, pipe_w) = control_pipe()?;

        command.current_dir(dir).env("RCMD_SUBSHELL", "1");
        let slave: std::fs::File = slave.into();
        command
            .stdin(slave.try_clone().context("dup pty slave")?)
            .stdout(slave.try_clone().context("dup pty slave")?)
            .stderr(slave);
        let ctl = pipe_w.as_raw_fd();
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // hand the hook channel to the shell on a fixed fd;
                // dup2 clears CLOEXEC - except in the equal-fd special
                // case, where it is a no-op and the flag must go by hand
                if ctl == CTL_FD {
                    if libc::fcntl(CTL_FD, libc::F_SETFD, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                } else if libc::dup2(ctl, CTL_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().with_context(|| format!("spawn {shell}"))?;
        drop(pipe_w);

        let mut sub = Subshell {
            kind,
            shell: shell.to_string(),
            master,
            child,
            pipe_r,
            rcdir,
            cwd: dir.to_path_buf(),
            agreed: dir.to_path_buf(),
            prompt_seen: false,
            ever_ready: false,
            last_output: Instant::now(),
            fed_at: None,
            buf: Vec::new(),
            pipe_acc: Vec::new(),
            carry: Vec::new(),
            note: None,
            failed: false,
            size: (cols, rows),
        };
        sub.debug(&format!(
            "spawned: master={} pipe_r={} rcdir={:?} size={:?}",
            sub.master.as_raw_fd(),
            sub.pipe_r.as_raw_fd(),
            sub.rcdir,
            sub.size,
        ));
        Ok(sub)
    }

    pub fn master_fd(&self) -> libc::c_int {
        self.master.as_raw_fd()
    }

    pub fn pipe_fd(&self) -> libc::c_int {
        self.pipe_r.as_raw_fd()
    }

    /// Drain the pty and the hook channel; respawn the shell if it
    /// exited. With `visible` false, terminal queries in the output get
    /// answered by the shim (nobody else will).
    pub fn pump(&mut self, visible: bool) {
        if self.failed {
            return;
        }
        if let Ok(Some(_)) = self.child.try_wait() {
            self.debug("child exited - respawning");
            self.respawn();
            return;
        }
        let mut chunk = [0u8; 4096];
        loop {
            let n = unsafe {
                libc::read(
                    self.master.as_raw_fd(),
                    chunk.as_mut_ptr().cast(),
                    chunk.len(),
                )
            };
            if n <= 0 {
                break; // EAGAIN, EOF or EIO (exit is caught by try_wait)
            }
            let chunk = &chunk[..n as usize];
            self.last_output = Instant::now();
            if !visible {
                self.answer_queries(chunk);
            }
            self.buf.extend_from_slice(chunk);
        }
        if self.buf.len() > BUF_CAP {
            let cut = self.buf.len() - BUF_CAP / 2;
            self.buf.drain(..cut);
        }
        loop {
            let n = unsafe {
                libc::read(
                    self.pipe_r.as_raw_fd(),
                    chunk.as_mut_ptr().cast(),
                    chunk.len(),
                )
            };
            if n <= 0 {
                break;
            }
            self.pipe_acc.extend_from_slice(&chunk[..n as usize]);
        }
        while let Some(pos) = self.pipe_acc.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pipe_acc.drain(..=pos).collect();
            let line = &line[..line.len() - 1];
            if !line.is_empty() {
                self.cwd = PathBuf::from(std::ffi::OsStr::from_bytes(line));
            }
            self.prompt_seen = true;
            self.debug("prompt message");
        }
        if !self.ever_ready && self.ready() {
            self.ever_ready = true;
        }
    }

    /// Still waiting for the shell's very first prompt (slow rc files,
    /// compinit, …)? Worth waiting for, unlike a running command.
    pub fn starting(&self) -> bool {
        !self.ever_ready && !self.failed
    }

    /// The buffered output (screen bytes) collected since the last take.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buf)
    }

    /// Is the shell sitting at its prompt, safe to inject into?
    pub fn ready(&self) -> bool {
        if self.failed {
            return false;
        }
        match self.kind {
            Kind::Plain => {
                self.fg_is_shell()
                    && self.last_output.elapsed() >= QUIET
                    && self.fed_at.is_none_or(|t| t.elapsed() >= FED_GUARD)
            }
            _ => self.prompt_seen,
        }
    }

    /// Last known working directory of the shell.
    pub fn cwd(&mut self) -> PathBuf {
        if self.kind == Kind::Plain
            && let Ok(cwd) = std::fs::read_link(format!("/proc/{}/cwd", self.child.id()))
        {
            self.cwd = cwd;
        }
        self.cwd.clone()
    }

    /// Temporary R1 debugging: RCMD_SUBSHELL_LOG=/path appends events.
    pub fn debug(&mut self, event: &str) {
        if let Ok(path) = std::env::var("RCMD_SUBSHELL_LOG")
            && let Ok(mut f) = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(path)
        {
            use std::io::Write as _;
            let _ = writeln!(
                f,
                "[{:?} pid={} ready={} prompt_seen={} cwd={} agreed={}] {event}",
                self.kind,
                self.child.id(),
                self.ready(),
                self.prompt_seen,
                self.cwd.display(),
                self.agreed.display(),
            );
        }
    }

    /// Type a line at the shell's prompt: ^U first (clears anything the
    /// user left half-typed), a leading space (history-friendly), then
    /// the line and Enter.
    pub fn feed_line(&mut self, line: &str) {
        self.prompt_seen = false;
        self.fed_at = Some(Instant::now());
        let mut bytes = Vec::with_capacity(line.len() + 3);
        bytes.push(0x15); // ^U
        bytes.push(b' ');
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        self.feed(&bytes);
        self.debug(&format!("feed_line: {line}"));
    }

    /// Raw keyboard passthrough into the shell's pty.
    pub fn feed(&mut self, bytes: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut off = 0;
        while off < bytes.len() && Instant::now() < deadline {
            let n = unsafe {
                libc::write(
                    self.master.as_raw_fd(),
                    bytes[off..].as_ptr().cast(),
                    bytes.len() - off,
                )
            };
            if n > 0 {
                off += n as usize;
            } else if std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
                std::thread::sleep(Duration::from_millis(5));
            } else {
                break;
            }
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        if (cols, rows) == self.size {
            return;
        }
        self.size = (cols, rows);
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws);
        }
    }

    /// `exit` (or a crash) killed the shell: start a fresh one in its
    /// last directory, like MC.
    fn respawn(&mut self) {
        let dir = if self.cwd.is_dir() {
            self.cwd.clone()
        } else {
            PathBuf::from("/")
        };
        match Subshell::spawn_shell(&self.shell, &dir, self.size.0, self.size.1) {
            Ok(fresh) => {
                // the fresh shell reuses the same rc dir - keep the old
                // self's Drop (runs on assignment) from deleting it
                self.rcdir = None;
                let old_buf = std::mem::take(&mut self.buf);
                let note = "[rcmd: the subshell exited - respawned]";
                *self = fresh;
                self.buf = old_buf;
                self.buf
                    .extend_from_slice(format!("\r\n{note}\r\n").as_bytes());
                self.note = Some(" the subshell exited - respawned ".into());
            }
            Err(err) => {
                self.failed = true;
                self.note = Some(format!(" subshell lost ({err}) - using plain exec "));
            }
        }
    }

    /// Foreground process group of the pty == the shell itself?
    fn fg_is_shell(&self) -> bool {
        let mut pgrp: libc::pid_t = 0;
        let rc = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCGPGRP, &raw mut pgrp) };
        rc == 0 && pgrp == self.child.id() as libc::pid_t
    }

    /// Answer the terminal queries a hidden program blocks on. Every
    /// real terminal answers DA1 - programs use it as the "no more
    /// replies coming" fence for capability probing (fish does at
    /// startup) - and DSR/CPR.
    fn answer_queries(&mut self, chunk: &[u8]) {
        let mut data = std::mem::take(&mut self.carry);
        let watermark = data.len();
        data.extend_from_slice(chunk);
        let mut reply = Vec::new();
        for (pat, ans) in [
            (&b"\x1b[c"[..], &b"\x1b[?6c"[..]),
            (b"\x1b[0c", b"\x1b[?6c"),
            (b"\x1b[5n", b"\x1b[0n"),
            (b"\x1b[6n", b"\x1b[1;1R"),
        ] {
            for (i, w) in data.windows(pat.len()).enumerate() {
                // only matches that end in the new bytes; the rest were
                // answered on a previous call
                if w == pat && i + pat.len() > watermark {
                    reply.extend_from_slice(ans);
                }
            }
        }
        if !reply.is_empty() {
            self.feed(&reply);
        }
        let keep = data.len().min(3);
        self.carry = data[data.len() - keep..].to_vec();
    }
}

impl Drop for Subshell {
    fn drop(&mut self) {
        // an already-reaped child means the pid may be recycled - don't
        // signal it (try_wait returns the cached status without waiting)
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            unsafe {
                libc::kill(self.child.id() as libc::pid_t, libc::SIGHUP);
            }
            let deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < deadline {
                if matches!(self.child.try_wait(), Ok(Some(_)) | Err(_)) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if let Some(dir) = &self.rcdir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// The shell command line plus any rc scaffolding it needs so that the
/// user's own startup files still run before our prompt hook lands.
fn build_command(shell: &str, kind: Kind) -> Result<(Option<PathBuf>, Command)> {
    let mut command = Command::new(shell);
    let rcdir = match kind {
        Kind::Bash => {
            let dir = rc_dir()?;
            let rc = dir.join("bashrc");
            std::fs::write(
                &rc,
                format!(
                    "[ -r ~/.bashrc ] && . ~/.bashrc\n\
                     __rcmd_cwd() {{ builtin pwd >&{CTL_FD}; }}\n\
                     PROMPT_COMMAND=\"__rcmd_cwd${{PROMPT_COMMAND:+;$PROMPT_COMMAND}}\"\n"
                ),
            )?;
            command.arg("--rcfile").arg(rc);
            Some(dir)
        }
        Kind::Zsh => {
            let dir = rc_dir()?;
            // the user's .zshenv must run in the env phase (before
            // /etc/zsh/zshrc - skip_global_compinit and friends), with
            // ZDOTDIR restored afterwards so the rc phase finds our stub
            std::fs::write(
                dir.join(".zshenv"),
                "_rcmd_zdotdir=\"$ZDOTDIR\"; ZDOTDIR=\"$HOME\"\n\
                 [[ -r ~/.zshenv ]] && source ~/.zshenv\n\
                 ZDOTDIR=\"$_rcmd_zdotdir\"; unset _rcmd_zdotdir\n",
            )?;
            std::fs::write(
                dir.join(".zshrc"),
                format!(
                    "ZDOTDIR=\"$HOME\"\n\
                     [[ -r ~/.zshrc ]] && source ~/.zshrc\n\
                     __rcmd_cwd() {{ builtin pwd >&{CTL_FD}; }}\n\
                     precmd_functions+=(__rcmd_cwd)\n"
                ),
            )?;
            command.env("ZDOTDIR", &dir);
            Some(dir)
        }
        Kind::Fish => {
            // fish redirects can't name arbitrary fds; /proc can
            command.arg("-C").arg(format!(
                "function __rcmd_cwd --on-event fish_prompt; \
                 builtin pwd > /proc/self/fd/{CTL_FD}; end"
            ));
            None
        }
        Kind::Plain => None,
    };
    Ok((rcdir, command))
}

fn rc_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("rcmd-subshell-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn open_pty(cols: u16, rows: u16) -> Result<(OwnedFd, OwnedFd)> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let (mut master, mut slave) = (0, 0);
    let rc = unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &raw const ws,
        )
    };
    if rc != 0 {
        bail!("openpty: {}", std::io::Error::last_os_error());
    }
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    set_nonblocking(&master)?;
    // don't leak the pty into the shell or F5/F6 job children (the
    // shell's stdio comes from dup2, which clears the flag)
    set_cloexec(&master)?;
    set_cloexec(&slave)?;
    Ok((master, slave))
}

fn control_pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        bail!("pipe2: {}", std::io::Error::last_os_error());
    }
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    set_nonblocking(&r)?;
    Ok((r, w))
}

fn set_nonblocking(fd: &OwnedFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        bail!("fcntl: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

fn set_cloexec(fd: &OwnedFd) -> Result<()> {
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        bail!("fcntl: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// Debug label for the status line / tests.
impl std::fmt::Debug for Subshell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Subshell({}, {:?})", self.shell, self.kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_until(
        sub: &mut Subshell,
        visible: bool,
        secs: u64,
        mut done: impl FnMut(&mut Subshell) -> bool,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            sub.pump(visible);
            if done(sub) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        false
    }

    fn spawn_with(shell: &str, dir: &Path) -> Option<Subshell> {
        let Some(path) = which(shell) else {
            eprintln!("skipping: no {shell} on this machine");
            return None;
        };
        Some(Subshell::spawn_shell(&path, dir, 80, 24).expect("spawn subshell"))
    }

    fn which(name: &str) -> Option<String> {
        let path = std::env::var("PATH").ok()?;
        path.split(':')
            .map(|d| format!("{d}/{name}"))
            .find(|p| std::fs::metadata(p).is_ok())
    }

    #[test]
    fn kind_detection() {
        assert_eq!(Kind::of("/usr/bin/bash"), Kind::Bash);
        assert_eq!(Kind::of("/bin/zsh"), Kind::Zsh);
        assert_eq!(Kind::of("/usr/bin/fish"), Kind::Fish);
        assert_eq!(Kind::of("/bin/sh"), Kind::Plain);
        assert_eq!(Kind::of("/bin/dash"), Kind::Plain);
        assert_eq!(Kind::of(""), Kind::Plain);
    }

    #[test]
    fn plain_shell_prompt_and_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let Some(mut sub) = spawn_with("sh", tmp.path()) else {
            return;
        };
        assert!(
            wait_until(&mut sub, false, 10, |s| s.ready()),
            "sh never became ready"
        );
        sub.feed_line("cd /");
        assert!(
            wait_until(&mut sub, false, 10, |s| s.ready()
                && s.cwd() == Path::new("/")),
            "sh cwd did not follow cd, cwd={:?}",
            sub.cwd()
        );
    }

    #[test]
    fn bash_hook_reports_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let Some(mut sub) = spawn_with("bash", tmp.path()) else {
            return;
        };
        assert!(
            wait_until(&mut sub, false, 10, |s| s.ready()),
            "no prompt message from bash"
        );
        assert_eq!(
            sub.cwd().canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
        sub.feed_line("cd /");
        assert!(
            wait_until(&mut sub, false, 10, |s| s.ready()
                && s.cwd() == Path::new("/")),
            "bash cwd did not follow cd, cwd={:?}",
            sub.cwd()
        );
    }

    #[test]
    fn bash_busy_then_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let Some(mut sub) = spawn_with("bash", tmp.path()) else {
            return;
        };
        assert!(wait_until(&mut sub, false, 10, |s| s.ready()));
        sub.feed_line("sleep 0.6");
        assert!(!sub.ready(), "feeding must clear readiness");
        assert!(
            wait_until(&mut sub, false, 10, |s| s.ready()),
            "prompt never came back after sleep"
        );
    }

    #[test]
    fn exit_respawns() {
        let tmp = tempfile::tempdir().unwrap();
        let Some(mut sub) = spawn_with("sh", tmp.path()) else {
            return;
        };
        assert!(wait_until(&mut sub, false, 10, |s| s.ready()));
        let pid = sub.child.id();
        sub.feed_line("exit");
        assert!(
            wait_until(&mut sub, false, 10, |s| s.child.id() != pid),
            "shell was not respawned"
        );
        assert!(!sub.failed);
        assert!(sub.note.take().is_some(), "respawn should leave a note");
        assert!(
            String::from_utf8_lossy(&sub.take_output()).contains("respawned"),
            "respawn note missing from the output screen"
        );
        assert!(
            wait_until(&mut sub, false, 10, |s| s.ready()),
            "respawned shell never became ready"
        );
    }

    #[test]
    fn hidden_da1_query_is_answered() {
        let tmp = tempfile::tempdir().unwrap();
        let Some(mut sub) = spawn_with("sh", tmp.path()) else {
            return;
        };
        assert!(wait_until(&mut sub, false, 10, |s| s.ready()));
        // the shell asks like fish does at startup; the shim's answer
        // lands on the prompt's input line, where the tty echoes it
        // (ECHOCTL renders the ESC as "^[") - the typed line spells ESC
        // out as \033, so "[?6c" can only come from the shim
        sub.feed_line("printf 'Q\\033[cQ'");
        assert!(
            wait_until(&mut sub, false, 10, |s| {
                s.buf.windows(4).any(|w| w == b"[?6c")
            }),
            "DA1 was never answered: {:?}",
            String::from_utf8_lossy(&sub.take_output())
        );
    }
}
