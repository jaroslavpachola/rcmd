//! Read-only archive VFS: zip, tar and cpio (each plain or gz/xz/bz2/
//! zstd compressed), `ar` archives and Debian and RPM packages
//! ISO 9660 images, patch files and mailboxes natively; rar
//! and 7z through an external tool (the 7z family, or unrar for .rar) -
//! listed once at open, members streamed out per read.
//!
//! The entry table is indexed once at open. A zip is parsed once too
//! and kept open: `open_read` goes straight to the member's bytes and
//! streams them. The other formats stream or slice their members as
//! each of them allows.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use flate2::read::GzDecoder;

use crate::ar;
use crate::cpio;
use crate::entry::{Entry, EntryKind};
use crate::iso;
use crate::mail;
use crate::patch;
use crate::rpm;
use crate::vfs::{FsProvider, Prefetched};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Zip,
    Tar(Comp),
    Cpio(Comp),
    /// An `ar` archive - a static library, most often - listed flat.
    Ar,
    /// A Debian package: an `ar` archive holding two tarballs.
    Deb,
    /// An RPM package: two headers and a compressed cpio payload.
    Rpm,
    /// An ISO 9660 image - a disc, browsed where it lies.
    Iso,
    /// A patch, browsed as the files it touches.
    Patch(Comp),
    /// An mbox, browsed as the messages in it.
    Mbox(Comp),
    /// rar / 7z via an external lister+extractor.
    Cmd,
}

/// What, if anything, the container stream is wrapped in. tar and cpio
/// both come plain or squeezed, and they answer to the same three
/// wrappers, so it is an axis of its own rather than a variant each.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Comp {
    None,
    Gz,
    Xz,
    Bz2,
    Zstd,
    /// Raw LZMA, which only rpm still ships - decoded by the same
    /// library as xz, told to work out which of the two it has.
    Lzma,
}

/// The formats rcmd has no reader for and the 7z family does. They all
/// list and extract through the same two commands, so adding one is a
/// matter of naming it.
const CMD_EXTENSIONS: [&str; 6] = [".rar", ".7z", ".lha", ".lzh", ".arj", ".cab"];

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
    /// Every path in `index` → where its entry stands in its parent's
    /// list: a lookup by name, where walking a directory of 40,000
    /// files for each one made indexing it quadratic.
    slots: HashMap<PathBuf, usize>,
    /// A hard link's name → the member that actually carries the bytes.
    /// cpio writes the data once, with one of the names.
    links: HashMap<PathBuf, PathBuf>,
    /// Paths whose bytes are a plain slice of the container file.
    slices: HashMap<PathBuf, Slice>,
    /// Files rcmd writes itself rather than finds: an rpm's tags are
    /// not a file in the package, but they read best as one.
    generated: HashMap<PathBuf, String>,
    /// An opened disc image, which locates its own members.
    iso: Option<iso::Image>,
    /// A patch's whole text and where each file's part of it starts.
    /// The text is kept once; the entries point into it.
    patch: Option<(String, Vec<patch::Piece>)>,
    /// The same arrangement for an mbox and its messages.
    mbox: Option<(String, Vec<mail::Message>)>,
    /// The files of a tar or a cpio, the container's own or one nested
    /// in it: which stream they are in, where in it their bytes start
    /// and how many there are - so a member is streamed rather than
    /// read into memory whole, and found without walking to it.
    members: HashMap<PathBuf, (Source, u64, u64)>,
    /// The unwrapped stream left where the last member read from it
    /// ended. Members read in the archive's order - which is what an
    /// extraction does - go on from there, where each used to start the
    /// decompression over from the first byte.
    stream: Kept,
    /// A zip, parsed once at open and kept.
    zip: Option<ZipIndex>,
    /// Every path, and each directory above it, by when the index first
    /// met it: the archive's own order, which a job reads its sources in.
    order: HashMap<PathBuf, u64>,
    /// Members of an external-tool archive unpacked ahead of a job, one
    /// run of the tool for the lot.
    unpacked: Arc<Mutex<Option<Unpacked>>>,
    /// How many times the container was opened and unwrapped from the
    /// top, for the tests to hold the reading to one pass.
    #[cfg(test)]
    opens: std::sync::atomic::AtomicUsize,
    /// How many times a plain stream was opened at a member instead.
    #[cfg(test)]
    seeks: std::sync::atomic::AtomicUsize,
}

/// A run of bytes inside the container, and what it is wrapped in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Slice {
    at: u64,
    len: u64,
    comp: Comp,
}

