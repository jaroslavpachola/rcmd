//! MC's directory tree: the figure of directories behind the Command
//! menu's tree dialog and the panel's tree listing mode.
//!
//! The whole point is staying cheap. mc builds its figure by scanning
//! "only a small subset of all the directories" and keeps the rest in a
//! cache file that goes stale; rcmd scans strictly on demand instead -
//! one `read_dir` per directory the user actually opens - so there is
//! no cache, no startup cost and nothing to go out of date beyond the
//! branches currently on screen (`rescan` refreshes those).
//!
//! That is also why [`Node::children`] is an `Option`: `None` means
//! "never looked", which is not the same as "looked and found none",
//! and only the second one may draw as a leaf.

use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// One directory in the figure.
#[derive(Debug)]
struct Node {
    name: OsString,
    /// `None` until this directory is scanned.
    children: Option<Vec<Node>>,
    expanded: bool,
}

impl Node {
    fn new(name: OsString) -> Node {
        Node {
            name,
            children: None,
            expanded: false,
        }
    }

    fn collapse_all(&mut self) {
        self.expanded = false;
        if let Some(children) = self.children.as_mut() {
            for child in children {
                child.collapse_all();
            }
        }
    }
}

/// A visible line of the figure, ready to draw.
#[derive(Debug, Clone)]
pub struct Row {
    pub path: PathBuf,
    /// Display name, lossy - `path` keeps the real bytes.
    pub name: String,
    pub depth: usize,
    pub expanded: bool,
    /// Scanned, and it does have subdirectories.
    pub has_children: bool,
    /// Never scanned, so whether it branches is still unknown.
    pub unknown: bool,
    /// Last of its siblings: the figure turns a corner here.
    pub last: bool,
    /// Per ancestor level, whether that level's trunk line continues
    /// past this row (false where the ancestor was a last child).
    pub trunk: Vec<bool>,
}

/// The tree figure plus where the cursor sits in it.
#[derive(Debug)]
pub struct Tree {
    root: Node,
    root_path: PathBuf,
    rows: Vec<Row>,
    cursor: usize,
    /// mc's dynamic navigation (its default): only the path down to the
    /// selection, its siblings and its children are shown, and the
    /// figure re-shapes itself as the cursor moves. `false` is mc's
    /// static mode, where every directory ever scanned stays visible.
    dynamic: bool,
    show_hidden: bool,
    /// Type-to-search string, shown in the dialog's status line.
    pub search: String,
}

