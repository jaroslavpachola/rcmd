//! Long-running file operations, executed on a worker thread.
//!
//! The worker streams [`JobEvent`]s to the UI over a channel. When it hits
//! a decision point (existing target, I/O error) it sends an `Ask*` event
//! and blocks until the UI answers with a [`Reply`]. Cancellation is a
//! shared flag checked between chunks and files; dropping the [`JobHandle`]
//! also unblocks a waiting worker because the reply channel closes.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::SystemTime;

use crate::entry::EntryKind;
use crate::mask::{self, Mask};
use crate::vfs::FsProvider;

const CHUNK: usize = 256 * 1024;

#[derive(Debug)]
pub enum JobEvent {
    /// Totals from the pre-scan (bytes is 0 for move/delete jobs).
    Total { files: u64, bytes: u64 },
    Progress {
        files_done: u64,
        bytes_done: u64,
        current: PathBuf,
        /// Bytes of `current` written so far, and how many it has in
        /// total - both 0 where the operation moves whole items rather
        /// than bytes (move, delete), which is what makes a per-file
        /// bar something the UI can simply leave out.
        file_done: u64,
        file_total: u64,
    },
    /// Target exists. `src` and `dst` are what the prompt puts on
    /// screen and what the sticky Update / Size-differs answers compare;
    /// `can_append` is false where the target is not a local file, so
    /// Append and Reget have nothing to open.
    AskOverwrite {
        path: PathBuf,
        src: FileFacts,
        dst: FileFacts,
        can_append: bool,
    },
    /// Operation failed; answer with Retry/Skip/SkipAll/Abort.
    AskError { path: PathBuf, message: String },
    /// One item moved, and where from - the record an undo is built
    /// from. Only sent where the move was a clean one: a move onto a
    /// name that was already taken is not reported, since putting the
    /// source back would not bring back what it landed on.
    Moved { from: PathBuf, to: PathBuf },
    /// One item sent to the trash, by where it was: what an undo of
    /// the F8 takes back out.
    Trashed { path: PathBuf },
    /// One item taken out of the trash, by where it went back to: what
    /// an undo of that puts in again.
    Restored { path: PathBuf },
    /// Something the job left alone, and why: the lines of the report a
    /// finished job can show, where a bare count said only how many.
    Skipped { path: PathBuf, reason: String },
    Done {
        files_done: u64,
        skipped: u64,
        aborted: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    Overwrite,
    OverwriteAll,
    /// mc's Update: from here on, overwrite only where the source is
    /// newer than the target.
    UpdateAll,
    /// mc's "If size differs": from here on, overwrite only where the
    /// two sizes disagree.
    SizeDiffersAll,
    /// mc's Append: put the source on the end of the target.
    Append,
    /// mc's Reget: resume - keep what is already there and copy only
    /// the rest of the source.
    Reget,
    Skip,
    SkipAll,
    Retry,
    Abort,
}

/// The size and modification time of one side of an overwrite question.
/// Missing metadata reads as a zero-length file of unknown age, which is
/// the safe way round: it never makes "newer" or "same size" true.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FileFacts {
    pub size: u64,
    pub mtime: Option<SystemTime>,
}

impl FileFacts {
    pub fn of_path(path: &Path) -> FileFacts {
        match path.symlink_metadata() {
            Ok(meta) => FileFacts {
                size: meta.len(),
                mtime: meta.modified().ok(),
            },
            Err(_) => FileFacts::default(),
        }
    }

    fn of_entry(entry: &crate::entry::Entry) -> FileFacts {
        FileFacts {
            size: entry.size,
            mtime: entry.mtime,
        }
    }

    /// Strictly newer than `other`. An unknown time is never newer, so
    /// mc's Update leaves such a target alone rather than clobbering it.
    fn newer_than(self, other: FileFacts) -> bool {
        match (self.mtime, other.mtime) {
            (Some(mine), Some(theirs)) => mine > theirs,
            _ => false,
        }
    }
}

/// What a copy or move does beyond moving the bytes - MC's copy dialog
/// checkboxes. The defaults are the careful ones and deliberately not
/// mc's: attributes are kept, links are recreated rather than followed,
/// relative symlinks keep pointing where they pointed, and a directory
/// copied onto an existing directory of its own name goes *inside* it
/// instead of merging into it. mc's default there is the merge, which is
/// the one that can silently mix two trees together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferOpts {
    /// Copy permissions and modification times onto the target.
    pub preserve: bool,
    /// Copy what a symlink points at, instead of the link itself.
    pub follow_links: bool,
    /// A directory copied onto an existing directory goes inside it;
    /// off merges the source's contents into the target, as mc does.
    pub dive: bool,
    /// Recompute relative symlinks so they resolve to the same file
    /// from wherever they land.
    pub stable_symlinks: bool,
    /// Replace an existing target without asking. Off everywhere the
    /// user has not already answered that question - which is what a
    /// synchronize is, and nothing else is.
    pub overwrite: bool,
    /// Read each copy back and compare it with the source. Doubles the
    /// reading, and is the only thing that turns "the write returned
    /// no error" into "the bytes are there" - which is what a failing
    /// stick or a long haul over sftp makes worth having.
    pub verify: bool,
    /// Push every copy to the disk before calling it done, and the
    /// directory entry after it: slower, and what a copy to a stick
    /// about to be pulled out needs.
    pub fsync: bool,
}

impl Default for TransferOpts {
    fn default() -> TransferOpts {
        TransferOpts {
            preserve: true,
            follow_links: false,
            dive: true,
            stable_symlinks: true,
            overwrite: false,
            verify: false,
            fsync: false,
        }
    }
}

/// MC's mask copy/rename: which of the sources take part, and what
/// they are called when they land.
#[derive(Debug, Clone)]
pub struct Rename {
    /// Only sources whose name matches are copied at all.
    pub source: Mask,
    /// The destination's last component when it carries wildcards;
    /// `None` leaves every name as it is.
    pub target: Option<String>,
}

impl Rename {
    /// A mask that neither filters nor renames is not worth carrying.
    pub fn new(source: Mask, target: Option<String>) -> Option<Rename> {
        (!source.is_catch_all() || target.is_some()).then_some(Rename { source, target })
    }

    fn accepts(&self, path: &Path) -> bool {
        self.source.matches(&file_name_of(path))
    }

    /// The name this source lands under, or `None` when the mask only
    /// filters and the name is kept.
    fn name_for(&self, path: &Path) -> Option<String> {
        let target = self.target.as_ref()?;
        let name = file_name_of(path);
        let caps = self.source.captures(&name)?;
        Some(mask::expand(target, &caps, &name))
    }
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// What to do with a target that already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overwrite {
    Replace,
    Append,
    Reget,
    Skip,
}

/// A sticky answer to "the target exists": mc's All, Update, "If size
/// differs" and None answer every remaining file without asking again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Policy {
    Ask,
    All,
    Newer,
    SizeDiffers,
    None,
}

pub struct JobHandle {
    pub events: Receiver<JobEvent>,
    pub replies: Sender<Reply>,
    cancel: Arc<AtomicBool>,
    /// Paused: the job stops at its next look at `cancel`, which every
    /// loop in here makes between one piece of work and the next.
    pause: Arc<AtomicBool>,
    /// Held: queued behind another job, and not begun.
    held: Arc<AtomicBool>,
    pub thread: Option<JoinHandle<()>>,
}

impl JobHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn set_paused(&self, paused: bool) {
        self.pause.store(paused, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.pause.load(Ordering::Relaxed)
    }

    /// Let a held job begin.
    pub fn release(&self) {
        self.held.store(false, Ordering::Relaxed);
    }

    pub fn is_held(&self) -> bool {
        self.held.load(Ordering::Relaxed)
    }
}

thread_local! {
    static HOLD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Every job started on this thread while `on` is held until
/// [`JobHandle::release`]: how a queued job is started without a
/// moment in which it has already begun. The flag is the caller's
/// thread's, so the job cannot race it.
pub fn hold_new_jobs(on: bool) {
    HOLD.with(|hold| hold.set(on));
}

pub fn spawn_copy(
    sources: Vec<PathBuf>,
    dest: PathBuf,
    opts: TransferOpts,
    rename: Option<Rename>,
) -> JobHandle {
    spawn_with(opts, move |ctx| {
        let sources = filter_sources(sources, rename.as_ref());
        let (files, bytes) = scan(&sources);
        let _ = ctx.tx.send(JobEvent::Total { files, bytes });
        room_for(ctx, &dest, bytes)?;
        let multiple = sources.len() > 1;
        let into_dir = dest.is_dir() || multiple;
        for src in &sources {
            if ctx.cancelled() {
                return Err(Aborted);
            }
            let target = renamed_target(src, &dest, multiple, into_dir, ctx.opts, rename.as_ref());
            ctx.copy_root = Some(src.clone());
            copy_tree(ctx, src, &target)?;
        }
        Ok(())
    })
}

/// Before the first byte: whether `need` bytes fit where `dest` is. A
/// copy that cannot fit is better told now than when the disk fills on
/// whichever file it had reached. Retry looks again (after something
/// was freed), Skip copies anyway - an overwrite frees what it
/// replaces, which this does not count - and Abort stops.
fn room_for(ctx: &mut Ctx, dest: &Path, need: u64) -> Result<(), Aborted> {
    loop {
        let Some(have) = free_bytes(dest) else {
            return Ok(());
        };
        if have >= need {
            return Ok(());
        }
        let message = format!(
            "not enough space: the copy needs {}, {} is free - Skip copies anyway",
            human(need),
            human(have)
        );
        match ctx.ask_error(dest, message)? {
            Decision::Retry => continue,
            // asked and answered: not a skipped file
            Decision::Skip => {
                ctx.skipped = ctx.skipped.saturating_sub(1);
                return Ok(());
            }
        }
    }
}

/// Bytes an unprivileged user may still write on the filesystem holding
/// `path` (or its nearest existing ancestor, for a target not made yet).
fn free_bytes(path: &Path) -> Option<u64> {
    #[cfg(test)]
    if let Some(&fake) = FAKE_FREE.lock().unwrap().get(path) {
        return Some(fake);
    }
    let dir = path.ancestors().find(|p| p.exists())?;
    let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0)
        .then(|| st.f_bavail as u64 * st.f_frsize as u64)
}

/// The free space a test says a directory has, in place of a disk small
/// enough to fill.
#[cfg(test)]
static FAKE_FREE: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<PathBuf, u64>>> =
    std::sync::LazyLock::new(Default::default);

/// "1.5G"-style, for a message.
fn human(bytes: u64) -> String {
    let mut value = bytes as f64;
    for unit in ["B", "K", "M", "G", "T"] {
        if value < 1024.0 || unit == "T" {
            return match unit {
                "B" => format!("{bytes}B"),
                _ => format!("{value:.1}{unit}"),
            };
        }
        value /= 1024.0;
    }
    unreachable!()
}

pub fn spawn_move(
    sources: Vec<PathBuf>,
    dest: PathBuf,
    opts: TransferOpts,
    rename: Option<Rename>,
) -> JobHandle {
    spawn_with(opts, move |ctx| {
        let sources = filter_sources(sources, rename.as_ref());
        // Totals start as item counts; a cross-device fallback re-announces
        // them with real file/byte numbers for that subtree.
        let mut totals = (sources.len() as u64, 0u64);
        let _ = ctx.tx.send(JobEvent::Total {
            files: totals.0,
            bytes: totals.1,
        });
        let multiple = sources.len() > 1;
        let into_dir = dest.is_dir() || multiple;
        for src in &sources {
            if ctx.cancelled() {
                return Err(Aborted);
            }
            let target = renamed_target(src, &dest, multiple, into_dir, ctx.opts, rename.as_ref());
            move_one(ctx, src, &target, &mut totals)?;
        }
        Ok(())
    })
}

/// Put moved items back: every pair is `(from, to)` as it was reported
/// by [`JobEvent::Moved`], walked newest first so a move that displaced
/// another is undone in the order it happened. A pair whose item is no
/// longer at `to`, or whose `from` is occupied again, is left alone and
/// counted as skipped - an undo that overwrote something would be a
/// second accident rather than the end of the first.
pub fn spawn_undo_move(pairs: Vec<(PathBuf, PathBuf)>) -> JobHandle {
    spawn(move |ctx| {
        let _ = ctx.tx.send(JobEvent::Total {
            files: pairs.len() as u64,
            bytes: 0,
        });
        let mut totals = (pairs.len() as u64, 0u64);
        for (from, to) in pairs.iter().rev() {
            if ctx.cancelled() {
                return Err(Aborted);
            }
            if !to.exists() {
                ctx.skipped += 1;
                continue;
            }
            if from.exists() {
                ctx.skipped += 1;
                continue;
            }
            if let Some(parent) = from.parent()
                && !parent.exists()
                && fs::create_dir_all(parent).is_err()
            {
                ctx.skipped += 1;
                continue;
            }
            move_one(ctx, to, from, &mut totals)?;
        }
        Ok(())
    })
}

/// Owner and permissions to write. Each `None` leaves that part as it
/// is, which is what makes one job serve both the chmod and the chown
/// windows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Attrs {
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub mode: Option<u32>,
}

impl Attrs {
    fn is_empty(&self) -> bool {
        *self == Attrs::default()
    }
}

/// MC's advanced chown: owner and/or permissions over whole trees. It
/// is a job rather than a loop because a deep tree is not something to
/// walk between two frames of the UI - and because half way through a
/// recursive chmod is exactly when you want a Cancel button.
pub fn spawn_attrs(paths: Vec<PathBuf>, attrs: Attrs, recursive: bool) -> JobHandle {
    spawn(move |ctx| {
        let total = if recursive {
            scan(&paths).0
        } else {
            paths.len() as u64
        };
        let _ = ctx.tx.send(JobEvent::Total {
            files: total,
            bytes: 0,
        });
        if attrs.is_empty() {
            return Ok(());
        }
        for path in &paths {
            set_attrs(ctx, path, attrs, recursive)?;
        }
        Ok(())
    })
}

fn set_attrs(ctx: &mut Ctx, path: &Path, attrs: Attrs, recursive: bool) -> Result<(), Aborted> {
    if ctx.cancelled() {
        return Err(Aborted);
    }
    ctx.progress(path);
    let Some(meta) = ctx.with_retry(path, || path.symlink_metadata())? else {
        return Ok(());
    };
    // children first: a directory that loses its execute bit cannot be
    // walked afterwards, and the recursion has to get in before that
    if recursive && meta.is_dir() {
        let Some(names) = read_names(ctx, path)? else {
            return Ok(());
        };
        for name in names {
            set_attrs(ctx, &path.join(&name), attrs, recursive)?;
        }
    }
    let writer = crate::vfs::LocalFs;
    if attrs.uid.is_some() || attrs.gid.is_some() {
        let done = ctx.with_retry(path, || {
            use crate::vfs::FsWrite;
            writer.set_owner(path, attrs.uid, attrs.gid)
        })?;
        if done.is_none() {
            return Ok(());
        }
    }
    // chmod follows symlinks, so a link's own mode is not a thing to set
    if let Some(mode) = attrs.mode
        && !meta.is_symlink()
    {
        let done = ctx.with_retry(path, || {
            use crate::vfs::FsWrite;
            writer.set_mode(path, mode)
        })?;
        if done.is_none() {
            return Ok(());
        }
    }
    ctx.files_done += 1;
    ctx.progress(path);
    Ok(())
}

pub fn spawn_delete(paths: Vec<PathBuf>, permanent: bool) -> JobHandle {
    spawn(move |ctx| {
        if permanent {
            let (files, _) = scan(&paths);
            let _ = ctx.tx.send(JobEvent::Total { files, bytes: 0 });
            for path in &paths {
                delete_tree(ctx, path)?;
            }
        } else {
            let _ = ctx.tx.send(JobEvent::Total {
                files: paths.len() as u64,
                bytes: 0,
            });
            for path in &paths {
                if ctx.cancelled() {
                    return Err(Aborted);
                }
                ctx.progress(path);
                if ctx.with_retry(path, || trash::delete(path))?.is_some() {
                    ctx.files_done += 1;
                    let _ = ctx.tx.send(JobEvent::Trashed { path: path.clone() });
                    ctx.progress(path);
                }
            }
        }
        Ok(())
    })
}

/// Take things out of the trash, back where they came from. `by_origin`
/// names them by where they were - an undo of F8, which knows nothing
/// else - and otherwise they are paths in a `trash://` panel, which is
/// F6 there. Something in the way at home is an error to Retry or Skip,
/// never an overwrite.
pub fn spawn_restore(
    trash: Arc<crate::trashcan::TrashFs>,
    paths: Vec<PathBuf>,
    by_origin: bool,
) -> JobHandle {
    spawn(move |ctx| {
        let _ = ctx.tx.send(JobEvent::Total {
            files: paths.len() as u64,
            bytes: 0,
        });
        for path in &paths {
            if ctx.cancelled() {
                return Err(Aborted);
            }
            ctx.progress(path);
            let restore = || match by_origin {
                true => {
                    let item = trash.find(path).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "no longer in the trash")
                    })?;
                    crate::trashcan::restore(&item).map(|()| item.original)
                }
                false => trash.restore(path),
            };
            if let Some(back) = ctx.with_retry(path, restore)? {
                ctx.files_done += 1;
                let _ = ctx.tx.send(JobEvent::Restored { path: back });
                ctx.progress(path);
            }
        }
        Ok(())
    })
}

/// Write a `sha256sum`-format checksum file for `paths` - one
/// `hash  name` line each, the names relative to `dir` so the file can
/// be checked next to what it describes, as `sha256sum -c` would.
pub fn spawn_checksums(dir: PathBuf, paths: Vec<PathBuf>, out: PathBuf) -> JobHandle {
    spawn(move |ctx| {
        let (files, bytes) = scan(&paths);
        let _ = ctx.tx.send(JobEvent::Total { files, bytes });
        let mut lines = String::new();
        for path in &paths {
            if ctx.cancelled() {
                return Err(Aborted);
            }
            for file in files_under(path) {
                if ctx.cancelled() {
                    return Err(Aborted);
                }
                ctx.progress(&file);
                match sha256_file(&file) {
                    Ok(hash) => {
                        let name = relative_to(&file, &dir).unwrap_or_else(|| file.clone());
                        lines.push_str(&format!("{hash}  {}\n", name.display()));
                        ctx.files_done += 1;
                        ctx.bytes_done += fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
                        ctx.progress(&file);
                    }
                    Err(err) => ctx.error(&file, &err.to_string())?,
                }
            }
        }
        if let Err(err) = fs::write(&out, lines) {
            ctx.error(&out, &err.to_string())?;
        }
        Ok(())
    })
}

/// Check a `sha256sum`-format file: every line is hashed again and
/// compared. `files_done` counts the ones that matched and `skipped`
/// the ones that did not - a missing file counts as not matching,
/// because it does not.
pub fn spawn_verify_checksums(dir: PathBuf, sums: PathBuf) -> JobHandle {
    spawn(move |ctx| {
        let text = match fs::read_to_string(&sums) {
            Ok(text) => text,
            Err(err) => return ctx.error(&sums, &err.to_string()),
        };
        let lines: Vec<(String, String)> = text
            .lines()
            .filter_map(|line| line.split_once("  "))
            .map(|(hash, name)| (hash.trim().to_lowercase(), name.trim().to_string()))
            .collect();
        let _ = ctx.tx.send(JobEvent::Total {
            files: lines.len() as u64,
            bytes: 0,
        });
        for (want, name) in lines {
            if ctx.cancelled() {
                return Err(Aborted);
            }
            let path = dir.join(&name);
            ctx.progress(&path);
            match sha256_file(&path) {
                Ok(have) if have == want => ctx.files_done += 1,
                _ => ctx.skipped += 1,
            }
            ctx.progress(&path);
        }
        Ok(())
    })
}

/// Every regular file under a path, the path itself if it is one.
fn files_under(path: &Path) -> Vec<PathBuf> {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Vec::new();
    };
    if meta.is_file() {
        return vec![path.to_path_buf()];
    }
    if !meta.is_dir() {
        return Vec::new(); // a symlink is not a file to checksum
    }
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(path) {
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            out.extend(files_under(&path));
        }
    }
    out
}

/// The SHA-256 of a file, lowercase hex - what `sha256sum` writes.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Overwrite every byte of each file, then unlink it. Far calls this
/// wipe and keeps it on Alt+Del.
///
/// What it promises is what it does: the bytes that were in those
/// blocks are written over once, and `fsync` is asked to put that on
/// the device. What it cannot promise is that no copy survives - a
/// copy-on-write filesystem writes the zeroes somewhere else, an SSD's
/// controller may do the same, and a snapshot or a backup was never
/// this file's to overwrite. It is a better delete, not an erasure.
pub fn spawn_wipe(paths: Vec<PathBuf>) -> JobHandle {
    spawn(move |ctx| {
        let (files, bytes) = scan(&paths);
        let _ = ctx.tx.send(JobEvent::Total { files, bytes });
        for path in &paths {
            wipe_tree(ctx, path)?;
        }
        Ok(())
    })
}

fn wipe_tree(ctx: &mut Ctx, path: &Path) -> Result<(), Aborted> {
    if ctx.cancelled() {
        return Err(Aborted);
    }
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) => return ctx.error(path, &err.to_string()),
    };
    // a symlink has no bytes of its own worth overwriting; the file it
    // points at is not the one that was asked for
    if meta.is_dir() {
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(err) => return ctx.error(path, &err.to_string()),
        };
        for entry in entries.flatten() {
            wipe_tree(ctx, &entry.path())?;
        }
        return match fs::remove_dir(path) {
            Ok(()) => {
                ctx.files_done += 1;
                ctx.progress(path);
                Ok(())
            }
            Err(err) => ctx.error(path, &err.to_string()),
        };
    }
    if meta.is_file()
        && let Err(err) = overwrite(path, meta.len())
    {
        return ctx.error(path, &err.to_string());
    }
    match fs::remove_file(path) {
        Ok(()) => {
            ctx.files_done += 1;
            ctx.bytes_done += meta.len();
            ctx.progress(path);
            Ok(())
        }
        Err(err) => ctx.error(path, &err.to_string()),
    }
}

/// One pass of zeroes over the whole file, flushed to the device.
fn overwrite(path: &Path, len: u64) -> io::Result<()> {
    let mut file = fs::OpenOptions::new().write(true).open(path)?;
    let zeros = vec![0u8; 64 * 1024];
    let mut left = len;
    while left > 0 {
        let take = left.min(zeros.len() as u64) as usize;
        file.write_all(&zeros[..take])?;
        left -= take as u64;
    }
    file.sync_all()
}

