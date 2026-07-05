use std::cmp::Ordering;
use std::collections::HashSet;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::archive::ArchiveFs;
use crate::entry::Entry;
use crate::glob::glob_match;
use crate::vfs::{FsProvider, LocalFs, is_archive_name};

/// How long a directory listing may take before the panel switches to a
/// non-blocking pending load (spinner, old listing stays visible).
const LOAD_GRACE: Duration = Duration::from_millis(100);

/// What triggered a listing request — decides where the cursor lands.
pub enum LoadKind {
    Reload,
    Enter,
    GoUp,
    Cd,
}

struct PendingLoad {
    target: PathBuf,
    kind: LoadKind,
    rx: mpsc::Receiver<io::Result<Vec<Entry>>>,
    cancel: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Ext,
    Size,
    Mtime,
}

/// Panel listing format: name-only, the classic three columns, or an
/// ls -l style long format. Pure presentation — rendering reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListMode {
    Brief,
    Full,
    Long,
}

/// One side of the two-panel view: a directory listing with a cursor and
/// per-directory marks. Pure state + logic, no rendering concerns.
pub struct Panel {
    /// Filesystem behind this panel; [`LocalFs`] normally, an
    /// [`ArchiveFs`] while browsing inside an archive, an `SftpFs`
    /// while connected to a remote host.
    pub fs: Arc<dyn FsProvider>,
    /// Path of the archive file when `fs` is an archive VFS.
    pub archive: Option<PathBuf>,
    /// `sftp://user@host[:port]` when `fs` is a remote filesystem.
    pub remote: Option<String>,
    /// Local directory, or the path *inside* the archive ("" = its root).
    pub cwd: PathBuf,
    pub entries: Vec<Entry>,
    pub cursor: usize,
    pub marked: HashSet<OsString>,
    pub sort_key: SortKey,
    pub sort_reverse: bool,
    pub show_hidden: bool,
    pub list_mode: ListMode,
    /// Glob applied to files (directories always show), MC's "filter".
    pub filter: Option<String>,
    /// When Some(label), the listing is an external result set (find /
    /// command output) instead of the directory; any reload restores the
    /// normal listing.
    pub panelized: Option<String>,
    pending: Option<PendingLoad>,
    /// Visited locations as display paths (`/dir` or `sftp://…/dir`);
    /// `history[hist_pos]` is the current one. Archive stops are not
    /// recorded — leaving the archive returns to its parent directory.
    history: Vec<String>,
    hist_pos: usize,
    /// Target index of an in-flight Alt+←/→ navigation; committed by
    /// [`Self::hist_note`] once the matching listing lands.
    hist_pending: Option<usize>,
}