/// The stream a member's bytes are in: the whole container unwrapped
/// (a tar, a cpio), or one slice of it unwrapped (a .deb's data.tar,
/// an rpm's payload).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Root,
    Slice(Slice),
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
        } else if name.ends_with(".tzst") {
            Kind::Tar(Comp::Zstd)
        } else if name.ends_with(".deb") || name.ends_with(".udeb") {
            Kind::Deb
        } else if name.ends_with(".rpm") {
            Kind::Rpm
        } else if name.ends_with(".iso") {
            Kind::Iso
        } else if name.ends_with(".a") || name.ends_with(".ar") {
            Kind::Ar
        } else if CMD_EXTENSIONS.iter().any(|ext| name.ends_with(ext)) {
            Kind::Cmd
        } else {
            let (stem, comp) = peel_comp(&name);
            if stem.ends_with(".tar") {
                Kind::Tar(comp)
            } else if stem.ends_with(".cpio") {
                Kind::Cpio(comp)
            } else if stem.ends_with(".patch") || stem.ends_with(".diff") {
                Kind::Patch(comp)
            } else if stem.ends_with(".mbox") || stem.ends_with(".mbx") {
                Kind::Mbox(comp)
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
            slices: HashMap::new(),
            generated: HashMap::new(),
            iso: None,
            patch: None,
            mbox: None,
            members: HashMap::new(),
            stream: Arc::new(Mutex::new(None)),
            zip: None,
            order: HashMap::new(),
            unpacked: Arc::new(Mutex::new(None)),
            slots: HashMap::new(),
            #[cfg(test)]
            opens: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            seeks: std::sync::atomic::AtomicUsize::new(0),
        };
        match kind {
            Kind::Zip => fs.index_zip()?,
            Kind::Cmd => fs.index_cmd(&name)?,
            Kind::Cpio(_) => fs.index_cpio()?,
            Kind::Ar => fs.index_ar()?,
            Kind::Deb => fs.index_deb()?,
            Kind::Rpm => fs.index_rpm()?,
            Kind::Iso => fs.index_iso()?,
            Kind::Patch(_) => fs.index_patch()?,
            Kind::Mbox(_) => fs.index_mbox()?,
            Kind::Tar(_) => fs.index_tar()?,
        }
        Ok(fs)
    }

    fn index_zip(&mut self) -> io::Result<()> {
        #[cfg(test)]
        self.opens
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let file = SharedFile::new(File::open(&self.path)?);
        let mut zip = zip::ZipArchive::new(file).map_err(zip_err)?;
        let mut numbers = HashMap::new();
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
            // a name written twice reads as its first copy
            numbers.entry(normalize_rel(&name)).or_insert(i);
            self.add(&name, kind, size, mode, None, None);
        }
        self.zip = Some(ZipIndex { zip, numbers });
        Ok(())
    }

    /// A zip member, streamed: its number looked up, its bytes read
    /// where they lie. Stored and deflated members - what zip tools
    /// write - are decoded here and checked against their CRC at the
    /// end; anything else (an encrypted member, another method) goes
    /// through the zip crate, which reads it whole or says why not.
    fn read_zip(&self, rel: &Path) -> io::Result<Box<dyn Read + Send>> {
        let index = self
            .zip
            .as_ref()
            .ok_or_else(|| io::Error::other("the zip is not open"))?;
        let &number = index
            .numbers
            .get(rel)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found in archive"))?;
        // a copy shares the parsed directory and the open file, and
        // has a position of its own
        let mut zip = index.zip.clone();
        let member = zip.by_index_raw(number).map_err(zip_err)?;
        let (start, packed, size, crc) = (
            member.data_start(),
            member.compressed_size(),
            member.size(),
            member.crc32(),
        );
        let (method, encrypted) = (member.compression(), member.encrypted());
        drop(member);
        let mut raw = index.zip_file().at(start);
        raw.limit(packed);
        let body: Box<dyn Read + Send> = match method {
            zip::CompressionMethod::Stored if !encrypted => Box::new(raw),
            zip::CompressionMethod::Deflated if !encrypted => {
                Box::new(flate2::read::DeflateDecoder::new(raw))
            }
            _ => {
                let mut member = zip.by_index(number).map_err(zip_err)?;
                let mut buf = Vec::with_capacity(member.size() as usize);
                member.read_to_end(&mut buf)?;
                return Ok(Box::new(Cursor::new(buf)));
            }
        };
        Ok(Box::new(Checked {
            body,
            left: size,
            crc: crc32fast::Hasher::new(),
            want: crc,
        }))
    }

    fn index_tar(&mut self) -> io::Result<()> {
        let reader = self.open_stream(Source::Root)?;
        self.index_tar_from(reader, Path::new(""), Source::Root)
    }

    fn index_tar_from(
        &mut self,
        reader: Box<dyn Read>,
        prefix: &Path,
        source: Source,
    ) -> io::Result<()> {
        let mut archive = tar::Archive::new(reader);
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
            } else if entry_type == tar::EntryType::Link {
                // a second name for a file written earlier: listed as a
                // file of that one's size, read as that one's bytes
                let Some(target) = header.link_name().ok().flatten() else {
                    continue;
                };
                let (name, target) = (
                    normalize_rel(&prefix.join(normalize_rel(&rel))),
                    normalize_rel(&prefix.join(normalize_rel(&target))),
                );
                let Some(&(_, _, size)) = self.members.get(&target) else {
                    continue; // points at nothing this tar has
                };
                let mtime = header
                    .mtime()
                    .ok()
                    .map(|secs| UNIX_EPOCH + Duration::from_secs(secs));
                let mode = header.mode().unwrap_or(0) & 0o7777;
                self.add(&name, EntryKind::File, size, mode, None, mtime);
                self.links.insert(name, target);
                continue;
            } else {
                // devices, FIFOs and sockets have no bytes, and a copy
                // out of an archive makes files: nothing to browse, as
                // in a cpio
                continue;
            };
            let mtime = header
                .mtime()
                .ok()
                .map(|secs| UNIX_EPOCH + Duration::from_secs(secs));
            let mode = header.mode().unwrap_or(0) & 0o7777;
            let size = header.size().unwrap_or(0);
            // positions are in the stream this tar is read from: the
            // container's, or a nested tarball's own
            if kind == EntryKind::File {
                self.members.insert(
                    normalize_rel(&prefix.join(normalize_rel(&rel))),
                    (source, member.raw_file_position(), size),
                );
            }
            self.add(
                &prefix.join(normalize_rel(&rel)),
                kind,
                size,
                mode,
                link,
                mtime,
            );
        }
        Ok(())
    }

    /// cpio streams have no index: read the whole thing once, keeping
    /// each header and skipping past its bytes. Hard links are written
    /// with the data attached to just one of the names, so the empty
    /// aliases are collected and pointed at the one that has it.
    fn index_cpio(&mut self) -> io::Result<()> {
        let reader = self.open_stream(Source::Root)?;
        self.index_cpio_from(reader, Path::new(""), Source::Root)
    }

    fn index_cpio_from(
        &mut self,
        reader: Box<dyn Read>,
        prefix: &Path,
        source: Source,
    ) -> io::Result<()> {
        // how far into the stream the reader is: right after a header,
        // that is where the member's bytes start
        let read = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let mut reader = cpio::Reader::new(Counting {
            inner: reader,
            read: std::rc::Rc::clone(&read),
        });
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
                self.members.insert(
                    normalize_rel(&prefix.join(&rel)),
                    (source, read.get(), header.size),
                );
                (EntryKind::File, None)
            } else {
                continue; // devices, fifos, sockets: nothing to browse
            };
            if kind == EntryKind::File && header.nlink > 1 {
                let id = (header.dev, header.ino);
                if header.size > 0 {
                    bodies.insert(id, (prefix.join(&rel), header.size));
                } else {
                    aliases.push((prefix.join(&rel), id));
                }
            }
            let mtime = Some(UNIX_EPOCH + Duration::from_secs(header.mtime));
            self.add(
                &prefix.join(&rel),
                kind,
                header.size,
                header.perm(),
                link,
                mtime,
            );
        }
        for (alias, id) in aliases {
            if let Some((body, size)) = bodies.get(&id).filter(|(body, _)| *body != alias) {
                self.links.insert(alias.clone(), body.clone());
                self.set_size(&alias, *size);
            }
        }
        Ok(())
    }

    /// An `ar` archive is a flat list, so the listing is the member
    /// table and nothing more.
    fn index_ar(&mut self) -> io::Result<()> {
        for member in ar::members(&self.path)? {
            let rel = normalize_rel(Path::new(&member.name));
            if rel.as_os_str().is_empty() {
                continue;
            }
            let mtime = Some(UNIX_EPOCH + Duration::from_secs(member.mtime));
            self.add(
                &rel,
                EntryKind::File,
                member.size,
                member.mode & 0o7777,
                None,
                mtime,
            );
            self.slices.insert(
                rel,
                Slice {
                    at: member.at,
                    len: member.size,
                    comp: Comp::None,
                },
            );
        }
        Ok(())
    }

    /// A Debian package is an `ar` archive of three members: a version
    /// stamp and two tarballs. Both tarballs are folded into the tree
    /// under a name of their own - `CONTROL/` for the package's
    /// metadata and maintainer scripts, `CONTENTS/` for the files it
    /// installs - so one panel shows the whole package instead of
    /// making you open two more archives to see it.
    fn index_deb(&mut self) -> io::Result<()> {
        let mut seen = false;
        for member in ar::members(&self.path)? {
            let lower = member.name.to_lowercase();
            let (stem, comp) = peel_comp(&lower);
            let slice = Slice {
                at: member.at,
                len: member.size,
                comp,
            };
            let prefix = match stem {
                "control.tar" => "CONTROL",
                "data.tar" => "CONTENTS",
                _ => {
                    // debian-binary and anything else rides along as a file
                    let rel = normalize_rel(Path::new(&member.name));
                    if !rel.as_os_str().is_empty() {
                        let mtime = Some(UNIX_EPOCH + Duration::from_secs(member.mtime));
                        self.add(&rel, EntryKind::File, member.size, 0o644, None, mtime);
                        self.slices.insert(
                            rel,
                            Slice {
                                comp: Comp::None,
                                ..slice
                            },
                        );
                    }
                    continue;
                }
            };
            let prefix = PathBuf::from(prefix);
            self.ensure_dir_chain(&prefix);
            let reader = self.open_stream(Source::Slice(slice))?;
            self.index_tar_from(reader, &prefix, Source::Slice(slice))?;
            seen = true;
        }
        if !seen {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "no control.tar or data.tar in this package",
            ));
        }
        Ok(())
    }

    /// An RPM package: the tags become a `CONTROL/header` you can read
    /// and the scriptlets become files beside it, while the payload -
    /// a cpio stream under whatever compressor the package names -
    /// hangs under `CONTENTS/`, the same shape a .deb gets.
    fn index_rpm(&mut self) -> io::Result<()> {
        let pkg = rpm::open(&self.path)?;
        if pkg.format != "cpio" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported rpm payload format: {}", pkg.format),
            ));
        }
        let comp = match pkg.compressor.as_str() {
            "gzip" => Comp::Gz,
            "xz" => Comp::Xz,
            "lzma" => Comp::Lzma,
            "bzip2" => Comp::Bz2,
            "zstd" => Comp::Zstd,
            "none" => Comp::None,
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported rpm payload compressor: {other}"),
                ));
            }
        };

        let control = PathBuf::from("CONTROL");
        self.ensure_dir_chain(&control);
        let mut generated = vec![("header".to_string(), rpm::header_text(&pkg))];
        generated.extend(
            rpm::scriptlets(&pkg)
                .into_iter()
                .map(|(name, body)| (name.to_string(), body)),
        );
        for (name, body) in generated {
            let rel = control.join(&name);
            let mode = if name == "header" { 0o644 } else { 0o755 };
            self.add(&rel, EntryKind::File, body.len() as u64, mode, None, None);
            self.generated.insert(rel, body);
        }

        let contents = PathBuf::from("CONTENTS");
        self.ensure_dir_chain(&contents);
        let len = std::fs::metadata(&self.path)?.len() - pkg.payload_at;
        let slice = Slice {
            at: pkg.payload_at,
            len,
            comp,
        };
        let reader = self.open_stream(Source::Slice(slice))?;
        self.index_cpio_from(reader, &contents, Source::Slice(slice))?;
        Ok(())
    }

    /// An ISO 9660 image already indexes itself - the walk happens at
    /// open, and every entry knows the sector its data starts at - so
    /// this only has to turn that tree into the panel's.
    fn index_iso(&mut self) -> io::Result<()> {
        let image = iso::Image::open(&self.path)?;
        for (dir, entries) in &image.tree {
            self.ensure_dir_chain(dir);
            for entry in entries {
                let kind = if entry.link.is_some() {
                    EntryKind::SymlinkFile
                } else if entry.is_dir {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                };
                let mtime = Some(UNIX_EPOCH + Duration::from_secs(entry.mtime));
                self.add(
                    &dir.join(&entry.name),
                    kind,
                    entry.size,
                    entry.mode,
                    entry.link.as_ref().map(PathBuf::from),
                    mtime,
                );
            }
        }
        self.iso = Some(image);
        Ok(())
    }

    fn read_iso(&self, rel: &Path) -> io::Result<Box<dyn Read + Send>> {
        let image = self
            .iso
            .as_ref()
            .ok_or_else(|| io::Error::other("no image"))?;
        let parent = rel.parent().unwrap_or(Path::new(""));
        let name = rel
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty path"))?;
        image
            .tree
            .get(parent)
            .and_then(|list| list.iter().find(|e| e.name == name))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found in image"))
            .and_then(|entry| image.read(entry))
    }

    /// A patch lists as the tree it would apply to: one entry per file
    /// it touches, holding that file's hunks and nothing else. Because
    /// the names are paths, `src/main.rs` in a patch that also touches
    /// `docs/` shows up under a `src/` of its own.
    fn index_patch(&mut self) -> io::Result<()> {
        let mut text = String::new();
        self.open_stream(Source::Root)?.read_to_string(&mut text)?;
        let pieces = patch::split(&text);
        if pieces.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "no diff headers in this file",
            ));
        }
        for piece in &pieces {
            let rel = normalize_rel(&piece.path);
            if rel.as_os_str().is_empty() {
                continue;
            }
            self.add(&rel, EntryKind::File, piece.len as u64, 0o644, None, None);
        }
        self.patch = Some((text, pieces));
        Ok(())
    }

    fn read_patch(&self, rel: &Path) -> io::Result<Box<dyn Read + Send>> {
        let (text, pieces) = self
            .patch
            .as_ref()
            .ok_or_else(|| io::Error::other("no patch"))?;
        pieces
            .iter()
            .find(|piece| normalize_rel(&piece.path) == rel)
            .map(|piece| {
                let body = text[piece.at..piece.at + piece.len].to_string();
                Box::new(Cursor::new(body.into_bytes())) as Box<dyn Read + Send>
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not in this patch"))
    }

    /// An mbox lists as its messages, numbered so the panel's name
    /// order is the mailbox's order.
    fn index_mbox(&mut self) -> io::Result<()> {
        let mut text = String::new();
        self.open_stream(Source::Root)?.read_to_string(&mut text)?;
        let messages = mail::split(&text);
        if messages.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "no messages in this mailbox",
            ));
        }
        for message in &messages {
            self.add(
                &message.name,
                EntryKind::File,
                message.len as u64,
                0o644,
                None,
                None,
            );
        }
        self.mbox = Some((text, messages));
        Ok(())
    }

    fn read_mbox(&self, rel: &Path) -> io::Result<Box<dyn Read + Send>> {
        let (text, messages) = self
            .mbox
            .as_ref()
            .ok_or_else(|| io::Error::other("no mailbox"))?;
        messages
            .iter()
            .find(|message| message.name == rel)
            .map(|message| {
                let body = text[message.at..message.at + message.len].to_string();
                Box::new(Cursor::new(body.into_bytes())) as Box<dyn Read + Send>
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not in this mailbox"))
    }

    /// Read a path out of a tar, a cpio, an `ar` or a package: text
    /// rcmd made up (an rpm's tags), a plain slice of the container (an
    /// `ar` member), or a member of a stream, followed there from where
    /// the last read left it. A hard link reads as what it links to.
    fn read_streamed(&self, rel: &Path) -> io::Result<Box<dyn Read + Send>> {
        if let Some(body) = self.generated.get(rel) {
            return Ok(Box::new(Cursor::new(body.clone().into_bytes())));
        }
        if let Some(slice) = self.slices.get(rel) {
            let mut file = File::open(&self.path)?;
            file.seek(SeekFrom::Start(slice.at))?;
            return Ok(Box::new(file.take(slice.len)));
        }
        let wanted = self.links.get(rel).map_or(rel, PathBuf::as_path);
        if let Some(&(source, at, len)) = self.members.get(wanted) {
            return self.read_member(source, at, len);
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "not found in archive",
        ))
    }

    /// A hard link's listing should show the size of what it points at,
    /// not the zero bytes its own record carries.
    fn set_size(&mut self, rel: &Path, size: u64) {
        let rel = normalize_rel(rel);
        let parent = rel.parent().map(Path::to_path_buf).unwrap_or_default();
        if let Some(&at) = self.slots.get(&rel)
            && let Some(entry) = self.index.get_mut(&parent).and_then(|l| l.get_mut(at))
        {
            entry.size = size;
        }
    }

    /// The entry at `rel`, by its slot.
    fn entry_at(&self, rel: &Path) -> Option<&Entry> {
        let parent = rel.parent().map(Path::to_path_buf).unwrap_or_default();
        let &at = self.slots.get(rel)?;
        self.index.get(&parent)?.get(at)
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
        let seen = self.order.len() as u64;
        let mut up = Some(rel.as_path());
        while let Some(path) = up.filter(|p| !p.as_os_str().is_empty()) {
            if self.order.contains_key(path) {
                break; // and so is everything above it
            }
            self.order.insert(path.to_path_buf(), seen);
            up = path.parent();
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
        match self.slots.get(&rel) {
            // an implicit dir may have been created first; real data wins
            Some(&at) => list[at] = entry,
            None => {
                self.slots.insert(rel.clone(), list.len());
                list.push(entry);
            }
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
        if !self.slots.contains_key(dir) {
            self.slots.insert(dir.to_path_buf(), list.len());
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
    fn index_cmd(&mut self, name: &str) -> io::Result<()> {
        const SEVENS: [&str; 3] = ["7z", "7zz", "7za"];
        let is_rar = name.ends_with(".rar");
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
        let extension = CMD_EXTENSIONS
            .iter()
            .find(|ext| name.ends_with(*ext))
            .copied()
            .unwrap_or("");
        let mut last = io::Error::new(
            io::ErrorKind::NotFound,
            if is_rar {
                "browsing .rar needs 7z (p7zip + rar codec) or unrar installed".to_string()
            } else {
                format!("browsing {extension} needs 7z / 7za (p7zip) installed")
            },
        );
        for backend in candidates {
            let output = std::process::Command::new(backend.program)
                .args(match backend.flavor {
                    CmdFlavor::SevenZip => &["l", "-ba", "-slt"][..],
                    CmdFlavor::Unrar => &["vt", "-p-"][..],
                })
                .arg("--")
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
            // unrar says "x is not RAR archive" and exits 0; a listing
            // names the archive before anything else
            if backend.flavor == CmdFlavor::Unrar
                && !text.lines().any(|l| l.trim_start().starts_with("Archive:"))
            {
                let first = text
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty() && !l.starts_with("UNRAR"))
                    .unwrap_or("not a rar archive");
                last = io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {first}", backend.program),
                );
                continue;
            }
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
        if let Some(unpacked) = self
            .unpacked
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_mut()
            && unpacked.ready.remove(rel)
        {
            // read once: the name goes now and the bytes with the handle,
            // so the scratch shrinks as the job's copies grow
            let path = unpacked.tree.join(rel);
            if let Ok(file) = File::open(&path) {
                let _ = std::fs::remove_file(&path);
                return Ok(Box::new(file));
            }
        }
        let backend = self
            .cmd
            .ok_or_else(|| io::Error::other("archive tool went away"))?;
        let output = std::process::Command::new(backend.program)
            // `--`: a member named `-p.txt` or `@list` is a name, not a
            // switch or a list file. `-spd`: nor is `*.txt` a wildcard,
            // or one member's view would stream several
            .args(match backend.flavor {
                CmdFlavor::SevenZip => &["x", "-so", "-spd", "--"][..],
                CmdFlavor::Unrar => &["p", "-inul", "-p-", "--"][..],
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

    /// Unpack every file under `paths` with one run of the tool, into a
    /// directory of its own under `scratch`: a solid 7z decompresses its
    /// block once for the lot, where a read per member went through it
    /// from the start each time.
    fn prefetch_cmd(
        &self,
        paths: &[PathBuf],
        scratch: &Path,
        step: &mut dyn FnMut(u64) -> bool,
    ) -> io::Result<Prefetched> {
        let Some(backend) = self.cmd else {
            return Ok(Prefetched::none());
        };
        let mut files = Vec::new();
        for path in paths {
            self.files_under(&normalize_rel(path), &mut files);
        }
        // a list file is lines of UTF-8; a name it cannot carry is read
        // the slow way
        files.retain(|f| f.to_str().is_some_and(|name| !name.contains(['\n', '\r'])));
        if files.is_empty() {
            return Ok(Prefetched::none());
        }
        let dir = scratch_dir(scratch)?;
        let guard = {
            let (dir, unpacked) = (dir.clone(), Arc::clone(&self.unpacked));
            Prefetched::with(move || {
                *unpacked.lock().unwrap_or_else(|p| p.into_inner()) = None;
                let _ = std::fs::remove_dir_all(&dir);
            })
        };
        let (tree, list) = (dir.join("tree"), dir.join("list"));
        std::fs::create_dir(&tree)?;
        let mut names = String::new();
        for file in &files {
            names.push_str(file.to_str().unwrap_or_default());
            names.push('\n');
        }
        std::fs::write(&list, names)?;
        let mut command = std::process::Command::new(backend.program);
        match backend.flavor {
            // -spd: a name in the list is a name, not a wildcard
            CmdFlavor::SevenZip => command
                .args(["x", "-y", "-bsp1", "-bso0", "-bse0", "-spd", "-scsUTF-8"])
                .arg(format!("-o{}", tree.display()))
                .arg(format!("-i@{}", list.display()))
                .arg("--")
                .arg(&self.path),
            CmdFlavor::Unrar => command
                .args(["x", "-y", "-o+", "-p-", "-idc", "-scfl", "--"])
                .arg(&self.path)
                .arg(format!("@{}", list.display()))
                .arg(format!("{}/", tree.display())),
        };
        let mut child = command
            .env("LC_ALL", "C")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let mut out = child.stdout.take().expect("piped");
        let mut buf = [0u8; 4096];
        let mut digits = 0u64;
        loop {
            let n = match out.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            // the tools draw their percentage over and over in place;
            // the last number before a '%' is how far they are
            let mut at = None;
            for &b in &buf[..n] {
                match b {
                    b'0'..=b'9' => digits = (digits * 10 + u64::from(b - b'0')).min(1000),
                    b'%' => {
                        at = Some(digits.min(100));
                        digits = 0;
                    }
                    _ => digits = 0,
                }
            }
            if let Some(percent) = at
                && !step(percent)
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
        }
        let _ = child.wait();
        // only a plain file inside the tree counts: not a name the tool
        // led somewhere else through a link it unpacked
        let root = tree.canonicalize()?;
        let ready = files
            .into_iter()
            .filter(|file| {
                let path = tree.join(file);
                path.symlink_metadata().is_ok_and(|m| m.is_file())
                    && path
                        .canonicalize()
                        .is_ok_and(|real| real.starts_with(&root))
            })
            .collect();
        *self.unpacked.lock().unwrap_or_else(|p| p.into_inner()) = Some(Unpacked { tree, ready });
        Ok(guard)
    }

    /// Every file at or under `rel`, by its path in the archive.
    fn files_under(&self, rel: &Path, out: &mut Vec<PathBuf>) {
        let Some(entry) = self.entry_at(rel) else {
            if rel.as_os_str().is_empty() {
                for child in self.index.get(rel).into_iter().flatten() {
                    self.files_under(&rel.join(&child.name), out);
                }
            }
            return;
        };
        match entry.kind {
            EntryKind::File => out.push(rel.to_path_buf()),
            EntryKind::Dir => {
                for child in self.index.get(rel).into_iter().flatten() {
                    self.files_under(&rel.join(&child.name), out);
                }
            }
            _ => {}
        }
    }

    /// A stream unwrapped from its first byte. Counted, in the tests,
    /// to hold a run of reads to one pass.
    fn open_stream(&self, source: Source) -> io::Result<Box<dyn Read + Send>> {
        #[cfg(test)]
        self.opens
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match source {
            Source::Root => {
                let comp = match self.kind {
                    Kind::Tar(comp) | Kind::Cpio(comp) | Kind::Patch(comp) | Kind::Mbox(comp) => {
                        comp
                    }
                    _ => unreachable!("zip, ar, deb and cmd use their own readers"),
                };
                let file = io::BufReader::new(File::open(&self.path)?);
                decompress(Box::new(file), comp)
            }
            Source::Slice(slice) => {
                let mut file = File::open(&self.path)?;
                file.seek(SeekFrom::Start(slice.at))?;
                decompress(
                    Box::new(io::BufReader::new(file).take(slice.len)),
                    slice.comp,
                )
            }
        }
    }

    /// A stream that is not compressed, opened at `at` rather than read
    /// up to it: a plain .tar, or a .deb's data.tar when it is stored
    /// plain. `None` where the stream has to be decoded from the top.
    fn seek_stream(&self, source: Source, at: u64) -> io::Result<Option<Box<dyn Read + Send>>> {
        let (start, len) = match source {
            Source::Root => match self.kind {
                Kind::Tar(Comp::None) | Kind::Cpio(Comp::None) => (0, None),
                _ => return Ok(None),
            },
            Source::Slice(slice) if slice.comp == Comp::None => (slice.at, Some(slice.len)),
            Source::Slice(_) => return Ok(None),
        };
        #[cfg(test)]
        self.seeks
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(start + at))?;
        let file = io::BufReader::new(file);
        Ok(Some(match len {
            Some(len) => Box::new(file.take(len.saturating_sub(at))),
            None => Box::new(file),
        }))
    }

    /// A member's bytes, streamed from its stream: on from where the
    /// last member left it when that is the same stream and not past
    /// this one, from the top otherwise.
    fn read_member(&self, source: Source, at: u64, len: u64) -> io::Result<Box<dyn Read + Send>> {
        let kept = self.stream.lock().unwrap_or_else(|p| p.into_inner()).take();
        let (pos, mut stream) = match kept {
            // the next member, straight on - past the header between
            Some((was, pos, stream)) if was == source && pos <= at && at - pos <= NEAR => {
                (pos, stream)
            }
            kept => match self.seek_stream(source, at)? {
                // a plain stream is the file itself: go to the member
                Some(stream) => (at, stream),
                None => match kept {
                    Some((was, pos, stream)) if was == source && pos <= at => (pos, stream),
                    _ => (0, self.open_stream(source)?),
                },
            },
        };
        let skip = at - pos;
        if io::copy(&mut (&mut stream).take(skip), &mut io::sink())? < skip {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the archive ends early",
            ));
        }
        Ok(Box::new(Member {
            stream: Some(stream),
            source,
            left: len,
            end: at + len,
            keep: self.stream.clone(),
        }))
    }
}

impl FsProvider for ArchiveFs {
    fn reopen(&self) -> Option<io::Result<Arc<dyn FsProvider>>> {
        Some(ArchiveFs::open(&self.path).map(|fs| Arc::new(fs) as Arc<dyn FsProvider>))
    }

    fn read_order(&self, paths: &mut [PathBuf]) {
        paths.sort_by_cached_key(|path| {
            self.order
                .get(&normalize_rel(path))
                .copied()
                .unwrap_or(u64::MAX)
        });
    }

    fn prefetch(
        &self,
        paths: &[PathBuf],
        scratch: &Path,
        step: &mut dyn FnMut(u64) -> bool,
    ) -> io::Result<Prefetched> {
        match self.kind {
            Kind::Cmd => self.prefetch_cmd(paths, scratch, step),
            _ => Ok(Prefetched::none()),
        }
    }

    fn read_dir(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        let dir = normalize_rel(dir);
        self.index
            .get(&dir)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such directory in archive"))
    }

    fn stat(&self, path: &Path) -> io::Result<Entry> {
        let path = normalize_rel(path);
        if path.file_name().is_none() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty path"));
        }
        self.entry_at(&path)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found in archive"))
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let rel = normalize_rel(path);
        match self.kind {
            Kind::Cmd => self.read_cmd(&rel),
            Kind::Iso => self.read_iso(&rel),
            Kind::Patch(_) => self.read_patch(&rel),
            Kind::Mbox(_) => self.read_mbox(&rel),
            Kind::Zip => self.read_zip(&rel),
            Kind::Tar(_) | Kind::Cpio(_) | Kind::Ar | Kind::Deb | Kind::Rpm => {
                self.read_streamed(&rel)
            }
        }
    }
}

/// A zip parsed once: the zip crate's archive, whose copies share the
/// directory and the file, and each member's number in it.
struct ZipIndex {
    zip: zip::ZipArchive<SharedFile>,
    numbers: HashMap<PathBuf, usize>,
}

impl ZipIndex {
    /// The open file, for reading a member's bytes where they lie.
    fn zip_file(&self) -> SharedFile {
        // the archive hands its reader out only by value, and a copy
        // of the archive is cheap
        self.zip.clone().into_inner().fresh()
    }
}

/// One open file read by many at once: each copy has its own position
/// and a small read-ahead buffer, and reads with `pread`, so none of
/// them moves the others. What lets a zip be parsed once and read
/// from by every member after.
struct SharedFile {
    file: Arc<File>,
    pos: u64,
    /// Where the readable part ends: a member's last byte, or none.
    end: Option<u64>,
    buf: Vec<u8>,
    /// The file offset `buf` starts at.
    buf_at: u64,
}

/// How much one read of the file takes ahead: the zip directory is
/// read a few dozen bytes at a time, and one syscall each is slow.
const READ_AHEAD: usize = 64 * 1024;

impl SharedFile {
    fn new(file: File) -> SharedFile {
        SharedFile {
            file: Arc::new(file),
            pos: 0,
            end: None,
            buf: Vec::new(),
            buf_at: 0,
        }
    }

    /// Another reader on the same file, at the start, with no limit.
    fn fresh(&self) -> SharedFile {
        SharedFile {
            file: Arc::clone(&self.file),
            pos: 0,
            end: None,
            buf: Vec::new(),
            buf_at: 0,
        }
    }

    fn at(mut self, pos: u64) -> SharedFile {
        self.pos = pos;
        self
    }

    /// Stop reading `len` bytes on from here.
    fn limit(&mut self, len: u64) {
        self.end = Some(self.pos.saturating_add(len));
    }
}

impl Clone for SharedFile {
    fn clone(&self) -> SharedFile {
        SharedFile {
            end: self.end,
            ..self.fresh().at(self.pos)
        }
    }
}

impl Read for SharedFile {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        use std::os::unix::fs::FileExt;
        let room = match self.end {
            Some(end) => end.saturating_sub(self.pos).min(out.len() as u64) as usize,
            None => out.len(),
        };
        if room == 0 {
            return Ok(0);
        }
        let out = &mut out[..room];
        let buffered = self.pos >= self.buf_at && self.pos < self.buf_at + self.buf.len() as u64;
        if !buffered {
            // a big read goes straight through; a small one fills the
            // buffer and is served from it
            if out.len() >= READ_AHEAD {
                let n = self.file.read_at(out, self.pos)?;
                self.pos += n as u64;
                return Ok(n);
            }
            self.buf.resize(READ_AHEAD, 0);
            let n = self.file.read_at(&mut self.buf, self.pos)?;
            self.buf.truncate(n);
            self.buf_at = self.pos;
            if n == 0 {
                return Ok(0);
            }
        }
        let from = (self.pos - self.buf_at) as usize;
        let n = out.len().min(self.buf.len() - from);
        out[..n].copy_from_slice(&self.buf[from..from + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for SharedFile {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let pos = match to {
            SeekFrom::Start(at) => Some(at),
            SeekFrom::Current(by) => self.pos.checked_add_signed(by),
            SeekFrom::End(by) => self.file.metadata()?.len().checked_add_signed(by),
        };
        self.pos = pos
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"))?;
        Ok(self.pos)
    }
}

/// A zip member's bytes as they are decoded, held to the size and the
/// CRC the directory gives: a damaged member is an error at its end,
/// not a file quietly written wrong.
struct Checked {
    body: Box<dyn Read + Send>,
    left: u64,
    crc: crc32fast::Hasher,
    want: u32,
}

impl Read for Checked {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let damaged = |why: &str| io::Error::new(io::ErrorKind::InvalidData, why.to_string());
        let n = self.body.read(out)?;
        if n as u64 > self.left {
            return Err(damaged("the zip member is longer than its size"));
        }
        self.left -= n as u64;
        self.crc.update(&out[..n]);
        if n == 0 && !out.is_empty() {
            if self.left != 0 {
                return Err(damaged("the zip member ends early"));
            }
            if self.crc.clone().finalize() != self.want {
                return Err(damaged("the zip member is damaged (CRC mismatch)"));
            }
        }
        Ok(n)
    }
}

/// The unwrapped stream between members: which one, and where in it
/// it stands.
type Kept = Arc<Mutex<Option<(Source, u64, Box<dyn Read + Send>)>>>;

/// How far ahead a kept stream is read on to a member rather than the
/// file opened again where the member is: a tar header or two.
const NEAR: u64 = 64 * 1024;

/// A reader that counts what went through it.
struct Counting {
    inner: Box<dyn Read>,
    read: std::rc::Rc<std::cell::Cell<u64>>,
}

impl Read for Counting {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read.set(self.read.get() + n as u64);
        Ok(n)
    }
}

/// One tar or cpio member being read. Read to its end, it hands the stream
/// back for the next member to go on from; dropped part way, the stream
/// goes with it, since where it stands is no longer known.
struct Member {
    stream: Option<Box<dyn Read + Send>>,
    source: Source,
    left: u64,
    end: u64,
    keep: Kept,
}

impl Read for Member {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(stream) = self.stream.as_mut() else {
            return Ok(0);
        };
        if self.left == 0 || buf.is_empty() {
            return Ok(0);
        }
        let want = buf.len().min(self.left as usize);
        let n = stream.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the archive ends early",
            ));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

impl Drop for Member {
    fn drop(&mut self) {
        if self.left == 0
            && let Some(stream) = self.stream.take()
        {
            *self.keep.lock().unwrap_or_else(|p| p.into_inner()) =
                Some((self.source, self.end, stream));
        }
    }
}

/// Files an external tool unpacked ahead of a job: where, and which of
/// them are still there to be read.
struct Unpacked {
    tree: PathBuf,
    ready: std::collections::HashSet<PathBuf>,
}

/// A fresh hidden directory under `parent`, for unpacking into.
fn scratch_dir(parent: &Path) -> io::Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = parent.join(format!(".rcmd-unpack-{}-{n}", std::process::id()));
        match std::fs::create_dir(&dir) {
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            other => return other.map(|()| dir),
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

/// Unwrap a container stream. zstd's decoder is the one that can fail
/// at construction - it reads the frame header up front.
fn decompress(reader: Box<dyn Read + Send>, comp: Comp) -> io::Result<Box<dyn Read + Send>> {
    Ok(match comp {
        Comp::None => reader,
        Comp::Gz => Box::new(GzDecoder::new(reader)),
        Comp::Xz => Box::new(xz2::read::XzDecoder::new(reader)),
        Comp::Bz2 => Box::new(bzip2::read::BzDecoder::new(reader)),
        Comp::Zstd => Box::new(
            ruzstd::decoding::StreamingDecoder::new(reader)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?,
        ),
        Comp::Lzma => {
            let stream = xz2::stream::Stream::new_auto_decoder(u64::MAX, 0)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
            Box::new(xz2::read::XzDecoder::new_stream(reader, stream))
        }
    })
}

/// What a single compressed file holds, decompressed as it is read:
/// `notes.txt.gz`, `kern.log.1.xz`. `None` when the name carries no
/// compression suffix, or when what is inside is a tar or a cpio - a
/// directory to browse, not a text to read.
pub fn decompressing(path: &Path) -> Option<io::Result<Box<dyn Read + Send>>> {
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    let (stem, comp) = peel_comp(&name);
    if comp == Comp::None || stem.ends_with(".tar") || stem.ends_with(".cpio") {
        return None;
    }
    Some(File::open(path).and_then(|file| decompress(Box::new(io::BufReader::new(file)), comp)))
}

/// Split a trailing compression suffix off a lowercased filename, so
/// "x.cpio.gz" and "x.tar.bz2" reach the same table as their plain
/// forms.
fn peel_comp(name: &str) -> (&str, Comp) {
    for (suffix, comp) in [
        (".gz", Comp::Gz),
        (".xz", Comp::Xz),
        (".bz2", Comp::Bz2),
        (".zst", Comp::Zstd),
    ] {
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

    /// A zip of `n` small deflated members, `d/fN.txt`.
    fn make_big_zip(dir: &Path, n: usize) -> PathBuf {
        let path = dir.join("many.zip");
        let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
        for i in 0..n {
            zip.start_file(
                format!("d/f{i}.txt"),
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            zip.write_all(format!("file {i}\n").repeat(50).as_bytes())
                .unwrap();
        }
        zip.finish().unwrap();
        path
    }

    #[test]
    fn a_zip_is_parsed_once_however_many_members_are_read() {
        // every member used to re-parse the whole directory and walk it
        // to the name: 4,000 small files took 105 s to extract
        let tmp = tempfile::tempdir().unwrap();
        let n = 3000;
        let fs = ArchiveFs::open(&make_big_zip(tmp.path(), n)).unwrap();
        let started = std::time::Instant::now();
        for i in 0..n {
            let mut body = String::new();
            fs.open_read(Path::new(&format!("d/f{i}.txt")))
                .unwrap()
                .read_to_string(&mut body)
                .unwrap();
            assert_eq!(body, format!("file {i}\n").repeat(50), "member {i}");
        }
        assert_eq!(fs.opens.load(std::sync::atomic::Ordering::Relaxed), 1);
        // generous for a debug build; the quadratic read took minutes
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{n} members took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_huge_directory_indexes_in_linear_time() {
        // each entry used to look for its name in the whole directory:
        // 40,000 files in one took seconds to list, before any reading
        let tmp = tempfile::tempdir().unwrap();
        let mut fs = ArchiveFs::open(&make_zip(tmp.path())).unwrap();
        let started = std::time::Instant::now();
        let n = 100_000;
        for i in 0..n {
            let name = PathBuf::from(format!("big/f{i}"));
            fs.add(&name, EntryKind::File, i as u64, 0o644, None, None);
        }
        // the same name again replaces, rather than adds
        fs.add(Path::new("big/f7"), EntryKind::File, 1, 0o600, None, None);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{n} entries took {:?}",
            started.elapsed()
        );
        assert_eq!(fs.read_dir(Path::new("big")).unwrap().len(), n);
        let seven = fs.stat(Path::new("big/f7")).unwrap();
        assert_eq!((seven.size, seven.mode), (1, 0o600));
        assert_eq!(fs.stat(Path::new("big/f99999")).unwrap().size, 99_999);
        assert!(fs.stat(Path::new("big")).unwrap().is_dir());
    }

    #[test]
    fn zip_members_stream_both_ways_they_are_stored() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mixed.zip");
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("stored.bin", stored).unwrap();
        zip.write_all(&big).unwrap();
        zip.start_file("deflated.bin", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(&big).unwrap();
        zip.start_file("empty", stored).unwrap();
        zip.finish().unwrap();
        let fs = ArchiveFs::open(&path).unwrap();
        for name in ["stored.bin", "deflated.bin"] {
            let mut body = Vec::new();
            fs.open_read(Path::new(name))
                .unwrap()
                .read_to_end(&mut body)
                .unwrap();
            assert!(body == big, "{name}");
        }
        // two readers at once do not move each other
        let (mut a, mut b) = (
            fs.open_read(Path::new("stored.bin")).unwrap(),
            fs.open_read(Path::new("deflated.bin")).unwrap(),
        );
        let (mut x, mut y) = ([0u8; 1000], [0u8; 1000]);
        for _ in 0..50 {
            a.read_exact(&mut x).unwrap();
            b.read_exact(&mut y).unwrap();
            assert_eq!(x, y);
        }
        let mut body = Vec::new();
        fs.open_read(Path::new("empty"))
            .unwrap()
            .read_to_end(&mut body)
            .unwrap();
        assert!(body.is_empty());
    }

    #[test]
    fn a_damaged_zip_member_is_an_error_not_a_wrong_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bad.zip");
        let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("a.txt", stored).unwrap();
        zip.write_all(b"the quick brown fox").unwrap();
        zip.finish().unwrap();
        // flip one byte of the member's data
        let mut bytes = std::fs::read(&path).unwrap();
        let at = bytes.windows(5).position(|w| w == b"quick").unwrap();
        bytes[at] = b'Q';
        std::fs::write(&path, bytes).unwrap();
        let fs = ArchiveFs::open(&path).unwrap();
        let mut body = Vec::new();
        let err = fs
            .open_read(Path::new("a.txt"))
            .unwrap()
            .read_to_end(&mut body)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
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

    /// Sixty members with sizes that need padding, as (name, body).
    fn sixty() -> Vec<(String, Vec<u8>)> {
        (0..60)
            .map(|n| {
                (
                    format!("f{n:02}.txt"),
                    format!("member {n}\n").repeat(n + 1).into_bytes(),
                )
            })
            .collect()
    }

    /// Read every one of `members` under `prefix` in order, and say how
    /// many times a stream was unwrapped from the top to do it.
    fn passes_to_read(fs: &ArchiveFs, prefix: &str, members: &[(String, Vec<u8>)]) -> usize {
        let before = fs.opens.load(std::sync::atomic::Ordering::Relaxed);
        for (name, body) in members {
            let mut got = Vec::new();
            fs.open_read(&Path::new(prefix).join(name))
                .unwrap()
                .read_to_end(&mut got)
                .unwrap();
            assert!(got == *body, "{prefix}{name}");
        }
        fs.opens.load(std::sync::atomic::Ordering::Relaxed) - before
    }

    #[test]
    fn a_cpio_is_read_in_one_pass_and_a_hard_link_finds_its_bytes() {
        // every member used to unwrap the stream from the top and walk
        // it to the name, and come back whole in memory
        let tmp = tempfile::tempdir().unwrap();
        let members = sixty();
        let mut rows: Vec<(&str, u32, u64, u64, &[u8])> = members
            .iter()
            .enumerate()
            .map(|(i, (name, body))| (name.as_str(), 0o100_644, 1, 10 + i as u64, &body[..]))
            .collect();
        // a hard link: the bytes ride with the first name only
        rows.push(("linked", 0o100_644, 2, 999, b"shared bytes\n"));
        rows.push(("alias", 0o100_644, 2, 999, b""));
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        gz.write_all(&write_newc(&rows)).unwrap();
        let path = tmp.path().join("many.cpio.gz");
        std::fs::write(&path, gz.finish().unwrap()).unwrap();
        let fs = ArchiveFs::open(&path).unwrap();
        assert_eq!(passes_to_read(&fs, "", &members), 1);
        let mut alias = String::new();
        fs.open_read(Path::new("alias"))
            .unwrap()
            .read_to_string(&mut alias)
            .unwrap();
        assert_eq!(alias, "shared bytes\n");
        // backwards still reads, from the top again
        assert_eq!(passes_to_read(&fs, "", &members[3..4]), 1);
    }

    #[test]
    fn an_rpm_payload_is_read_in_one_pass() {
        use crate::rpm::fixture::{Tag, build};
        let tmp = tempfile::tempdir().unwrap();
        let members = sixty();
        let rows: Vec<(&str, u32, u64, u64, &[u8])> = members
            .iter()
            .enumerate()
            .map(|(i, (name, body))| (name.as_str(), 0o100_644, 1, 10 + i as u64, &body[..]))
            .collect();
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        gz.write_all(&write_newc(&rows)).unwrap();
        let path = tmp.path().join("many-1.0-1.noarch.rpm");
        std::fs::write(
            &path,
            build(
                &[
                    Tag::Str(crate::rpm::NAME, "many"),
                    Tag::Str(crate::rpm::PAYLOADFORMAT, "cpio"),
                    Tag::Str(crate::rpm::PAYLOADCOMPRESSOR, "gzip"),
                ],
                &gz.finish().unwrap(),
            ),
        )
        .unwrap();
        let fs = ArchiveFs::open(&path).unwrap();
        assert_eq!(passes_to_read(&fs, "CONTENTS", &members), 1);
    }

    /// An `ar` archive by hand: the global header, then each member's
    /// 60-byte header and its bytes, padded to an even length.
    fn write_ar(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = b"!<arch>\n".to_vec();
        for (name, data) in members {
            let header = format!(
                "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                name,
                1_700_000_000,
                0,
                0,
                100_644,
                data.len()
            );
            out.extend_from_slice(header.as_bytes());
            out.extend_from_slice(data);
            if out.len() % 2 == 1 {
                out.push(b'\n');
            }
        }
        out
    }

    fn tar_of(members: &[(String, Vec<u8>)]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (name, body) in members {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, format!("./{name}"), &body[..])
                .unwrap();
        }
        tar.into_inner().unwrap()
    }

    #[test]
    fn a_deb_is_read_in_one_pass_per_half() {
        // a .deb's files are in tarballs inside it, and each one read
        // used to unwrap its tarball from the top and walk to it
        let tmp = tempfile::tempdir().unwrap();
        let members = sixty();
        let control = vec![("control".to_string(), b"Package: many\n".to_vec())];
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        gz.write_all(&tar_of(&control)).unwrap();
        let control_tar = gz.finish().unwrap();
        let mut xz = xz2::write::XzEncoder::new(Vec::new(), 1);
        xz.write_all(&tar_of(&members)).unwrap();
        let data_tar = xz.finish().unwrap();
        let path = tmp.path().join("many_1.0_all.deb");
        std::fs::write(
            &path,
            write_ar(&[
                ("debian-binary", b"2.0\n"),
                ("control.tar.gz", &control_tar),
                ("data.tar.xz", &data_tar),
            ]),
        )
        .unwrap();
        let fs = ArchiveFs::open(&path).unwrap();
        assert_eq!(passes_to_read(&fs, "CONTENTS", &members), 1);
        assert_eq!(passes_to_read(&fs, "CONTROL", &control), 1);
        let mut version = String::new();
        fs.open_read(Path::new("debian-binary"))
            .unwrap()
            .read_to_string(&mut version)
            .unwrap();
        assert_eq!(version, "2.0\n");
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

    /// A .deb the way dpkg-deb builds one, so the fixture matches what
    /// a package manager actually leaves on disk.
    fn make_deb(dir: &Path) -> Option<PathBuf> {
        if !tool_available("dpkg-deb", "--version") {
            eprintln!("skipping: no dpkg-deb to build the fixture");
            return None;
        }
        let root = dir.join("pkg");
        std::fs::create_dir_all(root.join("DEBIAN")).unwrap();
        std::fs::create_dir_all(root.join("usr/share/doc/hello")).unwrap();
        std::fs::write(
            root.join("DEBIAN/control"),
            "Package: hello\nVersion: 1.0\nArchitecture: all\nMaintainer: t <t@example>\n             Description: a fixture\n",
        )
        .unwrap();
        std::fs::write(root.join("DEBIAN/postinst"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(
            root.join("usr/share/doc/hello/README"),
            "installed by the package\n",
        )
        .unwrap();
        let path = dir.join("hello_1.0_all.deb");
        let ok = std::process::Command::new("dpkg-deb")
            .args(["--build", "--root-owner-group"])
            .arg(&root)
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        ok.then_some(path)
    }

    #[test]
    fn deb_shows_both_halves_of_the_package() {
        let tmp = tempfile::tempdir().unwrap();
        let Some(path) = make_deb(tmp.path()) else {
            return;
        };
        let fs = ArchiveFs::open(&path).unwrap();

        let mut names: Vec<_> = fs
            .read_dir(Path::new(""))
            .unwrap()
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["CONTENTS", "CONTROL", "debian-binary"]);

        // the version stamp is a plain ar member, read straight out
        let mut stamp = String::new();
        fs.open_read(Path::new("debian-binary"))
            .unwrap()
            .read_to_string(&mut stamp)
            .unwrap();
        assert_eq!(stamp, "2.0\n");

        // the control half: metadata and maintainer scripts
        let mut control = String::new();
        fs.open_read(Path::new("CONTROL/control"))
            .unwrap()
            .read_to_string(&mut control)
            .unwrap();
        assert!(control.contains("Package: hello"), "{control}");
        assert!(fs.stat(Path::new("CONTROL/postinst")).unwrap().size > 0);

        // the installed half, with its directory chain intact
        assert!(
            fs.stat(Path::new("CONTENTS/usr/share/doc"))
                .unwrap()
                .is_dir()
        );
        let mut readme = String::new();
        fs.open_read(Path::new("CONTENTS/usr/share/doc/hello/README"))
            .unwrap()
            .read_to_string(&mut readme)
            .unwrap();
        assert_eq!(readme, "installed by the package\n");

        assert!(fs.open_read(Path::new("CONTENTS/nothing/here")).is_err());
    }

    #[test]
    fn deb_reads_whichever_wrapper_dpkg_chose() {
        let tmp = tempfile::tempdir().unwrap();
        let Some(path) = make_deb(tmp.path()) else {
            return;
        };
        // dpkg-deb picks the compressor; whichever it was, the tarballs
        // must have come out readable
        let fs = ArchiveFs::open(&path).unwrap();
        assert!(!fs.read_dir(Path::new("CONTENTS")).unwrap().is_empty());
        assert!(!fs.read_dir(Path::new("CONTROL")).unwrap().is_empty());
    }

    #[test]
    fn ar_lists_a_static_library_flat() {
        if !tool_available("ar", "--version") {
            eprintln!("skipping: no ar binary to build the fixture");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("one.txt"), b"first\n").unwrap();
        std::fs::write(tmp.path().join("two.txt"), b"second\n").unwrap();
        assert!(
            std::process::Command::new("ar")
                .args(["rc", "box.a", "one.txt", "two.txt"])
                .current_dir(tmp.path())
                .status()
                .unwrap()
                .success()
        );
        let fs = ArchiveFs::open(&tmp.path().join("box.a")).unwrap();
        let mut names: Vec<_> = fs
            .read_dir(Path::new(""))
            .unwrap()
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["one.txt", "two.txt"]);
        let mut text = String::new();
        fs.open_read(Path::new("two.txt"))
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "second\n");
    }

    #[test]
    fn tar_zst_round_trip() {
        if !tool_available("zstd", "--version") {
            eprintln!("skipping: no zstd binary to build the fixture");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("t.tar");
        let mut builder = tar::Builder::new(File::create(&plain).unwrap());
        let mut header = tar::Header::new_gnu();
        header.set_size(9);
        header.set_cksum();
        builder
            .append_data(&mut header, "x.txt", &b"squeezed\n"[..])
            .unwrap();
        builder.finish().unwrap();
        drop(builder);
        assert!(
            std::process::Command::new("zstd")
                .args(["-q", "-f", "t.tar", "-o", "t.tar.zst"])
                .current_dir(tmp.path())
                .status()
                .unwrap()
                .success()
        );

        let fs = ArchiveFs::open(&tmp.path().join("t.tar.zst")).unwrap();
        let mut content = String::new();
        fs.open_read(Path::new("x.txt"))
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "squeezed\n");
    }

    #[test]
    fn rpm_shows_the_header_the_scripts_and_the_payload() {
        use crate::rpm::fixture::{Tag, build};

        let tmp = tempfile::tempdir().unwrap();
        // the payload is a gzipped cpio, the way an older rpm ships one
        let stream = write_newc(&[
            ("./usr/bin/hello", 0o100_755, 1, 1, b"#!/bin/sh\necho hi\n"),
            (
                "./usr/share/doc/hello/README",
                0o100_644,
                1,
                2,
                b"read me\n",
            ),
        ]);
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&stream).unwrap();
        let payload = gz.finish().unwrap();

        let path = tmp.path().join("hello-1.0-3.noarch.rpm");
        std::fs::write(
            &path,
            build(
                &[
                    Tag::Str(crate::rpm::NAME, "hello"),
                    Tag::Str(crate::rpm::VERSION, "1.0"),
                    Tag::Str(crate::rpm::RELEASE, "3"),
                    Tag::Str(crate::rpm::SUMMARY, "a fixture"),
                    Tag::Str(crate::rpm::PREUN, "#!/bin/sh\nexit 0"),
                    Tag::Str(crate::rpm::PAYLOADFORMAT, "cpio"),
                    Tag::Str(crate::rpm::PAYLOADCOMPRESSOR, "gzip"),
                ],
                &payload,
            ),
        )
        .unwrap();

        let fs = ArchiveFs::open(&path).unwrap();
        let mut names: Vec<_> = fs
            .read_dir(Path::new(""))
            .unwrap()
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["CONTENTS", "CONTROL"]);

        let mut control: Vec<_> = fs
            .read_dir(Path::new("CONTROL"))
            .unwrap()
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        control.sort();
        assert_eq!(control, ["header", "preun"]);

        let mut text = String::new();
        fs.open_read(Path::new("CONTROL/header"))
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert!(text.contains("Summary       a fixture"), "{text}");
        // the listed size has to be the text's own, not a leftover zero
        assert_eq!(
            fs.stat(Path::new("CONTROL/header")).unwrap().size,
            text.len() as u64
        );

        // the payload's "./" prefix must not survive into the tree
        assert!(fs.stat(Path::new("CONTENTS/usr/bin")).unwrap().is_dir());
        let hello = fs.stat(Path::new("CONTENTS/usr/bin/hello")).unwrap();
        assert_eq!(hello.mode, 0o755);
        let mut script = String::new();
        fs.open_read(Path::new("CONTENTS/usr/bin/hello"))
            .unwrap()
            .read_to_string(&mut script)
            .unwrap();
        assert_eq!(script, "#!/bin/sh\necho hi\n");
    }

    #[test]
    fn iso_browses_a_disc_image() {
        let Some(tool) = ["xorriso", "genisoimage", "mkisofs"]
            .into_iter()
            .find(|t| tool_available(t, "-version"))
        else {
            eprintln!("skipping: no ISO authoring tool");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("docs")).unwrap();
        std::fs::write(src.join("readme.txt"), b"on the disc\n").unwrap();
        std::fs::write(src.join("docs/manual.md"), b"# manual\n").unwrap();
        let path = tmp.path().join("disc.iso");
        let mut cmd = std::process::Command::new(tool);
        if tool == "xorriso" {
            cmd.args(["-as", "mkisofs"]);
        }
        let built = cmd
            .args(["-R", "-J", "-o"])
            .arg(&path)
            .arg(&src)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !built {
            eprintln!("skipping: the tool refused to build the fixture");
            return;
        }

        let fs = ArchiveFs::open(&path).unwrap();
        let mut names: Vec<_> = fs
            .read_dir(Path::new(""))
            .unwrap()
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["docs", "readme.txt"]);
        assert!(fs.stat(Path::new("docs")).unwrap().is_dir());

        let mut text = String::new();
        fs.open_read(Path::new("docs/manual.md"))
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "# manual\n");
        assert!(fs.open_read(Path::new("docs/missing.md")).is_err());
    }

    const PATCH: &str = "\
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1 +1 @@
-old
+new
diff --git a/docs/readme.md b/docs/readme.md
--- a/docs/readme.md
+++ b/docs/readme.md
@@ -1 +1 @@
-old title
+new title
";

    #[test]
    fn patch_lists_as_the_tree_it_would_apply_to() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("change.patch");
        std::fs::write(&path, PATCH).unwrap();
        let fs = ArchiveFs::open(&path).unwrap();

        let mut names: Vec<_> = fs
            .read_dir(Path::new(""))
            .unwrap()
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        names.sort();
        // the paths become directories, not one flat list of long names
        assert_eq!(names, ["docs", "src"]);
        assert!(fs.stat(Path::new("src")).unwrap().is_dir());

        let mut body = String::new();
        fs.open_read(Path::new("src/main.rs"))
            .unwrap()
            .read_to_string(&mut body)
            .unwrap();
        assert!(body.starts_with("diff --git a/src/main.rs"), "{body}");
        assert!(body.contains("+new"));
        assert!(!body.contains("readme"), "{body}");
        assert_eq!(
            fs.stat(Path::new("src/main.rs")).unwrap().size,
            body.len() as u64
        );
    }

    #[test]
    fn a_compressed_patch_is_the_same_patch() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("change.diff.gz");
        let mut gz = GzEncoder::new(File::create(&path).unwrap(), Compression::default());
        gz.write_all(PATCH.as_bytes()).unwrap();
        gz.finish().unwrap();

        let fs = ArchiveFs::open(&path).unwrap();
        let mut body = String::new();
        fs.open_read(Path::new("docs/readme.md"))
            .unwrap()
            .read_to_string(&mut body)
            .unwrap();
        assert!(body.contains("+new title"), "{body}");
    }

    #[test]
    fn a_file_with_no_diff_in_it_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("notes.patch");
        std::fs::write(
            &path,
            b"these are just notes
",
        )
        .unwrap();
        assert!(ArchiveFs::open(&path).is_err());
    }

    const MBOX: &str = "\
From alice@example.com Mon Aug 23 10:00:00 2026
From: Alice <alice@example.com>
Subject: the first message