/// Copy out of a read-only [`FsProvider`] (an archive) onto the local
/// filesystem, with the same progress/overwrite/error protocol as copy.
pub fn spawn_extract(fs: Arc<dyn FsProvider>, sources: Vec<PathBuf>, dest: PathBuf) -> JobHandle {
    spawn(move |ctx| {
        let mut sources = sources;
        fs.read_order(&mut sources);
        let (files, bytes) = scan_provider(&*fs, &sources);
        let _ = ctx.tx.send(JobEvent::Total { files, bytes });
        let _unpacked = prefetch(ctx, &*fs, &sources, Some(&dest))?;
        let into_dir = dest.is_dir() || sources.len() > 1;
        for src in &sources {
            if ctx.cancelled() {
                return Err(Aborted);
            }
            extract_tree(ctx, &*fs, src, &target_for(src, &dest, into_dir))?;
        }
        Ok(())
    })
}

/// Recursive size of one tree (files, bytes) for Ctrl+Space; one message
/// on completion, receiver-drop cancels nothing but the result is cheap.
pub fn spawn_dir_size(path: PathBuf) -> Receiver<(u64, u64)> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let totals = scan(std::slice::from_ref(&path));
        let _ = tx.send(totals);
    });
    rx
}

/// Ctrl+Space on a non-local panel: the same totals via [`FsProvider`]
/// traversal (sftp round-trips, archive walks) on a worker thread.
pub fn spawn_dir_size_fs(fs: Arc<dyn FsProvider>, path: PathBuf) -> Receiver<(u64, u64)> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let totals = scan_provider(&*fs, std::slice::from_ref(&path));
        let _ = tx.send(totals);
    });
    rx
}

/// Copy or move across providers: upload (local→remote), download
/// (remote→local) and remote↔remote all stream through the same chunk
/// loop; the dialogs protocol is identical to the local jobs. A move on
/// one provider tries `rename` first and degrades to copy+delete.
pub fn spawn_transfer(
    src_fs: Arc<dyn FsProvider>,
    sources: Vec<PathBuf>,
    dst_fs: Arc<dyn FsProvider>,
    dest: PathBuf,
    move_mode: bool,
    opts: TransferOpts,
    rename: Option<Rename>,
) -> JobHandle {
    spawn_with(opts, move |ctx| {
        let mut sources = filter_sources(sources, rename.as_ref());
        src_fs.read_order(&mut sources);
        let multiple = sources.len() > 1;
        // where each source lands, the masks and the dive switch asked
        // of the provider rather than of a local path
        let target = |src: &Path, into_dir: bool| -> PathBuf {
            if let Some(name) = rename.as_ref().and_then(|r| r.name_for(src)) {
                return dest.join(name);
            }
            let dir = src_fs.stat(src).map(|e| e.is_dir()).unwrap_or(false);
            if !multiple && !opts.dive && into_dir && dir {
                return dest.clone();
            }
            target_for(src, &dest, into_dir)
        };
        if dst_fs.writer().is_none() {
            return ctx.error(&dest, "destination is read-only");
        }
        if move_mode && src_fs.writer().is_none() {
            return ctx.error(&dest, "source is read-only - copy instead");
        }
        let same_fs = Arc::ptr_eq(&src_fs, &dst_fs);
        let into_dir = sources.len() > 1 || dst_fs.stat(&dest).map(|e| e.is_dir()).unwrap_or(false);
        if move_mode && same_fs {
            // rename-first, like the local move job: totals are items,
            // a copy fallback re-announces real numbers for its subtree
            let mut totals = (sources.len() as u64, 0u64);
            let _ = ctx.tx.send(JobEvent::Total {
                files: totals.0,
                bytes: totals.1,
            });
            for src in &sources {
                if ctx.cancelled() {
                    return Err(Aborted);
                }
                transfer_move_one(ctx, &*src_fs, src, &target(src, into_dir), &mut totals)?;
            }
        } else {
            let (files, bytes) = scan_provider(&*src_fs, &sources);
            let _ = ctx.tx.send(JobEvent::Total { files, bytes });
            let near = dst_fs.is_local().then_some(dest.as_path());
            let _unpacked = prefetch(ctx, &*src_fs, &sources, near)?;
            for src in &sources {
                if ctx.cancelled() {
                    return Err(Aborted);
                }
                transfer_tree(
                    ctx,
                    &*src_fs,
                    &*dst_fs,
                    src,
                    &target(src, into_dir),
                    same_fs,
                    move_mode,
                )?;
            }
        }
        Ok(())
    })
}

/// One step of a synchronize plan, for the path both roots have under
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStep {
    ToRight,
    ToLeft,
    DeleteLeft,
    DeleteRight,
}

/// Run a synchronize plan between two roots, each on its own provider.
/// A copy replaces what it lands on - opening the plan and leaving the
/// row on was the answer to that question. A delete on a local side
/// goes to the trash, where C-x u can fetch it; a server has no trash,
/// and deletes for good.
pub fn spawn_sync(
    left: (Arc<dyn FsProvider>, PathBuf),
    right: (Arc<dyn FsProvider>, PathBuf),
    steps: Vec<(PathBuf, SyncStep)>,
) -> JobHandle {
    let opts = TransferOpts {
        overwrite: true,
        ..TransferOpts::default()
    };
    spawn_with(opts, move |ctx| {
        let side = |to_right: bool| match to_right {
            true => (&left, &right),
            false => (&right, &left),
        };
        let (mut files, mut bytes) = (0u64, 0u64);
        for (rel, step) in &steps {
            match step {
                SyncStep::ToRight | SyncStep::ToLeft => {
                    let ((fs, root), _) = side(*step == SyncStep::ToRight);
                    let (f, b) = scan_provider(&**fs, &[root.join(rel)]);
                    files += f;
                    bytes += b;
                }
                _ => files += 1,
            }
        }
        let _ = ctx.tx.send(JobEvent::Total { files, bytes });
        for (rel, step) in &steps {
            if ctx.cancelled() {
                return Err(Aborted);
            }
            match step {
                SyncStep::ToRight | SyncStep::ToLeft => {
                    let ((src_fs, src_root), (dst_fs, dst_root)) = side(*step == SyncStep::ToRight);
                    let (src, dst) = (src_root.join(rel), dst_root.join(rel));
                    if dst_fs.writer().is_none() {
                        ctx.error(&dst, "that side is read-only")?;
                        continue;
                    }
                    if src_fs.is_local() && dst_fs.is_local() {
                        ctx.copy_root = Some(src.clone());
                        copy_tree(ctx, &src, &dst)?;
                    } else {
                        transfer_tree(ctx, &**src_fs, &**dst_fs, &src, &dst, false, false)?;
                    }
                }
                SyncStep::DeleteLeft | SyncStep::DeleteRight => {
                    let (fs, root) = match step {
                        SyncStep::DeleteLeft => &left,
                        _ => &right,
                    };
                    let path = root.join(rel);
                    ctx.progress(&path);
                    if fs.is_local() {
                        if ctx.with_retry(&path, || trash::delete(&path))?.is_some() {
                            ctx.files_done += 1;
                            let _ = ctx.tx.send(JobEvent::Trashed { path: path.clone() });
                        }
                    } else if fs.writer().is_none() {
                        ctx.error(&path, "that side is read-only")?;
                    } else {
                        delete_tree_fs(ctx, &**fs, &path)?;
                    }
                    ctx.progress(&path);
                }
            }
        }
        Ok(())
    })
}

/// Delete through a provider's write half (always permanent - there is
/// no remote trash).
pub fn spawn_delete_fs(fs: Arc<dyn FsProvider>, paths: Vec<PathBuf>) -> JobHandle {
    spawn(move |ctx| {
        if fs.writer().is_none() {
            let first = paths.first().cloned().unwrap_or_default();
            return ctx.error(&first, "filesystem is read-only");
        }
        let (files, _) = scan_provider(&*fs, &paths);
        let _ = ctx.tx.send(JobEvent::Total { files, bytes: 0 });
        for path in &paths {
            delete_tree_fs(ctx, &*fs, path)?;
        }
        Ok(())
    })
}

