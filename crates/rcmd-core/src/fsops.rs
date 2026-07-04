//! Long-running file operations, executed on a worker thread.
//!
//! The worker streams [`JobEvent`]s to the UI over a channel. When it hits
//! a decision point (existing target, I/O error) it sends an `Ask*` event
//! and blocks until the UI answers with a [`Reply`]. Cancellation is a
//! shared flag checked between chunks and files; dropping the [`JobHandle`]
//! also unblocks a waiting worker because the reply channel closes.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::entry::EntryKind;
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
    },
    /// Target exists; answer with Overwrite/OverwriteAll/Skip/SkipAll/Abort.
    AskOverwrite { path: PathBuf },
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
    Skip,
    SkipAll,
    Retry,
    Abort,
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

pub fn spawn_copy(sources: Vec<PathBuf>, dest: PathBuf) -> JobHandle {
    spawn(move |ctx| {
        let (files, bytes) = scan(&sources);
        let _ = ctx.tx.send(JobEvent::Total { files, bytes });
        let into_dir = dest.is_dir() || sources.len() > 1;
        for src in &sources {
            if ctx.cancelled() {
                return Err(Aborted);
            }
            copy_tree(ctx, src, &target_for(src, &dest, into_dir))?;
        }
        Ok(())
    })
}

