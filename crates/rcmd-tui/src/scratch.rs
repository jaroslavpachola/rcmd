//! Scratch files: the copy a remote or archived file is viewed or
//! edited through, the bulk-rename buffer, a `[[view]]` filter's
//! output, the subshell's rc files.
//!
//! They all live in one directory per process, made the way `mkdtemp`
//! makes one - a random name, mode 0700 - rather than at
//! `$TMPDIR/rcmd-<pid>-<name>`, a name anyone on the machine could guess
//! and plant a symlink at before rcmd got there. Each file in it is
//! created fresh (`O_EXCL`, 0600), and the directory goes when rcmd
//! exits.

use std::fs::File;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Mutex;

static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// This process's scratch directory, made on first use.
pub fn dir() -> io::Result<PathBuf> {
    let mut dir = DIR.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(dir) = dir.as_ref()
        && dir.is_dir()
    {
        return Ok(dir.clone());
    }
    // tempfile leaves a directory's mode to the umask; this one is 0700
    let made = tempfile::Builder::new()
        .prefix("rcmd-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?
        .keep();
    *dir = Some(made.clone());
    Ok(made)
}

/// A new, empty file whose name ends in `name` - the extension is what
/// an editor picks its syntax by - and where it is.
pub fn create(name: &str) -> io::Result<(File, PathBuf)> {
    let name: String = name
        .chars()
        .map(|c| if c == '/' || c == '\0' { '_' } else { c })
        .collect();
    tempfile::Builder::new()
        .prefix("")
        .suffix(&format!("-{name}"))
        .tempfile_in(dir()?)?
        .keep()
        .map_err(|err| err.error)
}

/// Remove the directory and whatever is still in it, on the way out.
pub fn cleanup() {
    if let Some(dir) = DIR.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_files_are_private_and_never_reused() {
        let (_, a) = create("notes.txt").unwrap();
        let (_, b) = create("notes.txt").unwrap();
        assert_ne!(a, b, "two files of one name must not share a path");
        for path in [&a, &b] {
            let name = path.file_name().unwrap().to_string_lossy();
            assert!(name.ends_with("-notes.txt"), "{name}");
        }
        let dir = a.parent().unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(dir), 0o700);
        assert_eq!(mode(&a), 0o600);
        assert!(
            !dir.file_name()
                .unwrap()
                .to_string_lossy()
                .contains(&std::process::id().to_string()),
            "the directory name is not guessable from the pid"
        );
        let (_, odd) = create("a/b").unwrap();
        assert_eq!(odd.parent(), Some(dir));
        let dir = dir.to_path_buf();
        cleanup();
        assert!(!dir.exists(), "cleanup left the directory behind");
    }
}
