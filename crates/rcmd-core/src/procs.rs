//! The running processes as a panel: `proc://` lists one row a process,
//! read from `/proc`, with its user, its share of a CPU, its resident
//! memory (as the size) and when it started (as the date). F3 reads its
//! command line and environment, and [`ProcFs::signal`] is what the
//! panel's F8 sends. Linux only: elsewhere there is no `/proc` to read,
//! and the listing says so.
//!
//! A process is named `COMM PID` - the kernel's short name for it, then
//! its number, since two processes share a name as often as not. A `/`
//! in the short name (a kernel thread's `kworker/0:1`) is written `_`,
//! so the name stays one path component.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use crate::entry::{Entry, EntryKind, EntryStat};
use crate::vfs::{FsProvider, RemoteFs};

pub const PREFIX: &str = "proc://";

/// The shortest time the CPU shares are measured over. A listing right
/// after the one before waits out the rest, so a quick reload shows a
/// share rather than noise.
const MIN_SPAN: Duration = Duration::from_millis(250);

/// What `/proc/PID/stat` says that the panel wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    pub comm: String,
    pub state: char,
    pub ppid: u32,
    /// Time spent on a CPU, user and system, in clock ticks.
    pub ticks: u64,
    pub threads: u64,
    /// Ticks after boot.
    pub start: u64,
    pub rss_pages: u64,
}

/// Parse a `/proc/PID/stat` line. The short name sits in parentheses and
/// may hold spaces and parentheses of its own, so the fields after it
/// are counted from the last `)`.
pub fn parse_stat(text: &str) -> Option<Stat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let comm = text.get(open + 1..close)?.to_string();
    // fields from 3 on (state), numbered as proc(5) numbers them
    let rest: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();
    let field = |n: usize| rest.get(n - 3).copied();
    let num = |n: usize| field(n)?.parse::<u64>().ok();
    Some(Stat {
        comm,
        state: field(3)?.chars().next()?,
        ppid: num(4)? as u32,
        ticks: num(14)? + num(15)?,
        threads: num(20)?,
        start: num(22)?,
        rss_pages: num(24)?,
    })
}

/// The name a process has in the panel.
pub fn name_of(comm: &str, pid: u32) -> OsString {
    OsString::from(format!("{} {pid}", comm.replace('/', "_")))
}

/// The number at the end of a name the panel shows.
pub fn pid_of(name: &std::ffi::OsStr) -> Option<u32> {
    name.to_str()?.rsplit_once(' ')?.1.parse().ok()
}

/// One process as the last listing saw it.
#[derive(Debug, Clone)]
struct Seen {
    ticks: u64,
    start: u64,
    command: String,
}

/// A process found by a scan, and the owner of its `/proc` directory.
struct Found {
    pid: u32,
    stat: Stat,
    ids: Option<(u32, u32)>,
}

#[derive(Default)]
struct Memory {
    seen: HashMap<u32, Seen>,
    at: Option<Instant>,
}

/// `/proc` as a filesystem of one directory.
pub struct ProcFs {
    root: PathBuf,
    memory: Mutex<Memory>,
}

impl ProcFs {
    pub fn new() -> Self {
        Self::with_root("/proc")
    }