fn transfer_move_one(
    ctx: &mut Ctx,
    fs: &dyn FsProvider,
    src: &Path,
    dst: &Path,
    totals: &mut (u64, u64),
) -> Result<(), Aborted> {
    ctx.progress(src);
    if src == dst {
        return ctx.error(src, "source and destination are the same file");
    }
    if dst.starts_with(src) {
        return ctx.error(src, "cannot move a directory into itself");
    }
    if ctx.may_overwrite_fs(fs, FileFacts::of_path(src), dst)? == Overwrite::Skip {
        return Ok(());
    }
    let writer = fs.writer().expect("checked in spawn_transfer");
    match writer.rename(src, dst) {
        Ok(()) => {
            ctx.files_done += 1;
            ctx.progress(src);
            Ok(())
        }
        Err(_) => {
            // fall back to copy + delete, with honest totals for it
            let (files, bytes) = scan_provider(fs, std::slice::from_ref(&src.to_path_buf()));
            totals.0 = totals.0.saturating_sub(1) + files;
            totals.1 += bytes;
            let _ = ctx.tx.send(JobEvent::Total {
                files: totals.0,
                bytes: totals.1,
            });
            transfer_tree(ctx, fs, fs, src, dst, true, true)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn transfer_tree(
    ctx: &mut Ctx,
    src_fs: &dyn FsProvider,
    dst_fs: &dyn FsProvider,
    src: &Path,
    dst: &Path,
    same_fs: bool,
    move_mode: bool,
) -> Result<(), Aborted> {
    if ctx.cancelled() {
        return Err(Aborted);
    }
    let Some(entry) = ctx.with_retry(src, || src_fs.stat(src))? else {
        return Ok(());
    };
    let writer = dst_fs.writer().expect("checked in spawn_transfer");
    // "follow links" copies what a link points at: reading the link's
    // path through the provider reaches its target
    let kind = match (entry.kind, ctx.opts.follow_links) {
        (EntryKind::SymlinkFile, true) => EntryKind::File,
        (EntryKind::SymlinkDir, true) => EntryKind::Dir,
        (kind, _) => kind,
    };
    match kind {
        EntryKind::Dir => {
            if same_fs && dst.starts_with(src) {
                return ctx.error(src, "cannot copy a directory into itself");
            }
            let created = ctx.with_retry(dst, || match writer.mkdir(dst) {
                Err(_) if dst_fs.stat(dst).map(|e| e.is_dir()).unwrap_or(false) => Ok(()),
                other => other,
            })?;
            if created.is_none() {
                return Ok(());
            }
            let Some(children) = ctx.with_retry(src, || src_fs.read_dir(src))? else {
                return Ok(());
            };
            for child in children {
                transfer_tree(
                    ctx,
                    src_fs,
                    dst_fs,
                    &src.join(&child.name),
                    &dst.join(&child.name),
                    same_fs,
                    move_mode,
                )?;
            }
            if ctx.opts.preserve
                && let Some(modified) = entry.mtime
            {
                let _ = writer.set_mtime(dst, modified);
            }
            if move_mode && let Some(sw) = src_fs.writer() {
                let _ = ctx.with_retry(src, || sw.remove_dir(src))?;
            }
            Ok(())
        }
        EntryKind::SymlinkDir | EntryKind::SymlinkFile | EntryKind::SymlinkBroken => {
            ctx.progress(src);
            if ctx.may_overwrite_fs(dst_fs, FileFacts::of_entry(&entry), dst)? == Overwrite::Skip {
                return Ok(());
            }
            let target = entry.link_target.clone().unwrap_or_default();
            let done = ctx.with_retry(src, || {
                let _ = writer.remove_file(dst); // overwrite was approved above
                writer.symlink(&target, dst)
            })?;
            if done.is_some() {
                ctx.files_done += 1;
                ctx.progress(src);
                if move_mode && let Some(sw) = src_fs.writer() {
                    let _ = ctx.with_retry(src, || sw.remove_file(src))?;
                }
            }
            Ok(())
        }
        EntryKind::File => {
            ctx.progress(src);
            if same_fs && src == dst {
                return ctx.error(src, "source and destination are the same file");
            }
            let mode = ctx.may_overwrite_fs(dst_fs, FileFacts::of_entry(&entry), dst)?;
            if mode == Overwrite::Skip {
                return Ok(());
            }
            transfer_file(ctx, src_fs, dst_fs, src, dst, &entry, move_mode, mode)?;
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn transfer_file(
    ctx: &mut Ctx,
    src_fs: &dyn FsProvider,
    dst_fs: &dyn FsProvider,
    src: &Path,
    dst: &Path,
    entry: &crate::entry::Entry,
    move_mode: bool,
    mode: Overwrite,
) -> Result<(), Aborted> {
    loop {
        if ctx.cancelled() {
            return Err(Aborted);
        }
        let start = ctx.bytes_done;
        match try_transfer_file(ctx, src_fs, dst_fs, src, dst, entry, mode) {
            Ok(()) => {
                ctx.files_done += 1;
                ctx.bytes_done = start + entry.size;
                ctx.progress(src);
                if move_mode && let Some(sw) = src_fs.writer() {
                    let _ = ctx.with_retry(src, || sw.remove_file(src))?;
                }
                return Ok(());
            }
            Err(CopyErr::Cancelled) => return Err(Aborted),
            Err(CopyErr::Io(err)) => {
                ctx.bytes_done = start;
                match ctx.ask_error(src, err.to_string())? {
                    Decision::Retry => continue,
                    Decision::Skip => return Ok(()),
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn try_transfer_file(
    ctx: &mut Ctx,
    src_fs: &dyn FsProvider,
    dst_fs: &dyn FsProvider,
    src: &Path,
    dst: &Path,
    entry: &crate::entry::Entry,
    mode: Overwrite,
) -> Result<(), CopyErr> {
    let writer = dst_fs.writer().expect("checked in spawn_transfer");
    ctx.begin_file(entry.size);
    // Reget: what is there is taken to be the head of the source
    let have = match mode {
        Overwrite::Reget => dst_fs.stat(dst).map(|e| e.size).unwrap_or(0),
        _ => 0,
    };
    if mode == Overwrite::Reget && have >= entry.size {
        return Ok(());
    }
    let fresh = mode == Overwrite::Replace;
    // a fresh copy over a file already there is written beside it and
    // put in its place once complete: a copy that fails leaves the old
    // file as it was, where writing in place had already truncated it
    let staged = fresh && dst_fs.stat(dst).is_ok();
    let written = match staged {
        true => staging_name(dst),
        false => dst.to_path_buf(),
    };
    let mut input = match mode {
        Overwrite::Reget => src_fs.open_read_at(src, have),
        _ => src_fs.open_read(src),
    }
    .map_err(CopyErr::Io)?;
    let mut output = match fresh {
        true => writer.open_write(&written),
        false => writer.open_append(&written),
    }
    .map_err(CopyErr::Io)?;
    ctx.bytes_done += have;
    ctx.file_done += have;
    let mut buf = vec![0u8; CHUNK];
    let copied = (|| {
        loop {
            if ctx.cancelled() {
                return Err(CopyErr::Cancelled);
            }
            let n = input.read(&mut buf).map_err(CopyErr::Io)?;
            if n == 0 {
                break;
            }
            output.write_all(&buf[..n]).map_err(CopyErr::Io)?;
            ctx.bytes_done += n as u64;
            ctx.file_done += n as u64;
            ctx.progress(src);
        }
        output.flush().map_err(CopyErr::Io)
    })();
    drop(output); // remote handles must close before setstat
    if copied.is_err() {
        // cancelled or failed: don't leave a torso behind - but an
        // append keeps what it was adding to
        if fresh {
            let _ = writer.remove_file(&written);
        }
        return copied;
    }
    if staged {
        // not every server renames over a name that is taken
        let _ = writer.remove_file(dst);
        if let Err(err) = writer.rename(&written, dst) {
            let _ = writer.remove_file(&written);
            return Err(CopyErr::Io(err));
        }
    }
    // an appended-to file is the target with more in it, not a copy
    if fresh {
        // across machines a uid means someone else: the special bits do
        // not travel, the way they do not out of an archive
        if ctx.opts.preserve && entry.mode != 0 {
            let _ = writer.set_mode(dst, entry.mode & 0o777);
        }
        if ctx.opts.preserve
            && let Some(modified) = entry.mtime
        {
            let _ = writer.set_mtime(dst, modified);
        }
    }
    if ctx.opts.verify {
        verify_fs(src_fs, src, dst_fs, dst).map_err(CopyErr::Io)?;
    }
    Ok(())
}

/// A hidden name beside `dst` for a provider copy to be staged under.
fn staging_name(dst: &Path) -> PathBuf {
    let name = dst.file_name().unwrap_or_default().to_string_lossy();
    dst.with_file_name(format!(".{name}.rcmd-{}", std::process::id()))
}

fn delete_tree_fs(ctx: &mut Ctx, fs: &dyn FsProvider, path: &Path) -> Result<(), Aborted> {
    if ctx.cancelled() {
        return Err(Aborted);
    }
    let Some(entry) = ctx.with_retry(path, || fs.stat(path))? else {
        return Ok(());
    };
    let writer = fs.writer().expect("checked in spawn_delete_fs");
    if entry.kind == EntryKind::Dir {
        let Some(children) = ctx.with_retry(path, || fs.read_dir(path))? else {
            return Ok(());
        };
        for child in children {
            delete_tree_fs(ctx, fs, &path.join(&child.name))?;
        }
        ctx.with_retry(path, || writer.remove_dir(path))?;
    } else {
        ctx.progress(path);
        if ctx.with_retry(path, || writer.remove_file(path))?.is_some() {
            ctx.files_done += 1;
        }
    }
    Ok(())
}

struct Aborted;

enum Decision {
    Retry,
    Skip,
}

enum CopyErr {
    Io(io::Error),
    Cancelled,
}

struct Ctx {
    tx: Sender<JobEvent>,
    rx: Receiver<Reply>,
    cancel: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    files_done: u64,
    bytes_done: u64,
    skipped: u64,
    /// The sticky answer to existing targets, once one has been given.
    policy: Policy,
    skip_all_errors: bool,
    opts: TransferOpts,
    /// The source currently being copied, so a symlink can tell whether
    /// it points inside the copy or out of it.
    copy_root: Option<PathBuf>,
    /// Files with more than one name, by device and inode, and where the
    /// first of those names was copied to: the second becomes a link to
    /// the copy, as the source's was, instead of a second file.
    links: std::collections::HashMap<(u64, u64), PathBuf>,
    /// Bytes written of the file in hand, and its size.
    file_done: u64,
    file_total: u64,
}

impl Ctx {
    /// Whether to stop - and the place a paused job waits, since every
    /// loop asks this between one piece of work and the next.
    fn cancelled(&self) -> bool {
        while self.pause.load(Ordering::Relaxed) && !self.cancel.load(Ordering::Relaxed) {
            thread::sleep(std::time::Duration::from_millis(50));
        }
        self.cancel.load(Ordering::Relaxed)
    }

    /// Report a move that can be undone. `clean` is false where the
    /// destination was already taken, which is the one case putting the
    /// source back would not restore.
    fn moved(&self, clean: bool, from: &Path, to: &Path) {
        if clean {
            let _ = self.tx.send(JobEvent::Moved {
                from: from.to_path_buf(),
                to: to.to_path_buf(),
            });
        }
    }

    fn progress(&self, current: &Path) {
        let _ = self.tx.send(JobEvent::Progress {
            files_done: self.files_done,
            bytes_done: self.bytes_done,
            current: current.to_path_buf(),
            file_done: self.file_done,
            file_total: self.file_total,
        });
    }

    /// Start counting a file's own bytes; `progress` reports them until
    /// the next file replaces them.
    fn begin_file(&mut self, size: u64) {
        self.file_done = 0;
        self.file_total = size;
    }

    fn ask_error(&mut self, path: &Path, message: String) -> Result<Decision, Aborted> {
        if self.skip_all_errors {
            self.skipped += 1;
            self.report(path, &message);
            return Ok(Decision::Skip);
        }
        if self
            .tx
            .send(JobEvent::AskError {
                path: path.to_path_buf(),
                message: message.clone(),
            })
            .is_err()
        {
            return Err(Aborted);
        }
        match self.rx.recv() {
            Ok(Reply::Retry) => Ok(Decision::Retry),
            Ok(Reply::Skip) => {
                self.skipped += 1;
                self.report(path, &message);
                Ok(Decision::Skip)
            }
            Ok(Reply::SkipAll) => {
                self.skip_all_errors = true;
                self.skipped += 1;
                self.report(path, &message);
                Ok(Decision::Skip)
            }
            _ => Err(Aborted),
        }
    }

    /// Run `op`, letting the user retry/skip/abort on failure.
    /// Ok(Some(v)) on success, Ok(None) if the item was skipped.
    fn with_retry<T, E: std::fmt::Display>(
        &mut self,
        path: &Path,
        mut op: impl FnMut() -> Result<T, E>,
    ) -> Result<Option<T>, Aborted> {
        loop {
            if self.cancelled() {
                return Err(Aborted);
            }
            match op() {
                Ok(v) => return Ok(Some(v)),
                Err(err) => match self.ask_error(path, err.to_string())? {
                    Decision::Retry => continue,
                    Decision::Skip => return Ok(None),
                },
            }
        }
    }

    /// Present a permanent error; only Skip/SkipAll/Abort make progress.
    fn error(&mut self, path: &Path, message: &str) -> Result<(), Aborted> {
        loop {
            match self.ask_error(path, message.to_string())? {
                Decision::Retry => continue,
                Decision::Skip => return Ok(()),
            }
        }
    }

    /// What to do about `dst` already existing. `can_append` says
    /// whether Append and Reget are on the table - they need a local
    /// file to open, so only the plain file copy offers them.
    fn may_overwrite(
        &mut self,
        src: FileFacts,
        dst: &Path,
        can_append: bool,
    ) -> Result<Overwrite, Aborted> {
        match dst.symlink_metadata() {
            Ok(meta) => self.decide_overwrite(
                src,
                FileFacts {
                    size: meta.len(),
                    mtime: meta.modified().ok(),
                },
                dst,
                can_append,
            ),
            Err(_) => Ok(Overwrite::Replace), // nothing there
        }
    }

    /// Provider-aware variant: existence and facts come through `fs`,
    /// and appending is never offered - a provider hands out a writer,
    /// not a file to seek in.
    fn may_overwrite_fs(
        &mut self,
        fs: &dyn FsProvider,
        src: FileFacts,
        dst: &Path,
    ) -> Result<Overwrite, Aborted> {
        // Append and Reget where this provider can add to a file
        let can_append = fs.writer().is_some_and(|w| w.can_append());
        match fs.stat(dst) {
            Ok(entry) => self.decide_overwrite(src, FileFacts::of_entry(&entry), dst, can_append),
            Err(_) => Ok(Overwrite::Replace),
        }
    }

    fn decide_overwrite(
        &mut self,
        src: FileFacts,
        dst_facts: FileFacts,
        dst: &Path,
        can_append: bool,
    ) -> Result<Overwrite, Aborted> {
        if self.opts.overwrite {
            return Ok(Overwrite::Replace);
        }
        match self.policy {
            Policy::All => return Ok(Overwrite::Replace),
            Policy::None => return Ok(self.skip(dst)),
            Policy::Newer => return Ok(self.sticky(src.newer_than(dst_facts), dst)),
            Policy::SizeDiffers => return Ok(self.sticky(src.size != dst_facts.size, dst)),
            Policy::Ask => {}
        }
        if self
            .tx
            .send(JobEvent::AskOverwrite {
                path: dst.to_path_buf(),
                src,
                dst: dst_facts,
                can_append,
            })
            .is_err()
        {
            return Err(Aborted);
        }
        match self.rx.recv() {
            Ok(Reply::Overwrite) => Ok(Overwrite::Replace),
            Ok(Reply::Append) => Ok(Overwrite::Append),
            Ok(Reply::Reget) => Ok(Overwrite::Reget),
            Ok(Reply::OverwriteAll) => {
                self.policy = Policy::All;
                Ok(Overwrite::Replace)
            }
            // the sticky answers decide this file too, not just the rest
            Ok(Reply::UpdateAll) => {
                self.policy = Policy::Newer;
                Ok(self.sticky(src.newer_than(dst_facts), dst))
            }
            Ok(Reply::SizeDiffersAll) => {
                self.policy = Policy::SizeDiffers;
                Ok(self.sticky(src.size != dst_facts.size, dst))
            }
            Ok(Reply::Skip) => Ok(self.skip(dst)),
            Ok(Reply::SkipAll) => {
                self.policy = Policy::None;
                Ok(self.skip(dst))
            }
            _ => Err(Aborted),
        }
    }

    fn sticky(&mut self, replace: bool, dst: &Path) -> Overwrite {
        if replace {
            Overwrite::Replace
        } else {
            self.skip(dst)
        }
    }

    fn skip(&mut self, dst: &Path) -> Overwrite {
        self.skipped += 1;
        self.report(dst, "already there, not overwritten");
        Overwrite::Skip
    }

    /// A line of the skip report.
    fn report(&self, path: &Path, reason: &str) {
        let _ = self.tx.send(JobEvent::Skipped {
            path: path.to_path_buf(),
            reason: reason.to_string(),
        });
    }
}

fn spawn(work: impl FnOnce(&mut Ctx) -> Result<(), Aborted> + Send + 'static) -> JobHandle {
    spawn_with(TransferOpts::default(), work)
}

fn spawn_with(
    opts: TransferOpts,
    work: impl FnOnce(&mut Ctx) -> Result<(), Aborted> + Send + 'static,
) -> JobHandle {
    let (event_tx, event_rx) = mpsc::channel();
    let (reply_tx, reply_rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    let pause = Arc::new(AtomicBool::new(false));
    let worker_pause = pause.clone();
    let held = Arc::new(AtomicBool::new(HOLD.with(|hold| hold.get())));
    let worker_held = held.clone();
    let thread = thread::spawn(move || {
        while worker_held.load(Ordering::Relaxed) && !worker_cancel.load(Ordering::Relaxed) {
            thread::sleep(std::time::Duration::from_millis(50));
        }
        let mut ctx = Ctx {
            tx: event_tx,
            rx: reply_rx,
            cancel: worker_cancel,
            pause: worker_pause,
            files_done: 0,
            bytes_done: 0,
            skipped: 0,
            policy: Policy::Ask,
            skip_all_errors: false,
            opts,
            copy_root: None,
            links: std::collections::HashMap::new(),
            file_done: 0,
            file_total: 0,
        };
        let aborted = work(&mut ctx).is_err();
        let _ = ctx.tx.send(JobEvent::Done {
            files_done: ctx.files_done,
            skipped: ctx.skipped,
            aborted,
        });
    });
    JobHandle {
        events: event_rx,
        replies: reply_tx,
        cancel,
        pause,
        held,
        thread: Some(thread),
    }
}

/// Count files and bytes ahead of a copy/delete; errors here are ignored,
/// they will surface as dialogs during the real operation.
fn scan(paths: &[PathBuf]) -> (u64, u64) {
    fn walk(path: &Path, files: &mut u64, bytes: &mut u64) {
        let Ok(meta) = path.symlink_metadata() else {
            return;
        };
        if meta.is_dir() {
            if let Ok(rd) = fs::read_dir(path) {
                for dent in rd.flatten() {
                    walk(&dent.path(), files, bytes);
                }
            }
        } else {
            *files += 1;
            if meta.is_file() {
                *bytes += meta.len();
            }
        }
    }
    let (mut files, mut bytes) = (0, 0);
    for path in paths {
        walk(path, &mut files, &mut bytes);
    }
    (files, bytes)
}

/// MC's stable symlinks: a *relative* link copied somewhere else would
/// point at a different file, so its value is recomputed from the new
/// location back to the same target.
///
/// With one refinement mc does not make, and which is very likely why mc
/// ships this switched off: a link pointing *inside* the tree being
/// copied is left exactly as it is. Rewriting those would aim the copy
/// back at the original tree, leaving it depending on a directory the
/// user may be about to delete; leaving them keeps the copy
/// self-contained. Only links reaching outside the copy - the ones that
/// would otherwise break - are rewritten.
fn stable_link_target(target: &Path, src: &Path, dst: &Path, root: Option<&Path>) -> PathBuf {
    if target.is_absolute() {
        return target.to_path_buf();
    }
    let (Some(src_dir), Some(dst_dir)) = (src.parent(), dst.parent()) else {
        return target.to_path_buf();
    };
    let pointed_at = lexical_join(src_dir, target);
    if let Some(root) = root
        && pointed_at.starts_with(root)
    {
        return target.to_path_buf();
    }
    relative_to(&pointed_at, &lexical_join(dst_dir, Path::new(""))).unwrap_or(pointed_at)
}

/// `base` + `rest`, with `.` dropped and `..` cancelled against the
/// component before it. Purely textual: the file it names need not
/// exist, which matters for a link that is copied before its target is.
pub fn lexical_join(base: &Path, rest: &Path) -> PathBuf {
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    let mut absolute = false;
    for component in base.components().chain(rest.components()) {
        match component {
            Component::RootDir => {
                absolute = true;
                out.clear();
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if out.last().is_some_and(|last| last != "..") {
                    out.pop();
                } else if !absolute {
                    out.push("..".into());
                }
            }
            other => out.push(other.as_os_str().to_os_string()),
        }
    }
    let mut path = if absolute {
        PathBuf::from("/")
    } else {
        PathBuf::new()
    };
    path.extend(out);
    path
}

/// `path` written relative to `base`, with `..` for each level that has
/// to be climbed. Both are taken as literal component lists.
pub fn relative_to(path: &Path, base: &Path) -> Option<PathBuf> {
    let mut theirs = path.components().peekable();
    let mut ours = base.components().peekable();
    while theirs.peek().is_some() && theirs.peek() == ours.peek() {
        theirs.next();
        ours.next();
    }
    let mut out = PathBuf::new();
    for _ in ours {
        out.push("..");
    }
    out.extend(theirs);
    (!out.as_os_str().is_empty()).then_some(out)
}

fn target_for(src: &Path, dest: &Path, into_dir: bool) -> PathBuf {
    if into_dir {
        dest.join(src.file_name().unwrap_or_default())
    } else {
        dest.to_path_buf()
    }
}

/// Sources the mask leaves out never take part - mc copies "all the
/// files matching the source mask" and quietly passes over the rest.
fn filter_sources(sources: Vec<PathBuf>, rename: Option<&Rename>) -> Vec<PathBuf> {
    match rename {
        Some(rename) => sources
            .into_iter()
            .filter(|src| rename.accepts(src))
            .collect(),
        None => sources,
    }
}

/// Where one source lands once a target mask has had its say.
fn renamed_target(
    src: &Path,
    dest: &Path,
    multiple: bool,
    into_dir: bool,
    opts: TransferOpts,
    rename: Option<&Rename>,
) -> PathBuf {
    match rename.and_then(|rename| rename.name_for(src)) {
        Some(name) => dest.join(name),
        None => transfer_target(src, dest, multiple, into_dir, opts),
    }
}

/// Where one source lands, with mc's "dive into subdirs" taken into
/// account: turned off, a lone directory copied onto an existing
/// directory merges its *contents* into it instead of landing inside
/// it. Only meaningful for a single source - several sources have to
/// keep their names apart.
fn transfer_target(
    src: &Path,
    dest: &Path,
    multiple: bool,
    into_dir: bool,
    opts: TransferOpts,
) -> PathBuf {
    if !multiple && !opts.dive && into_dir && src.is_dir() {
        return dest.to_path_buf();
    }
    target_for(src, dest, into_dir)
}

fn read_names(ctx: &mut Ctx, dir: &Path) -> Result<Option<Vec<std::ffi::OsString>>, Aborted> {
    ctx.with_retry(dir, || -> io::Result<Vec<std::ffi::OsString>> {
        let mut names = Vec::new();
        for dent in fs::read_dir(dir)? {
            names.push(dent?.file_name());
        }
        Ok(names)
    })
}

fn copy_tree(ctx: &mut Ctx, src: &Path, dst: &Path) -> Result<(), Aborted> {
    if ctx.cancelled() {
        return Err(Aborted);
    }
    // "Follow links" copies what a link points at, so the metadata that
    // decides the branch below is the followed one. A dangling link has
    // nothing to follow and is recreated as a link, which beats failing.
    let meta = if ctx.opts.follow_links {
        match fs::metadata(src) {
            Ok(meta) => Some(meta),
            Err(_) => ctx.with_retry(src, || src.symlink_metadata())?,
        }
    } else {
        ctx.with_retry(src, || src.symlink_metadata())?
    };
    let Some(meta) = meta else {
        return Ok(());
    };
    if meta.is_dir() {
        if inside(ctx, dst, src) {
            return ctx.error(src, "cannot copy a directory into itself");
        }
        let created = ctx.with_retry(dst, || match fs::create_dir(dst) {
            Err(ref e) if e.kind() == io::ErrorKind::AlreadyExists && dst.is_dir() => Ok(()),
            other => other,
        })?;
        if created.is_none() {
            return Ok(());
        }
        let Some(names) = read_names(ctx, src)? else {
            return Ok(());
        };
        for name in names {
            copy_tree(ctx, &src.join(&name), &dst.join(&name))?;
        }
        // after the children, so their creation doesn't bump it again
        if ctx.opts.preserve
            && let Ok(dir) = fs::File::open(dst)
        {
            preserve_attrs(&dir, src, &meta);
        }
        Ok(())
    } else if meta.is_symlink() {
        ctx.progress(src);
        if src == dst || same_file(&meta, dst, false) {
            return ctx.error(src, "source and destination are the same file");
        }
        if ctx.may_overwrite(FileFacts::of_path(src), dst, false)? == Overwrite::Skip {
            return Ok(());
        }
        let stable = ctx.opts.stable_symlinks;
        let root = ctx.copy_root.clone();
        let done = ctx.with_retry(src, || {
            let target = fs::read_link(src)?;
            let target = if stable {
                stable_link_target(&target, src, dst, root.as_deref())
            } else {
                target
            };
            let _ = fs::remove_file(dst); // overwrite was approved above
            make_symlink(&target, dst)
        })?;
        if done.is_some() {
            ctx.files_done += 1;
            ctx.progress(src);
        }
        Ok(())
    } else if is_special(&meta) {
        // mc's way: a FIFO, socket or device is made again at the
        // target, never opened and read - a FIFO would block the job
        // until a writer came, and a device is not its contents
        ctx.progress(src);
        if ctx.may_overwrite(FileFacts::of_path(src), dst, false)? == Overwrite::Skip {
            return Ok(());
        }
        let done = ctx.with_retry(src, || {
            let _ = fs::remove_file(dst); // overwrite was approved above
            make_node(dst, &meta)
        })?;
        if done.is_some() {
            ctx.files_done += 1;
            ctx.progress(src);
        }
        Ok(())
    } else {
        ctx.progress(src);
        // followed: the copy opens `dst` to write, and a symlink there
        // would take the truncation straight through to the source
        if src == dst || same_file(&meta, dst, true) {
            return ctx.error(src, "source and destination are the same file");
        }
        // the one place Append and Reget make sense: a local file
        // copied onto a local file
        let mode = ctx.may_overwrite(FileFacts::of_path(src), dst, true)?;
        if mode == Overwrite::Skip {
            return Ok(());
        }
        // a second name for a file already copied is a second name for
        // the copy, as it was for the source
        use std::os::unix::fs::MetadataExt;
        let key =
            (meta.nlink() > 1 && mode == Overwrite::Replace).then(|| (meta.dev(), meta.ino()));
        if let Some(first) = key.and_then(|key| ctx.links.get(&key).cloned()) {
            let linked = ctx.with_retry(dst, || {
                let _ = fs::remove_file(dst); // overwrite was approved above
                fs::hard_link(&first, dst)
            })?;
            if linked.is_some() {
                ctx.files_done += 1;
                ctx.bytes_done += meta.len();
                ctx.progress(src);
            }
            return Ok(());
        }
        let before = ctx.skipped;
        copy_file(ctx, src, dst, meta.len(), mode)?;
        if let Some(key) = key
            && ctx.skipped == before
        {
            ctx.links.insert(key, dst.to_path_buf());
        }
        Ok(())
    }
}

fn copy_file(
    ctx: &mut Ctx,
    src: &Path,
    dst: &Path,
    size: u64,
    mode: Overwrite,
) -> Result<(), Aborted> {
    #[cfg(debug_assertions)]
    test_gate(ctx)?;
    loop {
        if ctx.cancelled() {
            return Err(Aborted);
        }
        let start = ctx.bytes_done;
        ctx.begin_file(size);
        match try_copy_file(ctx, src, dst, mode).and_then(|()| match ctx.opts.verify {
            true => verify_copy(src, dst).map_err(|err| {
                // a copy that does not read back is no copy either
                if mode == Overwrite::Replace {
                    let _ = fs::remove_file(dst);
                }
                CopyErr::Io(err)
            }),
            false => Ok(()),
        }) {
            Ok(()) => {
                ctx.files_done += 1;
                ctx.bytes_done = start + size; // keep totals consistent with the scan
                ctx.file_done = ctx.file_total;
                ctx.progress(src);
                return Ok(());
            }
            Err(CopyErr::Cancelled) => return Err(Aborted),
            Err(CopyErr::Io(err)) => {
                ctx.bytes_done = start; // roll back partial progress
                match ctx.ask_error(src, err.to_string())? {
                    Decision::Retry => continue,
                    Decision::Skip => return Ok(()),
                }
            }
        }
    }
}

/// The job tests need a copy that is running for exactly as long as
/// they say. A debug build holds each file until the path named in
/// `RCMD_TEST_COPY_GATE` exists - cancel still works while it waits.
/// (They used to copy from a FIFO, which blocked the same way; a FIFO
/// is recreated now, never read.)
#[cfg(debug_assertions)]
fn test_gate(ctx: &Ctx) -> Result<(), Aborted> {
    let Some(gate) = std::env::var_os("RCMD_TEST_COPY_GATE") else {
        return Ok(());
    };
    while !Path::new(&gate).exists() {
        if ctx.cancelled() {
            return Err(Aborted);
        }
        thread::sleep(std::time::Duration::from_millis(20));
    }
    Ok(())
}

/// The verify pass through providers: both files read back, compared
/// chunk for chunk, as the local one does.
fn verify_fs(
    src_fs: &dyn FsProvider,
    src: &Path,
    dst_fs: &dyn FsProvider,
    dst: &Path,
) -> io::Result<()> {
    let (mut a, mut b) = (src_fs.open_read(src)?, dst_fs.open_read(dst)?);
    let (mut left, mut right) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    let fill = |r: &mut dyn Read, buf: &mut [u8]| -> io::Result<usize> {
        let mut have = 0;
        while have < buf.len() {
            match r.read(&mut buf[have..])? {
                0 => break,
                n => have += n,
            }
        }
        Ok(have)
    };
    loop {
        let n = fill(&mut *a, &mut left)?;
        let m = fill(&mut *b, &mut right)?;
        if n != m || left[..n] != right[..m] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the copy does not read back the same as the source",
            ));
        }
        if n == 0 {
            return Ok(());
        }
    }
}

/// Read both files back and compare them. An error here is reported
/// the way any other copy error is - Retry, Skip, Abort - because a
/// copy that did not arrive is a copy that failed, whatever the write
/// said at the time.
fn verify_copy(src: &Path, dst: &Path) -> io::Result<()> {
    let (mut a, mut b) = (
        crate::vfs::open_regular(src)?,
        crate::vfs::open_regular(dst)?,
    );
    let (mut left, mut right) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    loop {
        let n = read_full(&mut a, &mut left)?;
        let m = read_full(&mut b, &mut right)?;
        if n != m || left[..n] != right[..m] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the copy does not read back the same as the source",
            ));
        }
        if n == 0 {
            return Ok(());
        }
    }
}

/// Fill the buffer unless the file ends first: a short read is not the
/// end of a file, and comparing chunk for chunk needs it to be.
fn read_full(file: &mut fs::File, buf: &mut [u8]) -> io::Result<usize> {
    let mut have = 0;
    while have < buf.len() {
        match file.read(&mut buf[have..])? {
            0 => break,
            n => have += n,
        }
    }
    Ok(have)
}

/// Whether `dst` is the file `src_meta` describes. Comparing names
/// misses a hard link, and a name reached through a symlink; the
/// device and inode do not. `follow` says whether a symlink standing at
/// `dst` counts as what it points at - it does for anything that opens
/// `dst` to write into it.
fn same_file(src_meta: &fs::Metadata, dst: &Path, follow: bool) -> bool {
    use std::os::unix::fs::MetadataExt;
    let dst_meta = if follow {
        fs::metadata(dst)
    } else {
        fs::symlink_metadata(dst)
    };
    dst_meta.is_ok_and(|d| d.dev() == src_meta.dev() && d.ino() == src_meta.ino())
}

/// Whether `dst` lies inside the directory `src`. `link/x`, where
/// `link` points at `src`, is inside it however the names read, and a
/// copy there finds its own output and recurses until the names grow
/// too long. Symlinks are resolved only for the job's top source:
/// below it the names are as they were read, and the lexical test is
/// the whole story.
fn inside(ctx: &Ctx, dst: &Path, src: &Path) -> bool {
    if dst.starts_with(src) {
        return true;
    }
    if ctx.copy_root.as_deref() != Some(src) {
        return false;
    }
    fs::canonicalize(src).is_ok_and(|real| resolve(dst).starts_with(real))
}

/// `path` with its symlinks resolved, for a path whose tail need not
/// exist yet: the deepest ancestor that does is resolved and the rest
/// put back on.
fn resolve(path: &Path) -> PathBuf {
    let mut tail = Vec::new();
    let mut head = path;
    loop {
        if let Ok(real) = fs::canonicalize(head) {
            return tail.iter().rev().fold(real, |acc, part| acc.join(part));
        }
        match (head.parent(), head.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                head = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

fn is_special(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;
    let kind = meta.file_type();
    kind.is_fifo() || kind.is_socket() || kind.is_char_device() || kind.is_block_device()
}

/// Make a FIFO, socket or device node like the one `meta` describes.
/// A device needs privileges an ordinary user lacks; the error says so
/// and the usual Retry / Skip / Abort decides.
fn make_node(dst: &Path, meta: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let path = std::ffi::CString::new(dst.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in the name"))?;
    let rc = unsafe {
        libc::mknod(
            path.as_ptr(),
            meta.mode() as libc::mode_t,
            meta.rdev() as libc::dev_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        let err = io::Error::last_os_error();
        Err(io::Error::new(
            err.kind(),
            format!("cannot create the special file: {err}"),
        ))
    }
}

fn try_copy_file(ctx: &mut Ctx, src: &Path, dst: &Path, mode: Overwrite) -> Result<(), CopyErr> {
    let mut input = crate::vfs::open_regular(src).map_err(CopyErr::Io)?;
    let meta = input.metadata().map_err(CopyErr::Io)?;
    // Append and Reget add to a file that is already there; only a plain
    // copy creates one, and only a plain copy may delete it again.
    let fresh = mode == Overwrite::Replace;
    if fresh {
        return copy_fresh(ctx, &mut input, src, dst, &meta);
    }
    let mut output = {
        if mode == Overwrite::Reget {
            // resume: whatever is on disk is taken to be the head of the
            // source, so start reading where the target ends
            let have = fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
            if have >= meta.len() {
                return Ok(()); // nothing left to fetch
            }
            input.seek(SeekFrom::Start(have)).map_err(CopyErr::Io)?;
        }
        fs::OpenOptions::new()
            .append(true)
            .open(dst)
            .map_err(CopyErr::Io)?
    };
    pump(ctx, &mut input, &mut output, src, false, &meta)?;
    if ctx.opts.fsync {
        output.sync_all().map_err(CopyErr::Io)?;
    }
    Ok(())
}

/// A plain copy: into a new file beside the target, renamed over it
/// once every byte is there. An overwrite that fails part way leaves
/// the file it was to replace as it was - written in place, the target
/// was truncated at the first byte - and nothing half-written ever sits
/// under the target's name. Where a rename would change more than the
/// contents - the target is a symlink, has other hard links, or belongs
/// to someone else - the copy writes into it in place, as before.
fn copy_fresh(
    ctx: &mut Ctx,
    input: &mut fs::File,
    src: &Path,
    dst: &Path,
    meta: &fs::Metadata,
) -> Result<(), CopyErr> {
    let old = fs::symlink_metadata(dst).ok();
    let staged = match &old {
        None => true,
        Some(old) => {
            use std::os::unix::fs::MetadataExt;
            old.is_file() && old.nlink() == 1 && old.uid() == unsafe { libc::geteuid() }
        }
    };
    let (mut output, written) = match staged {
        true => staging_file(dst).map_err(CopyErr::Io)?,
        false => (
            fs::File::create(dst).map_err(CopyErr::Io)?,
            dst.to_path_buf(),
        ),
    };
    let mut copied = pump(ctx, input, &mut output, src, true, meta);
    if copied.is_ok() && ctx.opts.fsync {
        copied = output.sync_all().map_err(CopyErr::Io);
    }
    if copied.is_ok() && staged {
        // the target's own mode stays unless the copy brings the source's
        if !ctx.opts.preserve
            && let Some(old) = &old
        {
            let _ = output.set_permissions(old.permissions());
        }
        drop(output);
        copied = fs::rename(&written, dst).map_err(CopyErr::Io);
        if copied.is_err() {
            let _ = fs::remove_file(&written);
        } else if ctx.opts.fsync
            && let Some(dir) = dst.parent()
            && let Ok(dir) = fs::File::open(dir)
        {
            // the rename lives in the directory: that goes to disk too
            let _ = dir.sync_all();
        }
        return copied;
    }
    if copied.is_err() {
        // cancelled or failed, a file this copy made is not a copy:
        // don't leave a torso behind looking like one
        drop(output);
        let _ = fs::remove_file(&written);
    }
    copied
}

/// A new, empty file beside `dst` to stage a copy in: hidden, named for
/// the target and this process, never an existing file.
fn staging_file(dst: &Path) -> io::Result<(fs::File, PathBuf)> {
    let dir = dst.parent().unwrap_or(Path::new("."));
    let name = dst.file_name().unwrap_or_default().to_string_lossy();
    for n in 0..1000 {
        let path = dir.join(format!(".{name}.rcmd-{}-{n}", std::process::id()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((file, path)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free staging name",
    ))
}

/// Linux's FICLONE: share the source's blocks, copy-on-write.
const FICLONE: libc::c_ulong = 0x4004_9409;
/// How much one copy_file_range call is asked for: the loop's own chunk,
/// so the bar moves and the cancel is looked at as often as before.
const RANGE_CHUNK: usize = CHUNK;

/// The copy done by the kernel, where it can be: a reflink (instant and
/// free on btrfs and xfs), the data ranges alone for a sparse file, or
/// copy_file_range, which saves the round trip through user space.
/// `Ok(false)` = none of them applies; the read/write loop does it.
fn fast_copy(
    ctx: &mut Ctx,
    input: &fs::File,
    output: &fs::File,
    src: &Path,
    meta: &fs::Metadata,
) -> Result<bool, CopyErr> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    let (from, to) = (input.as_raw_fd(), output.as_raw_fd());
    let size = meta.len();
    let done = |ctx: &mut Ctx, n: u64| {
        ctx.bytes_done += n;
        ctx.file_done += n;
        ctx.progress(src);
    };
    if size > 0 && unsafe { libc::ioctl(to, FICLONE, from) } == 0 {
        done(ctx, size);
        return Ok(true);
    }
    // fewer blocks than bytes: holes, which a copy must not fill
    if size > 0 && meta.blocks() * 512 < size {
        return sparse_copy(ctx, input, output, src, size);
    }
    let mut total = 0u64;
    loop {
        if ctx.cancelled() {
            return Err(CopyErr::Cancelled);
        }
        let n = unsafe {
            libc::copy_file_range(
                from,
                std::ptr::null_mut(),
                to,
                std::ptr::null_mut(),
                RANGE_CHUNK,
                0,
            )
        };
        match n {
            // a /proc file says it is empty and says so here too, while
            // read() hands over its contents: the loop is the one to ask
            0 if total == 0 => return Ok(false),
            0 => return Ok(true),
            n if n > 0 => {
                total += n as u64;
                done(ctx, n as u64);
            }
            _ if total == 0 => return Ok(false), // not across these two
            _ => return Err(CopyErr::Io(io::Error::last_os_error())),
        }
    }
}

/// A sparse file's data ranges, and a length set to cover the holes
/// between and after them.
fn sparse_copy(
    ctx: &mut Ctx,
    input: &fs::File,
    output: &fs::File,
    src: &Path,
    size: u64,
) -> Result<bool, CopyErr> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::FileExt;
    let fd = input.as_raw_fd();
    let mut buf = vec![0u8; CHUNK];
    let mut pos: i64 = 0;
    while (pos as u64) < size {
        let data = unsafe { libc::lseek(fd, pos, libc::SEEK_DATA) };
        if data < 0 {
            let err = io::Error::last_os_error();
            return match err.raw_os_error() {
                Some(libc::ENXIO) => break, // nothing but a hole from here
                // no SEEK_DATA here: the plain loop, before anything was
                // written
                _ if pos == 0 => Ok(false),
                _ => Err(CopyErr::Io(err)),
            };
        }
        let hole = unsafe { libc::lseek(fd, data, libc::SEEK_HOLE) };
        let hole = if hole < 0 { size as i64 } else { hole };
        let mut at = data as u64;
        while at < hole as u64 {
            if ctx.cancelled() {
                return Err(CopyErr::Cancelled);
            }
            let want = ((hole as u64 - at) as usize).min(buf.len());
            let n = input.read_at(&mut buf[..want], at).map_err(CopyErr::Io)?;
            if n == 0 {
                break;
            }
            output.write_all_at(&buf[..n], at).map_err(CopyErr::Io)?;
            at += n as u64;
            ctx.bytes_done += n as u64;
            ctx.file_done += n as u64;
            ctx.progress(src);
        }
        pos = hole;
    }
    output.set_len(size).map_err(CopyErr::Io)?;
    Ok(true)
}

fn pump(
    ctx: &mut Ctx,
    input: &mut fs::File,
    output: &mut fs::File,
    src: &Path,
    fresh: bool,
    meta: &fs::Metadata,
) -> Result<(), CopyErr> {
    // a fresh copy is the kernel's to do where it can; an append or a
    // resume writes after what is there, which only the loop does
    if !(fresh && fast_copy(ctx, input, output, src, meta)?) {
        let mut buf = vec![0u8; CHUNK];
        loop {
            if ctx.cancelled() {
                return Err(CopyErr::Cancelled);
            }
            let n = input.read(&mut buf).map_err(CopyErr::Io)?;
            if n == 0 {
                break;
            }
            output.write_all(&buf[..n]).map_err(CopyErr::Io)?;
            ctx.bytes_done += n as u64;
            ctx.file_done += n as u64;
            ctx.progress(src);
        }
    }
    // an appended-to file keeps its own mode and its new mtime: it is
    // not a copy of the source, it is the target with more in it
    if fresh && ctx.opts.preserve {
        output
            .set_permissions(meta.permissions())
            .map_err(CopyErr::Io)?;
        preserve_attrs(output, src, meta);
    }
    Ok(())
}

/// What Preserve carries over besides the mode bits, onto an open file
/// or directory: the owner (only root may give a file away, so only
/// then), the extended attributes - where ACLs and SELinux labels live -
/// the mode again after them, and both times. Each is done as far as
/// the target's filesystem allows; a copy is not failed for an xattr.
fn preserve_attrs(target: &fs::File, src: &Path, meta: &fs::Metadata) {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    let fd = target.as_raw_fd();
    if unsafe { libc::geteuid() } == 0 {
        unsafe { libc::fchown(fd, meta.uid(), meta.gid()) };
    }
    copy_xattrs(src, fd);
    // a chown takes setuid off; the mode goes back on after it
    let _ = target.set_permissions(meta.permissions());
    let mut times = fs::FileTimes::new();
    if let Ok(modified) = meta.modified() {
        times = times.set_modified(modified);
    }
    if let Ok(accessed) = meta.accessed() {
        times = times.set_accessed(accessed);
    }
    let _ = target.set_times(times);
}

/// Every extended attribute of `src` onto the open file `fd`, as far as
/// this user may set them (the `user.` ones always; `security.` and
/// `trusted.` ones only as root).
fn copy_xattrs(src: &Path, fd: libc::c_int) {
    let Ok(path) = std::ffi::CString::new(src.as_os_str().as_encoded_bytes()) else {
        return;
    };
    let size = unsafe { libc::llistxattr(path.as_ptr(), std::ptr::null_mut(), 0) };
    if size <= 0 {
        return;
    }
    let mut names = vec![0u8; size as usize];
    let size = unsafe { libc::llistxattr(path.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
    if size <= 0 {
        return;
    }
    names.truncate(size as usize);
    for name in names.split(|&b| b == 0).filter(|n| !n.is_empty()) {
        let Ok(name) = std::ffi::CString::new(name) else {
            continue;
        };
        let len = unsafe { libc::lgetxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
        if len < 0 {
            continue;
        }
        let mut value = vec![0u8; len as usize];
        let len = unsafe {
            libc::lgetxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        if len < 0 {
            continue;
        }
        unsafe {
            libc::fsetxattr(fd, name.as_ptr(), value.as_ptr().cast(), len as usize, 0);
        }
    }
}

fn move_one(ctx: &mut Ctx, src: &Path, dst: &Path, totals: &mut (u64, u64)) -> Result<(), Aborted> {
    ctx.progress(src);
    ctx.copy_root = Some(src.to_path_buf());
    let same = fs::symlink_metadata(src).is_ok_and(|meta| same_file(&meta, dst, false));
    if src == dst || same {
        return ctx.error(src, "source and destination are the same file");
    }
    if inside(ctx, dst, src) {
        return ctx.error(src, "cannot move a directory into itself");
    }
    if ctx.may_overwrite(FileFacts::of_path(src), dst, false)? == Overwrite::Skip {
        return Ok(());
    }
    // whether this move can be taken back afterwards: only if nothing
    // was standing where it landed
    let clean = !dst.exists();
    loop {
        if ctx.cancelled() {
            return Err(Aborted);
        }
        match fs::rename(src, dst) {
            Ok(()) => {
                ctx.files_done += 1;
                ctx.moved(clean, src, dst);
                ctx.progress(src);
                return Ok(());
            }
            Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {
                // becomes a real copy: swap this item's "1" for its file
                // count and add its bytes so the gauge means something
                let (files, bytes) = scan(std::slice::from_ref(&src.to_path_buf()));
                totals.0 = totals.0.saturating_sub(1) + files;
                totals.1 += bytes;
                let _ = ctx.tx.send(JobEvent::Total {
                    files: totals.0,
                    bytes: totals.1,
                });
                let before = ctx.skipped;
                move_across(ctx, src, dst)?;
                // a half-moved tree has no single rename that undoes it
                ctx.moved(clean && ctx.skipped == before, src, dst);
                ctx.progress(src);
                return Ok(());
            }
            Err(err) => match ctx.ask_error(src, err.to_string())? {
                Decision::Retry => continue,
                Decision::Skip => return Ok(()),
            },
        }
    }
}

/// The move `rename` could not do: copy, then delete the source - but
/// only what arrived. A file whose copy was skipped, by an overwrite
/// answer or an error answer, stays where it was, and so does every
/// directory above it; the old copy-everything-then-delete-everything
/// deleted what it had skipped.
fn move_across(ctx: &mut Ctx, src: &Path, dst: &Path) -> Result<(), Aborted> {
    if ctx.cancelled() {
        return Err(Aborted);
    }
    let Some(meta) = ctx.with_retry(src, || src.symlink_metadata())? else {
        return Ok(());
    };
    let before = ctx.skipped;
    if !meta.is_dir() {
        copy_tree(ctx, src, dst)?;
        if ctx.skipped == before {
            ctx.with_retry(src, || fs::remove_file(src))?;
        }
        return Ok(());
    }
    if inside(ctx, dst, src) {
        return ctx.error(src, "cannot move a directory into itself");
    }
    let created = ctx.with_retry(dst, || match fs::create_dir(dst) {
        Err(ref e) if e.kind() == io::ErrorKind::AlreadyExists && dst.is_dir() => Ok(()),
        other => other,
    })?;
    if created.is_none() {
        return Ok(());
    }
    let Some(names) = read_names(ctx, src)? else {
        return Ok(());
    };
    for name in names {
        move_across(ctx, &src.join(&name), &dst.join(&name))?;
    }
    if ctx.opts.preserve
        && let Ok(dir) = fs::File::open(dst)
    {
        preserve_attrs(&dir, src, &meta);
    }
    if ctx.skipped == before {
        ctx.with_retry(src, || fs::remove_dir(src))?;
    }
    Ok(())
}

fn delete_tree(ctx: &mut Ctx, path: &Path) -> Result<(), Aborted> {
    if ctx.cancelled() {
        return Err(Aborted);
    }
    let Some(meta) = ctx.with_retry(path, || path.symlink_metadata())? else {
        return Ok(());
    };
    if meta.is_dir() {
        let Some(names) = read_names(ctx, path)? else {
            return Ok(());
        };
        for name in names {
            delete_tree(ctx, &path.join(&name))?;
        }
        ctx.with_retry(path, || fs::remove_dir(path))?;
    } else {
        ctx.progress(path);
        if ctx.with_retry(path, || fs::remove_file(path))?.is_some() {
            ctx.files_done += 1;
        }
    }
    Ok(())
}

/// Have the source unpack what the job is about to read, where it can
/// do that in one go, into a scratch directory beside `near` (the local
/// destination, which has the room the copies need anyway) or in the
/// temporary directory. How far it is shows on the file bar under the
/// first source's name, and a cancel stops it; a failure is only lost
/// time, as each file is then read the way it would have been.
fn prefetch(
    ctx: &mut Ctx,
    fs: &dyn FsProvider,
    sources: &[PathBuf],
    near: Option<&Path>,
) -> Result<crate::vfs::Prefetched, Aborted> {
    let scratch = match near {
        Some(dest) if dest.is_dir() => dest.to_path_buf(),
        Some(dest) => match dest.parent().filter(|p| p.is_dir()) {
            Some(parent) => parent.to_path_buf(),
            None => std::env::temp_dir(),
        },
        None => std::env::temp_dir(),
    };
    let label = sources.first().cloned().unwrap_or_default();
    ctx.begin_file(100);
    let unpacked = fs.prefetch(sources, &scratch, &mut |percent| {
        ctx.file_done = percent;
        ctx.progress(&label);
        !ctx.cancelled()
    });
    ctx.begin_file(0);
    match unpacked {
        Ok(unpacked) => Ok(unpacked),
        Err(_) if ctx.cancelled() => Err(Aborted),
        Err(_) => Ok(crate::vfs::Prefetched::none()),
    }
}

fn scan_provider(fs: &dyn FsProvider, paths: &[PathBuf]) -> (u64, u64) {
    fn walk(fs: &dyn FsProvider, path: &Path, files: &mut u64, bytes: &mut u64) {
        let Ok(entry) = fs.stat(path) else { return };
        if entry.kind == EntryKind::Dir {
            if let Ok(children) = fs.read_dir(path) {
                for child in children {
                    walk(fs, &path.join(&child.name), files, bytes);
                }
            }
        } else {
            *files += 1;
            *bytes += entry.size;
        }
    }
    let (mut files, mut bytes) = (0, 0);
    for path in paths {
        walk(fs, path, &mut files, &mut bytes);
    }
    (files, bytes)
}

fn extract_tree(ctx: &mut Ctx, fs: &dyn FsProvider, src: &Path, dst: &Path) -> Result<(), Aborted> {
    if ctx.cancelled() {
        return Err(Aborted);
    }
    let Some(entry) = ctx.with_retry(src, || fs.stat(src))? else {
        return Ok(());
    };
    match entry.kind {
        EntryKind::Dir => {
            let created = ctx.with_retry(dst, || match std::fs::create_dir(dst) {
                Err(ref e) if e.kind() == io::ErrorKind::AlreadyExists && dst.is_dir() => Ok(()),
                other => other,
            })?;
            if created.is_none() {
                return Ok(());
            }
            let Some(children) = ctx.with_retry(src, || fs.read_dir(src))? else {
                return Ok(());
            };
            for child in children {
                extract_tree(ctx, fs, &src.join(&child.name), &dst.join(&child.name))?;
            }
            if let Some(modified) = entry.mtime
                && let Ok(dir) = fs::File::open(dst)
            {
                let _ = dir.set_times(fs::FileTimes::new().set_modified(modified));
            }
            Ok(())
        }
        EntryKind::SymlinkDir | EntryKind::SymlinkFile | EntryKind::SymlinkBroken => {
            ctx.progress(src);
            if ctx.may_overwrite(FileFacts::of_entry(&entry), dst, false)? == Overwrite::Skip {
                return Ok(());
            }
            let target = entry.link_target.clone().unwrap_or_default();
            let done = ctx.with_retry(src, || {
                let _ = fs::remove_file(dst); // overwrite was approved above
                make_symlink(&target, dst)
            })?;
            if done.is_some() {
                ctx.files_done += 1;
                ctx.progress(src);
            }
            Ok(())
        }
        EntryKind::File => {
            ctx.progress(src);
            if ctx.may_overwrite(FileFacts::of_entry(&entry), dst, false)? == Overwrite::Skip {
                return Ok(());
            }
            extract_file(ctx, fs, src, dst, &entry)
        }
    }
}

fn extract_file(
    ctx: &mut Ctx,
    fs: &dyn FsProvider,
    src: &Path,
    dst: &Path,
    entry: &crate::entry::Entry,
) -> Result<(), Aborted> {
    loop {
        if ctx.cancelled() {
            return Err(Aborted);
        }
        let start = ctx.bytes_done;
        match try_extract_file(ctx, fs, src, dst, entry.mode, entry.mtime) {
            Ok(()) => {
                ctx.files_done += 1;
                ctx.bytes_done = start + entry.size;
                ctx.progress(src);
                return Ok(());
            }
            Err(CopyErr::Cancelled) => return Err(Aborted),
            Err(CopyErr::Io(err)) => {
                ctx.bytes_done = start;
                match ctx.ask_error(src, err.to_string())? {
                    Decision::Retry => continue,
                    Decision::Skip => return Ok(()),
                }
            }
        }
    }
}

fn try_extract_file(
    ctx: &mut Ctx,
    fs: &dyn FsProvider,
    src: &Path,
    dst: &Path,
    mode: u32,
    mtime: Option<std::time::SystemTime>,
) -> Result<(), CopyErr> {
    let mut input = fs.open_read(src).map_err(CopyErr::Io)?;
    let mut output = std::fs::File::create(dst).map_err(CopyErr::Io)?;
    let mut buf = vec![0u8; CHUNK];
    loop {
        if ctx.cancelled() {
            drop(output);
            let _ = std::fs::remove_file(dst);
            return Err(CopyErr::Cancelled);
        }
        let n = input.read(&mut buf).map_err(CopyErr::Io)?;
        if n == 0 {
            break;
        }
        output.write_all(&buf[..n]).map_err(CopyErr::Io)?;
        ctx.bytes_done += n as u64;
        ctx.progress(src);
    }
    // setuid, setgid and sticky stay in the archive: a program that
    // ran as whoever extracted it is not a permission an archive can
    // grant, and tar drops them for anyone but root too
    #[cfg(unix)]
    if mode != 0 {
        use std::os::unix::fs::PermissionsExt;
        output
            .set_permissions(std::fs::Permissions::from_mode(mode & 0o777))
            .map_err(CopyErr::Io)?;
    }
    if let Some(modified) = mtime {
        let _ = output.set_times(fs::FileTimes::new().set_modified(modified));
    }
    Ok(())
}

/// Copy local files INTO a zip archive. The archive is rewritten to a
/// temp file that renames over it: surviving members are copied across
/// still compressed, so nothing is decoded and re-encoded, and a member
/// with the same name as one being written is **replaced** rather than
/// shadowed by a second copy of the name. Appending in place was
/// cheaper and left two members with one name, which every reader
/// resolves its own way.
pub fn spawn_pack_zip(
    sources: Vec<PathBuf>,
    archive: PathBuf,
    inside: PathBuf,
    level: Option<u32>,
) -> JobHandle {
    spawn(move |ctx| {
        let (files, bytes) = scan(&sources);
        let _ = ctx.tx.send(JobEvent::Total { files, bytes });
        // what is about to be written; a member already there under one
        // of these names is replaced rather than shadowed
        let replacing: Vec<PathBuf> = sources
            .iter()
            .map(|src| inside.join(src.file_name().unwrap_or_default()))
            .collect();
        let temp = {
            let dir = archive.parent().unwrap_or_else(|| Path::new("."));
            let name = archive.file_name().unwrap_or_default().to_string_lossy();
            dir.join(format!(".{name}.rcmd-{}", std::process::id()))
        };
        let mut zip = match fs::File::create(&temp) {
            Ok(file) => zip::ZipWriter::new(file),
            Err(err) => {
                ctx.error(&archive, &err.to_string())?;
                return Ok(());
            }
        };
        // stream the members that survive across, uncompressed-copied
        // so nothing is decoded and re-encoded on the way
        let carried = (|| -> io::Result<()> {
            if !archive.exists() {
                // nothing to carry across: a name that is not there yet
                // is how packing creates an archive instead of adding
                // to one
                return Ok(());
            }
            let file = fs::File::open(&archive)?;
            let mut old = zip::ZipArchive::new(file)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            for i in 0..old.len() {
                let member = old
                    .by_index_raw(i)
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
                let Some(rel) = member.enclosed_name() else {
                    continue;
                };
                if replacing.iter().any(|target| under(&rel, target)) {
                    continue;
                }
                zip.raw_copy_file(member)
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            }
            Ok(())
        })();
        if let Err(err) = carried {
            let _ = zip.finish();
            let _ = fs::remove_file(&temp);
            ctx.error(&archive, &err.to_string())?;
            return Ok(());
        }

        let mut outcome = Ok(());
        for src in &sources {
            if ctx.cancelled() {
                outcome = Err(Aborted);
                break;
            }
            let name = src.file_name().unwrap_or_default();
            if let Err(abort) = pack_tree(ctx, &mut zip, src, &inside.join(name), level) {
                outcome = Err(abort);
                break;
            }
        }
        // always finalize: without the central directory the zip is broken
        if let Err(err) = zip.finish() {
            let _ = ctx.error(&archive, &format!("finalizing archive: {err}"));
        }
        match outcome {
            Ok(()) => {
                if let Err(err) = fs::rename(&temp, &archive) {
                    let _ = fs::remove_file(&temp);
                    return ctx.error(&archive, &format!("replacing archive: {err}"));
                }
                Ok(())
            }
            Err(abort) => {
                // the original is untouched: only the temp is discarded
                let _ = fs::remove_file(&temp);
                Err(abort)
            }
        }
    })
}

/// One change to make inside an archive. A path is relative to the
/// archive's root, the way a panel inside one addresses things.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArchiveOp {
    /// Drop this member, and everything under it if it is a directory.
    Remove(PathBuf),
    /// Move a member, and everything under it, to a new path.
    Rename { from: PathBuf, to: PathBuf },
    /// Add a directory entry that holds nothing yet.
    Mkdir(PathBuf),
}

impl ArchiveOp {
    /// What this op does to one existing member's path: `None` drops
    /// it, `Some(path)` keeps it there.
    fn apply(ops: &[ArchiveOp], path: &Path) -> Option<PathBuf> {
        let mut path = path.to_path_buf();
        for op in ops {
            match op {
                ArchiveOp::Remove(target) => {
                    if under(&path, target) {
                        return None;
                    }
                }
                ArchiveOp::Rename { from, to } => {
                    if let Ok(rest) = path.strip_prefix(from) {
                        // joining an empty rest would append a separator,
                        // and a trailing slash is how an archive says
                        // "directory" - renaming a file must not do that
                        path = if rest.as_os_str().is_empty() {
                            to.clone()
                        } else {
                            to.join(rest)
                        };
                    }
                }
                ArchiveOp::Mkdir(_) => {}
            }
        }
        Some(path)
    }
}

/// A member is "under" a target if it is the target or lives inside it.
fn under(path: &Path, target: &Path) -> bool {
    path == target || path.starts_with(target)
}

/// Edit an archive in one pass: every op is applied while the members
/// stream from the old container into a new one, which then renames
/// over the original.
///
/// One job per *batch*, not per file, which is the whole point - an
/// archive has no way to remove one member in place, so deleting five
/// of them one at a time would rewrite the container five times.
pub fn spawn_archive_edit(archive: PathBuf, ops: Vec<ArchiveOp>) -> JobHandle {
    spawn(move |ctx| {
        let _ = ctx.tx.send(JobEvent::Total {
            files: ops.len() as u64,
            bytes: 0,
        });
        let name = archive
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
        let temp = {
            let dir = archive.parent().unwrap_or_else(|| Path::new("."));
            dir.join(format!(".{name}.rcmd-{}", std::process::id()))
        };
        let result = if name.ends_with(".zip") {
            edit_zip(ctx, &archive, &ops, &temp)
        } else if is_tar_name(&name) {
            edit_tar(ctx, &archive, &ops, &name, &temp)
        } else {
            Ok(Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "only .zip and .tar[.gz/xz/bz2] archives can be changed",
            )))
        };
        match result {
            Ok(Ok(())) => {
                if let Err(err) = fs::rename(&temp, &archive) {
                    let _ = fs::remove_file(&temp);
                    return ctx.error(&archive, &format!("replacing archive: {err}"));
                }
                // the container was rewritten once, so the count that
                // means anything is how many changes it carried
                ctx.files_done = ops.len() as u64;
                ctx.progress(&archive);
                Ok(())
            }
            Ok(Err(err)) => {
                let _ = fs::remove_file(&temp);
                ctx.error(&archive, &err.to_string())?;
                Ok(())
            }
            Err(Aborted) => {
                let _ = fs::remove_file(&temp);
                Err(Aborted)
            }
        }
    })
}

/// Filenames [`spawn_archive_edit`] and the pack jobs treat as tar.
pub fn is_tar_name(name: &str) -> bool {
    TarComp::of(name).is_some()
}

/// What a tar rcmd writes is wrapped in, by its name - every suffix
/// named, so a name nobody taught it is refused rather than written
/// as whatever the last branch happened to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TarComp {
    Plain,
    Gz,
    Xz,
    Bz2,
    Zstd,
}

impl TarComp {
    fn of(name: &str) -> Option<TarComp> {
        const SUFFIXES: [(&str, TarComp); 10] = [
            (".tar", TarComp::Plain),
            (".tar.gz", TarComp::Gz),
            (".tgz", TarComp::Gz),
            (".tar.xz", TarComp::Xz),
            (".txz", TarComp::Xz),
            (".tar.bz2", TarComp::Bz2),
            (".tbz2", TarComp::Bz2),
            (".tbz", TarComp::Bz2),
            (".tar.zst", TarComp::Zstd),
            (".tzst", TarComp::Zstd),
        ];
        SUFFIXES
            .iter()
            .find(|(ext, _)| name.ends_with(ext))
            .map(|&(_, comp)| comp)
    }

    fn named(name: &str) -> io::Result<TarComp> {
        TarComp::of(name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name}: not a tar name rcmd can write"),
            )
        })
    }
}

fn edit_zip(
    ctx: &mut Ctx,
    archive: &Path,
    ops: &[ArchiveOp],
    temp: &Path,
) -> Result<Result<(), io::Error>, Aborted> {
    let mut old = match fs::File::open(archive).and_then(|f| {
        zip::ZipArchive::new(f).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }) {
        Ok(zip) => zip,
        Err(err) => return Ok(Err(err)),
    };
    let mut zip = match fs::File::create(temp) {
        Ok(file) => zip::ZipWriter::new(file),
        Err(err) => return Ok(Err(err)),
    };
    let copied = (|| -> io::Result<()> {
        for i in 0..old.len() {
            let member = old
                .by_index_raw(i)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            let Some(rel) = member.enclosed_name() else {
                continue;
            };
            let is_dir = member.is_dir();
            let Some(kept) = ArchiveOp::apply(ops, &rel) else {
                continue;
            };
            // a directory member keeps its trailing slash, which is
            // how a zip says it is one
            let name = if is_dir {
                format!("{}/", kept.display())
            } else {
                kept.display().to_string()
            };
            zip.raw_copy_file_rename(member, name)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        }
        for op in ops {
            if let ArchiveOp::Mkdir(dir) = op {
                let options = zip::write::SimpleFileOptions::default();
                zip.add_directory(dir.display().to_string(), options)
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            }
        }
        Ok(())
    })();
    if ctx.cancelled() {
        return Err(Aborted);
    }
    if let Err(err) = copied {
        return Ok(Err(err));
    }
    match zip.finish() {
        Ok(_) => Ok(Ok(())),
        Err(err) => Ok(Err(io::Error::new(io::ErrorKind::InvalidData, err))),
    }
}

fn edit_tar(
    ctx: &mut Ctx,
    archive: &Path,
    ops: &[ArchiveOp],
    name: &str,
    temp: &Path,
) -> Result<Result<(), io::Error>, Aborted> {
    let mut tar = match TarSink::create(temp, name, None) {
        Ok(sink) => tar::Builder::new(sink),
        Err(err) => return Ok(Err(err)),
    };
    tar.follow_symlinks(false);
    let copied = (|| -> io::Result<()> {
        let mut old = tar::Archive::new(tar_source(archive, name)?);
        for entry in old.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            let Some(kept) = ArchiveOp::apply(ops, &path) else {
                continue;
            };
            let mut header = entry.header().clone();
            let kind = header.entry_type();
            match entry.link_name()? {
                Some(link) if kind.is_symlink() || kind.is_hard_link() => {
                    tar.append_link(&mut header, &kept, &link)?;
                }
                _ => tar.append_data(&mut header, &kept, &mut entry)?,
            }
        }
        for op in ops {
            if let ArchiveOp::Mkdir(dir) = op {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                header.set_mode(0o755);
                header.set_cksum();
                tar.append_data(&mut header, dir, &mut io::empty())?;
            }
        }
        Ok(())
    })();
    if ctx.cancelled() {
        return Err(Aborted);
    }
    if let Err(err) = copied {
        return Ok(Err(err));
    }
    match tar.into_inner().and_then(TarSink::finish) {
        Ok(()) => Ok(Ok(())),
        Err(err) => Ok(Err(err)),
    }
}

/// Copy INTO a tar archive (R4): tars cannot append in place across
/// compressors, so the whole archive is rewritten - existing entries
/// stream into a temp file with the same compression, the new trees
/// are appended behind them, and the temp renames over the original.
/// Pack into a tar, plain or compressed as its name says, at `level`
/// (0 to 9) or the format's default.
pub fn spawn_pack_tar(
    sources: Vec<PathBuf>,
    archive: PathBuf,
    inside: PathBuf,
    level: Option<u32>,
) -> JobHandle {
    spawn(move |ctx| {
        let (files, bytes) = scan(&sources);
        let _ = ctx.tx.send(JobEvent::Total { files, bytes });
        let temp = {
            let dir = archive.parent().unwrap_or_else(|| Path::new("."));
            let name = archive.file_name().unwrap_or_default().to_string_lossy();
            dir.join(format!(".{name}.rcmd-{}", std::process::id()))
        };
        match rewrite_tar(ctx, &sources, &archive, &inside, &temp, level) {
            Ok(Ok(())) => {
                if let Err(err) = fs::rename(&temp, &archive) {
                    let _ = fs::remove_file(&temp);
                    return ctx.error(&archive, &format!("replacing archive: {err}"));
                }
                Ok(())
            }
            Ok(Err(err)) => {
                let _ = fs::remove_file(&temp);
                ctx.error(&archive, &err.to_string())?;
                Ok(())
            }
            Err(Aborted) => {
                let _ = fs::remove_file(&temp);
                Err(Aborted)
            }
        }
    })
}

/// The write half of a tar rewrite; `finish` flushes the compressor's
/// trailer explicitly instead of trusting Drop.
enum TarSink {
    Plain(fs::File),
    Gz(flate2::write::GzEncoder<fs::File>),
    Xz(xz2::write::XzEncoder<fs::File>),
    Bz(bzip2::write::BzEncoder<fs::File>),
    /// ruzstd compresses from a reader rather than as a writer, so the
    /// tar goes into a pipe and a thread compresses what comes out.
    Zstd {
        pipe: io::PipeWriter,
        worker: thread::JoinHandle<io::Result<()>>,
    },
}

/// A file that keeps the first error it meets and takes everything
/// after it without a word: ruzstd's compressor has no way to hand a
/// write error back, so it is kept here to be asked about at the end.
struct Recorded {
    file: fs::File,
    err: Option<io::Error>,
}

impl Write for Recorded {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.err.is_none()
            && let Err(err) = self.file.write_all(buf)
        {
            self.err = Some(err);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Write for TarSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            TarSink::Plain(w) => w.write(buf),
            TarSink::Gz(w) => w.write(buf),
            TarSink::Xz(w) => w.write(buf),
            TarSink::Bz(w) => w.write(buf),
            TarSink::Zstd { pipe, .. } => pipe.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            TarSink::Plain(w) => w.flush(),
            TarSink::Gz(w) => w.flush(),
            TarSink::Xz(w) => w.flush(),
            TarSink::Bz(w) => w.flush(),
            TarSink::Zstd { pipe, .. } => pipe.flush(),
        }
    }
}

impl TarSink {
    /// A tar written to `path`, wrapped as `archive_name` says, at
    /// `level` (0 to 9) or each format's default.
    fn create(path: &Path, archive_name: &str, level: Option<u32>) -> io::Result<TarSink> {
        let comp = TarComp::named(archive_name)?;
        let file = fs::File::create(path)?;
        Ok(match comp {
            TarComp::Plain => TarSink::Plain(file),
            TarComp::Gz => TarSink::Gz(flate2::write::GzEncoder::new(
                file,
                level.map_or(flate2::Compression::default(), |l| {
                    flate2::Compression::new(l.min(9))
                }),
            )),
            TarComp::Xz => TarSink::Xz(xz2::write::XzEncoder::new(file, level.unwrap_or(6).min(9))),
            TarComp::Bz2 => TarSink::Bz(bzip2::write::BzEncoder::new(
                file,
                level.map_or(bzip2::Compression::default(), |l| {
                    bzip2::Compression::new(l.clamp(1, 9))
                }),
            )),
            TarComp::Zstd => {
                // ruzstd has two levels: stored, and about zstd's 1
                let level = match level {
                    Some(0) => ruzstd::encoding::CompressionLevel::Uncompressed,
                    _ => ruzstd::encoding::CompressionLevel::Fastest,
                };
                let (reader, pipe) = io::pipe()?;
                let worker = thread::spawn(move || {
                    let mut out = Recorded { file, err: None };
                    ruzstd::encoding::compress(reader, &mut out, level);
                    match out.err {
                        Some(err) => Err(err),
                        None => out.file.sync_data(),
                    }
                });
                TarSink::Zstd { pipe, worker }
            }
        })
    }

    fn finish(self) -> io::Result<()> {
        match self {
            TarSink::Plain(_) => Ok(()),
            TarSink::Gz(w) => w.finish().map(|_| ()),
            TarSink::Xz(w) => w.finish().map(|_| ()),
            TarSink::Bz(w) => w.finish().map(|_| ()),
            TarSink::Zstd { pipe, worker } => {
                drop(pipe); // the end of the tar, for the compressor
                worker
                    .join()
                    .unwrap_or_else(|_| Err(io::Error::other("the zstd compressor failed")))
            }
        }
    }
}

fn tar_source(path: &Path, archive_name: &str) -> io::Result<Box<dyn Read>> {
    let comp = TarComp::named(archive_name)?;
    let file = fs::File::open(path)?;
    Ok(match comp {
        TarComp::Plain => Box::new(file),
        TarComp::Gz => Box::new(flate2::read::GzDecoder::new(file)),
        TarComp::Xz => Box::new(xz2::read::XzDecoder::new(file)),
        TarComp::Bz2 => Box::new(bzip2::read::BzDecoder::new(file)),
        TarComp::Zstd => Box::new(
            ruzstd::decoding::StreamingDecoder::new(file)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?,
        ),
    })
}

/// Ok(Ok) = temp holds the finished archive; Ok(Err) = fatal io error;
/// Err = cancelled. Per-item problems on the *new* entries go through
/// the ordinary retry/skip dialog inside `tar_tree`.
fn rewrite_tar(
    ctx: &mut Ctx,
    sources: &[PathBuf],
    archive: &Path,
    inside: &Path,
    temp: &Path,
    level: Option<u32>,
) -> Result<Result<(), io::Error>, Aborted> {
    let name = archive
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    let mut tar = match TarSink::create(temp, &name, level) {
        Ok(sink) => tar::Builder::new(sink),
        Err(err) => return Ok(Err(err)),
    };
    tar.follow_symlinks(false);
    // stream the existing entries across unchanged (append_data /
    // append_link re-handle long names, so GNU/PAX entries survive)
    let copied = (|| -> io::Result<()> {
        if !archive.exists() {
            return Ok(()); // a new archive, as in the zip above
        }
        let mut old = tar::Archive::new(tar_source(archive, &name)?);
        for entry in old.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            let mut header = entry.header().clone();
            let kind = header.entry_type();
            match entry.link_name()? {
                Some(link) if kind.is_symlink() || kind.is_hard_link() => {
                    tar.append_link(&mut header, &path, &link)?;
                }
                _ => tar.append_data(&mut header, &path, &mut entry)?,
            }
        }
        Ok(())
    })();
    if let Err(err) = copied {
        return Ok(Err(err));
    }
    for src in sources {
        if ctx.cancelled() {
            return Err(Aborted);
        }
        let base = src.file_name().unwrap_or_default();
        tar_tree(ctx, &mut tar, src, &inside.join(base))?;
    }
    match tar.into_inner().and_then(TarSink::finish) {
        Ok(()) => Ok(Ok(())),
        Err(err) => Ok(Err(err)),
    }
}

/// Append one tree to the tar builder. Progress and cancel are
/// per-item (unlike the chunked zip path) - the whole temp archive is
/// discarded on cancel, so finer granularity buys nothing.
fn tar_tree(
    ctx: &mut Ctx,
    tar: &mut tar::Builder<TarSink>,
    src: &Path,
    dst_rel: &Path,
) -> Result<(), Aborted> {
    if ctx.cancelled() {
        return Err(Aborted);
    }
    let Some(entry) = ctx.with_retry(src, || crate::entry::stat(src))? else {
        return Ok(());
    };
    if entry.kind == EntryKind::Dir {
        let _ = ctx.with_retry(src, || tar.append_dir(dst_rel, src))?;
        let Some(names) = read_names(ctx, src)? else {
            return Ok(());
        };
        for name in names {
            tar_tree(ctx, tar, &src.join(&name), &dst_rel.join(&name))?;
        }
        return Ok(());
    }
    ctx.progress(src);
    let done = ctx.with_retry(src, || tar.append_path_with_name(src, dst_rel))?;
    if done.is_some() {
        ctx.files_done += 1;
        if entry.kind == EntryKind::File {
            ctx.bytes_done += entry.size;
        }
        ctx.progress(src);
    }
    Ok(())
}

fn pack_tree(
    ctx: &mut Ctx,
    zip: &mut zip::ZipWriter<fs::File>,
    src: &Path,
    dst_rel: &Path,
    level: Option<u32>,
) -> Result<(), Aborted> {
    if ctx.cancelled() {
        return Err(Aborted);
    }
    let Some(entry) = ctx.with_retry(src, || crate::entry::stat(src))? else {
        return Ok(());
    };
    let rel_name = dst_rel.to_string_lossy().replace('\\', "/");
    let options = zip::write::SimpleFileOptions::default()
        .unix_permissions(if entry.mode == 0 { 0o644 } else { entry.mode })
        .large_file(true);
    // 0 is stored as it is; 1 to 9 is deflate's own scale
    let options = match level {
        Some(0) => options.compression_method(zip::CompressionMethod::Stored),
        Some(level) => options.compression_level(Some(i64::from(level.min(9)))),
        None => options,
    };
    match entry.kind {
        EntryKind::Dir => {
            let _ = zip.add_directory(format!("{rel_name}/"), options);
            let Some(names) = read_names(ctx, src)? else {
                return Ok(());
            };
            for name in names {
                pack_tree(ctx, zip, &src.join(&name), &dst_rel.join(&name), level)?;
            }
            Ok(())
        }
        EntryKind::SymlinkDir | EntryKind::SymlinkFile | EntryKind::SymlinkBroken => {
            ctx.progress(src);
            let target = entry.link_target.clone().unwrap_or_default();
            let done = ctx.with_retry(src, || {
                zip.add_symlink(&rel_name, target.to_string_lossy().as_ref(), options)
                    .map_err(|e| io::Error::other(e.to_string()))
            })?;
            if done.is_some() {
                ctx.files_done += 1;
                ctx.progress(src);
            }
            Ok(())
        }
        EntryKind::File => {
            ctx.progress(src);
            loop {
                if ctx.cancelled() {
                    let _ = zip.abort_file();
                    return Err(Aborted);
                }
                let start = ctx.bytes_done;
                match try_pack_file(ctx, zip, src, &rel_name, options) {
                    Ok(()) => {
                        ctx.files_done += 1;
                        ctx.bytes_done = start + entry.size;
                        ctx.progress(src);
                        return Ok(());
                    }
                    Err(CopyErr::Cancelled) => {
                        let _ = zip.abort_file();
                        return Err(Aborted);
                    }
                    Err(CopyErr::Io(err)) => {
                        let _ = zip.abort_file();
                        ctx.bytes_done = start;
                        match ctx.ask_error(src, err.to_string())? {
                            Decision::Retry => continue,
                            Decision::Skip => return Ok(()),
                        }
                    }
                }
            }
        }
    }
}

fn try_pack_file(
    ctx: &mut Ctx,
    zip: &mut zip::ZipWriter<fs::File>,
    src: &Path,
    rel_name: &str,
    options: zip::write::SimpleFileOptions,
) -> Result<(), CopyErr> {
    let mut input = fs::File::open(src).map_err(CopyErr::Io)?;
    zip.start_file(rel_name, options)
        .map_err(|e| CopyErr::Io(io::Error::other(e.to_string())))?;
    let mut buf = vec![0u8; CHUNK];
    loop {
        if ctx.cancelled() {
            return Err(CopyErr::Cancelled);
        }
        let n = input.read(&mut buf).map_err(CopyErr::Io)?;
        if n == 0 {
            break;
        }
        zip.write_all(&buf[..n]).map_err(CopyErr::Io)?;
        ctx.bytes_done += n as u64;
        ctx.progress(src);
    }
    Ok(())
}

#[cfg(unix)]
fn make_symlink(target: &Path, dst: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, dst)
}

#[cfg(not(unix))]
fn make_symlink(_target: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symlinks are not supported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    struct Outcome {
        files_done: u64,
        skipped: u64,
        aborted: bool,
        asks: Vec<String>,
    }

    /// Drive a job to completion, answering Ask* events from `replies`.
    fn run(handle: JobHandle, mut replies: Vec<Reply>) -> Outcome {
        let mut asks = Vec::new();
        loop {
            match handle.events.recv().expect("job died without Done") {
                JobEvent::AskOverwrite { path, .. } => {
                    asks.push(format!("overwrite:{}", path.display()));
                    handle.replies.send(replies.remove(0)).unwrap();
                }
                JobEvent::AskError { path, message } => {
                    asks.push(format!("error:{}:{message}", path.display()));
                    handle.replies.send(replies.remove(0)).unwrap();
                }
                JobEvent::Done {
                    files_done,
                    skipped,
                    aborted,
                } => {
                    return Outcome {
                        files_done,
                        skipped,
                        aborted,
                        asks,
                    };
                }
                _ => {}
            }
        }
    }

    /// Collect what a job moved, which is what the undo log is made of.
    fn run_moves(handle: JobHandle, mut replies: Vec<Reply>) -> (Outcome, Vec<(PathBuf, PathBuf)>) {
        let mut moved = Vec::new();
        loop {
            match handle.events.recv().expect("job died without Done") {
                JobEvent::Moved { from, to } => moved.push((from, to)),
                JobEvent::AskOverwrite { .. } | JobEvent::AskError { .. } => {
                    handle.replies.send(replies.remove(0)).unwrap();
                }
                JobEvent::Done {
                    files_done,
                    skipped,
                    aborted,
                } => {
                    return (
                        Outcome {
                            files_done,
                            skipped,
                            aborted,
                            asks: Vec::new(),
                        },
                        moved,
                    );
                }
                _ => {}
            }
        }
    }

    /// A FIFO is recreated, not read: opening one for its "contents"
    /// blocks until a writer turns up, and nothing ever did, with the
    /// cancel flag unchecked all the while.
    #[test]
    fn a_fifo_in_a_copied_tree_is_recreated_not_read() {
        use std::os::unix::fs::FileTypeExt;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("a.txt"), b"a").unwrap();
        let fifo = src.join("pipe");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        let dst = tmp.path().join("dst");

        let handle = spawn_copy(vec![src], dst.clone(), TransferOpts::default(), None);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match handle.events.recv_timeout(left) {
                Ok(JobEvent::Done {
                    aborted, skipped, ..
                }) => {
                    assert!(!aborted);
                    assert_eq!(skipped, 0);
                    break;
                }
                Ok(JobEvent::AskError { message, .. }) => panic!("asked: {message}"),
                Ok(_) => {}
                Err(_) => {
                    handle.cancel.store(true, Ordering::Relaxed);
                    panic!("the copy hung on the FIFO");
                }
            }
        }
        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"a");
        let made = fs::symlink_metadata(dst.join("pipe")).unwrap();
        assert!(made.file_type().is_fifo(), "{:?}", made.file_type());
    }

    #[test]
    fn open_read_refuses_what_is_not_a_regular_file() {
        use crate::vfs::LocalFs;
        let tmp = tempfile::tempdir().unwrap();
        let fifo = tmp.path().join("pipe");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        let (tx, rx) = mpsc::channel();
        let path = fifo.clone();
        thread::spawn(move || {
            let _ = tx.send(LocalFs.open_read(&path).is_err());
        });
        let refused = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("open_read blocked on a FIFO");
        assert!(refused);
    }

    /// Overwrite a file with a hard link of itself and the truncating
    /// create empties both names at once: the source is gone before a
    /// byte of it is read. Names differ, so only the inode tells.
    #[test]
    fn copying_a_file_onto_its_own_hard_link_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.txt");
        fs::write(&a, b"precious").unwrap();
        let out = tmp.path().join("out");
        fs::create_dir(&out).unwrap();
        fs::hard_link(&a, out.join("a.txt")).unwrap();

        let res = run(
            spawn_copy(vec![a.clone()], out.clone(), TransferOpts::default(), None),
            vec![Reply::Skip],
        );
        assert_eq!(fs::read(&a).unwrap(), b"precious");
        assert!(
            res.asks.iter().any(|ask| ask.contains("same file")),
            "{:?}",
            res.asks
        );
    }

    /// ...and the same through a symlink standing where the copy lands.
    #[test]
    fn copying_a_file_onto_a_symlink_to_itself_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.txt");
        fs::write(&a, b"precious").unwrap();
        let out = tmp.path().join("out");
        fs::create_dir(&out).unwrap();
        std::os::unix::fs::symlink(&a, out.join("a.txt")).unwrap();

        let res = run(
            spawn_copy(vec![a.clone()], out.clone(), TransferOpts::default(), None),
            vec![Reply::Skip],
        );
        assert_eq!(fs::read(&a).unwrap(), b"precious");
        assert!(
            res.asks.iter().any(|ask| ask.contains("same file")),
            "{:?}",
            res.asks
        );
    }

    /// `link/inner` where `link` points at the source is inside the
    /// source: the copy would find its own output and recurse.
    #[test]
    fn copying_a_directory_into_itself_through_a_symlink_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("d");
        fs::create_dir(&d).unwrap();
        fs::write(d.join("f.txt"), b"f").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&d, &link).unwrap();

        let res = run(
            spawn_copy(
                vec![d.clone()],
                link.join("inner"),
                TransferOpts::default(),
                None,
            ),
            vec![Reply::Skip],
        );
        assert!(
            res.asks.iter().any(|ask| ask.contains("into itself")),
            "{:?}",
            res.asks
        );
        assert!(!d.join("inner").exists());

        let res = run(
            spawn_move(
                vec![d.clone()],
                link.join("inner"),
                TransferOpts::default(),
                None,
            ),
            vec![Reply::Skip],
        );
        assert!(
            res.asks.iter().any(|ask| ask.contains("into itself")),
            "{:?}",
            res.asks
        );
        assert!(d.join("f.txt").exists());
    }

    /// A move onto a hard link of itself: rename() of one inode onto
    /// itself succeeds and does nothing, which reported a move that
    /// never happened.
    #[test]
    fn moving_a_file_onto_its_own_hard_link_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.txt");
        fs::write(&a, b"precious").unwrap();
        let b = tmp.path().join("b.txt");
        fs::hard_link(&a, &b).unwrap();

        let res = run(
            spawn_move(vec![a.clone()], b.clone(), TransferOpts::default(), None),
            vec![Reply::Skip],
        );
        assert!(
            res.asks.iter().any(|ask| ask.contains("same file")),
            "{:?}",
            res.asks
        );
        assert_eq!(fs::read(&a).unwrap(), b"precious");
    }

    /// A copy that fails part way, answered Skip, used to leave its
    /// torso at the target - a file that looks copied and is not.
    /// `/proc/self/mem` opens as a regular file and fails on the first
    /// read, which is a mid-copy error without a broken disk.
    #[test]
    fn a_failed_copy_leaves_no_partial_file() {
        let src = PathBuf::from("/proc/self/mem");
        if !src.exists() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let res = run(
            spawn_copy(
                vec![src],
                tmp.path().to_path_buf(),
                TransferOpts::default(),
                None,
            ),
            vec![Reply::Skip],
        );
        assert_eq!(res.skipped, 1, "{:?}", res.asks);
        assert!(
            !tmp.path().join("mem").exists(),
            "the partial copy was left behind"
        );
    }

    /// An overwrite that fails part way must leave the file it was to
    /// replace as it was. Written in place, the target was truncated at
    /// the first byte - and since a failed copy removes what it wrote,
    /// the user was left with neither file.
    #[test]
    fn a_failed_overwrite_keeps_the_old_file() {
        let src = PathBuf::from("/proc/self/mem");
        if !src.exists() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dst = tmp.path().join("mem");
        fs::write(&dst, b"precious").unwrap();
        let res = run(
            spawn_copy(
                vec![src],
                tmp.path().to_path_buf(),
                TransferOpts::default(),
                None,
            ),
            vec![Reply::Overwrite, Reply::Skip],
        );
        assert_eq!(res.skipped, 1, "{:?}", res.asks);
        assert_eq!(fs::read(&dst).unwrap(), b"precious");
        // and nothing half-written is lying next to it
        let names: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [std::ffi::OsString::from("mem")]);
    }

    /// A sparse file stays sparse: its holes were written out as zeros.
    #[test]
    fn a_sparse_file_stays_sparse() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("sparse.img");
        let file = fs::File::create(&src).unwrap();
        file.set_len(64 << 20).unwrap();
        std::os::unix::fs::FileExt::write_all_at(&file, b"head", 0).unwrap();
        std::os::unix::fs::FileExt::write_all_at(&file, b"tail", (64 << 20) - 4).unwrap();
        drop(file);
        if fs::metadata(&src).unwrap().blocks() * 512 >= 1 << 20 {
            return; // this filesystem does not do holes
        }
        let out = tmp.path().join("out");
        fs::create_dir(&out).unwrap();
        let res = run(
            spawn_copy(
                vec![src.clone()],
                out.clone(),
                TransferOpts::default(),
                None,
            ),
            vec![],
        );
        assert!(!res.aborted);
        let copy = out.join("sparse.img");
        let meta = fs::metadata(&copy).unwrap();
        assert_eq!(meta.len(), 64 << 20);
        assert!(
            meta.blocks() * 512 < 1 << 20,
            "the copy took {} blocks",
            meta.blocks()
        );
        let data = fs::read(&copy).unwrap();
        assert_eq!(&data[..4], b"head");
        assert_eq!(&data[data.len() - 4..], b"tail");
    }

    /// /proc files say they are empty and are not: whatever fast path
    /// the copy takes, their contents must arrive.
    #[test]
    fn a_proc_file_is_copied_with_its_contents() {
        let src = PathBuf::from("/proc/self/status");
        if !src.exists() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let res = run(
            spawn_copy(
                vec![src],
                tmp.path().to_path_buf(),
                TransferOpts::default(),
                None,
            ),
            vec![],
        );
        assert!(!res.aborted);
        let copied = fs::read_to_string(tmp.path().join("status")).unwrap();
        assert!(copied.contains("Name:"), "{copied:?}");
    }

    /// Preserve keeps what mc keeps: a directory's own mode, the access
    /// time beside the modification time, and two names for one file
    /// staying one file in the copy.
    #[test]
    fn preserve_keeps_directory_modes_access_times_and_hard_links() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("tree");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("a.txt"), b"one file").unwrap();
        fs::hard_link(src.join("a.txt"), src.join("b.txt")).unwrap();
        let then = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        fs::File::open(src.join("a.txt"))
            .unwrap()
            .set_times(fs::FileTimes::new().set_accessed(then).set_modified(then))
            .unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o700)).unwrap();
        let out = tmp.path().join("out");
        fs::create_dir(&out).unwrap();

        let res = run(
            spawn_copy(vec![src], out.clone(), TransferOpts::default(), None),
            vec![],
        );
        assert!(!res.aborted);
        let copy = out.join("tree");
        assert_eq!(
            fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let (a, b) = (
            fs::metadata(copy.join("a.txt")).unwrap(),
            fs::metadata(copy.join("b.txt")).unwrap(),
        );
        assert_eq!(a.ino(), b.ino(), "the two names are one file again");
        assert_eq!(a.accessed().unwrap(), then);
        assert_eq!(a.modified().unwrap(), then);
    }

    /// ...and extended attributes, which is where ACLs live.
    #[test]
    fn preserve_keeps_extended_attributes() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("tagged.txt");
        fs::write(&src, b"x").unwrap();
        let path = std::ffi::CString::new(src.as_os_str().as_encoded_bytes()).unwrap();
        let name = c"user.rcmd-test";
        let set =
            unsafe { libc::setxattr(path.as_ptr(), name.as_ptr(), b"kept".as_ptr().cast(), 4, 0) };
        if set != 0 {
            return; // this filesystem takes no user attributes
        }
        let out = tmp.path().join("out");
        fs::create_dir(&out).unwrap();
        let res = run(
            spawn_copy(vec![src], out.clone(), TransferOpts::default(), None),
            vec![],
        );
        assert!(!res.aborted);
        let copy =
            std::ffi::CString::new(out.join("tagged.txt").as_os_str().as_encoded_bytes()).unwrap();
        let mut value = [0u8; 16];
        let n =
            unsafe { libc::getxattr(copy.as_ptr(), name.as_ptr(), value.as_mut_ptr().cast(), 16) };
        assert_eq!(n, 4, "the attribute did not come along");
        assert_eq!(&value[..4], b"kept");
    }

    /// A copy that cannot fit says so before it writes a byte, rather
    /// than filling the disk and failing on whichever file it reached.
    #[test]
    fn a_copy_that_will_not_fit_asks_first() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("big.bin");
        fs::write(&src, vec![0u8; 4096]).unwrap();
        let out = tmp.path().join("small-disk");
        fs::create_dir(&out).unwrap();
        FAKE_FREE.lock().unwrap().insert(out.clone(), 100);
        let res = run(
            spawn_copy(
                vec![src.clone()],
                out.clone(),
                TransferOpts::default(),
                None,
            ),
            vec![Reply::Abort],
        );
        assert!(res.aborted);
        assert!(
            res.asks.iter().any(|ask| ask.contains("not enough space")),
            "{:?}",
            res.asks
        );
        assert!(!out.join("big.bin").exists(), "nothing was written");
        // Skip is "copy anyway"
        let res = run(
            spawn_copy(vec![src], out.clone(), TransferOpts::default(), None),
            vec![Reply::Skip],
        );
        assert!(!res.aborted);
        assert!(out.join("big.bin").exists());
    }

    /// A copy through providers - to a server, from one - answers the
    /// form as a local copy does: Preserve off leaves the time alone, a
    /// target mask renames. It used to ignore every switch.
    #[test]
    fn a_provider_transfer_honours_the_form() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("notes.txt");
        fs::write(&src, b"x").unwrap();
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        fs::File::open(&src)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
        let out = tmp.path().join("out");
        fs::create_dir(&out).unwrap();
        // two providers, so the copy is the one a remote transfer takes
        let (from, to): (Arc<dyn FsProvider>, Arc<dyn FsProvider>) =
            (Arc::new(crate::vfs::LocalFs), Arc::new(crate::vfs::LocalFs));
        let opts = TransferOpts {
            preserve: false,
            verify: true,
            ..TransferOpts::default()
        };
        let rename = Rename::new(Mask::new("*.txt"), Some("*.bak".into()));
        let res = run(
            spawn_transfer(from, vec![src], to, out.clone(), false, opts, rename),
            vec![],
        );
        assert!(!res.aborted, "{:?}", res.asks);
        let copy = out.join("notes.bak");
        assert!(copy.exists(), "the mask renamed it");
        assert_ne!(
            fs::metadata(&copy).unwrap().modified().unwrap(),
            old,
            "preserve was off"
        );
    }

    /// A skip says what and why, for the report a finished job keeps.
    #[test]
    fn a_sync_plan_copies_both_ways_and_deletes_into_the_trash() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        fs::create_dir_all(a.join("sub")).unwrap();
        fs::create_dir_all(b.join("sub")).unwrap();
        fs::write(a.join("sub/new.txt"), "from a").unwrap();
        fs::write(a.join("sub/both.txt"), "a wins").unwrap();
        fs::write(b.join("sub/both.txt"), "b loses").unwrap();
        fs::create_dir_all(b.join("only_b/x")).unwrap();
        fs::write(b.join("only_b/x/y"), "y").unwrap();
        let local: Arc<dyn FsProvider> = Arc::new(crate::vfs::LocalFs);
        let handle = spawn_sync(
            (local.clone(), a.clone()),
            (local, b.clone()),
            vec![
                (PathBuf::from("sub/new.txt"), SyncStep::ToRight),
                (PathBuf::from("sub/both.txt"), SyncStep::ToRight),
                (PathBuf::from("only_b"), SyncStep::ToLeft),
            ],
        );
        let outcome = run(handle, vec![]);
        assert!(!outcome.aborted, "{:?}", outcome.asks);
        assert_eq!(fs::read_to_string(b.join("sub/new.txt")).unwrap(), "from a");
        assert_eq!(
            fs::read_to_string(b.join("sub/both.txt")).unwrap(),
            "a wins"
        );
        assert_eq!(fs::read_to_string(a.join("only_b/x/y")).unwrap(), "y");
        assert!(
            outcome.asks.is_empty(),
            "a sync does not ask: {:?}",
            outcome.asks
        );
    }

    #[test]
    fn a_held_job_waits_and_a_paused_one_stops() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a.txt");
        fs::write(&src, "a").unwrap();
        let out = tmp.path().join("out");
        fs::create_dir(&out).unwrap();
        hold_new_jobs(true);
        let handle = spawn_copy(vec![src], out.clone(), TransferOpts::default(), None);
        hold_new_jobs(false);
        assert!(handle.is_held());
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!out.join("a.txt").exists(), "held: nothing begun");

        // released but paused: it stops at its first look
        handle.set_paused(true);
        handle.release();
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!out.join("a.txt").exists(), "paused: nothing written");

        handle.set_paused(false);
        let outcome = run(handle, vec![]);
        assert!(!outcome.aborted);
        assert!(out.join("a.txt").exists());

        // a job started afterwards is not held
        let later = spawn_delete(vec![out.join("a.txt")], true);
        assert!(!later.is_held());
        run(later, vec![]);
    }

    #[test]
    fn restore_puts_back_and_asks_before_overwriting() {
        use crate::trashcan::{TrashDir, TrashFs};
        let tmp = tempfile::tempdir().unwrap();
        let trash = tmp.path().join("Trash");
        fs::create_dir_all(trash.join("files")).unwrap();
        fs::create_dir_all(trash.join("info")).unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        for name in ["a.txt", "b.txt"] {
            fs::write(trash.join("files").join(name), name).unwrap();
            fs::write(
                trash.join("info").join(format!("{name}.trashinfo")),
                format!(
                    "[Trash Info]\nPath={}\nDeletionDate=2026-09-18T12:00:00\n",
                    home.join(name).display()
                ),
            )
            .unwrap();
        }
        // b.txt has a new file where it used to be
        fs::write(home.join("b.txt"), "new").unwrap();
        let fs_ = Arc::new(TrashFs::with_dirs(vec![TrashDir {
            path: trash.clone(),
            top: None,
        }]));
        let handle = spawn_restore(fs_, vec![home.join("a.txt"), home.join("b.txt")], true);
        let mut restored = Vec::new();
        let mut asks = 0;
        loop {
            match handle.events.recv().unwrap() {
                JobEvent::Restored { path } => restored.push(path),
                JobEvent::AskError { .. } => {
                    asks += 1;
                    handle.replies.send(Reply::Skip).unwrap();
                }
                JobEvent::Done { files_done, .. } => {
                    assert_eq!(files_done, 1);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(restored, [home.join("a.txt")]);
        assert_eq!(fs::read_to_string(home.join("a.txt")).unwrap(), "a.txt");
        assert_eq!(asks, 1, "the occupied name was asked about");
        assert_eq!(fs::read_to_string(home.join("b.txt")).unwrap(), "new");
        assert!(
            trash.join("files/b.txt").exists(),
            "and b.txt is still in the trash"
        );
    }

    #[test]
    fn every_skip_is_reported_with_its_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a.txt");
        fs::write(&src, b"new").unwrap();
        let out = tmp.path().join("out");
        fs::create_dir(&out).unwrap();
        fs::write(out.join("a.txt"), b"old").unwrap();
        let handle = spawn_copy(vec![src], out.clone(), TransferOpts::default(), None);
        let mut report = Vec::new();
        loop {
            match handle.events.recv().unwrap() {
                JobEvent::AskOverwrite { .. } => handle.replies.send(Reply::Skip).unwrap(),
                JobEvent::Skipped { path, reason } => report.push((path, reason)),
                JobEvent::Done { .. } => break,
                _ => {}
            }
        }
        assert_eq!(
            report,
            [(
                out.join("a.txt"),
                "already there, not overwritten".to_string()
            )]
        );
    }

    fn providers() -> (Arc<dyn FsProvider>, Arc<dyn FsProvider>) {
        (Arc::new(crate::vfs::LocalFs), Arc::new(crate::vfs::LocalFs))
    }

    /// Reget through a provider: the head already there stays, the rest
    /// is fetched from where it ends - an interrupted download finished
    /// rather than started over. It was a local copy's answer only.
    #[test]
    fn a_provider_copy_resumes() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("big.bin");
        let body: Vec<u8> = (0..300_000u32).map(|n| n as u8).collect();
        fs::write(&src, &body).unwrap();
        let out = tmp.path().join("out");
        fs::create_dir(&out).unwrap();
        fs::write(out.join("big.bin"), &body[..100_000]).unwrap();
        let (from, to) = providers();
        let handle = spawn_transfer(
            from,
            vec![src],
            to,
            out.clone(),
            false,
            TransferOpts::default(),
            None,
        );
        let mut offered = false;
        loop {
            match handle.events.recv().unwrap() {
                JobEvent::AskOverwrite { can_append, .. } => {
                    offered = can_append;
                    handle.replies.send(Reply::Reget).unwrap();
                }
                JobEvent::Done { aborted, .. } => {
                    assert!(!aborted);
                    break;
                }
                _ => {}
            }
        }
        assert!(offered, "Reget was not offered");
        assert_eq!(fs::read(out.join("big.bin")).unwrap(), body);
    }

    /// ...and a provider overwrite that fails keeps the old file, as a
    /// local one does now.
    #[test]
    fn a_failed_provider_overwrite_keeps_the_old_file() {
        let src = PathBuf::from("/proc/self/mem");
        if !src.exists() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("mem"), b"precious").unwrap();
        let (from, to) = providers();
        let res = run(
            spawn_transfer(
                from,
                vec![src],
                to,
                tmp.path().to_path_buf(),
                false,
                TransferOpts::default(),
                None,
            ),
            vec![Reply::Overwrite, Reply::Skip],
        );
        assert_eq!(res.skipped, 1, "{:?}", res.asks);
        assert_eq!(fs::read(tmp.path().join("mem")).unwrap(), b"precious");
        assert_eq!(
            fs::read_dir(tmp.path()).unwrap().count(),
            1,
            "no staging file left"
        );
    }

    /// The cross-device fallback copies and then deletes. Whatever the
    /// copy skipped - an overwrite answered Skip, an error answered
    /// Skip - never arrived, so it must not be deleted from the source.
    #[test]
    fn a_move_across_devices_keeps_what_it_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.txt"), b"a").unwrap();
        fs::write(src.join("b.txt"), b"new-b").unwrap();
        fs::write(src.join("sub/c.txt"), b"c").unwrap();
        let dst = tmp.path().join("dst");
        fs::create_dir(&dst).unwrap();
        fs::write(dst.join("b.txt"), b"old-b").unwrap();

        let (from, to) = (src.clone(), dst.clone());
        let out = run(
            spawn(move |ctx| move_across(ctx, &from, &to)),
            vec![Reply::Skip],
        );
        assert!(!out.aborted);
        assert_eq!(out.skipped, 1);
        // what arrived left the source
        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"a");
        assert_eq!(fs::read(dst.join("sub/c.txt")).unwrap(), b"c");
        assert!(!src.join("a.txt").exists());
        assert!(!src.join("sub").exists());
        // what was skipped is still where it was, and so is its directory
        assert_eq!(fs::read(src.join("b.txt")).unwrap(), b"new-b");
        assert_eq!(fs::read(dst.join("b.txt")).unwrap(), b"old-b");
    }

    /// Skip on a copy error is the same story: the file never arrived.
    #[test]
    fn a_move_across_devices_keeps_a_file_it_could_not_read() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("ok.txt"), b"ok").unwrap();
        let locked = src.join("locked.txt");
        fs::write(&locked, b"secret").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::File::open(&locked).is_ok() {
            return; // running as root: nothing is unreadable
        }
        let dst = tmp.path().join("dst");

        let (from, to) = (src.clone(), dst.clone());
        let out = run(
            spawn(move |ctx| move_across(ctx, &from, &to)),
            vec![Reply::Skip],
        );
        assert!(!out.aborted);
        assert!(!src.join("ok.txt").exists());
        assert_eq!(fs::read(dst.join("ok.txt")).unwrap(), b"ok");
        assert!(locked.exists(), "the unreadable file was deleted");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn a_move_reports_what_it_moved_and_the_undo_puts_it_back() {
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("to");
        fs::create_dir_all(from.join("tree/deep")).unwrap();
        fs::create_dir(&to).unwrap();
        fs::write(from.join("one.txt"), b"first").unwrap();
        fs::write(from.join("tree/deep/two.txt"), b"second").unwrap();

        let sources = vec![from.join("one.txt"), from.join("tree")];
        let (out, moved) = run_moves(
            spawn_move(sources, to.clone(), TransferOpts::default(), None),
            vec![],
        );
        assert!(!out.aborted && out.skipped == 0);
        assert_eq!(moved.len(), 2, "one pair per item moved: {moved:?}");
        assert!(to.join("one.txt").exists() && to.join("tree/deep/two.txt").exists());

        let (undone, _) = run_moves(spawn_undo_move(moved), vec![]);
        assert_eq!(undone.skipped, 0);
        assert!(from.join("one.txt").exists(), "the file came back");
        assert_eq!(
            fs::read_to_string(from.join("tree/deep/two.txt")).unwrap(),
            "second",
            "and so did the tree, with what was in it"
        );
        assert!(!to.join("one.txt").exists());
    }

    #[test]
    fn a_move_onto_a_taken_name_is_not_offered_as_undoable() {
        let tmp = tempfile::tempdir().unwrap();
        let to = tmp.path().join("to");
        fs::create_dir(&to).unwrap();
        fs::write(tmp.path().join("one.txt"), b"new").unwrap();
        fs::write(to.join("one.txt"), b"old").unwrap();

        // answer the overwrite prompt with yes: the move happens, but
        // putting the source back would not bring back what it landed on
        let (out, moved) = run_moves(
            spawn_move(
                vec![tmp.path().join("one.txt")],
                to.clone(),
                TransferOpts::default(),
                None,
            ),
            vec![Reply::Overwrite],
        );
        assert_eq!(out.files_done, 1);
        assert!(moved.is_empty(), "{moved:?}");
    }

    #[test]
    fn an_undo_leaves_a_name_that_was_taken_again_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let to = tmp.path().join("to");
        fs::create_dir(&to).unwrap();
        fs::write(tmp.path().join("one.txt"), b"moved").unwrap();
        let (_, moved) = run_moves(
            spawn_move(
                vec![tmp.path().join("one.txt")],
                to.clone(),
                TransferOpts::default(),
                None,
            ),
            vec![],
        );
        assert_eq!(moved.len(), 1);
        // something else is standing where it came from now
        fs::write(tmp.path().join("one.txt"), b"someone else").unwrap();

        let (undone, _) = run_moves(spawn_undo_move(moved), vec![]);
        assert_eq!(undone.skipped, 1);
        assert_eq!(
            fs::read_to_string(tmp.path().join("one.txt")).unwrap(),
            "someone else",
            "the undo did not overwrite it"
        );
        assert!(to.join("one.txt").exists(), "and left the moved copy alone");
    }

    #[test]
    fn copy_recursive_tree_with_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("tree");
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::write(src.join("a.txt"), b"hello").unwrap();
        fs::write(src.join("nested/b.txt"), b"world").unwrap();
        std::os::unix::fs::symlink("a.txt", src.join("link")).unwrap();
        let dst = tmp.path().join("dst");
        fs::create_dir(&dst).unwrap();

        let out = run(
            spawn_copy(
                vec![src.clone()],
                dst.clone(),
                TransferOpts::default(),
                None,
            ),
            vec![],
        );

        assert!(!out.aborted);
        assert!(out.asks.is_empty());
        assert_eq!(out.files_done, 3);
        assert_eq!(fs::read(dst.join("tree/a.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(dst.join("tree/nested/b.txt")).unwrap(), b"world");
        assert_eq!(
            fs::read_link(dst.join("tree/link")).unwrap(),
            PathBuf::from("a.txt")
        );
    }

    #[test]
    fn stable_symlinks_keep_pointing_at_the_same_file() {
        // /src/link -> ../target/file, copied to /out/deeper/link
        let target = stable_link_target(
            Path::new("../target/file"),
            Path::new("/src/link"),
            Path::new("/out/deeper/link"),
            None,
        );
        assert_eq!(target, Path::new("../../target/file"));

        // an absolute link already points where it points
        assert_eq!(
            stable_link_target(
                Path::new("/etc/hosts"),
                Path::new("/a/l"),
                Path::new("/b/l"),
                None
            ),
            Path::new("/etc/hosts")
        );

        // a link that does not move keeps its own value
        assert_eq!(
            stable_link_target(
                Path::new("sibling"),
                Path::new("/a/l"),
                Path::new("/a/l2"),
                None
            ),
            Path::new("sibling")
        );

        // ...and neither is a link that points inside the tree being
        // copied: rewriting it would tie the copy to the original
        assert_eq!(
            stable_link_target(
                Path::new("../a.txt"),
                Path::new("/tree/nested/link"),
                Path::new("/out/tree/nested/link"),
                Some(Path::new("/tree")),
            ),
            Path::new("../a.txt")
        );
    }

    #[test]
    fn a_copied_relative_link_still_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let src_dir = tmp.path().join("src");
        let out_dir = tmp.path().join("out/deeper");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&out_dir).unwrap();
        fs::write(tmp.path().join("data.txt"), b"payload").unwrap();
        std::os::unix::fs::symlink("../data.txt", src_dir.join("link")).unwrap();

        let out = run(
            spawn_copy(
                vec![src_dir.join("link")],
                out_dir.clone(),
                TransferOpts::default(),
                None,
            ),
            vec![],
        );
        assert_eq!(out.files_done, 1);
        // it is still a link, and it still reads the same bytes
        let copied = out_dir.join("link");
        assert!(copied.symlink_metadata().unwrap().is_symlink());
        assert_eq!(fs::read(&copied).unwrap(), b"payload");

        // ...which it would not without the rewrite
        let plain = TransferOpts {
            stable_symlinks: false,
            ..TransferOpts::default()
        };
        let out_dir2 = tmp.path().join("out2");
        fs::create_dir_all(&out_dir2).unwrap();
        run(
            spawn_copy(vec![src_dir.join("link")], out_dir2.clone(), plain, None),
            vec![],
        );
        assert_eq!(
            fs::read_link(out_dir2.join("link")).unwrap(),
            Path::new("../data.txt")
        );
    }

    #[test]
    fn follow_links_copies_the_content_instead_of_the_link() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("data.txt"), b"payload").unwrap();
        std::os::unix::fs::symlink("data.txt", tmp.path().join("link")).unwrap();
        let out = tmp.path().join("out");
        fs::create_dir(&out).unwrap();

        let opts = TransferOpts {
            follow_links: true,
            ..TransferOpts::default()
        };
        run(
            spawn_copy(vec![tmp.path().join("link")], out.clone(), opts, None),
            vec![],
        );
        let copied = out.join("link");
        assert!(
            !copied.symlink_metadata().unwrap().is_symlink(),
            "a real file now"
        );
        assert_eq!(fs::read(&copied).unwrap(), b"payload");
    }

    #[test]
    fn preserve_off_leaves_the_targets_own_times() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("f.txt");
        fs::write(&src, b"x").unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        set_mtime(&src, old);
        let out = tmp.path().join("out");
        fs::create_dir(&out).unwrap();

        let opts = TransferOpts {
            preserve: false,
            ..TransferOpts::default()
        };
        run(
            spawn_copy(vec![src.clone()], out.clone(), opts, None),
            vec![],
        );
        let copied = fs::metadata(out.join("f.txt")).unwrap().modified().unwrap();
        assert!(copied > old, "the copy is new, not as old as the source");

        // ...and on, the source's time comes along
        let out2 = tmp.path().join("out2");
        fs::create_dir(&out2).unwrap();
        run(
            spawn_copy(vec![src], out2.clone(), TransferOpts::default(), None),
            vec![],
        );
        assert_eq!(
            fs::metadata(out2.join("f.txt"))
                .unwrap()
                .modified()
                .unwrap(),
            old
        );
    }

    /// MC's "dive into subdirs": off, a directory copied onto an
    /// existing one of its own name merges into it.
    #[test]
    fn dive_decides_whether_a_directory_lands_inside_or_merges() {
        let make = |root: &Path| {
            let foo = root.join("foo");
            fs::create_dir_all(&foo).unwrap();
            fs::write(foo.join("bar"), b"x").unwrap();
            let bla_foo = root.join("bla/foo");
            fs::create_dir_all(&bla_foo).unwrap();
            (foo, bla_foo)
        };

        let on = tempfile::tempdir().unwrap();
        let (foo, bla_foo) = make(on.path());
        run(
            spawn_copy(vec![foo], bla_foo.clone(), TransferOpts::default(), None),
            vec![],
        );
        assert!(bla_foo.join("foo/bar").is_file(), "dive on: inside it");

        let off = tempfile::tempdir().unwrap();
        let (foo, bla_foo) = make(off.path());
        let opts = TransferOpts {
            dive: false,
            ..TransferOpts::default()
        };
        run(spawn_copy(vec![foo], bla_foo.clone(), opts, None), vec![]);
        assert!(bla_foo.join("bar").is_file(), "dive off: merged in");
        assert!(!bla_foo.join("foo").exists());
    }

    /// The per-file bar needs the file's own numbers, not just the
    /// running total.
    #[test]
    fn progress_reports_the_file_in_hand() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("big.bin");
        fs::write(&src, vec![7u8; CHUNK * 3 + 11]).unwrap();
        let out = tmp.path().join("out");
        fs::create_dir(&out).unwrap();

        let handle = spawn_copy(vec![src], out, TransferOpts::default(), None);
        let (mut seen_total, mut seen_partial) = (0u64, false);
        loop {
            match handle.events.recv().unwrap() {
                JobEvent::Progress {
                    file_done,
                    file_total,
                    ..
                } => {
                    seen_total = seen_total.max(file_total);
                    if file_done > 0 && file_done < file_total {
                        seen_partial = true;
                    }
                }
                JobEvent::Done { .. } => break,
                _ => {}
            }
        }
        assert_eq!(seen_total, (CHUNK * 3 + 11) as u64);
        assert!(seen_partial, "the bar needs something between 0 and done");
    }

    #[test]
    fn a_recursive_chmod_reaches_the_whole_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tree");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/file.txt"), b"x").unwrap();

        let out = run(
            spawn_attrs(
                vec![dir.clone()],
                Attrs {
                    mode: Some(0o755),
                    ..Attrs::default()
                },
                true,
            ),
            vec![],
        );
        assert_eq!(
            out.files_done, 3,
            "the directory, the subdirectory, the file"
        );
        for path in [dir.clone(), dir.join("sub"), dir.join("sub/file.txt")] {
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o755, "{}", path.display());
        }
    }

    /// The order matters: a directory whose execute bit is going away
    /// has to be walked *before* it loses it, or everything under it is
    /// silently missed.
    #[test]
    fn a_directory_is_changed_after_what_is_inside_it() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tree");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("file.txt"), b"x").unwrap();

        let out = run(
            spawn_attrs(
                vec![dir.clone()],
                Attrs {
                    mode: Some(0o600), // no execute: unreadable as a directory
                    ..Attrs::default()
                },
                true,
            ),
            vec![],
        );
        assert_eq!(
            out.files_done, 2,
            "the file was reached before the door shut"
        );
        // the directory has no execute bit now, so nothing inside it can
        // even be looked at until it is given one back - which is the
        // whole reason the walk has to happen before the change
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let file = fs::metadata(dir.join("file.txt")).unwrap();
        assert_eq!(file.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn without_recursion_only_the_named_paths_change() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tree");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("file.txt"), b"x").unwrap();
        fs::set_permissions(dir.join("file.txt"), fs::Permissions::from_mode(0o644)).unwrap();

        run(
            spawn_attrs(
                vec![dir.clone()],
                Attrs {
                    mode: Some(0o700),
                    ..Attrs::default()
                },
                false,
            ),
            vec![],
        );
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(dir.join("file.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644,
            "the file inside was not named, so it was not touched"
        );
    }

    #[test]
    fn an_empty_change_does_nothing_at_all() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("f.txt");
        fs::write(&file, b"x").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        let out = run(
            spawn_attrs(vec![file.clone()], Attrs::default(), true),
            vec![],
        );
        assert_eq!(out.files_done, 0);
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    /// MC copies "all the files matching the source mask" - the rest
    /// are passed over, not skipped-with-a-question.
    #[test]
    fn a_source_mask_leaves_the_others_where_they_are() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        fs::create_dir(&out).unwrap();
        for name in ["keep.txt", "ignore.log"] {
            fs::write(tmp.path().join(name), b"x").unwrap();
        }

        let rename = Rename::new(Mask::new("*.txt"), None);
        assert!(rename.is_some(), "a filtering mask is worth carrying");
        let result = run(
            spawn_copy(
                vec![tmp.path().join("keep.txt"), tmp.path().join("ignore.log")],
                out.clone(),
                TransferOpts::default(),
                rename,
            ),
            vec![],
        );
        assert_eq!(result.files_done, 1);
        assert!(out.join("keep.txt").is_file());
        assert!(!out.join("ignore.log").exists());
    }

    #[test]
    fn a_target_mask_renames_on_the_way() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        fs::create_dir(&out).unwrap();
        fs::write(tmp.path().join("foo.tar.gz"), b"x").unwrap();

        run(
            spawn_copy(
                vec![tmp.path().join("foo.tar.gz")],
                out.clone(),
                TransferOpts::default(),
                Rename::new(Mask::new("*.tar.gz"), Some("*.tgz".into())),
            ),
            vec![],
        );
        assert!(out.join("foo.tgz").is_file(), "renamed as it landed");
        assert!(!out.join("foo.tar.gz").exists());
    }

    #[test]
    fn a_mask_that_neither_filters_nor_renames_is_dropped() {
        assert!(Rename::new(Mask::new("*"), None).is_none());
    }

    /// MC's Append: the source goes on the end of what is already there.
    #[test]
    fn append_adds_to_the_target_instead_of_replacing_it() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("log.txt");
        fs::write(&src, b"second\n").unwrap();
        let dst_dir = tmp.path().join("out");
        fs::create_dir(&dst_dir).unwrap();
        fs::write(dst_dir.join("log.txt"), b"first\n").unwrap();

        let out = run(
            spawn_copy(vec![src], dst_dir.clone(), TransferOpts::default(), None),
            vec![Reply::Append],
        );
        assert_eq!(out.files_done, 1);
        assert_eq!(out.skipped, 0);
        assert_eq!(
            fs::read(dst_dir.join("log.txt")).unwrap(),
            b"first\nsecond\n"
        );
    }

    /// MC's Reget: what is on disk is taken to be the head of the
    /// source, so only the rest is fetched.
    #[test]
    fn reget_resumes_a_half_copied_file() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("big.bin");
        fs::write(&src, b"0123456789").unwrap();
        let dst_dir = tmp.path().join("out");
        fs::create_dir(&dst_dir).unwrap();
        fs::write(dst_dir.join("big.bin"), b"0123").unwrap();

        let out = run(
            spawn_copy(
                vec![src.clone()],
                dst_dir.clone(),
                TransferOpts::default(),
                None,
            ),
            vec![Reply::Reget],
        );
        assert_eq!(out.files_done, 1);
        assert_eq!(fs::read(dst_dir.join("big.bin")).unwrap(), b"0123456789");

        // a target that is already as long as the source has nothing left
        let out = run(
            spawn_copy(vec![src], dst_dir.clone(), TransferOpts::default(), None),
            vec![Reply::Reget],
        );
        assert_eq!(out.files_done, 1);
        assert_eq!(fs::read(dst_dir.join("big.bin")).unwrap(), b"0123456789");
    }

    /// MC's Update: answered once, it decides every remaining file by
    /// comparing modification times - including the file it was
    /// answered on.
    #[test]
    fn update_overwrites_only_where_the_source_is_newer() {
        let tmp = tempfile::tempdir().unwrap();
        let dst_dir = tmp.path().join("out");
        fs::create_dir(&dst_dir).unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let new = old + std::time::Duration::from_secs(3600);

        // a.txt: source newer than target; b.txt: the other way round
        let a = tmp.path().join("a.txt");
        fs::write(&a, b"new-a").unwrap();
        set_mtime(&a, new);
        fs::write(dst_dir.join("a.txt"), b"old-a").unwrap();
        set_mtime(&dst_dir.join("a.txt"), old);

        let b = tmp.path().join("b.txt");
        fs::write(&b, b"old-b").unwrap();
        set_mtime(&b, old);
        fs::write(dst_dir.join("b.txt"), b"new-b").unwrap();
        set_mtime(&dst_dir.join("b.txt"), new);

        let out = run(
            spawn_copy(vec![a, b], dst_dir.clone(), TransferOpts::default(), None),
            vec![Reply::UpdateAll],
        );
        // asked once, then the policy answered the rest
        assert_eq!(out.asks.len(), 1);
        assert_eq!(fs::read(dst_dir.join("a.txt")).unwrap(), b"new-a");
        assert_eq!(fs::read(dst_dir.join("b.txt")).unwrap(), b"new-b");
        assert_eq!(out.skipped, 1);
    }

    /// MC's "If size differs": same shape, comparing lengths.
    #[test]
    fn size_differs_overwrites_only_the_ones_that_differ() {
        let tmp = tempfile::tempdir().unwrap();
        let dst_dir = tmp.path().join("out");
        fs::create_dir(&dst_dir).unwrap();

        let same = tmp.path().join("same.txt");
        fs::write(&same, b"1234").unwrap();
        fs::write(dst_dir.join("same.txt"), b"abcd").unwrap();

        let grown = tmp.path().join("grown.txt");
        fs::write(&grown, b"123456").unwrap();
        fs::write(dst_dir.join("grown.txt"), b"abcd").unwrap();

        let out = run(
            spawn_copy(
                vec![grown, same],
                dst_dir.clone(),
                TransferOpts::default(),
                None,
            ),
            vec![Reply::SizeDiffersAll],
        );
        assert_eq!(out.asks.len(), 1);
        assert_eq!(fs::read(dst_dir.join("grown.txt")).unwrap(), b"123456");
        assert_eq!(fs::read(dst_dir.join("same.txt")).unwrap(), b"abcd");
        assert_eq!(out.skipped, 1);
    }

    /// The prompt has to say what it is asking about.
    #[test]
    fn the_overwrite_question_carries_both_files() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("f.txt");
        fs::write(&src, b"123456").unwrap();
        let dst_dir = tmp.path().join("out");
        fs::create_dir(&dst_dir).unwrap();
        fs::write(dst_dir.join("f.txt"), b"ab").unwrap();

        let handle = spawn_copy(vec![src], dst_dir.clone(), TransferOpts::default(), None);
        let (mut asked_src, mut asked_dst, mut appendable) = (0, 0, false);
        loop {
            match handle.events.recv().unwrap() {
                JobEvent::AskOverwrite {
                    src,
                    dst,
                    can_append,
                    ..
                } => {
                    asked_src = src.size;
                    asked_dst = dst.size;
                    appendable = can_append;
                    handle.replies.send(Reply::Skip).unwrap();
                }
                JobEvent::Done { .. } => break,
                _ => {}
            }
        }
        assert_eq!((asked_src, asked_dst), (6, 2));
        assert!(appendable, "a local file copy can append");
    }

    fn set_mtime(path: &Path, when: std::time::SystemTime) {
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_times(fs::FileTimes::new().set_modified(when))
            .unwrap();
    }

    #[test]
    fn overwrite_asks_and_honors_skip_then_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("new.txt");
        fs::write(&src, b"new").unwrap();
        let dst_dir = tmp.path().join("out");
        fs::create_dir(&dst_dir).unwrap();
        fs::write(dst_dir.join("new.txt"), b"old").unwrap();

        let out = run(
            spawn_copy(
                vec![src.clone()],
                dst_dir.clone(),
                TransferOpts::default(),
                None,
            ),
            vec![Reply::Skip],
        );
        assert_eq!(out.asks.len(), 1);
        assert_eq!(out.skipped, 1);
        assert_eq!(fs::read(dst_dir.join("new.txt")).unwrap(), b"old");

        let out = run(
            spawn_copy(vec![src], dst_dir.clone(), TransferOpts::default(), None),
            vec![Reply::Overwrite],
        );
        assert_eq!(out.files_done, 1);
        assert_eq!(fs::read(dst_dir.join("new.txt")).unwrap(), b"new");
    }

    #[test]
    fn copy_dir_into_itself_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("d");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("f"), b"x").unwrap();

        let out = run(
            spawn_copy(
                vec![dir.clone()],
                dir.clone(),
                TransferOpts::default(),
                None,
            ),
            vec![Reply::Skip],
        );

        assert!(!out.aborted);
        assert_eq!(out.asks.len(), 1);
        assert!(out.asks[0].contains("into itself"));
        assert!(!dir.join("d").exists());
    }

    #[test]
    fn move_renames_within_filesystem() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a.txt");
        fs::write(&src, b"payload").unwrap();
        let dst_dir = tmp.path().join("out");
        fs::create_dir(&dst_dir).unwrap();

        let out = run(
            spawn_move(
                vec![src.clone()],
                dst_dir.clone(),
                TransferOpts::default(),
                None,
            ),
            vec![],
        );

        assert!(!out.aborted);
        assert!(!src.exists());
        assert_eq!(fs::read(dst_dir.join("a.txt")).unwrap(), b"payload");
    }

    #[test]
    fn permanent_delete_removes_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("gone");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/f"), b"x").unwrap();
        fs::write(dir.join("g"), b"y").unwrap();

        let out = run(spawn_delete(vec![dir.clone()], true), vec![]);

        assert!(!out.aborted);
        assert_eq!(out.files_done, 2);
        assert!(!dir.exists());
    }

    /// An archive is somebody else's files: a setuid or setgid bit in
    /// one would make the extracted program run as whoever extracted
    /// it, which is not a permission the archive can grant. tar drops
    /// them for anyone but root; so does this.
    #[test]
    fn extracting_drops_setuid_and_setgid() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let archive_path = tmp.path().join("a.tar.gz");
        let gz = GzEncoder::new(
            fs::File::create(&archive_path).unwrap(),
            Compression::default(),
        );
        let mut tar = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_size(2);
        header.set_mode(0o6755);
        header.set_cksum();
        tar.append_data(&mut header, "tool", &b"#!"[..]).unwrap();
        tar.into_inner().unwrap().finish().unwrap();

        let afs = Arc::new(crate::archive::ArchiveFs::open(&archive_path).unwrap());
        let out = tmp.path().join("out");
        fs::create_dir(&out).unwrap();
        let result = run(
            spawn_extract(afs, vec![PathBuf::from("tool")], out.clone()),
            vec![],
        );
        assert!(!result.aborted);
        let mode = fs::metadata(out.join("tool")).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o755, "{mode:o}");
    }

    /// A 7z's members come out of one run of the tool, unpacked beside
    /// the destination and gone from there once the job is done.
    #[test]
    fn extracting_a_7z_unpacks_it_once_and_cleans_up() {
        let Some(packer) = ["7za", "7z", "7zz"].into_iter().find(|tool| {
            std::process::Command::new(tool)
                .arg("-h")
                .stdout(std::process::Stdio::null())
                .status()
                .is_ok()
        }) else {
            eprintln!("skipping: no 7z binary");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("tree/sub")).unwrap();
        for n in 0..20 {
            fs::write(src.join(format!("tree/sub/{n}.txt")), format!("{n}\n")).unwrap();
        }
        fs::write(src.join("solo.txt"), b"solo\n").unwrap();
        let status = std::process::Command::new(packer)
            .args(["a", "-bd", "../box.7z", "."])
            .current_dir(&src)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let afs = Arc::new(crate::archive::ArchiveFs::open(&tmp.path().join("box.7z")).unwrap());
        let out = tmp.path().join("out");
        fs::create_dir(&out).unwrap();
        let result = run(
            spawn_extract(
                afs,
                vec![PathBuf::from("solo.txt"), PathBuf::from("tree")],
                out.clone(),
            ),
            vec![],
        );
        assert!(!result.aborted);
        assert_eq!(result.files_done, 21);
        for n in 0..20 {
            assert_eq!(
                fs::read_to_string(out.join(format!("tree/sub/{n}.txt"))).unwrap(),
                format!("{n}\n")
            );
        }
        assert_eq!(fs::read_to_string(out.join("solo.txt")).unwrap(), "solo\n");
        let mut left: Vec<_> = fs::read_dir(&out)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        left.sort();
        assert_eq!(left, ["solo.txt", "tree"]);
    }

    /// A level reaches the archive: stored is bigger than best, a
    /// .tar.zst reads back through the reader and the zstd tool, and
    /// the same goes for a zip at 0 and at 9.
    #[test]
    fn packing_takes_a_level_and_writes_tar_zst() {
        let tmp = tempfile::tempdir().unwrap();
        let payload = tmp.path().join("payload");
        fs::create_dir(&payload).unwrap();
        let text = "a line that repeats, and compresses well\n".repeat(4000);
        fs::write(payload.join("big.txt"), &text).unwrap();
        let size = |name: &str, level: Option<u32>| {
            let archive = tmp.path().join(name);
            let _ = fs::remove_file(&archive);
            type Pack = fn(Vec<PathBuf>, PathBuf, PathBuf, Option<u32>) -> JobHandle;
            let pack: Pack = match name.ends_with(".zip") {
                true => spawn_pack_zip,
                false => spawn_pack_tar,
            };
            let out = run(
                pack(
                    vec![payload.clone()],
                    archive.clone(),
                    PathBuf::new(),
                    level,
                ),
                vec![],
            );
            assert!(!out.aborted, "{name} at {level:?}");
            let afs = crate::archive::ArchiveFs::open(&archive).unwrap();
            let mut back = String::new();
            afs.open_read(Path::new("payload/big.txt"))
                .unwrap()
                .read_to_string(&mut back)
                .unwrap();
            assert!(back == text, "{name} at {level:?} did not read back");
            fs::metadata(&archive).unwrap().len()
        };
        // 0 stores, where the format has a way to: xz's 0 and bzip2's
        // lowest still compress, as `xz -0` and `bzip2 -1` do
        for name in ["x.tar.gz", "x.zip", "x.tar.zst"] {
            let (stored, best) = (size(name, Some(0)), size(name, Some(9)));
            assert!(stored > best * 4, "{name}: {stored} stored, {best} at 9");
            assert!(size(name, None) < stored, "{name}: the default compresses");
        }
        for name in ["x.tar.xz", "x.tar.bz2"] {
            for level in [Some(0), Some(9), None] {
                size(name, level);
            }
        }
        let zst = tmp.path().join("x.tar.zst");
        size("x.tar.zst", None);
        if let Ok(status) = std::process::Command::new("zstd")
            .args(["-q", "-t"])
            .arg(&zst)
            .status()
        {
            assert!(status.success(), "zstd -t refused what was written");
        }
    }

    #[test]
    fn a_tar_name_nobody_taught_it_is_refused_not_bzipped() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["x.tar", "x.tgz", "x.tar.xz", "x.tbz"] {
            assert!(
                TarSink::create(&tmp.path().join(name), name, None).is_ok(),
                "{name}"
            );
        }
        let err = TarSink::create(&tmp.path().join("x.tar.lz"), "x.tar.lz", None)
            .err()
            .expect("a .tar.lz was written");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!tmp.path().join("x.tar.lz").exists());
        assert!(tar_source(&tmp.path().join("x.tar"), "x.tar.lz").is_err());
    }

    #[test]
    fn extract_from_targz_recreates_tree() {
        use flate2::Compression;
        use flate2::write::GzEncoder;

        let tmp = tempfile::tempdir().unwrap();
        let archive_path = tmp.path().join("a.tar.gz");
        let gz = GzEncoder::new(
            fs::File::create(&archive_path).unwrap(),
            Compression::default(),
        );
        let mut tar = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_size(6);
        header.set_mode(0o640);
        header.set_cksum();
        tar.append_data(&mut header, "sub/data.txt", &b"inside"[..])
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();

        let afs = Arc::new(crate::archive::ArchiveFs::open(&archive_path).unwrap());
        let out = tmp.path().join("out");
        fs::create_dir(&out).unwrap();

        let result = run(
            spawn_extract(afs, vec![PathBuf::from("sub")], out.clone()),
            vec![],
        );

        assert!(!result.aborted);
        assert_eq!(result.files_done, 1);
        assert_eq!(fs::read(out.join("sub/data.txt")).unwrap(), b"inside");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(out.join("sub/data.txt"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o640);
        }
    }

    #[test]
    fn copy_preserves_mtime() {
        use std::time::{Duration, UNIX_EPOCH};
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("old.txt");
        fs::write(&src, b"x").unwrap();
        let stamp = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        fs::File::open(&src)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(stamp))
            .unwrap();
        let out = tmp.path().join("out");
        fs::create_dir(&out).unwrap();

        let result = run(
            spawn_copy(vec![src], out.clone(), TransferOpts::default(), None),
            vec![],
        );

        assert!(!result.aborted);
        let copied = fs::metadata(out.join("old.txt"))
            .unwrap()
            .modified()
            .unwrap();
        let diff = copied
            .duration_since(stamp)
            .unwrap_or_else(|e| e.duration());
        assert!(diff < Duration::from_secs(2), "mtime drifted by {diff:?}");
    }

    #[test]
    fn pack_appends_into_existing_zip() {
        use std::io::Write as _;
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("box.zip");
        let mut zip = zip::ZipWriter::new(fs::File::create(&archive).unwrap());
        zip.start_file("existing.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"was here").unwrap();
        zip.finish().unwrap();

        let payload = tmp.path().join("payload");
        fs::create_dir(&payload).unwrap();
        fs::write(payload.join("new.txt"), b"added").unwrap();

        let result = run(
            spawn_pack_zip(vec![payload.clone()], archive.clone(), PathBuf::new(), None),
            vec![],
        );
        assert!(!result.aborted);
        assert_eq!(result.files_done, 1);

        let afs = crate::archive::ArchiveFs::open(&archive).unwrap();
        let mut content = String::new();
        afs.open_read(Path::new("payload/new.txt"))
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "added");
        // pre-existing member survived the append
        content.clear();
        afs.open_read(Path::new("existing.txt"))
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "was here");
    }

    /// A name that is not there yet is packed into rather than
    /// appended to: this is what Alt+F5 does, and the only difference
    /// from appending is that there is nothing to carry across.
    #[test]
    fn checksums_are_written_and_checked() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::write(dir.join("a.txt"), b"alpha").unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/b.txt"), b"bravo").unwrap();
        let sums = dir.join("SHA256SUMS");

        let out = run(
            spawn_checksums(
                dir.to_path_buf(),
                vec![dir.join("a.txt"), dir.join("sub")],
                sums.clone(),
            ),
            vec![],
        );
        assert!(!out.aborted && out.asks.is_empty(), "{:?}", out.asks);
        assert_eq!(out.files_done, 2);

        let text = fs::read_to_string(&sums).unwrap();
        // what sha256sum writes, and what it reads: hash, two spaces,
        // the name relative to where the file sits
        assert!(text.contains("  a.txt\n"), "{text}");
        assert!(text.contains("  sub/b.txt\n"), "{text}");
        let hash = text
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap();
        assert!(
            hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()),
            "{hash}"
        );

        let out = run(
            spawn_verify_checksums(dir.to_path_buf(), sums.clone()),
            vec![],
        );
        assert_eq!((out.files_done, out.skipped), (2, 0), "both matched");

        // change one byte and it does not match any more
        fs::write(dir.join("a.txt"), b"alphb").unwrap();
        let out = run(
            spawn_verify_checksums(dir.to_path_buf(), sums.clone()),
            vec![],
        );
        assert_eq!((out.files_done, out.skipped), (1, 1));

        // ...and a file that is not there does not match either
        fs::remove_file(dir.join("sub/b.txt")).unwrap();
        let out = run(spawn_verify_checksums(dir.to_path_buf(), sums), vec![]);
        assert_eq!((out.files_done, out.skipped), (0, 2));
    }

    #[test]
    fn sha256_is_the_hash_everything_else_calls_sha256() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f");
        fs::write(&path, b"abc").unwrap();
        // the canonical test vector
        assert_eq!(
            sha256_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        fs::write(&path, b"").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn verify_reads_the_copy_back() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.bin");
        let dst = tmp.path().join("dst");
        fs::create_dir(&dst).unwrap();
        // bigger than one chunk, so the comparison has to walk it
        fs::write(&src, vec![7u8; CHUNK * 2 + 11]).unwrap();

        let opts = TransferOpts {
            verify: true,
            ..TransferOpts::default()
        };
        let out = run(
            spawn_copy(vec![src.clone()], dst.clone(), opts, None),
            vec![],
        );
        assert!(!out.aborted && out.asks.is_empty(), "{:?}", out.asks);
        assert_eq!(out.files_done, 1);

        // ...and it is the comparison that would have caught a bad one
        let other = tmp.path().join("other.bin");
        fs::write(&other, vec![7u8; CHUNK * 2 + 10]).unwrap();
        assert!(verify_copy(&src, &other).is_err(), "a shorter file passed");
        fs::write(&other, {
            let mut bytes = vec![7u8; CHUNK * 2 + 11];
            bytes[CHUNK + 5] = 9;
            bytes
        })
        .unwrap();
        assert!(verify_copy(&src, &other).is_err(), "a changed byte passed");
        assert!(verify_copy(&src, &dst.join("src.bin")).is_ok());
    }

    #[test]
    fn wipe_overwrites_before_it_unlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path().join("tree");
        fs::create_dir_all(tree.join("deep")).unwrap();
        fs::write(tree.join("secret.txt"), b"the password is hunter2").unwrap();
        fs::write(tree.join("deep/also.txt"), b"and here too").unwrap();
        std::os::unix::fs::symlink("secret.txt", tree.join("link")).unwrap();

        let out = run(spawn_wipe(vec![tree.clone()]), vec![]);
        assert!(!out.aborted && out.asks.is_empty(), "{:?}", out.asks);
        assert!(!tree.exists(), "the tree went");

        // the bytes are gone from the filesystem as far as reading it
        // can tell - which is all this promises
        let mut found = false;
        for entry in walkdir(tmp.path()) {
            if let Ok(text) = fs::read(&entry) {
                found |= String::from_utf8_lossy(&text).contains("hunter2");
            }
        }
        assert!(!found, "the words are still somewhere in the tempdir");
    }

    /// Every file under a directory, for the test above.
    fn walkdir(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(root) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walkdir(&path));
            } else {
                out.push(path);
            }
        }
        out
    }

    #[test]
    fn packing_into_a_name_that_is_not_there_yet_creates_it() {
        for name in ["new.zip", "new.tar", "new.tar.gz", "new.tar.bz2"] {
            let tmp = tempfile::tempdir().unwrap();
            let archive = tmp.path().join(name);
            let payload = tmp.path().join("payload");
            fs::create_dir(&payload).unwrap();
            fs::write(payload.join("one.txt"), b"first").unwrap();
            fs::create_dir(payload.join("deep")).unwrap();
            fs::write(payload.join("deep/two.txt"), b"second").unwrap();

            let handle = match name.ends_with(".zip") {
                true => {
                    spawn_pack_zip(vec![payload.clone()], archive.clone(), PathBuf::new(), None)
                }
                false => {
                    spawn_pack_tar(vec![payload.clone()], archive.clone(), PathBuf::new(), None)
                }
            };
            let result = run(handle, vec![]);
            assert!(!result.aborted, "{name}");
            assert_eq!(result.files_done, 2, "{name}");

            let afs = crate::archive::ArchiveFs::open(&archive).unwrap();
            for (member, want) in [
                ("payload/one.txt", "first"),
                ("payload/deep/two.txt", "second"),
            ] {
                let mut content = String::new();
                afs.open_read(Path::new(member))
                    .unwrap()
                    .read_to_string(&mut content)
                    .unwrap();
                assert_eq!(content, want, "{name}: {member}");
            }
        }
    }

    /// An archive with a small tree in it, in whichever container.
    fn seed_archive(dir: &Path, name: &str) -> PathBuf {
        let archive = dir.join(name);
        if name.ends_with(".zip") {
            let mut zip = zip::ZipWriter::new(fs::File::create(&archive).unwrap());
            let options = zip::write::SimpleFileOptions::default();
            for (member, body) in [
                ("keep.txt", &b"kept"[..]),
                ("drop.txt", &b"dropped"[..]),
                ("dir/inner.txt", &b"inside"[..]),
                ("dir/second.txt", &b"also inside"[..]),
            ] {
                zip.start_file(member, options).unwrap();
                zip.write_all(body).unwrap();
            }
            zip.finish().unwrap();
        } else {
            let sink = TarSink::create(&archive, name, None).unwrap();
            let mut tar = tar::Builder::new(sink);
            for (member, body) in [
                ("keep.txt", &b"kept"[..]),
                ("drop.txt", &b"dropped"[..]),
                ("dir/inner.txt", &b"inside"[..]),
                ("dir/second.txt", &b"also inside"[..]),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                tar.append_data(&mut header, member, body).unwrap();
            }
            tar.into_inner().unwrap().finish().unwrap();
        }
        archive
    }

    fn member(archive: &Path, path: &str) -> Option<String> {
        let afs = crate::archive::ArchiveFs::open(archive).ok()?;
        let mut text = String::new();
        afs.open_read(Path::new(path))
            .ok()?
            .read_to_string(&mut text)
            .ok()?;
        Some(text)
    }

    #[test]
    fn archive_edit_removes_renames_and_makes_directories() {
        for name in ["box.zip", "box.tar", "box.tar.gz"] {
            let tmp = tempfile::tempdir().unwrap();
            let archive = seed_archive(tmp.path(), name);
            let result = run(
                spawn_archive_edit(
                    archive.clone(),
                    vec![
                        ArchiveOp::Remove(PathBuf::from("drop.txt")),
                        ArchiveOp::Rename {
                            from: PathBuf::from("dir"),
                            to: PathBuf::from("moved"),
                        },
                        ArchiveOp::Mkdir(PathBuf::from("fresh")),
                    ],
                ),
                vec![],
            );
            assert!(!result.aborted, "{name}");

            assert_eq!(
                member(&archive, "keep.txt").as_deref(),
                Some("kept"),
                "{name}"
            );
            assert!(member(&archive, "drop.txt").is_none(), "{name}");
            // a rename takes the whole subtree with it
            assert_eq!(
                member(&archive, "moved/inner.txt").as_deref(),
                Some("inside"),
                "{name}"
            );
            assert_eq!(
                member(&archive, "moved/second.txt").as_deref(),
                Some("also inside"),
                "{name}"
            );
            assert!(member(&archive, "dir/inner.txt").is_none(), "{name}");

            let afs = crate::archive::ArchiveFs::open(&archive).unwrap();
            assert!(afs.stat(Path::new("fresh")).unwrap().is_dir(), "{name}");
            assert!(afs.stat(Path::new("moved")).unwrap().is_dir(), "{name}");
        }
    }

    #[test]
    fn renaming_one_file_does_not_turn_it_into_a_directory() {
        // joining an empty remainder onto the new name appends a
        // separator, and a trailing slash is how an archive spells
        // "directory" - the file would arrive as an empty folder
        let ops = [ArchiveOp::Rename {
            from: PathBuf::from("keep.txt"),
            to: PathBuf::from("renamed.txt"),
        }];
        assert_eq!(
            ArchiveOp::apply(&ops, Path::new("keep.txt")),
            Some(PathBuf::from("renamed.txt"))
        );
        assert_eq!(
            ArchiveOp::apply(&ops, Path::new("keep.txt.bak")),
            Some(PathBuf::from("keep.txt.bak"))
        );
    }

    #[test]
    fn removing_a_directory_takes_what_is_inside_it() {
        for name in ["box.zip", "box.tar"] {
            let tmp = tempfile::tempdir().unwrap();
            let archive = seed_archive(tmp.path(), name);
            let result = run(
                spawn_archive_edit(
                    archive.clone(),
                    vec![ArchiveOp::Remove(PathBuf::from("dir"))],
                ),
                vec![],
            );
            assert!(!result.aborted, "{name}");
            assert!(member(&archive, "dir/inner.txt").is_none(), "{name}");
            assert!(member(&archive, "dir/second.txt").is_none(), "{name}");
            assert_eq!(
                member(&archive, "keep.txt").as_deref(),
                Some("kept"),
                "{name}"
            );
        }
    }

    #[test]
    fn an_archive_no_one_can_change_says_so_and_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("box.cpio");
        fs::write(&archive, b"whatever").unwrap();
        let result = run(
            spawn_archive_edit(archive.clone(), vec![ArchiveOp::Remove(PathBuf::from("x"))]),
            vec![Reply::Skip],
        );
        assert!(!result.aborted);
        assert!(
            result.asks.iter().any(|a| a.contains("can be changed")),
            "{:?}",
            result.asks
        );
        // the archive it could not change is exactly as it was
        assert_eq!(fs::read(&archive).unwrap(), b"whatever");
    }

    #[test]
    fn packing_into_a_zip_replaces_rather_than_shadows() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = seed_archive(tmp.path(), "box.zip");
        let payload = tmp.path().join("keep.txt");
        fs::write(&payload, b"the new bytes").unwrap();

        let result = run(
            spawn_pack_zip(vec![payload], archive.clone(), PathBuf::new(), None),
            vec![],
        );
        assert!(!result.aborted);
        assert_eq!(
            member(&archive, "keep.txt").as_deref(),
            Some("the new bytes")
        );
        // one member with that name, not two with the old one hidden
        let zip = zip::ZipArchive::new(fs::File::open(&archive).unwrap()).unwrap();
        let count = zip.file_names().filter(|n| *n == "keep.txt").count();
        assert_eq!(count, 1);
        // and everything else survived the rewrite
        assert_eq!(member(&archive, "dir/inner.txt").as_deref(), Some("inside"));
    }

    #[test]
    fn pack_rewrites_tar_archives() {
        for name in ["box.tar", "box.tar.gz"] {
            let tmp = tempfile::tempdir().unwrap();
            let archive = tmp.path().join(name);
            // existing archive with one member
            {
                let sink = TarSink::create(&archive, name, None).unwrap();
                let mut tar = tar::Builder::new(sink);
                let mut header = tar::Header::new_gnu();
                header.set_size(8);
                header.set_mode(0o644);
                header.set_cksum();
                tar.append_data(&mut header, "existing.txt", &b"was here"[..])
                    .unwrap();
                tar.into_inner().unwrap().finish().unwrap();
            }
            let payload = tmp.path().join("payload");
            fs::create_dir(&payload).unwrap();
            fs::write(payload.join("new.txt"), b"added").unwrap();
            std::os::unix::fs::symlink("new.txt", payload.join("link")).unwrap();

            let result = run(
                spawn_pack_tar(vec![payload.clone()], archive.clone(), PathBuf::new(), None),
                vec![],
            );
            assert!(!result.aborted, "{name}");
            assert_eq!(result.files_done, 2, "{name}"); // file + symlink

            let afs = crate::archive::ArchiveFs::open(&archive).unwrap();
            let mut content = String::new();
            afs.open_read(Path::new("payload/new.txt"))
                .unwrap()
                .read_to_string(&mut content)
                .unwrap();
            assert_eq!(content, "added", "{name}");
            content.clear();
            afs.open_read(Path::new("existing.txt"))
                .unwrap()
                .read_to_string(&mut content)
                .unwrap();
            assert_eq!(content, "was here", "{name}");
            let link = afs.stat(Path::new("payload/link")).unwrap();
            assert_eq!(
                link.link_target.as_deref(),
                Some(Path::new("new.txt")),
                "{name}"
            );
        }
    }

    #[test]
    fn transfer_across_providers_copies_tree_with_metadata() {
        use crate::vfs::LocalFs;
        use std::time::{Duration, UNIX_EPOCH};
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("tree");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("sub/f.txt"), b"payload").unwrap();
        std::os::unix::fs::symlink("f.txt", src.join("sub/link")).unwrap();
        let stamp = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        fs::File::open(src.join("sub/f.txt"))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(stamp))
            .unwrap();
        let dst = tmp.path().join("dst");
        fs::create_dir(&dst).unwrap();

        // two distinct Arcs → the cross-provider streaming path
        let out = run(
            spawn_transfer(
                Arc::new(LocalFs),
                vec![src.clone()],
                Arc::new(LocalFs),
                dst.clone(),
                false,
                TransferOpts::default(),
                None,
            ),
            vec![],
        );

        assert!(!out.aborted, "asks: {:?}", out.asks);
        assert_eq!(out.files_done, 2);
        assert_eq!(fs::read(dst.join("tree/sub/f.txt")).unwrap(), b"payload");
        assert_eq!(
            fs::read_link(dst.join("tree/sub/link")).unwrap(),
            PathBuf::from("f.txt")
        );
        let copied = fs::metadata(dst.join("tree/sub/f.txt"))
            .unwrap()
            .modified()
            .unwrap();
        let diff = copied
            .duration_since(stamp)
            .unwrap_or_else(|e| e.duration());
        assert!(diff < Duration::from_secs(2), "mtime drifted by {diff:?}");
        assert!(src.exists(), "copy must not remove the source");
    }

    #[test]
    fn transfer_move_same_provider_renames_and_removes_source() {
        use crate::vfs::LocalFs;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a.txt");
        fs::write(&src, b"gone").unwrap();
        let dst = tmp.path().join("out");
        fs::create_dir(&dst).unwrap();
        let fs_arc: Arc<dyn FsProvider> = Arc::new(LocalFs);

        let out = run(
            spawn_transfer(
                fs_arc.clone(),
                vec![src.clone()],
                fs_arc,
                dst.clone(),
                true,
                TransferOpts::default(),
                None,
            ),
            vec![],
        );

        assert!(!out.aborted);
        assert!(!src.exists());
        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"gone");
    }

    #[test]
    fn transfer_move_across_providers_copies_then_deletes() {
        use crate::vfs::LocalFs;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("tree");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("f"), b"x").unwrap();
        let dst = tmp.path().join("dst");
        fs::create_dir(&dst).unwrap();

        let out = run(
            spawn_transfer(
                Arc::new(LocalFs),
                vec![src.clone()],
                Arc::new(LocalFs),
                dst.clone(),
                true,
                TransferOpts::default(),
                None,
            ),
            vec![],
        );

        assert!(!out.aborted, "asks: {:?}", out.asks);
        assert_eq!(fs::read(dst.join("tree/f")).unwrap(), b"x");
        assert!(!src.exists(), "move must remove the source tree");
    }

    #[test]
    fn transfer_overwrite_asks_through_provider() {
        use crate::vfs::LocalFs;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("f.txt");
        fs::write(&src, b"new").unwrap();
        let dst = tmp.path().join("out");
        fs::create_dir(&dst).unwrap();
        fs::write(dst.join("f.txt"), b"old").unwrap();

        let out = run(
            spawn_transfer(
                Arc::new(LocalFs),
                vec![src],
                Arc::new(LocalFs),
                dst.clone(),
                false,
                TransferOpts::default(),
                None,
            ),
            vec![Reply::Skip],
        );

        assert_eq!(out.skipped, 1);
        assert_eq!(fs::read(dst.join("f.txt")).unwrap(), b"old");
    }

    #[test]
    fn delete_fs_removes_tree_through_provider() {
        use crate::vfs::LocalFs;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("gone");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/f"), b"x").unwrap();

        let out = run(
            spawn_delete_fs(Arc::new(LocalFs), vec![dir.clone()]),
            vec![],
        );

        assert!(!out.aborted);
        assert_eq!(out.files_done, 1);
        assert!(!dir.exists());
    }

    #[test]
    fn transfer_into_readonly_provider_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let archive_path = tmp.path().join("a.zip");
        let mut zip = zip::ZipWriter::new(fs::File::create(&archive_path).unwrap());
        zip.start_file("x", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.finish().unwrap();
        let afs: Arc<dyn FsProvider> =
            Arc::new(crate::archive::ArchiveFs::open(&archive_path).unwrap());
        let src = tmp.path().join("f");
        fs::write(&src, b"x").unwrap();

        let out = run(
            spawn_transfer(
                Arc::new(crate::vfs::LocalFs),
                vec![src],
                afs,
                PathBuf::from("/"),
                false,
                TransferOpts::default(),
                None,
            ),
            vec![Reply::Skip],
        );

        assert_eq!(out.asks.len(), 1);
        assert!(out.asks[0].contains("read-only"));
    }

    #[test]
    fn abort_reply_stops_the_job() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a");
        fs::write(&src, b"1").unwrap();
        let dst_dir = tmp.path().join("out");
        fs::create_dir(&dst_dir).unwrap();
        fs::write(dst_dir.join("a"), b"old").unwrap();

        let out = run(
            spawn_copy(vec![src], dst_dir.clone(), TransferOpts::default(), None),
            vec![Reply::Abort],
        );

        assert!(out.aborted);
        assert_eq!(fs::read(dst_dir.join("a")).unwrap(), b"old");
    }
}
