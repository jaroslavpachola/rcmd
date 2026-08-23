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
}

impl Default for TransferOpts {
    fn default() -> TransferOpts {
        TransferOpts {
            preserve: true,
            follow_links: false,
            dive: true,
            stable_symlinks: true,
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
    pub thread: Option<JoinHandle<()>>,
}

impl JobHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
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
                    ctx.progress(path);
                }
            }
        }
        Ok(())
    })
}

/// Copy out of a read-only [`FsProvider`] (an archive) onto the local
/// filesystem, with the same progress/overwrite/error protocol as copy.
pub fn spawn_extract(fs: Arc<dyn FsProvider>, sources: Vec<PathBuf>, dest: PathBuf) -> JobHandle {
    spawn(move |ctx| {
        let (files, bytes) = scan_provider(&*fs, &sources);
        let _ = ctx.tx.send(JobEvent::Total { files, bytes });
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
) -> JobHandle {
    spawn(move |ctx| {
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
                transfer_move_one(
                    ctx,
                    &*src_fs,
                    src,
                    &target_for(src, &dest, into_dir),
                    &mut totals,
                )?;
            }
        } else {
            let (files, bytes) = scan_provider(&*src_fs, &sources);
            let _ = ctx.tx.send(JobEvent::Total { files, bytes });
            for src in &sources {
                if ctx.cancelled() {
                    return Err(Aborted);
                }
                transfer_tree(
                    ctx,
                    &*src_fs,
                    &*dst_fs,
                    src,
                    &target_for(src, &dest, into_dir),
                    same_fs,
                    move_mode,
                )?;
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
    match entry.kind {
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
            if let Some(modified) = entry.mtime {
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
            if ctx.may_overwrite_fs(dst_fs, FileFacts::of_entry(&entry), dst)? == Overwrite::Skip {
                return Ok(());
            }
            transfer_file(ctx, src_fs, dst_fs, src, dst, &entry, move_mode)?;
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
) -> Result<(), Aborted> {
    loop {
        if ctx.cancelled() {
            return Err(Aborted);
        }
        let start = ctx.bytes_done;
        match try_transfer_file(ctx, src_fs, dst_fs, src, dst, entry) {
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

fn try_transfer_file(
    ctx: &mut Ctx,
    src_fs: &dyn FsProvider,
    dst_fs: &dyn FsProvider,
    src: &Path,
    dst: &Path,
    entry: &crate::entry::Entry,
) -> Result<(), CopyErr> {
    let writer = dst_fs.writer().expect("checked in spawn_transfer");
    ctx.begin_file(entry.size);
    let mut input = src_fs.open_read(src).map_err(CopyErr::Io)?;
    let mut output = writer.open_write(dst).map_err(CopyErr::Io)?;
    let mut buf = vec![0u8; CHUNK];
    loop {
        if ctx.cancelled() {
            drop(output);
            let _ = writer.remove_file(dst); // don't leave a torso behind
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
    output.flush().map_err(CopyErr::Io)?;
    drop(output); // remote handles must close before setstat
    if entry.mode != 0 {
        let _ = writer.set_mode(dst, entry.mode);
    }
    if let Some(modified) = entry.mtime {
        let _ = writer.set_mtime(dst, modified);
    }
    Ok(())
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
    /// Bytes written of the file in hand, and its size.
    file_done: u64,
    file_total: u64,
}

impl Ctx {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
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
            return Ok(Decision::Skip);
        }
        if self
            .tx
            .send(JobEvent::AskError {
                path: path.to_path_buf(),
                message,
            })
            .is_err()
        {
            return Err(Aborted);
        }
        match self.rx.recv() {
            Ok(Reply::Retry) => Ok(Decision::Retry),
            Ok(Reply::Skip) => {
                self.skipped += 1;
                Ok(Decision::Skip)
            }
            Ok(Reply::SkipAll) => {
                self.skip_all_errors = true;
                self.skipped += 1;
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
        match fs.stat(dst) {
            Ok(entry) => self.decide_overwrite(src, FileFacts::of_entry(&entry), dst, false),
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
        match self.policy {
            Policy::All => return Ok(Overwrite::Replace),
            Policy::None => return Ok(self.skip()),
            Policy::Newer => return Ok(self.sticky(src.newer_than(dst_facts))),
            Policy::SizeDiffers => return Ok(self.sticky(src.size != dst_facts.size)),
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
                Ok(self.sticky(src.newer_than(dst_facts)))
            }
            Ok(Reply::SizeDiffersAll) => {
                self.policy = Policy::SizeDiffers;
                Ok(self.sticky(src.size != dst_facts.size))
            }
            Ok(Reply::Skip) => Ok(self.skip()),
            Ok(Reply::SkipAll) => {
                self.policy = Policy::None;
                Ok(self.skip())
            }
            _ => Err(Aborted),
        }
    }

    fn sticky(&mut self, replace: bool) -> Overwrite {
        if replace {
            Overwrite::Replace
        } else {
            self.skip()
        }
    }

    fn skip(&mut self) -> Overwrite {
        self.skipped += 1;
        Overwrite::Skip
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
    let thread = thread::spawn(move || {
        let mut ctx = Ctx {
            tx: event_tx,
            rx: reply_rx,
            cancel: worker_cancel,
            files_done: 0,
            bytes_done: 0,
            skipped: 0,
            policy: Policy::Ask,
            skip_all_errors: false,
            opts,
            copy_root: None,
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
fn lexical_join(base: &Path, rest: &Path) -> PathBuf {
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
fn relative_to(path: &Path, base: &Path) -> Option<PathBuf> {
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
        if dst.starts_with(src) {
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
            && let Ok(modified) = meta.modified()
            && let Ok(dir) = fs::File::open(dst)
        {
            let _ = dir.set_times(fs::FileTimes::new().set_modified(modified));
        }
        Ok(())
    } else if meta.is_symlink() {
        ctx.progress(src);
        if src == dst {
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
    } else {
        ctx.progress(src);
        if src == dst {
            return ctx.error(src, "source and destination are the same file");
        }
        // the one place Append and Reget make sense: a local file
        // copied onto a local file
        let mode = ctx.may_overwrite(FileFacts::of_path(src), dst, true)?;
        if mode == Overwrite::Skip {
            return Ok(());
        }
        copy_file(ctx, src, dst, meta.len(), mode)
    }
}

fn copy_file(
    ctx: &mut Ctx,
    src: &Path,
    dst: &Path,
    size: u64,
    mode: Overwrite,
) -> Result<(), Aborted> {
    loop {
        if ctx.cancelled() {
            return Err(Aborted);
        }
        let start = ctx.bytes_done;
        ctx.begin_file(size);
        match try_copy_file(ctx, src, dst, mode) {
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

fn try_copy_file(ctx: &mut Ctx, src: &Path, dst: &Path, mode: Overwrite) -> Result<(), CopyErr> {
    let mut input = fs::File::open(src).map_err(CopyErr::Io)?;
    let meta = input.metadata().map_err(CopyErr::Io)?;
    // Append and Reget add to a file that is already there; only a plain
    // copy creates one, and only a plain copy may delete it again.
    let fresh = mode == Overwrite::Replace;
    let mut output = if fresh {
        fs::File::create(dst).map_err(CopyErr::Io)?
    } else {
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
    let mut buf = vec![0u8; CHUNK];
    loop {
        if ctx.cancelled() {
            drop(output);
            if fresh {
                let _ = fs::remove_file(dst); // don't leave a torso behind
            }
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
    // an appended-to file keeps its own mode and its new mtime: it is
    // not a copy of the source, it is the target with more in it
    if fresh && ctx.opts.preserve {
        output
            .set_permissions(meta.permissions())
            .map_err(CopyErr::Io)?;
        if let Ok(modified) = meta.modified() {
            let _ = output.set_times(fs::FileTimes::new().set_modified(modified));
        }
    }
    Ok(())
}

fn move_one(ctx: &mut Ctx, src: &Path, dst: &Path, totals: &mut (u64, u64)) -> Result<(), Aborted> {
    ctx.progress(src);
    if src == dst {
        return ctx.error(src, "source and destination are the same file");
    }
    if dst.starts_with(src) {
        return ctx.error(src, "cannot move a directory into itself");
    }
    if ctx.may_overwrite(FileFacts::of_path(src), dst, false)? == Overwrite::Skip {
        return Ok(());
    }
    loop {
        if ctx.cancelled() {
            return Err(Aborted);
        }
        match fs::rename(src, dst) {
            Ok(()) => {
                ctx.files_done += 1;
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
                copy_tree(ctx, src, dst)?;
                delete_tree(ctx, src)?;
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
    #[cfg(unix)]
    if mode != 0 {
        use std::os::unix::fs::PermissionsExt;
        output
            .set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(CopyErr::Io)?;
    }
    if let Some(modified) = mtime {
        let _ = output.set_times(fs::FileTimes::new().set_modified(modified));
    }
    Ok(())
}

/// Copy local files INTO a zip archive by appending members. Only zip
/// supports in-place append; tar would need a full rewrite. Existing
/// members are never touched - a same-named member is appended and
/// shadows the old one for readers that pick the latest entry.
pub fn spawn_pack_zip(sources: Vec<PathBuf>, archive: PathBuf, inside: PathBuf) -> JobHandle {
    spawn(move |ctx| {
        let (files, bytes) = scan(&sources);
        let _ = ctx.tx.send(JobEvent::Total { files, bytes });
        let Some(file) = ctx.with_retry(&archive, || {
            fs::OpenOptions::new().read(true).write(true).open(&archive)
        })?
        else {
            return Ok(());
        };
        let mut zip = match zip::ZipWriter::new_append(file) {
            Ok(zip) => zip,
            Err(err) => {
                ctx.error(&archive, &err.to_string())?;
                return Ok(());
            }
        };
        let mut outcome = Ok(());
        for src in &sources {
            if ctx.cancelled() {
                outcome = Err(Aborted);
                break;
            }
            let name = src.file_name().unwrap_or_default();
            if let Err(abort) = pack_tree(ctx, &mut zip, src, &inside.join(name)) {
                outcome = Err(abort);
                break;
            }
        }
        // always finalize: without the central directory the zip is broken
        if let Err(err) = zip.finish() {
            let _ = ctx.error(&archive, &format!("finalizing archive: {err}"));
        }
        outcome
    })
}

/// Copy INTO a tar archive (R4): tars cannot append in place across
/// compressors, so the whole archive is rewritten - existing entries
/// stream into a temp file with the same compression, the new trees
/// are appended behind them, and the temp renames over the original.
pub fn spawn_pack_tar(sources: Vec<PathBuf>, archive: PathBuf, inside: PathBuf) -> JobHandle {
    spawn(move |ctx| {
        let (files, bytes) = scan(&sources);
        let _ = ctx.tx.send(JobEvent::Total { files, bytes });
        let temp = {
            let dir = archive.parent().unwrap_or_else(|| Path::new("."));
            let name = archive.file_name().unwrap_or_default().to_string_lossy();
            dir.join(format!(".{name}.rcmd-{}", std::process::id()))
        };
        match rewrite_tar(ctx, &sources, &archive, &inside, &temp) {
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
}

impl Write for TarSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            TarSink::Plain(w) => w.write(buf),
            TarSink::Gz(w) => w.write(buf),
            TarSink::Xz(w) => w.write(buf),
            TarSink::Bz(w) => w.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            TarSink::Plain(w) => w.flush(),
            TarSink::Gz(w) => w.flush(),
            TarSink::Xz(w) => w.flush(),
            TarSink::Bz(w) => w.flush(),
        }
    }
}

impl TarSink {
    fn create(path: &Path, archive_name: &str) -> io::Result<TarSink> {
        let file = fs::File::create(path)?;
        Ok(if archive_name.ends_with(".tar") {
            TarSink::Plain(file)
        } else if archive_name.ends_with(".tar.gz") || archive_name.ends_with(".tgz") {
            TarSink::Gz(flate2::write::GzEncoder::new(
                file,
                flate2::Compression::default(),
            ))
        } else if archive_name.ends_with(".tar.xz") || archive_name.ends_with(".txz") {
            TarSink::Xz(xz2::write::XzEncoder::new(file, 6))
        } else {
            TarSink::Bz(bzip2::write::BzEncoder::new(
                file,
                bzip2::Compression::default(),
            ))
        })
    }

    fn finish(self) -> io::Result<()> {
        match self {
            TarSink::Plain(_) => Ok(()),
            TarSink::Gz(w) => w.finish().map(|_| ()),
            TarSink::Xz(w) => w.finish().map(|_| ()),
            TarSink::Bz(w) => w.finish().map(|_| ()),
        }
    }
}

fn tar_source(path: &Path, archive_name: &str) -> io::Result<Box<dyn Read>> {
    let file = fs::File::open(path)?;
    Ok(if archive_name.ends_with(".tar") {
        Box::new(file)
    } else if archive_name.ends_with(".tar.gz") || archive_name.ends_with(".tgz") {
        Box::new(flate2::read::GzDecoder::new(file))
    } else if archive_name.ends_with(".tar.xz") || archive_name.ends_with(".txz") {
        Box::new(xz2::read::XzDecoder::new(file))
    } else {
        Box::new(bzip2::read::BzDecoder::new(file))
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
) -> Result<Result<(), io::Error>, Aborted> {
    let name = archive
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    let mut tar = match TarSink::create(temp, &name) {
        Ok(sink) => tar::Builder::new(sink),
        Err(err) => return Ok(Err(err)),
    };
    tar.follow_symlinks(false);
    // stream the existing entries across unchanged (append_data /
    // append_link re-handle long names, so GNU/PAX entries survive)
    let copied = (|| -> io::Result<()> {
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
    match entry.kind {
        EntryKind::Dir => {
            let _ = zip.add_directory(format!("{rel_name}/"), options);
            let Some(names) = read_names(ctx, src)? else {
                return Ok(());
            };
            for name in names {
                pack_tree(ctx, zip, &src.join(&name), &dst_rel.join(&name))?;
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
            spawn_pack_zip(vec![payload.clone()], archive.clone(), PathBuf::new()),
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

    #[test]
    fn pack_rewrites_tar_archives() {
        for name in ["box.tar", "box.tar.gz"] {
            let tmp = tempfile::tempdir().unwrap();
            let archive = tmp.path().join(name);
            // existing archive with one member
            {
                let sink = TarSink::create(&archive, name).unwrap();
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
                spawn_pack_tar(vec![payload.clone()], archive.clone(), PathBuf::new()),
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
            spawn_transfer(fs_arc.clone(), vec![src.clone()], fs_arc, dst.clone(), true),
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