impl Tree {
    /// A tree rooted at `/`, opened down to `start` with `start`
    /// selected.
    pub fn new(start: &Path, show_hidden: bool) -> Tree {
        let root_path = PathBuf::from("/");
        let mut tree = Tree {
            root: Node::new(OsString::from("/")),
            root_path,
            rows: Vec::new(),
            cursor: 0,
            dynamic: true,
            show_hidden,
            search: String::new(),
        };
        tree.reveal(start);
        tree
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn dynamic(&self) -> bool {
        self.dynamic
    }

    pub fn selected(&self) -> Option<&Row> {
        self.rows.get(self.cursor)
    }

    pub fn selected_path(&self) -> Option<PathBuf> {
        self.selected().map(|row| row.path.clone())
    }

    /// Open every level from the root down to `path` and put the cursor
    /// there. A level that cannot be reached (gone, unreadable, hidden
    /// while hidden files are off) stops the walk at the deepest
    /// directory that is actually in the figure.
    pub fn reveal(&mut self, path: &Path) {
        self.open_chain(path);
        self.flatten();
        self.select_path(path);
    }

    /// Topmost row to draw in a window `height` rows tall. The cursor
    /// is kept mid-window rather than at an edge: the figure grows
    /// *below* the selection as it opens, and pinning the cursor to the
    /// last row would hide exactly what was just expanded. Renderer and
    /// mouse mapping share it, so a click lands where it looks.
    pub fn first_visible(&self, height: usize) -> usize {
        let last_top = self.rows.len().saturating_sub(height);
        self.cursor.saturating_sub(height / 2).min(last_top)
    }

    /// Put the cursor on a row by index - what a mouse click does.
    pub fn select_row(&mut self, index: usize) {
        self.set_cursor(index);
    }

    pub fn up(&mut self) {
        self.set_cursor(self.cursor.saturating_sub(1));
    }

    pub fn down(&mut self) {
        self.set_cursor(self.cursor + 1);
    }

    pub fn page_up(&mut self, rows: usize) {
        self.set_cursor(self.cursor.saturating_sub(rows.max(1)));
    }

    pub fn page_down(&mut self, rows: usize) {
        self.set_cursor(self.cursor + rows.max(1));
    }

    pub fn first(&mut self) {
        self.set_cursor(0);
    }

    pub fn last(&mut self) {
        self.set_cursor(usize::MAX);
    }

    /// Left: collapse an open branch, or step to the parent. In dynamic
    /// mode there is nothing to collapse - the figure only ever holds
    /// the current path - so it is always the parent, which is what mc
    /// documents for that mode.
    pub fn left(&mut self) {
        let Some(row) = self.rows.get(self.cursor) else {
            return;
        };
        let path = row.path.clone();
        if !self.dynamic && row.expanded && row.has_children {
            if let Some(node) = self.node_mut(&path) {
                node.expanded = false;
            }
            self.flatten();
            self.select_path(&path);
            return;
        }
        if let Some(parent) = path.parent().map(Path::to_path_buf) {
            self.select_path(&parent);
            self.normalize();
        }
    }

    /// Right: open this directory, then step into its first child.
    pub fn right(&mut self) {
        let Some(path) = self.selected_path() else {
            return;
        };
        self.open_chain(&path);
        self.flatten();
        self.select_path(&path);
        let depth = self.rows.get(self.cursor).map_or(0, |row| row.depth);
        if self
            .rows
            .get(self.cursor + 1)
            .is_some_and(|row| row.depth == depth + 1)
        {
            self.cursor += 1;
        }
        self.normalize();
    }

    /// mc's F2/C-r: forget what we knew about this directory and look
    /// again, which is how a tree that has gone stale is repaired.
    pub fn rescan(&mut self) {
        let Some(path) = self.selected_path() else {
            return;
        };
        if let Some(node) = self.node_mut(&path) {
            node.children = None;
            node.expanded = false;
        }
        self.open_chain(&path);
        self.flatten();
        self.select_path(&path);
    }

    /// mc's F3: drop this directory from the figure (not from disk).
    /// The parent keeps it out until it is rescanned.
    pub fn forget(&mut self) {
        let Some(path) = self.selected_path() else {
            return;
        };
        let Some(parent) = path.parent().map(Path::to_path_buf) else {
            return;
        };
        let Some(name) = path.file_name().map(OsStr::to_os_string) else {
            return;
        };
        if let Some(node) = self.node_mut(&parent)
            && let Some(children) = node.children.as_mut()
        {
            children.retain(|child| child.name != name);
        }
        self.flatten();
        self.select_path(&parent);
    }

    /// mc's F4: switch between dynamic and static navigation.
    pub fn toggle_mode(&mut self) {
        self.dynamic = !self.dynamic;
        let path = self.selected_path();
        if self.dynamic {
            self.normalize();
        } else if let Some(path) = path {
            self.open_chain(&path);
            self.flatten();
            self.select_path(&path);
        }
    }

    /// Extend the search string. Returns false (and changes nothing) if
    /// no directory from the cursor on starts with it.
    pub fn search_push(&mut self, c: char) -> bool {
        let mut wanted = self.search.clone();
        wanted.push(c);
        match self.find_from(self.cursor, &wanted) {
            Some(index) => {
                self.search = wanted;
                self.set_cursor(index);
                true
            }
            None => false,
        }
    }

    pub fn search_pop(&mut self) {
        self.search.pop();
    }

    pub fn clear_search(&mut self) {
        self.search.clear();
    }

    /// mc's C-s inside the tree: the next directory matching the search
    /// string, or one line down when there is none.
    pub fn search_next(&mut self) {
        if self.search.is_empty() {
            self.down();
            return;
        }
        let search = self.search.clone();
        match self.find_from(self.cursor + 1, &search) {
            Some(index) => self.set_cursor(index),
            None => self.down(),
        }
    }

    /// First row at or after `from` whose name starts with `prefix`,
    /// wrapping once so a search never dead-ends at the bottom.
    fn find_from(&self, from: usize, prefix: &str) -> Option<usize> {
        let prefix = prefix.to_lowercase();
        let hit = |row: &Row| row.name.to_lowercase().starts_with(&prefix);
        let after = self.rows.iter().skip(from).position(hit).map(|i| i + from);
        after.or_else(|| self.rows.iter().take(from).position(hit))
    }

    fn set_cursor(&mut self, index: usize) {
        self.cursor = index.min(self.rows.len().saturating_sub(1));
        self.normalize();
    }

    /// Dynamic mode re-shapes the figure around wherever the cursor
    /// landed: everything collapses, then the path down to the
    /// selection (and the selection itself) opens again.
    fn normalize(&mut self) {
        if !self.dynamic {
            return;
        }
        let Some(path) = self.selected_path() else {
            return;
        };
        self.root.collapse_all();
        self.open_chain(&path);
        self.flatten();
        self.select_path(&path);
    }

    /// Put the cursor on `path`, or on the deepest ancestor of it that
    /// the figure actually holds.
    fn select_path(&mut self, path: &Path) {
        let mut wanted = path;
        loop {
            if let Some(index) = self.rows.iter().position(|row| row.path == wanted) {
                self.cursor = index;
                return;
            }
            match wanted.parent() {
                Some(parent) => wanted = parent,
                None => break,
            }
        }
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
    }

    /// Scan and expand every directory from the root down to `path`
    /// inclusive, stopping at the first level that is not there.
    fn open_chain(&mut self, path: &Path) {
        let Some(parts) = self.relative(path) else {
            return;
        };
        let mut current = self.root_path.clone();
        for part in parts.iter().map(Some).chain(std::iter::once(None)) {
            let show_hidden = self.show_hidden;
            let Some(node) = self.node_mut(&current) else {
                return;
            };
            if node.children.is_none() {
                node.children = Some(scan(&current, show_hidden));
            }
            node.expanded = true;
            let Some(part) = part else { return };
            let known = node
                .children
                .as_ref()
                .is_some_and(|children| children.iter().any(|child| child.name == *part));
            if !known {
                return;
            }
            current.push(part);
        }
    }

    /// `path`'s components below the root, or `None` when it is not
    /// under the root at all.
    fn relative(&self, path: &Path) -> Option<Vec<OsString>> {
        let rest = path.strip_prefix(&self.root_path).ok()?;
        Some(
            rest.components()
                .map(|c| c.as_os_str().to_os_string())
                .collect(),
        )
    }

    fn node_mut(&mut self, path: &Path) -> Option<&mut Node> {
        let parts = self.relative(path)?;
        let mut node = &mut self.root;
        for part in parts {
            let children = node.children.as_mut()?;
            let index = children.iter().position(|child| child.name == part)?;
            node = &mut children[index];
        }
        Some(node)
    }

    fn flatten(&mut self) {
        let mut rows = Vec::new();
        let mut trunk = Vec::new();
        push_rows(
            &self.root,
            self.root_path.clone(),
            0,
            true,
            &mut trunk,
            &mut rows,
        );
        self.rows = rows;
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
    }
}

fn push_rows(
    node: &Node,
    path: PathBuf,
    depth: usize,
    last: bool,
    trunk: &mut Vec<bool>,
    out: &mut Vec<Row>,
) {
    let name = if depth == 0 {
        path.display().to_string()
    } else {
        node.name.to_string_lossy().into_owned()
    };
    out.push(Row {
        path: path.clone(),
        name,
        depth,
        expanded: node.expanded,
        has_children: node.children.as_ref().is_some_and(|c| !c.is_empty()),
        unknown: node.children.is_none(),
        last,
        trunk: trunk.clone(),
    });
    if !node.expanded {
        return;
    }
    let Some(children) = node.children.as_ref() else {
        return;
    };
    if depth > 0 {
        trunk.push(!last);
    }
    for (i, child) in children.iter().enumerate() {
        push_rows(
            child,
            path.join(&child.name),
            depth + 1,
            i + 1 == children.len(),
            trunk,
            out,
        );
    }
    if depth > 0 {
        trunk.pop();
    }
}

/// The subdirectories of `path`, sorted the way the panel sorts names.
/// An unreadable directory scans as empty rather than as an error: the
/// figure just shows it as a leaf, which is all mc does too.
fn scan(path: &Path, show_hidden: bool) -> Vec<Node> {
    let Ok(read) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut names: Vec<OsString> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name();
        if !show_hidden && name.as_encoded_bytes().first() == Some(&b'.') {
            continue;
        }
        let is_dir = match entry.file_type() {
            Ok(kind) if kind.is_dir() => true,
            // a symlink to a directory is a directory in the panel, so
            // it is one here; nothing descends it until asked, which is
            // what keeps a link loop from mattering
            Ok(kind) if kind.is_symlink() => {
                std::fs::metadata(entry.path()).is_ok_and(|meta| meta.is_dir())
            }
            _ => false,
        };
        if is_dir {
            names.push(name);
        }
    }
    names.sort_by(|a, b| name_order(a, b));
    names.into_iter().map(Node::new).collect()
}

