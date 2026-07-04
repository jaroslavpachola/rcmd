use std::io;
use std::path::{Path, PathBuf};

use crate::entry::{read_dir, Entry};

/// One side of the two-panel view: a directory listing with a cursor.
/// Pure state + logic, no rendering concerns.
pub struct Panel {
    pub cwd: PathBuf,
    pub entries: Vec<Entry>,
    pub cursor: usize,
}

impl Panel {
    pub fn new(cwd: PathBuf) -> io::Result<Self> {
        let mut panel = Panel {
            cwd,
            entries: Vec::new(),
            cursor: 0,
        };
        panel.reload()?;
        Ok(panel)
    }

    /// Re-read the current directory. On failure the previous listing is kept.
    pub fn reload(&mut self) -> io::Result<()> {
        let mut entries = read_dir(&self.cwd)?;
        sort_entries(&mut entries);
        if self.cwd.parent().is_some() {
            entries.insert(0, Entry::parent());
        }
        self.cursor = self.cursor.min(entries.len().saturating_sub(1));
        self.entries = entries;
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

    /// Enter the selected entry if it is a directory.
    /// Returns true if the panel changed directory.
    pub fn enter(&mut self) -> io::Result<bool> {
        let Some(entry) = self.selected() else {
            return Ok(false);
        };
        if !entry.is_dir() {
            return Ok(false);
        }
        if entry.is_parent() {
            return self.go_up();
        }
        let target = self.cwd.join(&entry.name);
        self.change_dir(target)?;
        Ok(true)
    }

    /// Go to the parent directory, placing the cursor on the directory
    /// we came from (MC behavior). Returns true if the panel moved.
    pub fn go_up(&mut self) -> io::Result<bool> {
        let Some(parent) = self.cwd.parent().map(Path::to_path_buf) else {
            return Ok(false);
        };
        let came_from = self.cwd.file_name().map(|n| n.to_os_string());
        self.change_dir(parent)?;
        if let Some(name) = came_from {
            if let Some(pos) = self.entries.iter().position(|e| e.name == name) {
                self.cursor = pos;
            }
        }
        Ok(true)
    }

    /// Switch to `target`; on failure (e.g. permission denied) the panel
    /// stays where it was, with its listing and cursor intact.
    fn change_dir(&mut self, target: PathBuf) -> io::Result<()> {
        let prev_cwd = std::mem::replace(&mut self.cwd, target);
        let prev_cursor = std::mem::replace(&mut self.cursor, 0);
        if let Err(err) = self.reload() {
            self.cwd = prev_cwd;
            self.cursor = prev_cursor;
            return Err(err);
        }
        Ok(())
    }
}

/// Directories first, then case-insensitive by name; ties broken bytewise
/// so ordering is total even for names differing only in case.
fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.is_dir()
            .cmp(&a.is_dir())
            .then_with(|| {
                a.name
                    .to_string_lossy()
                    .to_lowercase()
                    .cmp(&b.name.to_string_lossy().to_lowercase())
            })
            .then_with(|| a.name.cmp(&b.name))
    });
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
        dir
    }

    #[test]
    fn listing_sorts_dirs_first_then_name_case_insensitive() {
        let tree = make_tree();
        let panel = Panel::new(tree.path().to_path_buf()).unwrap();
        let names: Vec<String> = panel
            .entries
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["..", "Docs", "src", "cargo.lock", "README.md"]);
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
    fn enter_on_file_is_a_no_op() {
        let tree = make_tree();
        let mut panel = Panel::new(tree.path().to_path_buf()).unwrap();
        panel.move_bottom();
        assert!(!panel.enter().unwrap());
        assert_eq!(panel.cwd, tree.path());
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
