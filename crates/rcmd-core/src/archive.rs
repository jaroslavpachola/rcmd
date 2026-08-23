//! Read-only archive VFS: zip, tar and cpio (each plain or gz/xz/bz2
//! compressed) natively; rar and 7z through an external tool (the 7z
//! family, or unrar for .rar) - listed once at open, members streamed
//! out per read.
//!
//! The entry table is indexed once at open; `open_read` re-opens the
//! archive and decodes just the requested member into memory (compressed
//! streams cannot seek), so memory use is bounded by the largest single
//! member, not the archive.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Cursor, Read};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use flate2::read::GzDecoder;

use crate::cpio;
use crate::entry::{Entry, EntryKind};
use crate::vfs::FsProvider;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Zip,
    Tar(Comp),
    Cpio(Comp),
    /// rar / 7z via an external lister+extractor.
    Cmd,
}

/// What, if anything, the container stream is wrapped in. tar and cpio
/// both come plain or squeezed, and they answer to the same three
/// wrappers, so it is an axis of its own rather than a variant each.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Comp {
    None,
    Gz,
    Xz,
    Bz2,
}

/// Which external tool serves a [`Kind::Cmd`] archive.
#[derive(Clone, Copy, PartialEq, Eq)]
struct CmdBackend {
    program: &'static str,
    flavor: CmdFlavor,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CmdFlavor {
    SevenZip,
    Unrar,
}

pub struct ArchiveFs {
    path: PathBuf,
    kind: Kind,
    cmd: Option<CmdBackend>,
    /// Directory (relative, "" = archive root) → its entries.
    index: HashMap<PathBuf, Vec<Entry>>,
    /// A hard link's name → the member that actually carries the bytes.
    /// cpio writes the data once, with one of the names.
    links: HashMap<PathBuf, PathBuf>,
}

impl ArchiveFs {
    pub fn open(path: &Path) -> io::Result<Self> {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
        let kind = if name.ends_with(".zip") {
            Kind::Zip
        } else if name.ends_with(".tgz") {
            Kind::Tar(Comp::Gz)
        } else if name.ends_with(".txz") {
            Kind::Tar(Comp::Xz)
        } else if name.ends_with(".tbz2") || name.ends_with(".tbz") {
            Kind::Tar(Comp::Bz2)
        } else if name.ends_with(".rar") || name.ends_with(".7z") {
            Kind::Cmd
        } else {
            let (stem, comp) = peel_comp(&name);
            if stem.ends_with(".tar") {
                Kind::Tar(comp)
            } else if stem.ends_with(".cpio") {
                Kind::Cpio(comp)
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unsupported archive type",
                ));
            }
        };
        let mut fs = ArchiveFs {
            path: path.to_path_buf(),
            kind,
            cmd: None,
            index: HashMap::from([(PathBuf::new(), Vec::new())]),
            links: HashMap::new(),
        };
        match kind {
            Kind::Zip => fs.index_zip()?,
            Kind::Cmd => fs.index_cmd(name.ends_with(".rar"))?,
            Kind::Cpio(_) => fs.index_cpio()?,
            Kind::Tar(_) => fs.index_tar()?,
        }
        Ok(fs)
    }

    fn index_zip(&mut self) -> io::Result<()> {
        let mut zip = zip::ZipArchive::new(File::open(&self.path)?).map_err(zip_err)?;
        for i in 0..zip.len() {
            let member = zip.by_index_raw(i).map_err(zip_err)?;
            let Some(rel) = member.enclosed_name() else {
                continue; // refuse names escaping the archive root
            };
            let kind = if member.is_dir() {
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            let mode = member.unix_mode().unwrap_or(0) & 0o7777;
            let (size, name) = (member.size(), rel.clone());
            drop(member);
            self.add(&name, kind, size, mode, None, None);
        }
        Ok(())
    }

    fn index_tar(&mut self) -> io::Result<()> {
        let mut archive = tar::Archive::new(self.raw_reader()?);
        for member in archive.entries()? {
            let member = member?;
            let header = member.header();
            let rel = member.path()?.into_owned();
            let entry_type = header.entry_type();
            let (kind, link) = if entry_type.is_dir() {
                (EntryKind::Dir, None)
            } else if entry_type.is_symlink() {
                let link = header.link_name().ok().flatten().map(|c| c.into_owned());
                (EntryKind::SymlinkFile, link)
            } else if entry_type.is_file() {
                (EntryKind::File, None)
            } else {
                continue; // devices, fifos, hard links: skip for now
            };
            let mtime = header
                .mtime()
                .ok()
                .map(|secs| UNIX_EPOCH + Duration::from_secs(secs));
            let mode = header.mode().unwrap_or(0) & 0o7777;
            let size = header.size().unwrap_or(0);
            self.add(&rel, kind, size, mode, link, mtime);
        }
        Ok(())
    }

    /// cpio streams have no index: read the whole thing once, keeping
    /// each header and skipping past its bytes. Hard links are written
    /// with the data attached to just one of the names, so the empty
    /// aliases are collected and pointed at the one that has it.
    fn index_cpio(&mut self) -> io::Result<()> {
        let mut reader = cpio::Reader::new(self.raw_reader()?);
        // (dev, ino) → the member carrying the bytes, and its size
        let mut bodies: HashMap<(u64, u64), (PathBuf, u64)> = HashMap::new();
        let mut aliases: Vec<(PathBuf, (u64, u64))> = Vec::new();
        while let Some(header) = reader.next_member()? {
            let rel = normalize_rel(&header.path);
            if rel.as_os_str().is_empty() {
                continue;
            }
            let (kind, link) = if header.is_dir() {
                (EntryKind::Dir, None)
            } else if header.is_symlink() {
                let target = String::from_utf8_lossy(&reader.data()?).into_owned();
                (EntryKind::SymlinkFile, Some(PathBuf::from(target)))
            } else if header.is_file() {
                (EntryKind::File, None)
            } else {
                continue; // devices, fifos, sockets: nothing to browse
            };
            if kind == EntryKind::File && header.nlink > 1 {
                let id = (header.dev, header.ino);
                if header.size > 0 {
                    bodies.insert(id, (rel.clone(), header.size));
                } else {
                    aliases.push((rel.clone(), id));
                }
            }
            let mtime = Some(UNIX_EPOCH + Duration::from_secs(header.mtime));
            self.add(&rel, kind, header.size, header.perm(), link, mtime);
        }
        for (alias, id) in aliases {
            if let Some((body, size)) = bodies.get(&id).filter(|(body, _)| *body != alias) {
                self.links.insert(alias.clone(), body.clone());
                self.set_size(&alias, *size);
            }
        }
        Ok(())
    }

    /// A hard link's listing should show the size of what it points at,
    /// not the zero bytes its own record carries.
    fn set_size(&mut self, rel: &Path, size: u64) {
        let parent = rel.parent().map(Path::to_path_buf).unwrap_or_default();
        let Some(name) = rel.file_name() else { return };
        if let Some(entry) = self
            .index
            .get_mut(&parent)
            .and_then(|list| list.iter_mut().find(|e| e.name == name))
        {
            entry.size = size;
        }
    }

    fn add(
        &mut self,
        rel: &Path,
        kind: EntryKind,
        size: u64,
        mode: u32,
        link_target: Option<PathBuf>,
        mtime: Option<std::time::SystemTime>,
    ) {
        let rel = normalize_rel(rel);
        if rel.as_os_str().is_empty() {
            return;
        }
        let parent = rel.parent().map(Path::to_path_buf).unwrap_or_default();
        self.ensure_dir_chain(&parent);
        let name = rel.file_name().unwrap_or_default().to_os_string();
        let entry = Entry {
            name: name.clone(),
            kind,
            size,
            mtime,
            mode,
            link_target,
            extra: Default::default(),
        };
        let list = self.index.entry(parent).or_default();
        match list.iter_mut().find(|e| e.name == name) {
            // an implicit dir may have been created first; real data wins
            Some(existing) => *existing = entry,
            None => list.push(entry),
        }
        if kind == EntryKind::Dir {
            self.index.entry(rel).or_default();
        }
    }

    /// Archives may contain "a/b/file" without explicit entries for a/ and
    /// a/b/ - materialize the whole chain.
    fn ensure_dir_chain(&mut self, dir: &Path) {
        if dir.as_os_str().is_empty() || self.index.contains_key(dir) {
            return;
        }
        let parent = dir.parent().map(Path::to_path_buf).unwrap_or_default();
        self.ensure_dir_chain(&parent);
        self.index.insert(dir.to_path_buf(), Vec::new());
        let name = dir.file_name().unwrap_or_default().to_os_string();
        let list = self.index.entry(parent).or_default();
        if !list.iter().any(|e| e.name == name) {
            list.push(Entry {
                name,
                kind: EntryKind::Dir,
                size: 0,
                mtime: None,
                mode: 0o755,
                link_target: None,
                extra: Default::default(),
            });
        }
    }

    /// List a rar/7z through the first working tool: the 7z family
    /// reads both formats (rar needs its nonfree codec), unrar covers
    /// .rar where 7z can't.
    fn index_cmd(&mut self, is_rar: bool) -> io::Result<()> {
        const SEVENS: [&str; 3] = ["7z", "7zz", "7za"];
        let mut candidates: Vec<CmdBackend> = SEVENS
            .iter()
            .map(|p| CmdBackend {
                program: p,
                flavor: CmdFlavor::SevenZip,
            })
            .collect();
        if is_rar {
            candidates.push(CmdBackend {
                program: "unrar",
                flavor: CmdFlavor::Unrar,
            });
        }
        let mut last = io::Error::new(
            io::ErrorKind::NotFound,
            if is_rar {
                "browsing .rar needs 7z (p7zip + rar codec) or unrar installed"
            } else {
                "browsing .7z needs 7z / 7za (p7zip) installed"
            },
        );
        for backend in candidates {
            let output = std::process::Command::new(backend.program)
                .args(match backend.flavor {
                    CmdFlavor::SevenZip => &["l", "-ba", "-slt"][..],
                    CmdFlavor::Unrar => &["vt", "-p-"][..],
                })
                .arg(&self.path)
                .env("LC_ALL", "C")
                .stdin(std::process::Stdio::null())
                .output();
            let output = match output {
                Ok(out) => out,
                Err(_) => continue, // tool not installed: try the next
            };
            if !output.status.success() {
                let err = String::from_utf8_lossy(&output.stderr);
                let first = err
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("listing failed")
                    .trim();
                last = io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {first}", backend.program),
                );
                continue;
            }
            let text = String::from_utf8_lossy(&output.stdout);
            let members = match backend.flavor {
                CmdFlavor::SevenZip => parse_7z_slt(&text),
                CmdFlavor::Unrar => parse_unrar_vt(&text),
            };
            for m in members {
                self.add(&m.path, m.kind, m.size, m.mode, m.link, m.mtime);
            }
            self.cmd = Some(backend);
            return Ok(());
        }
        Err(last)
    }

    /// Stream one member out through the resolved tool.
    fn read_cmd(&self, rel: &Path) -> io::Result<Box<dyn Read + Send>> {
        let backend = self
            .cmd
            .ok_or_else(|| io::Error::other("archive tool went away"))?;
        let output = std::process::Command::new(backend.program)
            .args(match backend.flavor {
                CmdFlavor::SevenZip => &["x", "-so"][..],
                CmdFlavor::Unrar => &["p", "-inul", "-p-"][..],
            })
            .arg(&self.path)
            .arg(rel)
            .env("LC_ALL", "C")
            .stdin(std::process::Stdio::null())
            .output()?;
        if !output.status.success() && output.stdout.is_empty() {
            let err = String::from_utf8_lossy(&output.stderr);
            let first = err
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("extraction failed")
                .trim();
            return Err(io::Error::other(format!("{}: {first}", backend.program)));
        }
        Ok(Box::new(Cursor::new(output.stdout)))
    }

    /// Stream one member out by walking the archive again - the only
    /// way in without an index, and the same cost tar already pays.
    fn read_cpio(&self, rel: &Path) -> io::Result<Box<dyn Read + Send>> {
        let wanted = self.links.get(rel).unwrap_or(&rel.to_path_buf()).clone();
        let mut reader = cpio::Reader::new(self.raw_reader()?);
        while let Some(header) = reader.next_member()? {
            if normalize_rel(&header.path) == wanted {
                return Ok(Box::new(Cursor::new(reader.data()?)));
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "not found in archive",
        ))
    }

    fn raw_reader(&self) -> io::Result<Box<dyn Read>> {
        let file = File::open(&self.path)?;
        let comp = match self.kind {
            Kind::Tar(comp) | Kind::Cpio(comp) => comp,
            Kind::Zip | Kind::Cmd => unreachable!("zip/cmd use their own readers"),
        };
        Ok(match comp {
            Comp::None => Box::new(file),
            Comp::Gz => Box::new(GzDecoder::new(file)),
            Comp::Xz => Box::new(xz2::read::XzDecoder::new(file)),
            Comp::Bz2 => Box::new(bzip2::read::BzDecoder::new(file)),
        })
    }
}