Hello Bob.

From bob@example.com Mon Aug 23 11:00:00 2026
From: Bob <bob@example.com>
Subject: =?UTF-8?B?YSByZXBseQ==?=

From here on it is just body text.
";

    #[test]
    fn mbox_lists_its_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("inbox.mbox");
        std::fs::write(&path, MBOX).unwrap();
        let fs = ArchiveFs::open(&path).unwrap();

        let names: Vec<_> = fs
            .read_dir(Path::new(""))
            .unwrap()
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        // numbered, so name order is the order they arrived in, and the
        // encoded subject is readable
        assert_eq!(names, ["0001 the first message", "0002 a reply"]);

        let mut body = String::new();
        fs.open_read(Path::new("0001 the first message"))
            .unwrap()
            .read_to_string(&mut body)
            .unwrap();
        // an ordinary message, without the mbox's own separator line
        assert!(body.starts_with("From: Alice"), "{body}");
        assert!(body.contains("Hello Bob."));
        assert!(!body.contains("a reply"), "{body}");

        // a "From " line inside a body is body text, not a new message
        let mut second = String::new();
        fs.open_read(Path::new("0002 a reply"))
            .unwrap()
            .read_to_string(&mut second)
            .unwrap();
        assert!(second.contains("From here on it is just body text."));
    }

    #[test]
    fn a_file_with_no_messages_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("empty.mbox");
        std::fs::write(
            &path,
            b"not a mailbox at all
",
        )
        .unwrap();
        assert!(ArchiveFs::open(&path).is_err());
    }

    #[test]
    fn peels_compression_suffixes() {
        assert!(matches!(peel_comp("x.cpio.gz"), ("x.cpio", Comp::Gz)));
        assert!(matches!(peel_comp("x.tar.bz2"), ("x.tar", Comp::Bz2)));
        assert!(matches!(peel_comp("x.tar"), ("x.tar", Comp::None)));
        assert!(matches!(
            peel_comp("data.tar.zst"),
            ("data.tar", Comp::Zstd)
        ));
    }

    #[test]
    fn refuses_unknown_extensions_and_garbage() {
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("not-an-archive.zip");
        std::fs::write(&bogus, b"this is not a zip").unwrap();
        assert!(ArchiveFs::open(&bogus).is_err());
        assert!(ArchiveFs::open(Path::new("file.wad")).is_err());
    }

    #[test]
    fn the_external_tool_formats_reach_the_external_tool() {
        // 7z is not installed everywhere, so what is checked here is
        // that these extensions get routed to it and named in the
        // message - not that a listing comes back
        let tmp = tempfile::tempdir().unwrap();
        for ext in [".lha", ".lzh", ".arj", ".cab", ".7z"] {
            let path = tmp.path().join(format!("box{ext}"));
            std::fs::write(&path, b"not really an archive").unwrap();
            let err = match ArchiveFs::open(&path) {
                Ok(_) => panic!("{ext}: garbage listed as an archive"),
                Err(err) => err.to_string(),
            };
            assert!(
                !err.contains("unsupported archive type"),
                "{ext} was not routed: {err}"
            );
            if err.contains("needs 7z") {
                assert!(err.contains(ext), "{ext} was not named: {err}");
            }
        }
        // .rar has a second tool that can serve it, and says so
        let path = tmp.path().join("box.rar");
        std::fs::write(&path, b"not really an archive").unwrap();
        let err = match ArchiveFs::open(&path) {
            Ok(_) => panic!("garbage listed as a rar"),
            Err(err) => err.to_string(),
        };
        assert!(!err.contains("unsupported archive type"), "{err}");
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
    fn a_lone_compressed_file_reads_as_what_it_holds() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write as _;
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("kern.log.1.gz");
        let mut gz = GzEncoder::new(std::fs::File::create(&log).unwrap(), Compression::default());
        gz.write_all(b"booted\n").unwrap();
        gz.finish().unwrap();
        let mut text = String::new();
        decompressing(&log)
            .expect("a .gz is one")
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "booted\n");
        // a tarball is a directory to browse, and plain text is plain
        assert!(decompressing(&tmp.path().join("x.tar.gz")).is_none());
        assert!(decompressing(&tmp.path().join("notes.txt")).is_none());
    }

    /// Reading a tar.gz's members in order is one pass through the
    /// stream - an extraction used to decompress from the first byte
    /// for every member, which is quadratic in the archive's size.
    #[test]
    fn members_read_in_order_are_one_pass() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("many.tar.gz");
        let gz = GzEncoder::new(File::create(&path).unwrap(), Compression::fast());
        let mut tar = tar::Builder::new(gz);
        for n in 0..50 {
            let body = vec![n as u8; 10_000];
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, format!("f{n:02}.bin"), &body[..])
                .unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
        let fs = ArchiveFs::open(&path).unwrap();
        let before = fs.opens.load(std::sync::atomic::Ordering::Relaxed);
        for n in 0..50 {
            let mut body = Vec::new();
            fs.open_read(Path::new(&format!("f{n:02}.bin")))
                .unwrap()
                .read_to_end(&mut body)
                .unwrap();
            assert_eq!(body, vec![n as u8; 10_000], "member {n}");
        }
        let opens = fs.opens.load(std::sync::atomic::Ordering::Relaxed) - before;
        assert_eq!(
            opens, 1,
            "the stream was opened {opens} times for 50 members"
        );
        // out of order still works, from the top again
        let mut body = Vec::new();
        fs.open_read(Path::new("f03.bin"))
            .unwrap()
            .read_to_end(&mut body)
            .unwrap();
        assert_eq!(body, vec![3u8; 10_000]);
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

    /// A member name is a name: not a switch, not a list file, not a
    /// wildcard that streams its neighbours along with it.
    #[test]
    fn sevenz_member_names_are_taken_literally() {
        let Some(packer) = ["7za", "7z", "7zz"]
            .into_iter()
            .find(|tool| tool_available(tool, "-h"))
        else {
            eprintln!("skipping: no 7z binary");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let names = ["-p.txt", "@list", "*.txt", "plain.txt"];
        for name in names {
            std::fs::write(tmp.path().join(name), format!("I am {name}\n")).unwrap();
        }
        let status = std::process::Command::new(packer)
            .args(["a", "-bd", "box.7z", "--"])
            .args(names)
            .current_dir(tmp.path())
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());

        let fs = ArchiveFs::open(&tmp.path().join("box.7z")).unwrap();
        for name in names {
            let mut content = String::new();
            fs.open_read(Path::new(name))
                .unwrap()
                .read_to_string(&mut content)
                .unwrap();
            assert_eq!(content, format!("I am {name}\n"));
        }
    }

    #[test]
    fn a_tar_hard_link_lists_and_reads_as_what_it_links_to() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("links.tar.gz");
        let gz = flate2::write::GzEncoder::new(
            File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut tar = tar::Builder::new(gz);
        let body = b"one file, two names\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o640);
        header.set_cksum();
        tar.append_data(&mut header, "dir/first.txt", &body[..])
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Link);
        header.set_size(0);
        header.set_mode(0o640);
        tar.append_link(&mut header, "other/second.txt", "dir/first.txt")
            .unwrap();
        // a link to nothing in the archive is left out, not listed empty
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Link);
        header.set_size(0);
        tar.append_link(&mut header, "dangling.txt", "not/here.txt")
            .unwrap();
        // and a FIFO has nothing to copy out
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Fifo);
        header.set_size(0);
        header.set_cksum();
        tar.append_data(&mut header, "pipe", &[][..]).unwrap();
        tar.into_inner().unwrap().finish().unwrap();

        let fs = ArchiveFs::open(&path).unwrap();
        let second = fs.stat(Path::new("other/second.txt")).unwrap();
        assert_eq!(second.kind, EntryKind::File);
        assert_eq!(second.size, body.len() as u64);
        assert_eq!(second.mode, 0o640);
        let mut got = Vec::new();
        fs.open_read(Path::new("other/second.txt"))
            .unwrap()
            .read_to_end(&mut got)
            .unwrap();
        assert_eq!(got, body);
        assert!(fs.stat(Path::new("dangling.txt")).is_err());
        assert!(fs.stat(Path::new("pipe")).is_err());
    }

    #[test]
    fn a_plain_tar_goes_straight_to_a_member() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("plain.tar");
        let members = sixty();
        std::fs::write(&path, tar_of(&members)).unwrap();
        let fs = ArchiveFs::open(&path).unwrap();
        let before = fs.opens.load(std::sync::atomic::Ordering::Relaxed);
        // backwards: every read is behind the last, and none starts over
        let backwards: Vec<_> = members.iter().rev().cloned().collect();
        assert_eq!(passes_to_read(&fs, "", &backwards), 0);
        assert_eq!(fs.opens.load(std::sync::atomic::Ordering::Relaxed), before);
        assert_eq!(fs.seeks.load(std::sync::atomic::Ordering::Relaxed), 60);
        // and forwards goes on from one member to the next, as it stands
        let seeks = fs.seeks.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(passes_to_read(&fs, "", &members[1..]), 0);
        assert_eq!(fs.seeks.load(std::sync::atomic::Ordering::Relaxed), seeks);
    }

    #[test]
    fn a_job_reads_an_archive_in_its_own_order() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("order.tar.gz");
        // written in readdir order, which is no order by name
        let names = [
            "zeta/a.txt",
            "zeta/b.txt",
            "alpha.txt",
            "mid/c.txt",
            "beta.txt",
        ];
        let members: Vec<_> = names
            .iter()
            .map(|n| (n.to_string(), n.as_bytes().to_vec()))
            .collect();
        let gz = flate2::write::GzEncoder::new(
            File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut gz = gz;
        std::io::Write::write_all(&mut gz, &tar_of(&members)).unwrap();
        gz.finish().unwrap();
        let fs = ArchiveFs::open(&path).unwrap();
        // marked in a name-sorted panel
        let mut sources: Vec<PathBuf> = ["alpha.txt", "beta.txt", "mid", "nowhere", "zeta"]
            .iter()
            .map(PathBuf::from)
            .collect();
        fs.read_order(&mut sources);
        assert_eq!(
            sources,
            ["zeta", "alpha.txt", "mid", "beta.txt", "nowhere"]
                .iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        );
        // read in that order - a directory's files in listing order - it
        // is one pass
        let before = fs.opens.load(std::sync::atomic::Ordering::Relaxed);
        for name in [
            "zeta/a.txt",
            "zeta/b.txt",
            "alpha.txt",
            "mid/c.txt",
            "beta.txt",
        ] {
            let mut got = String::new();
            fs.open_read(Path::new(name))
                .unwrap()
                .read_to_string(&mut got)
                .unwrap();
            assert_eq!(got, name);
        }
        assert_eq!(
            fs.opens.load(std::sync::atomic::Ordering::Relaxed) - before,
            1
        );
    }

    #[test]
    fn an_external_tool_archive_unpacks_once_for_a_job() {
        let Some(packer) = ["7za", "7z", "7zz"]
            .into_iter()
            .find(|tool| tool_available(tool, "-h"))
        else {
            eprintln!("skipping: no 7z binary");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("deep/er")).unwrap();
        for n in 0..40 {
            std::fs::write(src.join(format!("deep/er/f{n}.txt")), format!("file {n}\n")).unwrap();
        }
        std::fs::write(src.join("top.txt"), b"top\n").unwrap();
        std::fs::write(src.join("left.txt"), b"left alone\n").unwrap();
        std::fs::write(src.join("*.txt"), b"a star\n").unwrap();
        let status = std::process::Command::new(packer)
            .args(["a", "-bd", "-ms=on", "../box.7z", "."])
            .current_dir(&src)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let fs = ArchiveFs::open(&tmp.path().join("box.7z")).unwrap();
        let scratch = tmp.path().join("scratch");
        std::fs::create_dir(&scratch).unwrap();
        let mut steps = Vec::new();
        let sources = [
            PathBuf::from("deep"),
            PathBuf::from("top.txt"),
            PathBuf::from("*.txt"),
        ];
        let unpacked = fs
            .prefetch(&sources, &scratch, &mut |p| {
                steps.push(p);
                true
            })
            .unwrap();
        assert!(steps.iter().all(|&p| p <= 100), "{steps:?}");
        let files_in = |dir: &Path| {
            let mut n = 0;
            let mut stack = vec![dir.to_path_buf()];
            while let Some(d) = stack.pop() {
                for e in std::fs::read_dir(d).unwrap() {
                    let e = e.unwrap();
                    if e.file_type().unwrap().is_dir() {
                        stack.push(e.path());
                    } else if e.file_name() != "list" {
                        n += 1;
                    }
                }
            }
            n
        };
        // the selection, and only it, is on disk
        assert_eq!(files_in(&scratch), 42);
        let read = |name: &str| {
            let mut got = String::new();
            fs.open_read(Path::new(name))
                .unwrap()
                .read_to_string(&mut got)
                .unwrap();
            got
        };
        for n in 0..40 {
            assert_eq!(read(&format!("deep/er/f{n}.txt")), format!("file {n}\n"));
        }
        assert_eq!(read("top.txt"), "top\n");
        assert_eq!(read("*.txt"), "a star\n");
        // each file was read from the unpacked tree, which let it go
        assert_eq!(files_in(&scratch), 0);
        // a second read, and one outside the selection, go to the tool
        assert_eq!(read("top.txt"), "top\n");
        assert_eq!(read("left.txt"), "left alone\n");
        drop(unpacked);
        assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0);
        // and a cancel stops the tool and leaves nothing behind
        let err = fs.prefetch(&sources, &scratch, &mut |_| false).err();
        if let Some(err) = err {
            assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        }
        assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0);
    }
}
