//! Read-only archive VFS: zip, tar, tar.gz/tgz.
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

use crate::entry::{Entry, EntryKind};
use crate::vfs::FsProvider;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Zip,
    Tar,
    TarGz,
    TarXz,
    TarBz2,
}

pub struct ArchiveFs {
    path: PathBuf,
    kind: Kind,
    /// Directory (relative, "" = archive root) → its entries.
    index: HashMap<PathBuf, Vec<Entry>>,
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
        } else if name.ends_with(".tar") {
            Kind::Tar
        } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
            Kind::TarGz
        } else if name.ends_with(".tar.xz") || name.ends_with(".txz") {
            Kind::TarXz
        } else if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") || name.ends_with(".tbz") {
            Kind::TarBz2
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported archive type",
            ));
        };
        let mut fs = ArchiveFs {
            path: path.to_path_buf(),
            kind,
            index: HashMap::from([(PathBuf::new(), Vec::new())]),
        };
        match kind {
            Kind::Zip => fs.index_zip()?,
            _ => fs.index_tar()?,
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
    /// a/b/ — materialize the whole chain.
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

    fn raw_reader(&self) -> io::Result<Box<dyn Read>> {
        let file = File::open(&self.path)?;
        Ok(match self.kind {
            Kind::Tar => Box::new(file),
            Kind::TarGz => Box::new(GzDecoder::new(file)),
            Kind::TarXz => Box::new(xz2::read::XzDecoder::new(file)),
            Kind::TarBz2 => Box::new(bzip2::read::BzDecoder::new(file)),
            Kind::Zip => unreachable!("zip uses the zip reader"),
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

        // nested file with NO explicit dir entries — chain must materialize
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

    #[test]
    fn refuses_unknown_extensions_and_garbage() {
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("not-an-archive.zip");
        std::fs::write(&bogus, b"this is not a zip").unwrap();
        assert!(ArchiveFs::open(&bogus).is_err());
        assert!(ArchiveFs::open(Path::new("file.rar")).is_err());
    }
}