impl FsProvider for ArchiveFs {
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        let dir = normalize_rel(dir);
        self.index
            .get(&dir)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such directory in archive"))
    }

    fn stat(&self, path: &Path) -> io::Result<Entry> {
        let path = normalize_rel(path);
        let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty path"))?;
        self.index
            .get(&parent)
            .and_then(|list| list.iter().find(|e| e.name == name))
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found in archive"))
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let rel = normalize_rel(path);
        match self.kind {
            Kind::Cmd => self.read_cmd(&rel),
            Kind::Cpio(_) => self.read_cpio(&rel),
            Kind::Zip => {
                let mut zip = zip::ZipArchive::new(File::open(&self.path)?).map_err(zip_err)?;
                for i in 0..zip.len() {
                    let mut member = zip.by_index(i).map_err(zip_err)?;
                    let matches = member
                        .enclosed_name()
                        .map(|n| normalize_rel(&n) == rel)
                        .unwrap_or(false);
                    if matches {
                        let mut buf = Vec::with_capacity(member.size() as usize);
                        member.read_to_end(&mut buf)?;
                        return Ok(Box::new(Cursor::new(buf)));
                    }
                }
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "not found in archive",
                ))
            }
            _ => {
                let mut archive = tar::Archive::new(self.raw_reader()?);
                for member in archive.entries()? {
                    let mut member = member?;
                    if normalize_rel(&member.path()?) == rel {
                        let mut buf = Vec::with_capacity(member.size() as usize);
                        member.read_to_end(&mut buf)?;
                        return Ok(Box::new(Cursor::new(buf)));
                    }
                }
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "not found in archive",
                ))
            }
        }
    }
}

