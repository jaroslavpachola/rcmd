use std::cmp::Ordering;
use std::collections::HashSet;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::archive::ArchiveFs;
use crate::entry::Entry;
use crate::glob::glob_match;
use crate::vfs::{FsProvider, LocalFs, is_archive_name};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Ext,
    Size,
    Mtime,
}

/// One side of the two-panel view: a directory listing with a cursor and
/// per-directory marks. Pure state + logic, no rendering concerns.
pub struct Panel {
    /// Filesystem behind this panel; [`LocalFs`] normally, an
    /// [`ArchiveFs`] while browsing inside an archive.
    pub fs: Arc<dyn FsProvider>,
    /// Path of the archive file when `fs` is an archive VFS.
    pub archive: Option<PathBuf>,
    /// Local directory, or the path *inside* the archive ("" = its root).
    pub cwd: PathBuf,
    pub entries: Vec<Entry>,
    pub cursor: usize,
    pub marked: HashSet<OsString>,
    pub sort_key: SortKey,
    pub sort_reverse: bool,
    pub show_hidden: bool,
    /// Glob applied to files (directories always show), MC's "filter".
    pub filter: Option<String>,
    /// When Some(label), the listing is an external result set (find /
    /// command output) instead of the directory; any reload restores the
    /// normal listing.
    pub panelized: Option<String>,
}

impl Panel {
    pub fn new(cwd: PathBuf) -> io::Result<Self> {
        let mut panel = Panel {
            fs: Arc::new(LocalFs),
            archive: None,
            cwd,
            entries: Vec::new(),
            cursor: 0,
            marked: HashSet::new(),
            sort_key: SortKey::Name,
            sort_reverse: false,
            show_hidden: true,
            filter: None,
            panelized: None,
        };
        panel.reload()?;
        Ok(panel)
    }

    pub fn is_local(&self) -> bool {
        self.archive.is_none()
    }

    /// Panel location for titles: `path` or `archive.zip://inside`.
    pub fn display_path(&self) -> String {
        match &self.archive {
            Some(archive) => format!("{}://{}", archive.display(), self.cwd.display()),
            None => self.cwd.display().to_string(),
        }
    }

    /// The real directory to run commands in / resolve paths against:
    /// inside an archive, that is the archive's parent directory.
    pub fn local_cwd(&self) -> PathBuf {
        match &self.archive {
            Some(archive) => archive
                .parent()
                .unwrap_or_else(|| Path::new("/"))
                .to_path_buf(),
            None => self.cwd.clone(),
        }
    }

    /// Re-read the current directory, keeping the cursor on the same entry
    /// where possible and pruning marks for entries that no longer exist.
    /// On failure the previous listing is kept.
    pub fn reload(&mut self) -> io::Result<()> {
        self.panelized = None;
        let keep = self.selected().map(|e| e.name.clone());
        // re-index the archive so appended members (F5 into a zip) appear
        if let Some(archive) = &self.archive {
            self.fs = Arc::new(ArchiveFs::open(archive)?);
        }
        let mut entries = self.fs.read_dir(&self.cwd)?;
        if !self.show_hidden {
            entries.retain(|e| !e.is_hidden());
        }
        if let Some(pattern) = &self.filter {
            entries.retain(|e| e.is_dir() || glob_match(pattern, &e.name.to_string_lossy()));
        }
        sort_entries(&mut entries, self.sort_key, self.sort_reverse);
        // ".." also at an archive's root: it leads back out of the archive
        if self.cwd.parent().is_some() || self.archive.is_some() {
            entries.insert(0, Entry::parent());
        }
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
            self.change_dir(self.cwd.join(&name))?;
            return Ok(true);
        }
        if self.archive.is_none() && is_archive_name(&name) {
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

    /// Change to an arbitrary local directory (command-line `cd`);
    /// leaves any archive the panel was browsing.
    pub fn cd(&mut self, target: PathBuf) -> io::Result<()> {
        let prev_fs = self.fs.clone();
        let prev_archive = self.archive.take();
        self.fs = Arc::new(LocalFs);
        match self.change_dir(target) {
            Ok(()) => Ok(()),
            Err(err) => {
                self.fs = prev_fs;
                self.archive = prev_archive;
                Err(err)
            }
        }
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
        let start = usize::from(self.entries.first().is_some_and(Entry::is_parent));
        sort_entries(&mut self.entries[start..], self.sort_key, self.sort_reverse);
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

/// Directories always group first (even reversed); ties broken bytewise so
/// ordering is total even for names differing only in case.
fn sort_entries(entries: &mut [Entry], key: SortKey, reverse: bool) {
    entries.sort_by(|a, b| {
        let dirs_first = b.is_dir().cmp(&a.is_dir());
        if dirs_first != Ordering::Equal {
            return dirs_first;
        }
        let ord = match key {
            SortKey::Name => name_cmp(a, b),
            SortKey::Ext => a.ext().cmp(&b.ext()).then_with(|| name_cmp(a, b)),
            SortKey::Size => a.size.cmp(&b.size).then_with(|| name_cmp(a, b)),
            SortKey::Mtime => a.mtime.cmp(&b.mtime).then_with(|| name_cmp(a, b)),
        };
        if reverse { ord.reverse() } else { ord }
    });
}

fn name_cmp(a: &Entry, b: &Entry) -> Ordering {
    a.name
        .to_string_lossy()
        .to_lowercase()
        .cmp(&b.name.to_string_lossy().to_lowercase())
        .then_with(|| a.name.cmp(&b.name))
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