pub fn spawn_move(sources: Vec<PathBuf>, dest: PathBuf) -> JobHandle {
    spawn(move |ctx| {
        // Totals start as item counts; a cross-device fallback re-announces
        // them with real file/byte numbers for that subtree.
        let mut totals = (sources.len() as u64, 0u64);
        let _ = ctx.tx.send(JobEvent::Total {
            files: totals.0,
            bytes: totals.1,
        });
        let into_dir = dest.is_dir() || sources.len() > 1;
        for src in &sources {
            if ctx.cancelled() {
                return Err(Aborted);
            }
            move_one(ctx, src, &target_for(src, &dest, into_dir), &mut totals)?;
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
    overwrite_all: bool,
    skip_all_overwrites: bool,
    skip_all_errors: bool,
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
        });
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

    /// Whether we may write over `dst`. Ok(false) means skip this item.
    fn may_overwrite(&mut self, dst: &Path) -> Result<bool, Aborted> {
        if dst.symlink_metadata().is_err() {
            return Ok(true); // nothing there
        }
        if self.overwrite_all {
            return Ok(true);
        }
        if self.skip_all_overwrites {
            self.skipped += 1;
            return Ok(false);
        }
        if self
            .tx
            .send(JobEvent::AskOverwrite {
                path: dst.to_path_buf(),
            })
            .is_err()
        {
            return Err(Aborted);
        }
        match self.rx.recv() {
            Ok(Reply::Overwrite) => Ok(true),
            Ok(Reply::OverwriteAll) => {
                self.overwrite_all = true;
                Ok(true)
            }
            Ok(Reply::Skip) => {
                self.skipped += 1;
                Ok(false)
            }
            Ok(Reply::SkipAll) => {
                self.skip_all_overwrites = true;
                self.skipped += 1;
                Ok(false)
            }
            _ => Err(Aborted),
        }
    }
}

fn spawn(work: impl FnOnce(&mut Ctx) -> Result<(), Aborted> + Send + 'static) -> JobHandle {
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
            overwrite_all: false,
            skip_all_overwrites: false,
            skip_all_errors: false,
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

fn target_for(src: &Path, dest: &Path, into_dir: bool) -> PathBuf {
    if into_dir {
        dest.join(src.file_name().unwrap_or_default())
    } else {
        dest.to_path_buf()
    }
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
    let Some(meta) = ctx.with_retry(src, || src.symlink_metadata())? else {
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
        if let Ok(modified) = meta.modified() {
            if let Ok(dir) = fs::File::open(dst) {
                let _ = dir.set_times(fs::FileTimes::new().set_modified(modified));
            }
        }
        Ok(())
    } else if meta.is_symlink() {
        ctx.progress(src);
        if src == dst {
            return ctx.error(src, "source and destination are the same file");
        }
        if !ctx.may_overwrite(dst)? {
            return Ok(());
        }
        let done = ctx.with_retry(src, || {
            let target = fs::read_link(src)?;
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
        if !ctx.may_overwrite(dst)? {
            return Ok(());
        }
        copy_file(ctx, src, dst, meta.len())
    }
}

fn copy_file(ctx: &mut Ctx, src: &Path, dst: &Path, size: u64) -> Result<(), Aborted> {
    loop {
        if ctx.cancelled() {
            return Err(Aborted);
        }
        let start = ctx.bytes_done;
        match try_copy_file(ctx, src, dst) {
            Ok(()) => {
                ctx.files_done += 1;
                ctx.bytes_done = start + size; // keep totals consistent with the scan
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

fn try_copy_file(ctx: &mut Ctx, src: &Path, dst: &Path) -> Result<(), CopyErr> {
    let mut input = fs::File::open(src).map_err(CopyErr::Io)?;
    let meta = input.metadata().map_err(CopyErr::Io)?;
    let mut output = fs::File::create(dst).map_err(CopyErr::Io)?;
    let mut buf = vec![0u8; CHUNK];
    loop {
        if ctx.cancelled() {
            drop(output);
            let _ = fs::remove_file(dst); // don't leave a torso behind
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
    output
        .set_permissions(meta.permissions())
        .map_err(CopyErr::Io)?;
    if let Ok(modified) = meta.modified() {
        let _ = output.set_times(fs::FileTimes::new().set_modified(modified));
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
    if !ctx.may_overwrite(dst)? {
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
            if let Some(modified) = entry.mtime {
                if let Ok(dir) = fs::File::open(dst) {
                    let _ = dir.set_times(fs::FileTimes::new().set_modified(modified));
                }
            }
            Ok(())
        }
        EntryKind::SymlinkDir | EntryKind::SymlinkFile | EntryKind::SymlinkBroken => {
            ctx.progress(src);
            if !ctx.may_overwrite(dst)? {
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
            if !ctx.may_overwrite(dst)? {
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
/// members are never touched — a same-named member is appended and
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
                JobEvent::AskOverwrite { path } => {
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
                    }
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

        let out = run(spawn_copy(vec![src.clone()], dst.clone()), vec![]);

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
    fn overwrite_asks_and_honors_skip_then_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("new.txt");
        fs::write(&src, b"new").unwrap();
        let dst_dir = tmp.path().join("out");
        fs::create_dir(&dst_dir).unwrap();
        fs::write(dst_dir.join("new.txt"), b"old").unwrap();

        let out = run(
            spawn_copy(vec![src.clone()], dst_dir.clone()),
            vec![Reply::Skip],
        );
        assert_eq!(out.asks.len(), 1);
        assert_eq!(out.skipped, 1);
        assert_eq!(fs::read(dst_dir.join("new.txt")).unwrap(), b"old");

        let out = run(
            spawn_copy(vec![src], dst_dir.clone()),
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
            spawn_copy(vec![dir.clone()], dir.clone()),
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

        let out = run(spawn_move(vec![src.clone()], dst_dir.clone()), vec![]);

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
        use flate2::write::GzEncoder;
        use flate2::Compression;

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

        let result = run(spawn_copy(vec![src], out.clone()), vec![]);

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
    fn abort_reply_stops_the_job() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a");
        fs::write(&src, b"1").unwrap();
        let dst_dir = tmp.path().join("out");
        fs::create_dir(&dst_dir).unwrap();
        fs::write(dst_dir.join("a"), b"old").unwrap();

        let out = run(spawn_copy(vec![src], dst_dir.clone()), vec![Reply::Abort]);

        assert!(out.aborted);
        assert_eq!(fs::read(dst_dir.join("a")).unwrap(), b"old");
    }
}