/// One member as reported by an external lister.
struct CmdMember {
    path: PathBuf,
    kind: EntryKind,
    size: u64,
    mode: u32,
    mtime: Option<std::time::SystemTime>,
    link: Option<PathBuf>,
}

/// `7z l -ba -slt`: blank-line separated `Key = Value` records.
fn parse_7z_slt(text: &str) -> Vec<CmdMember> {
    let mut out = Vec::new();
    for record in text.split("\n\n") {
        let mut path = None;
        let mut folder = false;
        let mut attrs = String::new();
        let mut size = 0u64;
        let mut mtime = None;
        let mut link = None;
        for line in record.lines() {
            let Some((key, value)) = line.split_once(" = ") else {
                continue;
            };
            match key.trim() {
                "Path" => path = Some(PathBuf::from(value)),
                "Folder" => folder = value.trim() == "+",
                "Attributes" => attrs = value.trim().to_string(),
                "Size" => size = value.trim().parse().unwrap_or(0),
                "Modified" => mtime = parse_datetime(value),
                "Symbolic Link" if !value.trim().is_empty() => {
                    link = Some(PathBuf::from(value.trim()));
                }
                _ => {}
            }
        }
        let Some(path) = path else { continue };
        // "D drwxrwxr-x" (7z) or a bare unix string (rar)
        let unix = attrs
            .split_whitespace()
            .find(|t| t.len() == 10 && t.starts_with(['-', 'd', 'l']));
        let is_dir = folder
            || attrs
                .split_whitespace()
                .next()
                .is_some_and(|t| t.chars().all(|c| c.is_ascii_uppercase()) && t.contains('D'))
            || unix.is_some_and(|u| u.starts_with('d'));
        let kind = if link.is_some() {
            EntryKind::SymlinkFile
        } else if is_dir {
            EntryKind::Dir
        } else {
            EntryKind::File
        };
        out.push(CmdMember {
            path,
            kind,
            size,
            mode: unix.map(parse_unix_mode).unwrap_or(0o644),
            mtime,
            link,
        });
    }
    out
}