    /// A `/proc` somewhere else - a test's own.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        ProcFs {
            root: root.into(),
            memory: Mutex::new(Memory::default()),
        }
    }

    fn memory(&self) -> std::sync::MutexGuard<'_, Memory> {
        self.memory.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Every process there is now, with what its row needs.
    fn scan(&self) -> io::Result<Vec<Found>> {
        let dir = fs::read_dir(&self.root).map_err(|err| match err.kind() {
            io::ErrorKind::NotFound => io::Error::new(
                io::ErrorKind::NotFound,
                "no /proc here: the process panel is Linux only",
            ),
            _ => err,
        })?;
        let mut out = Vec::new();
        for item in dir.flatten() {
            let Some(pid) = item.file_name().to_str().and_then(|n| n.parse().ok()) else {
                continue;
            };
            // a process that ended since the directory was read is gone
            // from the listing, not an error in it
            let Some(stat) = self.stat_of(pid) else {
                continue;
            };
            let ids = item.metadata().ok().map(|m| (m.uid(), m.gid()));
            out.push(Found { pid, stat, ids });
        }
        Ok(out)
    }

    fn stat_of(&self, pid: u32) -> Option<Stat> {
        let text = fs::read_to_string(self.root.join(pid.to_string()).join("stat")).ok()?;
        parse_stat(&text)
    }

    /// The command line, arguments split by spaces; a kernel thread has
    /// none and is shown by its name in brackets, as ps does.
    fn command(&self, pid: u32, comm: &str) -> String {
        let raw = read_small(&self.root.join(pid.to_string()).join("cmdline")).unwrap_or_default();
        let args: Vec<String> = raw
            .split(|&b| b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        match args.is_empty() {
            true => format!("[{comm}]"),
            false => args.join(" "),
        }
    }

    /// The processes, each with its CPU share since the listing before -
    /// or, for the first listing, over a short wait taken now.
    fn list(&self) -> io::Result<Vec<Entry>> {
        let mut procs = self.scan()?;
        let (mut before, mut at) = {
            let memory = self.memory();
            let before: HashMap<u32, u64> =
                memory.seen.iter().map(|(pid, s)| (*pid, s.ticks)).collect();
            (before, memory.at)
        };
        let waited = at.map(|at| at.elapsed()).unwrap_or_default();
        if at.is_none() || waited < MIN_SPAN {
            if at.is_none() {
                before = procs.iter().map(|p| (p.pid, p.stat.ticks)).collect();
                at = Some(Instant::now());
            }
            std::thread::sleep(MIN_SPAN.saturating_sub(waited));
            procs = self.scan()?;
        }
        let span = at.map(|at| at.elapsed()).unwrap_or(MIN_SPAN).as_secs_f64();
        let hz = clock_ticks() as f64;
        let boot = boot_time(&self.root);
        let page = page_size();

        let mut seen = HashMap::with_capacity(procs.len());
        let entries = procs
            .into_iter()
            .map(|Found { pid, stat, ids }| {
                // a pid not seen before, or reused since, counts from
                // nothing - which it would have, being new
                let used = before
                    .get(&pid)
                    .map(|&b| stat.ticks.saturating_sub(b))
                    .unwrap_or(0);
                let cpu = (used as f64 / hz / span * 1000.0).round() as u32;
                let command = self.command(pid, &stat.comm);
                let entry = Entry {
                    name: name_of(&stat.comm, pid),
                    kind: EntryKind::File,
                    size: stat.rss_pages * page,
                    mtime: boot.map(|b| b + Duration::from_secs_f64(stat.start as f64 / hz)),
                    mode: 0o444,
                    link_target: None,
                    extra: EntryStat {
                        uid: ids.map(|i| i.0),
                        gid: ids.map(|i| i.1),
                        cpu: Some(cpu),
                        ..Default::default()
                    },
                };
                seen.insert(
                    pid,
                    Seen {
                        ticks: stat.ticks,
                        start: stat.start,
                        command,
                    },
                );
                entry
            })
            .collect();
        *self.memory() = Memory {
            seen,
            at: Some(Instant::now()),
        };
        Ok(entries)
    }

    /// The pid a path in the panel names, if it is the process the last
    /// listing showed under that name: a pid used again since by
    /// something else is refused, not signalled.
    fn resolve(&self, path: &Path) -> io::Result<u32> {
        let gone = || {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is not running", path.display()),
            )
        };
        let mut parts = path
            .components()
            .filter(|c| matches!(c, Component::Normal(_)));
        let (Some(Component::Normal(name)), None) = (parts.next(), parts.next()) else {
            return Err(gone());
        };
        let pid = pid_of(name).ok_or_else(gone)?;
        let stat = self.stat_of(pid).ok_or_else(gone)?;
        if name_of(&stat.comm, pid) != name {
            return Err(gone());
        }
        let known = self.memory().seen.get(&pid).map(|s| s.start);
        if known.is_some_and(|start| start != stat.start) {
            return Err(gone());
        }
        Ok(pid)
    }

    /// Send `signal` to the process `path` names.
    pub fn signal(&self, path: &Path, signal: i32) -> io::Result<()> {
        let pid = self.resolve(path)?;
        let pid = libc::pid_t::try_from(pid).map_err(io::Error::other)?;
        match unsafe { libc::kill(pid, signal) } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }

    /// Whether the process `path` names has ended: gone, or a zombie
    /// only its parent's wait still holds - or another process now,
    /// under the pid it had.
    pub fn has_ended(&self, path: &Path) -> bool {
        match self.resolve(path) {
            Ok(pid) => self.stat_of(pid).is_none_or(|s| s.state == 'Z'),
            Err(_) => true,
        }
    }

    /// What F3 shows: who the process is, how it was started, where it
    /// runs, and its environment where that may be read.
    fn describe(&self, pid: u32) -> io::Result<String> {
        let stat = self.stat_of(pid).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("process {pid} has ended"))
        })?;
        let dir = self.root.join(pid.to_string());
        let link = |name: &str| match fs::read_link(dir.join(name)) {
            Ok(target) => target.display().to_string(),
            Err(err) => format!("({})", reason(&err)),
        };
        let user = fs::metadata(&dir)
            .map(|m| crate::users::user_name(m.uid()))
            .unwrap_or_default();
        let mut out = format!(
            "{} {pid}\n\ncommand:    {}\nexecutable: {}\ndirectory:  {}\nuser:       {user}\nparent:     {}\nstate:      {}\nthreads:    {}\nresident:   {} bytes\n\nenvironment:\n",
            stat.comm,
            self.command(pid, &stat.comm),
            link("exe"),
            link("cwd"),
            stat.ppid,
            state_name(stat.state),
            stat.threads,
            stat.rss_pages * page_size(),
        );
        match read_small(&dir.join("environ")) {
            Ok(raw) => {
                for var in raw.split(|&b| b == 0).filter(|v| !v.is_empty()) {
                    out.push_str(&String::from_utf8_lossy(var));
                    out.push('\n');
                }
            }
            Err(err) => out.push_str(&format!("({})\n", reason(&err))),
        }
        Ok(out)
    }
}