impl Panel {
    pub fn new(cwd: PathBuf) -> io::Result<Self> {
        let mut panel = Panel {
            fs: Arc::new(LocalFs),
            archive: None,
            remote: None,
            cwd,
            entries: Vec::new(),
            cursor: 0,
            marked: HashSet::new(),
            sort_key: SortKey::Name,
            sort_reverse: false,
            show_hidden: true,
            list_mode: ListMode::Full,
            filter: None,
            panelized: None,
            pending: None,
            history: Vec::new(),
            hist_pos: 0,
            hist_pending: None,
        };
        panel.reload()?;
        Ok(panel)
    }

    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    pub fn cancel_pending(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.cancel.store(true, AtomicOrdering::Relaxed);
        }
    }

    /// List `target` on a worker thread. Fast results (within the grace
    /// window) apply before returning — Ok(true) — so ordinary local
    /// navigation feels synchronous. Slow filesystems leave the panel
    /// untouched with a pending load (Ok(false)), resolved later by
    /// [`poll_pending`]; typing never blocks.
    pub fn request_dir(&mut self, target: PathBuf, kind: LoadKind) -> io::Result<bool> {
        self.cancel_pending();
        let fs = self.fs.clone();
        let dir = target.clone();
        let show_hidden = self.show_hidden;
        let filter = self.filter.clone();
        let (sort_key, sort_reverse) = (self.sort_key, self.sort_reverse);
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = prepare_listing(
                &*fs,
                &dir,
                show_hidden,
                filter.as_deref(),
                sort_key,
                sort_reverse,
            );
            if !flag.load(AtomicOrdering::Relaxed) {
                let _ = tx.send(result);
            }
        });
        match rx.recv_timeout(LOAD_GRACE) {
            Ok(Ok(entries)) => {
                self.finish_load(target, entries, &kind);
                Ok(true)
            }
            Ok(Err(err)) => Err(err),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.pending = Some(PendingLoad {
                    target,
                    kind,
                    rx,
                    cancel,
                });
                Ok(false)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(io::Error::other("listing worker died"))
            }
        }
    }

    /// Applies a finished pending load. None = still loading or idle.
    pub fn poll_pending(&mut self) -> Option<io::Result<()>> {
        let pending = self.pending.as_ref()?;
        match pending.rx.try_recv() {
            Ok(Ok(entries)) => {
                let pending = self.pending.take().expect("pending load");
                self.finish_load(pending.target, entries, &pending.kind);
                Some(Ok(()))
            }
            Ok(Err(err)) => {
                self.pending = None;
                Some(Err(err))
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.pending = None;
                Some(Err(io::Error::other("listing worker died")))
            }
        }
    }

    fn finish_load(&mut self, target: PathBuf, mut entries: Vec<Entry>, kind: &LoadKind) {
        let came_from = match kind {
            LoadKind::GoUp => self.cwd.file_name().map(|n| n.to_os_string()),
            _ => None,
        };
        let keep = match kind {
            LoadKind::Reload => self.selected().map(|e| e.name.clone()),
            _ => None,
        };
        self.panelized = None;
        self.cwd = target;
        if self.cwd.parent().is_some() || self.archive.is_some() {
            entries.insert(0, Entry::parent());
        }
        match kind {
            LoadKind::Reload => {
                self.marked
                    .retain(|name| entries.iter().any(|e| &e.name == name));
                self.entries = entries;
                self.cursor = keep
                    .and_then(|name| self.entries.iter().position(|e| e.name == name))
                    .unwrap_or_else(|| self.cursor.min(self.entries.len().saturating_sub(1)));
            }
            LoadKind::GoUp => {
                self.marked.clear();
                self.entries = entries;
                self.cursor = came_from
                    .and_then(|name| self.entries.iter().position(|e| e.name == name))
                    .unwrap_or(0);
            }
            LoadKind::Enter | LoadKind::Cd => {
                self.marked.clear();
                self.entries = entries;
                self.cursor = 0;
            }
        }
        if self.archive.is_none() {
            self.hist_note();
        }
    }

    /// Record the current location in the history (dedup in place; a new
    /// stop drops any forward entries, browser-style). An in-flight
    /// back/forward target is committed instead when it matches.
    fn hist_note(&mut self) {
        let loc = self.display_path();
        if let Some(pos) = self.hist_pending.take()
            && self.history.get(pos) == Some(&loc)
        {
            self.hist_pos = pos;
            return;
        }
        if self.history.get(self.hist_pos) == Some(&loc) {
            return;
        }
        self.history.truncate(self.hist_pos + 1);
        self.history.push(loc);
        self.hist_pos = self.history.len() - 1;
    }

    /// Previous history location to navigate to, if any.
    pub fn hist_back(&mut self) -> Option<String> {
        let pos = self.hist_pos.checked_sub(1)?;
        self.hist_pending = Some(pos);
        Some(self.history[pos].clone())
    }

    /// Next history location to navigate to, if any.
    pub fn hist_forward(&mut self) -> Option<String> {
        let pos = self.hist_pos + 1;
        if pos >= self.history.len() {
            return None;
        }
        self.hist_pending = Some(pos);
        Some(self.history[pos].clone())
    }

    pub fn is_local(&self) -> bool {
        self.archive.is_none() && self.remote.is_none()
    }

    pub fn is_remote(&self) -> bool {
        self.remote.is_some()
    }

    /// Panel location for titles: `path`, `archive.zip://inside` or
    /// `sftp://user@host/path`.
    pub fn display_path(&self) -> String {
        if let Some(prefix) = &self.remote {
            return format!("{prefix}{}", self.cwd.display());
        }
        match &self.archive {
            Some(archive) => format!("{}://{}", archive.display(), self.cwd.display()),
            None => self.cwd.display().to_string(),
        }
    }

    /// The real directory to run commands in / resolve paths against:
    /// inside an archive, that is the archive's parent directory; on a
    /// remote panel there is no meaningful local directory, use home.
    pub fn local_cwd(&self) -> PathBuf {
        if self.remote.is_some() {
            return std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/"));
        }
        match &self.archive {
            Some(archive) => archive
                .parent()
                .unwrap_or_else(|| Path::new("/"))
                .to_path_buf(),
            None => self.cwd.clone(),
        }
    }

    /// Switch the panel onto an established remote connection. The first
    /// listing was fetched by the connect worker, so this never blocks
    /// and never fails.
    pub fn adopt_remote(
        &mut self,
        fs: Arc<dyn FsProvider>,
        prefix: String,
        start: PathBuf,
        raw: Vec<Entry>,
    ) {
        self.cancel_pending();
        self.fs = fs;
        self.archive = None;
        self.remote = Some(prefix);
        self.panelized = None;
        self.marked.clear();
        let mut entries = shape_listing(
            raw,
            self.show_hidden,
            self.filter.as_deref(),
            self.sort_key,
            self.sort_reverse,
        );
        self.cwd = start;
        if self.cwd.parent().is_some() {
            entries.insert(0, Entry::parent());
        }
        self.entries = entries;
        self.cursor = 0;
        self.hist_note();
    }

    /// Leave a remote connection (or an archive) for a local directory;
    /// on failure the panel stays where it was.
    pub fn to_local(&mut self, target: PathBuf) -> io::Result<()> {
        let prev_fs = std::mem::replace(&mut self.fs, Arc::new(LocalFs));
        let prev_archive = self.archive.take();
        let prev_remote = self.remote.take();
        match self.change_dir(target) {
            Ok(()) => Ok(()),
            Err(err) => {
                self.fs = prev_fs;
                self.archive = prev_archive;
                self.remote = prev_remote;
                Err(err)
            }
        }
    }

    /// Re-read the current directory, keeping the cursor on the same entry
    /// where possible and pruning marks for entries that no longer exist.
    /// On failure the previous listing is kept. Local reloads go through
    /// the non-blocking loader; archives re-index synchronously (fast,
    /// local file).
    pub fn reload(&mut self) -> io::Result<()> {
        if self.archive.is_none() {
            return self
                .request_dir(self.cwd.clone(), LoadKind::Reload)
                .map(|_| ());
        }
        self.panelized = None;
        let keep = self.selected().map(|e| e.name.clone());
        // re-index the archive so appended members (F5 into a zip) appear
        if let Some(archive) = &self.archive {
            self.fs = Arc::new(ArchiveFs::open(archive)?);
        }
        let mut entries = prepare_listing(
            &*self.fs,
            &self.cwd,
            self.show_hidden,
            self.filter.as_deref(),
            self.sort_key,
            self.sort_reverse,
        )?;
        // ".." also at an archive's root: it leads back out of the archive
        entries.insert(0, Entry::parent());
        self.marked
            .retain(|name| entries.iter().any(|e| &e.name == name));
        self.entries = entries;
        self.cursor = keep
            .and_then(|name| self.entries.iter().position(|e| e.name == name))
            .unwrap_or_else(|| self.cursor.min(self.entries.len().saturating_sub(1)));
        Ok(())
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.entries.get(self.cursor)
    }

    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if self.cursor + 1 < self.entries.len() {
            self.cursor += 1;
        }
    }

    pub fn move_top(&mut self) {
        self.cursor = 0;
    }

    pub fn move_bottom(&mut self) {
        self.cursor = self.entries.len().saturating_sub(1);
    }

    pub fn page_up(&mut self, page: usize) {
        self.cursor = self.cursor.saturating_sub(page);
    }

    pub fn page_down(&mut self, page: usize) {
        self.cursor = (self.cursor + page).min(self.entries.len().saturating_sub(1));
    }

    /// Enter the selected entry: a directory, or an archive file (which
    /// switches the panel onto the archive's VFS).
    /// Returns true if the panel changed directory.
    pub fn enter(&mut self) -> io::Result<bool> {
        let Some(entry) = self.selected() else {
            return Ok(false);
        };
        let name = entry.name.clone();
        if entry.is_parent() {
            return self.go_up();
        }
        if entry.is_dir() {
            if self.archive.is_none() {
                self.request_dir(self.cwd.join(&name), LoadKind::Enter)?;
            } else {
                self.change_dir(self.cwd.join(&name))?;
            }
            return Ok(true);
        }
        if self.is_local() && is_archive_name(&name) {
            self.enter_archive(self.cwd.join(&name))?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Go to the parent directory, placing the cursor on the directory
    /// we came from (MC behavior). At an archive's root this leaves the
    /// archive. Returns true if the panel moved.
    pub fn go_up(&mut self) -> io::Result<bool> {
        let Some(parent) = self.cwd.parent().map(Path::to_path_buf) else {
            if let Some(archive) = self.archive.clone() {
                return self.exit_archive(&archive).map(|()| true);
            }
            return Ok(false);
        };
        if self.archive.is_none() {
            self.request_dir(parent, LoadKind::GoUp)?;
            return Ok(true);
        }
        let came_from = self.cwd.file_name().map(|n| n.to_os_string());
        self.change_dir(parent)?;
        if let Some(name) = came_from
            && let Some(pos) = self.entries.iter().position(|e| e.name == name)
        {
            self.cursor = pos;
        }
        Ok(true)
    }

    fn enter_archive(&mut self, path: PathBuf) -> io::Result<()> {
        let archive_fs = Arc::new(ArchiveFs::open(&path)?);
        let prev_fs = std::mem::replace(&mut self.fs, archive_fs);
        let prev_archive = self.archive.replace(path);
        match self.change_dir(PathBuf::new()) {
            Ok(()) => Ok(()),
            Err(err) => {
                self.fs = prev_fs;
                self.archive = prev_archive;
                Err(err)
            }
        }
    }

    fn exit_archive(&mut self, archive: &Path) -> io::Result<()> {
        let parent = archive
            .parent()
            .unwrap_or_else(|| Path::new("/"))
            .to_path_buf();
        let prev_fs = std::mem::replace(&mut self.fs, Arc::new(LocalFs));
        let prev_archive = self.archive.take();
        match self.change_dir(parent) {
            Ok(()) => {
                if let Some(name) = archive.file_name()
                    && let Some(pos) = self.entries.iter().position(|e| e.name == name)
                {
                    self.cursor = pos;
                }
                Ok(())
            }
            Err(err) => {
                self.fs = prev_fs;
                self.archive = prev_archive;
                Err(err)
            }
        }
    }

    /// Change directory on the panel's current filesystem (command-line
    /// `cd`): local panels and remote panels stay on their filesystem;
    /// a panel inside an archive leaves it (cd targets are local paths).
    pub fn cd(&mut self, target: PathBuf) -> io::Result<()> {
        if self.archive.is_none() {
            return self.request_dir(target, LoadKind::Cd).map(|_| ());
        }
        self.to_local(target)
    }

    /// Switch to `target`, clearing marks (marks are per-directory); on
    /// failure (e.g. permission denied) the panel stays exactly as it was.
    fn change_dir(&mut self, target: PathBuf) -> io::Result<()> {
        let prev_cwd = std::mem::replace(&mut self.cwd, target);
        let prev_cursor = std::mem::replace(&mut self.cursor, 0);
        let prev_entries = std::mem::take(&mut self.entries);
        let prev_marked = std::mem::take(&mut self.marked);
        if let Err(err) = self.reload() {
            self.cwd = prev_cwd;
            self.cursor = prev_cursor;
            self.entries = prev_entries;
            self.marked = prev_marked;
            return Err(err);
        }
        Ok(())
    }

    /// Same sort key again toggles reverse, like MC's sort dialog.
    pub fn set_sort(&mut self, key: SortKey) {
        if self.sort_key == key {
            self.sort_reverse = !self.sort_reverse;
        } else {
            self.sort_key = key;
            self.sort_reverse = false;
        }
        self.resort();
    }

    /// Re-sort the existing listing in place, keeping the cursor on the
    /// same entry.
    pub fn resort(&mut self) {
        let keep = self.selected().map(|e| e.name.clone());
        let parent = self
            .entries
            .first()
            .is_some_and(Entry::is_parent)
            .then(|| self.entries.remove(0));
        sort_entries(&mut self.entries, self.sort_key, self.sort_reverse);
        if let Some(parent) = parent {
            self.entries.insert(0, parent);
        }
        if let Some(name) = keep
            && let Some(pos) = self.entries.iter().position(|e| e.name == name)
        {
            self.cursor = pos;
        }
    }

    pub fn toggle_hidden(&mut self) -> io::Result<()> {
        self.show_hidden = !self.show_hidden;
        self.reload()
    }

    pub fn is_marked(&self, entry: &Entry) -> bool {
        self.marked.contains(&entry.name)
    }

    /// Toggle the mark on the cursor entry and advance, like MC's Insert.
    pub fn toggle_mark(&mut self) {
        if let Some(entry) = self.selected()
            && !entry.is_parent()
        {
            let name = entry.name.clone();
            if !self.marked.remove(&name) {
                self.marked.insert(name);
            }
        }
        self.move_down();
    }

    pub fn mark_glob(&mut self, pattern: &str, mark: bool) {
        for entry in &self.entries {
            if entry.is_parent() {
                continue;
            }
            if glob_match(pattern, &entry.name.to_string_lossy()) {
                if mark {
                    self.marked.insert(entry.name.clone());
                } else {
                    self.marked.remove(&entry.name);
                }
            }
        }
    }

    pub fn invert_marks(&mut self) {
        for entry in &self.entries {
            if entry.is_parent() {
                continue;
            }
            if !self.marked.remove(&entry.name) {
                self.marked.insert(entry.name.clone());
            }
        }
    }

    pub fn marked_stats(&self) -> (usize, u64) {
        let mut count = 0;
        let mut bytes = 0;
        for entry in &self.entries {
            if self.marked.contains(&entry.name) {
                count += 1;
                bytes += entry.size;
            }
        }
        (count, bytes)
    }

    /// Replace the listing with an external result set (entry names are
    /// paths relative to `cwd`, so marking and file operations work
    /// unchanged). Ctrl+R / any reload restores the directory listing.
    pub fn panelize(&mut self, entries: Vec<Entry>, label: String) {
        self.entries = entries;
        self.panelized = Some(label);
        self.marked.clear();
        self.cursor = 0;
    }

    /// Quick search: first entry whose name starts with `prefix`
    /// (case-insensitive), scanning from `from` and wrapping around.
    pub fn find_prefix(&self, prefix: &str, from: usize) -> Option<usize> {
        if self.entries.is_empty() {
            return None;
        }
        let prefix = prefix.to_lowercase();
        let n = self.entries.len();
        (0..n).map(|i| (from + i) % n).find(|&i| {
            let entry = &self.entries[i];
            !entry.is_parent()
                && entry
                    .name
                    .to_string_lossy()
                    .to_lowercase()
                    .starts_with(&prefix)
        })
    }

    /// Paths the next file operation applies to: the marked entries if any,
    /// otherwise the cursor entry (".." is never a target).
    pub fn targets(&self) -> Vec<PathBuf> {
        if !self.marked.is_empty() {
            self.entries
                .iter()
                .filter(|e| self.marked.contains(&e.name))
                .map(|e| self.cwd.join(&e.name))
                .collect()
        } else {
            self.selected()
                .filter(|e| !e.is_parent())
                .map(|e| vec![self.cwd.join(&e.name)])
                .unwrap_or_default()
        }
    }
}

/// Listing + filtering + sorting, without touching panel state — safe to
/// run on a worker thread.
fn prepare_listing(
    fs: &dyn FsProvider,
    dir: &Path,
    show_hidden: bool,
    filter: Option<&str>,
    key: SortKey,
    reverse: bool,
) -> io::Result<Vec<Entry>> {
    Ok(shape_listing(
        fs.read_dir(dir)?,
        show_hidden,
        filter,
        key,
        reverse,
    ))
}

/// The filter/sort half of [`prepare_listing`], for entries that were
/// fetched elsewhere (e.g. by the SFTP connect worker).
fn shape_listing(
    mut entries: Vec<Entry>,
    show_hidden: bool,
    filter: Option<&str>,
    key: SortKey,
    reverse: bool,
) -> Vec<Entry> {
    if !show_hidden {
        entries.retain(|e| !e.is_hidden());
    }
    if let Some(pattern) = filter {
        entries.retain(|e| e.is_dir() || glob_match(pattern, &e.name.to_string_lossy()));
    }
    sort_entries(&mut entries, key, reverse);
    entries
}

/// Directories always group first (even reversed); ties broken bytewise so
/// ordering is total even for names differing only in case. The lowercase
/// sort key is computed once per entry, which matters at 100k entries.
fn sort_entries(entries: &mut Vec<Entry>, key: SortKey, reverse: bool) {
    let mut decorated: Vec<(bool, String, Entry)> = std::mem::take(entries)
        .into_iter()
        .map(|e| (!e.is_dir(), e.name.to_string_lossy().to_lowercase(), e))
        .collect();
    decorated.sort_by(|a, b| {
        let dirs_first = a.0.cmp(&b.0);
        if dirs_first != Ordering::Equal {
            return dirs_first;
        }
        let ord = match key {
            SortKey::Name => name_cmp(a, b),
            SortKey::Ext => a.2.ext().cmp(&b.2.ext()).then_with(|| name_cmp(a, b)),
            SortKey::Size => a.2.size.cmp(&b.2.size).then_with(|| name_cmp(a, b)),
            SortKey::Mtime => a.2.mtime.cmp(&b.2.mtime).then_with(|| name_cmp(a, b)),
        };
        if reverse { ord.reverse() } else { ord }
    });
    *entries = decorated.into_iter().map(|(_, _, e)| e).collect();
}

fn name_cmp(a: &(bool, String, Entry), b: &(bool, String, Entry)) -> Ordering {
    a.1.cmp(&b.1).then_with(|| a.2.name.cmp(&b.2.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::create_dir(dir.path().join("Docs")).unwrap();
        fs::write(dir.path().join("README.md"), "hi").unwrap();
        fs::write(dir.path().join("cargo.lock"), "").unwrap();
        fs::write(dir.path().join(".hidden"), "").unwrap();
        dir
    }

    fn names(panel: &Panel) -> Vec<String> {
        panel
            .entries
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn listing_sorts_dirs_first_then_name_case_insensitive() {
        let tree = make_tree();
        let panel = Panel::new(tree.path().to_path_buf()).unwrap();
        assert_eq!(
            names(&panel),
            ["..", "Docs", "src", ".hidden", "cargo.lock", "README.md"]
        );
    }

    #[test]
    fn hidden_toggle_filters_dotfiles() {
        let tree = make_tree();
        let mut panel = Panel::new(tree.path().to_path_buf()).unwrap();
        panel.toggle_hidden().unwrap();
        assert!(!names(&panel).contains(&".hidden".to_string()));
        panel.toggle_hidden().unwrap();
        assert!(names(&panel).contains(&".hidden".to_string()));
    }

    #[test]
    fn sort_by_size_and_reverse_toggle() {
        let tree = make_tree();
        let mut panel = Panel::new(tree.path().to_path_buf()).unwrap();
        panel.set_sort(SortKey::Size);
        let sizes: Vec<u64> = panel
            .entries
            .iter()
            .filter(|e| !e.is_dir())
            .map(|e| e.size)
            .collect();
        assert!(sizes.windows(2).all(|w| w[0] <= w[1]));
        panel.set_sort(SortKey::Size); // same key again → reverse
        assert!(panel.sort_reverse);
        let sizes: Vec<u64> = panel
            .entries
            .iter()
            .filter(|e| !e.is_dir())
            .map(|e| e.size)
            .collect();
        assert!(sizes.windows(2).all(|w| w[0] >= w[1]));
        // dirs still group first even when reversed
        assert!(panel.entries[1].is_dir());
    }

    #[test]
    fn resort_keeps_cursor_on_same_entry() {
        let tree = make_tree();
        let mut panel = Panel::new(tree.path().to_path_buf()).unwrap();
        let pos = panel
            .entries
            .iter()
            .position(|e| e.name == "README.md")
            .unwrap();
        panel.cursor = pos;
        panel.set_sort(SortKey::Ext);
        assert_eq!(panel.selected().unwrap().name, "README.md");
    }

    #[test]
    fn marks_toggle_glob_invert_and_targets() {
        let tree = make_tree();
        let mut panel = Panel::new(tree.path().to_path_buf()).unwrap();
        panel.move_top();
        panel.toggle_mark(); // on "..": no mark, but cursor advances
        assert!(panel.marked.is_empty());
        assert_eq!(panel.cursor, 1);
        panel.toggle_mark(); // marks "Docs"
        assert_eq!(panel.marked_stats().0, 1);

        panel.mark_glob("*.md", true);
        assert!(
            panel.is_marked(
                panel
                    .entries
                    .iter()
                    .find(|e| e.name == "README.md")
                    .unwrap()
            )
        );

        panel.invert_marks();
        let (count, _) = panel.marked_stats();
        assert_eq!(count, panel.entries.len() - 1 - 2); // all except "..", Docs, README.md

        panel.marked.clear();
        panel.mark_glob("*", true);
        let targets = panel.targets();
        assert_eq!(targets.len(), panel.entries.len() - 1);
        assert!(targets.iter().all(|p| p.parent() == Some(tree.path())));

        // without marks, targets falls back to the cursor entry
        panel.marked.clear();
        panel.move_top();
        assert!(panel.targets().is_empty()); // ".." is never a target
        panel.move_down();
        assert_eq!(panel.targets().len(), 1);
    }

    #[test]
    fn filter_hides_files_but_not_dirs() {
        let tree = make_tree();
        let mut panel = Panel::new(tree.path().to_path_buf()).unwrap();
        panel.filter = Some("*.md".into());
        panel.reload().unwrap();
        let listed = names(&panel);
        assert!(listed.contains(&"README.md".to_string()));
        assert!(!listed.contains(&"cargo.lock".to_string()));
        assert!(listed.contains(&"src".to_string())); // dirs always show
        panel.filter = None;
        panel.reload().unwrap();
        assert!(names(&panel).contains(&"cargo.lock".to_string()));
    }

    #[test]
    fn find_prefix_is_case_insensitive_and_wraps() {
        let tree = make_tree();
        let panel = Panel::new(tree.path().to_path_buf()).unwrap();
        // entries: .., Docs, src, .hidden, cargo.lock, README.md
        let readme = panel.find_prefix("read", 0).unwrap();
        assert_eq!(panel.entries[readme].name, "README.md");
        // wraps: searching for "docs" starting past its position
        let docs = panel.find_prefix("docs", readme).unwrap();
        assert_eq!(panel.entries[docs].name, "Docs");
        assert_eq!(panel.find_prefix("nope", 0), None);
        // "." matches ".hidden" but never the ".." pseudo-entry
        let hidden = panel.find_prefix(".", 0).unwrap();
        assert_eq!(panel.entries[hidden].name, ".hidden");
    }

    #[test]
    fn enter_and_go_up_restores_cursor_on_origin_dir() {
        let tree = make_tree();
        let mut panel = Panel::new(tree.path().to_path_buf()).unwrap();
        let src_pos = panel.entries.iter().position(|e| e.name == "src").unwrap();
        panel.cursor = src_pos;
        assert!(panel.enter().unwrap());
        assert_eq!(panel.cwd, tree.path().join("src"));
        assert!(panel.go_up().unwrap());
        assert_eq!(panel.cwd, tree.path());
        assert_eq!(panel.selected().unwrap().name, "src");
    }

    #[test]
    fn changing_directory_clears_marks() {
        let tree = make_tree();
        let mut panel = Panel::new(tree.path().to_path_buf()).unwrap();
        panel.mark_glob("*", true);
        assert!(!panel.marked.is_empty());
        let src_pos = panel.entries.iter().position(|e| e.name == "src").unwrap();
        panel.cursor = src_pos;
        panel.enter().unwrap();
        assert!(panel.marked.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn local_entries_carry_extended_stat() {
        use std::os::unix::fs::MetadataExt;
        let tree = make_tree();
        let panel = Panel::new(tree.path().to_path_buf()).unwrap();
        let file = panel
            .entries
            .iter()
            .find(|e| !e.is_dir() && !e.is_parent())
            .unwrap();
        let meta = std::fs::metadata(tree.path().join(&file.name)).unwrap();
        assert_eq!(file.extra.uid, Some(meta.uid()));
        assert_eq!(file.extra.gid, Some(meta.gid()));
        assert_eq!(file.extra.nlink, Some(meta.nlink()));
        assert_eq!(file.extra.inode, Some(meta.ino()));
        assert!(file.extra.atime.is_some());
        assert!(file.extra.ctime.is_some());
    }

    #[test]
    fn history_walks_back_forward_and_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for name in ["a", "b", "c"] {
            fs::create_dir(root.join(name)).unwrap();
        }
        let mut panel = Panel::new(root.clone()).unwrap();
        panel.cd(root.join("a")).unwrap();
        panel.cd(root.join("b")).unwrap();
        // reloads do not grow the history
        panel.reload().unwrap();

        let back = panel.hist_back().unwrap();
        assert_eq!(back, root.join("a").display().to_string());
        panel.cd(PathBuf::from(&back)).unwrap();
        assert_eq!(panel.cwd, root.join("a"));

        let back = panel.hist_back().unwrap();
        assert_eq!(back, root.display().to_string());
        panel.cd(PathBuf::from(&back)).unwrap();
        assert!(panel.hist_back().is_none());

        let fwd = panel.hist_forward().unwrap();
        assert_eq!(fwd, root.join("a").display().to_string());
        panel.cd(PathBuf::from(&fwd)).unwrap();

        // a new stop from the middle drops the forward entries
        panel.cd(root.join("c")).unwrap();
        assert!(panel.hist_forward().is_none());
        assert_eq!(
            panel.hist_back().unwrap(),
            root.join("a").display().to_string()
        );
    }

    #[test]
    fn abandoned_history_navigation_self_corrects() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for name in ["a", "b"] {
            fs::create_dir(root.join(name)).unwrap();
        }
        let mut panel = Panel::new(root.clone()).unwrap();
        panel.cd(root.join("a")).unwrap();
        // start going back, but navigate somewhere else instead
        let _ = panel.hist_back().unwrap();
        panel.cd(root.join("b")).unwrap();
        assert!(panel.hist_forward().is_none());
        assert_eq!(
            panel.hist_back().unwrap(),
            root.join("a").display().to_string()
        );
    }

    #[test]
    fn enter_on_file_is_a_no_op() {
        let tree = make_tree();
        let mut panel = Panel::new(tree.path().to_path_buf()).unwrap();
        panel.move_bottom();
        assert!(!panel.enter().unwrap());
        assert_eq!(panel.cwd, tree.path());
    }

    #[test]
    fn enter_and_leave_archive() {
        use std::io::Write as _;
        let tree = make_tree();
        let zip_path = tree.path().join("bundle.zip");
        let mut zip = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("inner/file.txt", options).unwrap();
        zip.write_all(b"zipped").unwrap();
        zip.finish().unwrap();

        let mut panel = Panel::new(tree.path().to_path_buf()).unwrap();
        let pos = panel
            .entries
            .iter()
            .position(|e| e.name == "bundle.zip")
            .unwrap();
        panel.cursor = pos;
        assert!(panel.enter().unwrap());
        assert!(!panel.is_local());
        assert_eq!(panel.cwd, PathBuf::new());
        assert!(panel.display_path().contains("bundle.zip://"));
        assert_eq!(panel.local_cwd(), tree.path());
        // root of the archive: "..", inner/
        assert!(panel.entries[0].is_parent());
        assert!(panel.entries.iter().any(|e| e.name == "inner"));

        // descend, then climb all the way back out
        let inner = panel
            .entries
            .iter()
            .position(|e| e.name == "inner")
            .unwrap();
        panel.cursor = inner;
        assert!(panel.enter().unwrap());
        assert!(panel.entries.iter().any(|e| e.name == "file.txt"));
        assert!(panel.go_up().unwrap()); // back to archive root
        assert!(panel.go_up().unwrap()); // out of the archive
        assert!(panel.is_local());
        assert_eq!(panel.cwd, tree.path());
        assert_eq!(panel.selected().unwrap().name, "bundle.zip");
    }

    #[test]
    fn cd_leaves_archive() {
        use std::io::Write as _;
        let tree = make_tree();
        let zip_path = tree.path().join("bundle.zip");
        let mut zip = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        zip.start_file("f", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"x").unwrap();
        zip.finish().unwrap();

        let mut panel = Panel::new(tree.path().to_path_buf()).unwrap();
        panel.cursor = panel
            .entries
            .iter()
            .position(|e| e.name == "bundle.zip")
            .unwrap();
        panel.enter().unwrap();
        assert!(!panel.is_local());
        panel.cd(tree.path().join("src")).unwrap();
        assert!(panel.is_local());
        assert_eq!(panel.cwd, tree.path().join("src"));
    }

    struct SlowFs {
        delay: Duration,
    }

    impl FsProvider for SlowFs {
        fn read_dir(&self, _dir: &Path) -> io::Result<Vec<Entry>> {
            std::thread::sleep(self.delay);
            Ok(vec![Entry {
                name: OsString::from("slow-file.txt"),
                kind: crate::entry::EntryKind::File,
                size: 1,
                mtime: None,
                mode: 0o644,
                link_target: None,
                extra: Default::default(),
            }])
        }
        fn stat(&self, _path: &Path) -> io::Result<Entry> {
            Err(io::Error::other("unused"))
        }
        fn open_read(&self, _path: &Path) -> io::Result<Box<dyn std::io::Read + Send>> {
            Err(io::Error::other("unused"))
        }
    }

    #[test]
    fn slow_listing_goes_pending_then_applies() {
        let tree = make_tree();
        let mut panel = Panel::new(tree.path().to_path_buf()).unwrap();
        panel.fs = Arc::new(SlowFs {
            delay: Duration::from_millis(300),
        });
        let target = tree.path().join("src");
        let done = panel.request_dir(target.clone(), LoadKind::Enter).unwrap();
        assert!(!done);
        assert!(panel.is_loading());
        assert_eq!(panel.cwd, tree.path()); // untouched while pending

        let mut applied = None;
        for _ in 0..100 {
            if let Some(result) = panel.poll_pending() {
                applied = Some(result);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        applied.expect("load finished").unwrap();
        assert!(!panel.is_loading());
        assert_eq!(panel.cwd, target);
        assert!(panel.entries.iter().any(|e| e.name == "slow-file.txt"));
    }

    #[test]
    fn cancelled_pending_load_never_applies() {
        let tree = make_tree();
        let mut panel = Panel::new(tree.path().to_path_buf()).unwrap();
        panel.fs = Arc::new(SlowFs {
            delay: Duration::from_millis(250),
        });
        let done = panel
            .request_dir(tree.path().join("src"), LoadKind::Enter)
            .unwrap();
        assert!(!done);
        panel.cancel_pending();
        assert!(!panel.is_loading());
        std::thread::sleep(Duration::from_millis(350));
        assert!(panel.poll_pending().is_none());
        assert_eq!(panel.cwd, tree.path());
        assert!(!panel.entries.iter().any(|e| e.name == "slow-file.txt"));
    }

    #[test]
    fn adopt_remote_and_return_to_local() {
        let tree = make_tree();
        let mut panel = Panel::new(tree.path().to_path_buf()).unwrap();
        panel.mark_glob("*", true);

        let raw = vec![
            Entry {
                name: OsString::from("etc"),
                kind: crate::entry::EntryKind::Dir,
                size: 0,
                mtime: None,
                mode: 0o755,
                link_target: None,
                extra: Default::default(),
            },
            Entry {
                name: OsString::from("motd"),
                kind: crate::entry::EntryKind::File,
                size: 5,
                mtime: None,
                mode: 0o644,
                link_target: None,
                extra: Default::default(),
            },
        ];
        panel.adopt_remote(
            Arc::new(SlowFs {
                delay: Duration::from_millis(0),
            }),
            "sftp://bob@box".into(),
            PathBuf::from("/srv"),
            raw,
        );

        assert!(panel.is_remote());
        assert!(!panel.is_local());
        assert_eq!(panel.display_path(), "sftp://bob@box/srv");
        assert!(panel.marked.is_empty());
        // "..", then dirs first
        assert!(panel.entries[0].is_parent());
        assert_eq!(panel.entries[1].name, "etc");
        assert_eq!(panel.entries[2].name, "motd");
        // remote targets resolve against the remote cwd
        panel.cursor = 2;
        assert_eq!(panel.targets(), vec![PathBuf::from("/srv/motd")]);

        panel.to_local(tree.path().to_path_buf()).unwrap();
        assert!(panel.is_local());
        assert_eq!(panel.cwd, tree.path());
        assert!(panel.entries.iter().any(|e| e.name == "README.md"));
    }

    #[test]
    fn cursor_movement_clamps_at_both_ends() {
        let tree = make_tree();
        let mut panel = Panel::new(tree.path().to_path_buf()).unwrap();
        panel.page_up(100);
        assert_eq!(panel.cursor, 0);
        panel.page_down(100);
        assert_eq!(panel.cursor, panel.entries.len() - 1);
        panel.move_down();
        assert_eq!(panel.cursor, panel.entries.len() - 1);
    }
}