/// `unrar vt`: blank-line separated `Key: Value` records (with a
/// banner up front, filtered out by requiring a Name field).
fn parse_unrar_vt(text: &str) -> Vec<CmdMember> {
    let mut out = Vec::new();
    for record in text.split("\n\n") {
        let mut name = None;
        let mut is_dir = false;
        let mut size = 0u64;
        let mut mode = 0o644;
        let mut mtime = None;
        let mut link = None;
        for line in record.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "Name" => name = Some(PathBuf::from(value)),
                "Type" => is_dir = value == "Directory",
                "Size" => size = value.parse().unwrap_or(0),
                "mtime" => mtime = parse_datetime(value),
                "Attributes" => {
                    if value.len() == 10 {
                        mode = parse_unix_mode(value);
                    }
                }
                "Target" => link = Some(PathBuf::from(value)),
                _ => {}
            }
        }
        let Some(path) = name else { continue };
        let kind = if link.is_some() {
            EntryKind::SymlinkFile
        } else if is_dir {
            EntryKind::Dir
        } else {
            EntryKind::File
        };
        out.push(CmdMember {
            path,
            kind,
            size,
            mode,
            mtime,
            link,
        });
    }
    out
}

/// "-rw-rw-r--" / "drwxrwxr-x" → permission bits (setuid/sticky
/// letters count as plain execute - close enough for a listing).
fn parse_unix_mode(s: &str) -> u32 {
    let mut mode = 0u32;
    for (i, c) in s.chars().skip(1).take(9).enumerate() {
        if c != '-' {
            mode |= 1 << (8 - i);
        }
    }
    mode
}