/// Lowercased first, exact bytes as the tiebreak - the panel's name
/// ordering, so the tree and the listing agree.
fn name_order(a: &OsStr, b: &OsStr) -> Ordering {
    let lower = |name: &OsStr| name.to_string_lossy().to_lowercase();
    lower(a).cmp(&lower(b)).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// `a/{one,two}`, `b/deep`, and a hidden `.dot` - enough shape to
    /// test siblings, children and the hidden-files flag.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for path in ["a/one", "a/two", "b/deep", ".dot/inside"] {
            fs::create_dir_all(dir.path().join(path)).unwrap();
        }
        fs::write(dir.path().join("a/file.txt"), "not a directory").unwrap();
        dir
    }

    fn names(tree: &Tree) -> Vec<&str> {
        tree.rows().iter().map(|row| row.name.as_str()).collect()
    }

    #[test]
    fn reveal_opens_the_path_and_selects_it() {
        let dir = fixture();
        let target = dir.path().join("a");
        let tree = Tree::new(&target, true);
        assert_eq!(tree.selected_path().as_deref(), Some(target.as_path()));
        // the root and both of a's children came along
        assert_eq!(tree.rows()[0].path, Path::new("/"));
        assert!(names(&tree).contains(&"one"));
        assert!(names(&tree).contains(&"two"));
    }

    #[test]
    fn files_are_not_in_the_figure() {
        let dir = fixture();
        let tree = Tree::new(&dir.path().join("a"), true);
        assert!(!names(&tree).contains(&"file.txt"));
    }

    #[test]
    fn hidden_directories_follow_the_flag() {
        let dir = fixture();
        let shown = Tree::new(&dir.path().join("a"), true);
        assert!(names(&shown).contains(&".dot"));
        let hidden = Tree::new(&dir.path().join("a"), false);
        assert!(!names(&hidden).contains(&".dot"));
    }

    #[test]
    fn dynamic_mode_leaves_other_branches_shut() {
        let dir = fixture();
        let tree = Tree::new(&dir.path().join("a"), true);
        // b is a sibling, so it shows - but nothing inside it does
        assert!(names(&tree).contains(&"b"));
        assert!(!names(&tree).contains(&"deep"));
    }

    #[test]
    fn static_mode_keeps_every_scanned_branch() {
        let dir = fixture();
        let mut tree = Tree::new(&dir.path().join("b"), true);
        tree.toggle_mode();
        assert!(!tree.dynamic());
        assert!(names(&tree).contains(&"deep"));
        // walking to a's subtree no longer shuts b's
        tree.reveal(&dir.path().join("a"));
        assert!(names(&tree).contains(&"deep"));
        assert!(names(&tree).contains(&"one"));
    }

    #[test]
    fn right_steps_into_the_first_child_and_left_returns() {
        let dir = fixture();
        let mut tree = Tree::new(&dir.path().join("a"), true);
        tree.right();
        assert_eq!(
            tree.selected_path().as_deref(),
            Some(dir.path().join("a/one").as_path())
        );
        tree.left();
        assert_eq!(
            tree.selected_path().as_deref(),
            Some(dir.path().join("a").as_path())
        );
    }

    #[test]
    fn down_moves_to_the_next_sibling() {
        let dir = fixture();
        let mut tree = Tree::new(&dir.path().join("a"), true);
        let before = tree.cursor();
        tree.down();
        assert!(tree.cursor() > before);
        assert_eq!(
            tree.selected_path().as_deref(),
            Some(dir.path().join("a/one").as_path())
        );
    }

    #[test]
    fn forget_drops_a_branch_and_rescan_brings_it_back() {
        let dir = fixture();
        let mut tree = Tree::new(&dir.path().join("a/one"), true);
        tree.forget();
        // the cursor fell back to the parent, and `one` is gone
        assert_eq!(
            tree.selected_path().as_deref(),
            Some(dir.path().join("a").as_path())
        );
        assert!(!names(&tree).contains(&"one"));
        tree.rescan();
        assert!(names(&tree).contains(&"one"));
    }

    #[test]
    fn forgetting_the_root_is_a_no_op() {
        let dir = fixture();
        let mut tree = Tree::new(dir.path(), true);
        tree.first();
        tree.forget();
        assert_eq!(tree.rows()[0].path, Path::new("/"));
    }

    #[test]
    fn search_walks_the_matches() {
        let dir = fixture();
        let mut tree = Tree::new(dir.path(), true);
        tree.right(); // into a
        tree.left();
        assert!(tree.search_push('b'));
        assert_eq!(
            tree.selected_path().as_deref(),
            Some(dir.path().join("b").as_path())
        );
        // nothing starts with "bz", so the string is left as it was
        assert!(!tree.search_push('z'));
        assert_eq!(tree.search, "b");
    }

    #[test]
    fn a_missing_path_selects_the_deepest_ancestor() {
        let dir = fixture();
        let tree = Tree::new(&dir.path().join("a/nope/deeper"), true);
        assert_eq!(
            tree.selected_path().as_deref(),
            Some(dir.path().join("a").as_path())
        );
    }

    #[test]
    fn trunk_flags_describe_the_figure() {
        let dir = fixture();
        let tree = Tree::new(&dir.path().join("a"), true);
        let rows = tree.rows();
        let one = rows.iter().find(|row| row.name == "one").unwrap();
        let two = rows.iter().find(|row| row.name == "two").unwrap();
        assert!(!one.last, "one has a sibling below it");
        assert!(two.last, "two is the last child of a");
        assert_eq!(one.depth, two.depth);
        // `one` was never opened, so the figure cannot know if it branches
        assert!(one.unknown);
    }
}