impl Default for ProcFs {
    fn default() -> Self {
        Self::new()
    }
}

fn reason(err: &io::Error) -> String {
    match err.kind() {
        io::ErrorKind::PermissionDenied => "not readable: another user's process".into(),
        _ => err.to_string(),
    }
}

fn state_name(state: char) -> String {
    let word = match state {
        'R' => "running",
        'S' => "sleeping",
        'D' => "waiting on I/O",
        'Z' => "zombie",
        'T' => "stopped",
        't' => "stopped by a debugger",
        'I' => "idle",
        'X' => "dead",
        _ => "",
    };
    format!("{state} {word}").trim_end().to_string()
}

/// A `/proc` file, which says it is empty and is not: read to the end,
/// up to a limit no command line or environment reaches.
fn read_small(path: &Path) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    fs::File::open(path)?
        .take(1024 * 1024)
        .read_to_end(&mut out)?;
    Ok(out)
}

fn clock_ticks() -> u64 {
    match unsafe { libc::sysconf(libc::_SC_CLK_TCK) } {
        n if n > 0 => n as u64,
        _ => 100,
    }
}

fn page_size() -> u64 {
    match unsafe { libc::sysconf(libc::_SC_PAGESIZE) } {
        n if n > 0 => n as u64,
        _ => 4096,
    }
}