/// "YYYY-MM-DD HH:MM:SS[.,frac]" (what 7z and unrar print under
/// LC_ALL=C) → SystemTime, treated as UTC - close enough for a column.
fn parse_datetime(s: &str) -> Option<std::time::SystemTime> {
    let (date, time) = s.trim().split_once(' ')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, u32, u32) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let mut t = time.split(':');
    let (h, min): (u64, u64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    let sec: u64 = t
        .next()
        .unwrap_or("0")
        .split(['.', ','])
        .next()?
        .parse()
        .ok()?;
    // days-from-civil (Howard Hinnant), valid for the Gregorian calendar
    let y = y - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = u64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + u64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe as i64 - 719_468;
    if days < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(days as u64 * 86_400 + h * 3_600 + min * 60 + sec))
}

/// Split a trailing compression suffix off a lowercased filename, so
/// "x.cpio.gz" and "x.tar.bz2" reach the same table as their plain
/// forms.
fn peel_comp(name: &str) -> (&str, Comp) {
    for (suffix, comp) in [(".gz", Comp::Gz), (".xz", Comp::Xz), (".bz2", Comp::Bz2)] {
        if let Some(stem) = name.strip_suffix(suffix) {
            return (stem, comp);
        }
    }
    (name, Comp::None)
}

/// Keep only normal components: strips "./", trailing slashes, and any
/// leading "/" or ".." an ill-formed archive might carry.
fn normalize_rel(path: &Path) -> PathBuf {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part),
            _ => None,
        })
        .collect()
}

fn zip_err(err: zip::result::ZipError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    fn make_targz(dir: &Path) -> PathBuf {
        let path = dir.join("test.tar.gz");
        let gz = GzEncoder::new(File::create(&path).unwrap(), Compression::default());
        let mut tar = tar::Builder::new(gz);

        let mut header = tar::Header::new_gnu();
        header.set_size(6);
        header.set_mode(0o644);
        header.set_mtime(1_700_000_000);
        header.set_cksum();
        tar.append_data(&mut header, "top.txt", &b"hello\n"[..])
            .unwrap();

        // nested file with NO explicit dir entries - chain must materialize
        let mut header = tar::Header::new_gnu();
        header.set_size(5);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, "deep/nest/prog", &b"data\n"[..])
            .unwrap();

        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_link_name("top.txt").unwrap();
        header.set_cksum();
        tar.append_data(&mut header, "link", &b""[..]).unwrap();

        tar.into_inner().unwrap().finish().unwrap();
        path
    }

    fn make_zip(dir: &Path) -> PathBuf {
        let path = dir.join("test.zip");
        let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("readme.md", options).unwrap();
        zip.write_all(b"# hi\n").unwrap();
        zip.start_file("src/main.rs", options).unwrap();
        zip.write_all(b"fn main() {}\n").unwrap();
        zip.finish().unwrap();
        path
    }

    #[test]
    fn targz_index_listing_and_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = ArchiveFs::open(&make_targz(tmp.path())).unwrap();

        let root = fs.read_dir(Path::new("")).unwrap();
        let names: Vec<_> = root
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"top.txt".into()));
        assert!(names.contains(&"deep".into()));
        assert!(names.contains(&"link".into()));

        let deep = fs.read_dir(Path::new("deep")).unwrap();
        assert_eq!(deep.len(), 1);
        assert!(deep[0].is_dir());

        let prog = fs.stat(Path::new("deep/nest/prog")).unwrap();
        assert_eq!(prog.kind, EntryKind::File);
        assert_eq!(prog.size, 5);
        assert_eq!(prog.mode, 0o755);

        let link = fs.stat(Path::new("link")).unwrap();
        assert_eq!(link.link_target, Some(PathBuf::from("top.txt")));

        let mut content = String::new();
        fs.open_read(Path::new("top.txt"))
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "hello\n");

        assert!(fs.read_dir(Path::new("missing")).is_err());
        assert!(fs.open_read(Path::new("missing")).is_err());
    }

    #[test]
    fn zip_index_listing_and_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = ArchiveFs::open(&make_zip(tmp.path())).unwrap();

        let root = fs.read_dir(Path::new("")).unwrap();
        assert_eq!(root.len(), 2); // readme.md + implicit src/
        let src = fs.stat(Path::new("src")).unwrap();
        assert!(src.is_dir());

        let mut content = String::new();
        fs.open_read(Path::new("src/main.rs"))
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "fn main() {}\n");
    }

    #[test]
    fn tar_xz_and_bz2_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        for (name, boxed) in [("t.tar.xz", true), ("t.tar.bz2", false)] {
            let path = tmp.path().join(name);
            let writer: Box<dyn Write> = if boxed {
                Box::new(xz2::write::XzEncoder::new(File::create(&path).unwrap(), 6))
            } else {
                Box::new(bzip2::write::BzEncoder::new(
                    File::create(&path).unwrap(),
                    bzip2::Compression::default(),
                ))
            };
            let mut tar = tar::Builder::new(writer);
            let mut header = tar::Header::new_gnu();
            header.set_size(4);
            header.set_cksum();
            tar.append_data(&mut header, "x.txt", &b"data"[..]).unwrap();
            tar.finish().unwrap();
            drop(tar);

            let fs = ArchiveFs::open(&path).unwrap();
            let mut content = String::new();
            fs.open_read(Path::new("x.txt"))
                .unwrap()
                .read_to_string(&mut content)
                .unwrap();
            assert_eq!(content, "data", "{name}");
        }
    }

    /// A newc stream, written by hand so the fixture does not need GNU
    /// cpio installed: `(name, st_mode, nlink, ino, data)`.
    fn write_newc(members: &[(&str, u32, u64, u64, &[u8])]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        let trailer = ("TRAILER!!!", 0, 1, 0, &b""[..]);
        for (name, mode, nlink, ino, data) in members.iter().copied().chain([trailer]) {
            let name = format!("{name}\0");
            out.extend_from_slice(b"070701");
            for value in [
                ino,
                u64::from(mode),
                0,
                0,
                nlink,
                1_700_000_000,
                data.len() as u64,
                3,
                4,
                0,
                0,
                name.len() as u64,
                0,
            ] {
                out.extend_from_slice(format!("{value:08X}").as_bytes());
            }
            out.extend_from_slice(name.as_bytes());
            while !out.len().is_multiple_of(4) {
                out.push(0);
            }
            out.extend_from_slice(data);
            while !out.len().is_multiple_of(4) {
                out.push(0);
            }
        }
        out
    }

    fn make_cpio(dir: &Path, name: &str) -> PathBuf {
        let stream = write_newc(&[
            ("dir", 0o040_755, 1, 1, b""),
            ("dir/nest/deep.txt", 0o100_644, 1, 2, b"deep\n"),
            ("top.txt", 0o100_600, 1, 3, b"hello cpio\n"),
            ("link", 0o120_777, 1, 4, b"top.txt"),
            ("dev/null", 0o020_666, 1, 5, b""),
            // a hard link pair: the bytes ride with the second name
            ("alias.txt", 0o100_644, 2, 9, b""),
            ("real.txt", 0o100_644, 2, 9, b"shared bytes\n"),
        ]);
        let path = dir.join(name);
        let bytes = if name.ends_with(".gz") {
            let mut gz = GzEncoder::new(Vec::new(), Compression::default());
            gz.write_all(&stream).unwrap();
            gz.finish().unwrap()
        } else {
            stream
        };
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn cpio_index_listing_and_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = ArchiveFs::open(&make_cpio(tmp.path(), "box.cpio")).unwrap();

        let root = fs.read_dir(Path::new("")).unwrap();
        let mut names: Vec<_> = root
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        names.sort();
        // "dev/null" is a device node - nothing a panel can open, so it
        // is dropped whole, the same as tar drops one, and the "dev/"
        // that held only it never appears either
        assert_eq!(names, ["alias.txt", "dir", "link", "real.txt", "top.txt"]);
        assert!(fs.stat(Path::new("dev")).is_err());

        // the nest/ chain has no record of its own and must materialize
        let deep = fs.stat(Path::new("dir/nest/deep.txt")).unwrap();
        assert_eq!(deep.size, 5);
        assert_eq!(deep.mode, 0o644);
        assert!(fs.stat(Path::new("dir/nest")).unwrap().is_dir());

        let top = fs.stat(Path::new("top.txt")).unwrap();
        assert_eq!(top.mode, 0o600);
        assert_eq!(
            top.mtime,
            Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
        );

        let link = fs.stat(Path::new("link")).unwrap();
        assert_eq!(link.kind, EntryKind::SymlinkFile);
        assert_eq!(link.link_target, Some(PathBuf::from("top.txt")));

        let mut content = String::new();
        fs.open_read(Path::new("dir/nest/deep.txt"))
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "deep\n");
    }

    #[test]
    fn cpio_hard_link_borrows_the_size_and_the_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = ArchiveFs::open(&make_cpio(tmp.path(), "box.cpio")).unwrap();

        // the alias record carries no bytes at all; the listing must
        // still say what opening it will give you
        assert_eq!(fs.stat(Path::new("alias.txt")).unwrap().size, 13);
        let mut content = String::new();
        fs.open_read(Path::new("alias.txt"))
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "shared bytes\n");
    }

    #[test]
    fn cpio_gz_is_the_same_archive_through_a_wrapper() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = ArchiveFs::open(&make_cpio(tmp.path(), "box.cpio.gz")).unwrap();
        let mut content = String::new();
        fs.open_read(Path::new("top.txt"))
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "hello cpio\n");
    }

    #[test]
    fn cpio_round_trip_against_gnu_cpio() {
        if !tool_available("cpio", "--version") {
            eprintln!("skipping: no cpio binary to build the fixture");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("sub/inner.txt"), b"written by cpio\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("sub/inner.txt", src.join("point")).unwrap();
        std::fs::hard_link(src.join("sub/inner.txt"), src.join("second-name")).unwrap();

        for format in ["newc", "odc", "bin"] {
            let out = std::fs::File::create(tmp.path().join("box.cpio")).unwrap();
            let list = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("find . | cpio --quiet -o -H {format} 2>/dev/null"))
                .current_dir(&src)
                .stdout(out)
                .status()
                .unwrap();
            assert!(list.success(), "{format}");

            let fs = ArchiveFs::open(&tmp.path().join("box.cpio")).unwrap();
            let mut content = String::new();
            fs.open_read(Path::new("sub/inner.txt"))
                .unwrap()
                .read_to_string(&mut content)
                .unwrap();
            assert_eq!(content, "written by cpio\n", "{format}");
            assert!(fs.stat(Path::new("sub")).unwrap().is_dir(), "{format}");
            let mut shared = String::new();
            fs.open_read(Path::new("second-name"))
                .unwrap()
                .read_to_string(&mut shared)
                .unwrap();
            assert_eq!(shared, "written by cpio\n", "{format}");
            #[cfg(unix)]
            assert_eq!(
                fs.stat(Path::new("point")).unwrap().link_target,
                Some(PathBuf::from("sub/inner.txt")),
                "{format}"
            );
        }
    }

    #[test]
    fn peels_compression_suffixes() {
        assert!(matches!(peel_comp("x.cpio.gz"), ("x.cpio", Comp::Gz)));
        assert!(matches!(peel_comp("x.tar.bz2"), ("x.tar", Comp::Bz2)));
        assert!(matches!(peel_comp("x.tar"), ("x.tar", Comp::None)));
    }

    #[test]
    fn refuses_unknown_extensions_and_garbage() {
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("not-an-archive.zip");
        std::fs::write(&bogus, b"this is not a zip").unwrap();
        assert!(ArchiveFs::open(&bogus).is_err());
        assert!(ArchiveFs::open(Path::new("file.lha")).is_err());
    }

    #[test]
    fn parses_7z_slt_listings() {
        let text = "Path = sub\nSize = 0\nModified = 2026-07-27 14:51:57.7878693\nAttributes = D drwxrwxr-x\n\nPath = file.txt\nFolder = -\nSize = 10\nModified = 2026-07-27 14:51:57.787869360\nAttributes =  -rw-rw-r--\nSymbolic Link = \n";
        let members = parse_7z_slt(text);
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].path, PathBuf::from("sub"));
        assert_eq!(members[0].kind, EntryKind::Dir);
        assert_eq!(members[1].path, PathBuf::from("file.txt"));
        assert_eq!(members[1].kind, EntryKind::File);
        assert_eq!(members[1].size, 10);
        assert_eq!(members[1].mode, 0o664);
        assert!(members[1].mtime.is_some());
    }

    #[test]
    fn parses_unrar_vt_listings() {
        let text = "UNRAR 7.00 freeware\n\nArchive: test.rar\nDetails: RAR 5\n\n        Name: file.txt\n        Type: File\n        Size: 10\n       mtime: 2026-07-27 14:51:57,787869360\n  Attributes: -rw-rw-r--\n\n        Name: sub\n        Type: Directory\n  Attributes: drwxrwxr-x\n";
        let members = parse_unrar_vt(text);
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].path, PathBuf::from("file.txt"));
        assert_eq!(members[0].kind, EntryKind::File);
        assert_eq!(members[0].size, 10);
        assert_eq!(members[0].mode, 0o664);
        assert_eq!(members[1].kind, EntryKind::Dir);
    }

    #[test]
    fn datetime_and_mode_helpers() {
        let t = parse_datetime("1970-01-01 00:00:00").unwrap();
        assert_eq!(t, UNIX_EPOCH);
        let t = parse_datetime("2001-09-09 01:46:40.5").unwrap();
        assert_eq!(t, UNIX_EPOCH + Duration::from_secs(1_000_000_000));
        assert!(parse_datetime("junk").is_none());
        assert_eq!(parse_unix_mode("-rw-r--r--"), 0o644);
        assert_eq!(parse_unix_mode("drwxr-xr-x"), 0o755);
        assert_eq!(parse_unix_mode("-rwxrwxrwx"), 0o777);
    }

    fn tool_available(program: &str, probe: &str) -> bool {
        std::process::Command::new(program)
            .arg(probe)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    }

    #[test]
    fn rar_round_trip_via_external_tools() {
        if !tool_available("rar", "-iver") {
            eprintln!("skipping: no rar binary to build the fixture");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("hello.txt"), b"hello rar\n").unwrap();
        std::fs::write(tmp.path().join("sub/inner.txt"), b"deep\n").unwrap();
        let status = std::process::Command::new("rar")
            .args(["a", "-idq", "box.rar", "hello.txt", "sub"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        assert!(status.success());

        let fs = match ArchiveFs::open(&tmp.path().join("box.rar")) {
            Ok(fs) => fs,
            Err(err) => {
                eprintln!("skipping: no rar-capable lister ({err})");
                return;
            }
        };
        let root = fs.read_dir(Path::new("")).unwrap();
        let names: Vec<_> = root
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"hello.txt".into()), "{names:?}");
        assert!(names.contains(&"sub".into()));
        let mut content = String::new();
        fs.open_read(Path::new("sub/inner.txt"))
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "deep\n");
    }

    #[test]
    fn sevenz_round_trip_via_external_tools() {
        if !tool_available("7za", "-h") && !tool_available("7z", "-h") {
            eprintln!("skipping: no 7z binary");
            return;
        }
        let packer = if tool_available("7za", "-h") {
            "7za"
        } else {
            "7z"
        };
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("hello.txt"), b"hello 7z\n").unwrap();
        let status = std::process::Command::new(packer)
            .args(["a", "-bd", "box.7z", "hello.txt"])
            .current_dir(tmp.path())
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());

        let fs = ArchiveFs::open(&tmp.path().join("box.7z")).unwrap();
        let mut content = String::new();
        fs.open_read(Path::new("hello.txt"))
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "hello 7z\n");
        let entry = fs.stat(Path::new("hello.txt")).unwrap();
        assert_eq!(entry.size, 9);
    }
}