/// When the machine booted, from `btime` in `/proc/stat`.
fn boot_time(root: &Path) -> Option<SystemTime> {
    let text = fs::read_to_string(root.join("stat")).ok()?;
    let secs = text
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

fn is_top(path: &Path) -> bool {
    !path.components().any(|c| matches!(c, Component::Normal(_)))
}

fn top_entry() -> Entry {
    Entry {
        name: OsString::from("/"),
        kind: EntryKind::Dir,
        size: 0,
        mtime: None,
        mode: 0o555,
        link_target: None,
        extra: Default::default(),
    }
}

impl FsProvider for ProcFs {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        match is_top(dir) {
            true => self.list(),
            false => Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "a process is not a directory",
            )),
        }
    }

    fn stat(&self, path: &Path) -> io::Result<Entry> {
        if is_top(path) {
            return Ok(top_entry());
        }
        let pid = self.resolve(path)?;
        let stat = self.stat_of(pid).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("process {pid} has ended"))
        })?;
        Ok(Entry {
            name: name_of(&stat.comm, pid),
            kind: EntryKind::File,
            size: stat.rss_pages * page_size(),
            mtime: None,
            mode: 0o444,
            link_target: None,
            extra: Default::default(),
        })
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let pid = self.resolve(path)?;
        Ok(Box::new(io::Cursor::new(self.describe(pid)?.into_bytes())))
    }

    /// The command line, from the last listing.
    fn note(&self, path: &Path) -> Option<String> {
        let pid = pid_of(path.file_name()?)?;
        self.memory().seen.get(&pid).map(|s| s.command.clone())
    }
}

impl RemoteFs for ProcFs {
    fn prefix(&self) -> &str {
        PREFIX
    }

    /// There is one directory, and everything is in it.
    fn realpath(&self, _path: &Path) -> io::Result<PathBuf> {
        Ok(PathBuf::from("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stat_line_is_read_past_a_name_with_brackets_and_spaces() {
        let line = "4242 (tmux: (server) x) S 1 4242 4242 0 -1 4194560 900 0 0 0 \
                    120 30 0 0 20 0 3 0 5000 123456 789 18446744073709551615";
        let stat = parse_stat(line).unwrap();
        assert_eq!(stat.comm, "tmux: (server) x");
        assert_eq!(stat.state, 'S');
        assert_eq!(stat.ppid, 1);
        assert_eq!(stat.ticks, 150);
        assert_eq!(stat.threads, 3);
        assert_eq!(stat.start, 5000);
        assert_eq!(stat.rss_pages, 789);
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn a_name_carries_its_pid_and_no_slash() {
        let name = name_of("kworker/0:1H", 17);
        assert_eq!(name, "kworker_0:1H 17");
        assert_eq!(pid_of(&name), Some(17));
        assert_eq!(pid_of(std::ffi::OsStr::new("bash")), None);
    }

    /// A child of the test's own, listed, described and then stopped
    /// through the panel's path to it.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_process_is_listed_described_and_signalled() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .env("RCMD_PROC_TEST", "marker")
            .spawn()
            .unwrap();
        let fs = ProcFs::new();
        let entries = fs.read_dir(Path::new("/")).unwrap();
        let name = name_of("sleep", child.id());
        let entry = entries.iter().find(|e| e.name == name).expect("listed");
        assert!(entry.size > 0, "a resident size");
        assert!(entry.mtime.is_some(), "a start time");
        assert_eq!(entry.extra.uid, Some(unsafe { libc::getuid() }));
        assert!(entry.extra.cpu.is_some());

        let path = Path::new("/").join(&name);
        assert_eq!(fs.note(&path).as_deref(), Some("sleep 30"));
        let mut text = String::new();
        fs.open_read(&path)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert!(text.contains("command:    sleep 30"), "{text}");
        assert!(text.contains("RCMD_PROC_TEST=marker"), "{text}");

        // a name whose pid is some other process is not signalled
        let wrong = Path::new("/").join(name_of("bash", child.id()));
        assert!(fs.signal(&wrong, libc::SIGTERM).is_err());

        fs.signal(&path, libc::SIGTERM).unwrap();
        let status = child.wait().unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGTERM));
        assert!(fs.signal(&path, libc::SIGTERM).is_err(), "gone now");
    }

    #[test]
    fn no_proc_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = ProcFs::with_root(tmp.path().join("none"));
        let err = fs.read_dir(Path::new("/")).unwrap_err();
        assert!(err.to_string().contains("Linux only"), "{err}");
    }
}
