//! ISO 9660 images - what a CD, a DVD and every Linux installer
//! download is. The base format's names are shouted 8.3 with a version
//! suffix (`READ_ME.TXT;1`), so two extensions exist to carry real
//! ones, and both are read: **Rock Ridge**, which also brings Unix
//! modes and symlinks, and **Joliet**, which brings UTF-16 names and
//! nothing else. Rock Ridge wins where a disc has both, because a file
//! manager on Unix wants the modes.
//!
//! The image is a plain seekable file, so nothing is copied at open:
//! each entry records the sector its data starts at and how long it is.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Every offset in the format is counted in these.
const SECTOR: u64 = 2048;
/// Volume descriptors start here, one per sector.
const DESCRIPTORS_AT: u64 = 16;
/// A directory tree deeper than this is a loop, not a disc.
const MAX_DEPTH: usize = 32;

/// One entry as the image describes it.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    /// Where the data starts, in sectors.
    pub extent: u32,
    pub size: u64,
    /// Rock Ridge's mode where the disc has one, else a plain default.
    pub mode: u32,
    pub mtime: u64,
    /// Rock Ridge symlink target.
    pub link: Option<String>,
}

pub struct Image {
    path: PathBuf,
    /// Directory (relative, "" = root) → its entries.
    pub tree: HashMap<PathBuf, Vec<Entry>>,
    /// What the volume calls itself, and which extension named it.
    pub label: String,
    pub flavour: &'static str,
}

impl Image {
    pub fn open(path: &Path) -> io::Result<Image> {
        let mut file = File::open(path)?;
        let mut primary = None;
        let mut joliet = None;
        let mut label = String::new();

        for index in 0..32u64 {
            let mut sector = [0u8; SECTOR as usize];
            file.seek(SeekFrom::Start((DESCRIPTORS_AT + index) * SECTOR))?;
            if file.read_exact(&mut sector).is_err() {
                break;
            }
            if &sector[1..6] != b"CD001" {
                break;
            }
            match sector[0] {
                1 => {
                    primary = Some(root_record(&sector));
                    label = sector[40..72].iter().map(|b| *b as char).collect();
                }
                // supplementary: Joliet if it escapes into UCS-2
                2 if is_joliet(&sector[88..120]) => joliet = Some(root_record(&sector)),
                255 => break, // terminator
                _ => {}
            }
        }

        let Some(primary) = primary else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not an ISO 9660 image",
            ));
        };

        let mut image = Image {
            path: path.to_path_buf(),
            tree: HashMap::from([(PathBuf::new(), Vec::new())]),
            label: label.trim_end().to_string(),
            flavour: "ISO 9660",
        };

        // Rock Ridge lives in the primary tree; read it first and keep
        // Joliet for a disc that has no Unix names at all
        let (root, ucs2) = match (image.has_rock_ridge(&mut file, primary)?, joliet) {
            (true, _) => {
                image.flavour = "Rock Ridge";
                (primary, false)
            }
            (false, Some(joliet)) => {
                image.flavour = "Joliet";
                (joliet, true)
            }
            (false, None) => (primary, false),
        };
        image.walk(&mut file, root, Path::new(""), ucs2, 0)?;
        Ok(image)
    }

    /// One entry's bytes, read from where its record said they were.
    pub fn read(&self, entry: &Entry) -> io::Result<Box<dyn Read + Send>> {
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(u64::from(entry.extent) * SECTOR))?;
        Ok(Box::new(file.take(entry.size)))
    }

    /// A disc has Rock Ridge if its root directory's own record carries
    /// the system-use entries. Checking one record beats guessing.
    fn has_rock_ridge(&self, file: &mut File, root: (u32, u64)) -> io::Result<bool> {
        let (extent, size) = root;
        let block = self.block(file, extent, size.min(SECTOR))?;
        let Some(first) = block
            .first()
            .copied()
            .filter(|len| *len as usize <= block.len())
        else {
            return Ok(false);
        };
        let record = &block[..first as usize];
        Ok(system_use(record)
            .iter()
            .any(|(tag, _)| tag == b"RR" || tag == b"NM" || tag == b"PX"))
    }

    fn block(&self, file: &mut File, extent: u32, len: u64) -> io::Result<Vec<u8>> {
        file.seek(SeekFrom::Start(u64::from(extent) * SECTOR))?;
        let mut buf = vec![0u8; len as usize];
        file.read_exact(&mut buf)?;
        Ok(buf)
    }

    fn walk(
        &mut self,
        file: &mut File,
        dir: (u32, u64),
        at: &Path,
        ucs2: bool,
        depth: usize,
    ) -> io::Result<()> {
        if depth > MAX_DEPTH {
            return Ok(());
        }
        let (extent, size) = dir;
        let block = self.block(file, extent, size)?;
        let mut entries = Vec::new();
        let mut offset = 0usize;
        while offset < block.len() {
            let len = block[offset] as usize;
            if len == 0 {
                // records never straddle a sector: skip to the next one
                offset = (offset / SECTOR as usize + 1) * SECTOR as usize;
                continue;
            }
            if offset + len > block.len() {
                break;
            }
            if let Some(entry) = parse_record(&block[offset..offset + len], ucs2) {
                entries.push(entry);
            }
            offset += len;
        }

        for entry in &entries {
            if entry.is_dir {
                let sub = at.join(&entry.name);
                self.tree.entry(sub.clone()).or_default();
                let dir = (entry.extent, entry.size);
                self.walk(file, dir, &sub, ucs2, depth + 1)?;
            }
        }
        self.tree.insert(at.to_path_buf(), entries);
        Ok(())
    }
}

/// The root directory record sits inside a volume descriptor at a fixed
/// place: its extent and how many bytes of directory it holds.
fn root_record(sector: &[u8]) -> (u32, u64) {
    let record = &sector[156..156 + 34];
    (le32(&record[2..6]), u64::from(le32(&record[10..14])))
}

/// A supplementary descriptor is Joliet when its escape sequences name
/// one of UCS-2's three levels.
fn is_joliet(escapes: &[u8]) -> bool {
    escapes
        .windows(3)
        .any(|w| w == b"%/@" || w == b"%/C" || w == b"%/E")
}

/// One directory record, or `None` for the two the format uses to point
/// at itself and its parent.
fn parse_record(record: &[u8], ucs2: bool) -> Option<Entry> {
    if record.len() < 33 {
        return None;
    }
    let name_len = record[32] as usize;
    let name_bytes = record.get(33..33 + name_len)?;
    if name_bytes == b"\0" || name_bytes == b"\x01" {
        return None; // "." and ".."
    }
    let is_dir = record[25] & 0x02 != 0;
    let mut name = if ucs2 {
        String::from_utf16_lossy(
            &name_bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_be_bytes(*c))
                .collect::<Vec<_>>(),
        )
    } else {
        String::from_utf8_lossy(name_bytes).into_owned()
    };
    // "READ_ME.TXT;1" - the version suffix is noise on every disc
    if let Some((stem, version)) = name.rsplit_once(';')
        && version.chars().all(|c| c.is_ascii_digit())
    {
        name = stem.to_string();
    }
    // a trailing dot is what an extensionless name looks like in 8.3
    if !is_dir && name.ends_with('.') {
        name.pop();
    }

    let mut entry = Entry {
        name,
        is_dir,
        extent: le32(&record[2..6]),
        size: u64::from(le32(&record[10..14])),
        mode: if is_dir { 0o755 } else { 0o444 },
        mtime: record_time(&record[18..25]),
        link: None,
    };
    apply_rock_ridge(&mut entry, record);
    (!entry.name.is_empty()).then_some(entry)
}

/// Rock Ridge's NM (the real name), PX (the POSIX mode) and SL (a
/// symlink target), where the disc carries them.
fn apply_rock_ridge(entry: &mut Entry, record: &[u8]) {
    let mut name = String::new();
    let mut link = String::new();
    let mut has_name = false;
    let mut has_link = false;
    for (tag, data) in system_use(record) {
        match &tag {
            b"NM" if data.len() > 5 => {
                has_name = true;
                name.push_str(&String::from_utf8_lossy(&data[5..]));
            }
            b"PX" if data.len() >= 12 => {
                entry.mode = le32(&data[4..8]) & 0o7777;
                let kind = le32(&data[4..8]) & 0o170_000;
                if kind == 0o120_000 {
                    entry.link = Some(String::new());
                }
            }
            b"SL" if data.len() > 5 => {
                has_link = true;
                // component records: flags byte, length byte, then text
                let mut at = 5;
                while at + 1 < data.len() {
                    let flags = data[at];
                    let len = data[at + 1] as usize;
                    let Some(part) = data.get(at + 2..at + 2 + len) else {
                        break;
                    };
                    if !link.is_empty() && !link.ends_with('/') {
                        link.push('/');
                    }
                    match flags & 0x0e {
                        0x02 => link.push('.'),
                        0x04 => link.push_str(".."),
                        0x08 => link.push('/'),
                        _ => link.push_str(&String::from_utf8_lossy(part)),
                    }
                    at += 2 + len;
                }
            }
            _ => {}
        }
    }
    if has_name && !name.is_empty() {
        entry.name = name;
    }
    if has_link {
        entry.link = Some(link);
    }
}

/// The system-use area is whatever follows the name (and its padding):
/// a run of `(tag, length, version, payload)` records.
fn system_use(record: &[u8]) -> Vec<([u8; 2], Vec<u8>)> {
    let Some(name_len) = record.get(32).map(|n| *n as usize) else {
        return Vec::new();
    };
    let mut at = 33 + name_len + usize::from(name_len % 2 == 0);
    let mut out = Vec::new();
    while at + 3 <= record.len() {
        let len = record[at + 2] as usize;
        if len < 3 || at + len > record.len() {
            break;
        }
        out.push(([record[at], record[at + 1]], record[at..at + len].to_vec()));
        at += len;
    }
    out
}

/// The 7-byte record time: year since 1900, month, day, hour, minute,
/// second, and a quarter-hour offset from GMT.
fn record_time(field: &[u8]) -> u64 {
    let (year, month, day) = (
        i64::from(field[0]) + 1900,
        u32::from(field[1]),
        u32::from(field[2]),
    );
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return 0;
    }
    // days-from-civil (Howard Hinnant)
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = u64::from((month + 9) % 12);
    let doy = (153 * mp + 2) / 5 + u64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe as i64 - 719_468;
    if days < 0 {
        return 0;
    }
    let local = days as u64 * 86_400
        + u64::from(field[3]) * 3_600
        + u64::from(field[4]) * 60
        + u64::from(field[5]);
    // the offset is signed quarter-hours east of GMT
    local.saturating_sub((field[6] as i8 as i64 * 900) as u64)
}

fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xorriso() -> Option<&'static str> {
        ["xorriso", "genisoimage", "mkisofs"]
            .into_iter()
            .find(|tool| {
                std::process::Command::new(tool)
                    .arg("-version")
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok()
            })
    }

    /// Build an image from a fixture tree with whichever flags are
    /// asked for: `-R` for Rock Ridge, `-J` for Joliet, `--norock` for
    /// the shouted 8.3 names the base format gives you on its own.
    /// xorriso writes Rock Ridge unless told not to, so the plainer
    /// fixtures have to say so.
    fn build(tool: &str, dir: &Path, flags: &[&str]) -> Option<PathBuf> {
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("subdir")).unwrap();
        std::fs::write(src.join("readme.txt"), b"on the disc\n").unwrap();
        std::fs::write(src.join("subdir/deep.txt"), b"further in\n").unwrap();
        std::fs::write(src.join("a-long-lower-case-name.md"), b"# long\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("readme.txt", src.join("point")).unwrap();

        let out = dir.join(format!("disc{}.iso", flags.concat().replace('-', "")));
        let _ = std::fs::remove_file(&out);
        let mut cmd = std::process::Command::new(tool);
        if tool == "xorriso" {
            cmd.args(["-as", "mkisofs"]);
        }
        let ok = cmd
            .args(flags)
            .arg("-o")
            .arg(&out)
            .arg(&src)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        ok.then_some(out)
    }

    fn names(image: &Image, dir: &str) -> Vec<String> {
        let mut out: Vec<_> = image
            .tree
            .get(Path::new(dir))
            .unwrap()
            .iter()
            .map(|e| e.name.clone())
            .collect();
        out.sort();
        out
    }

    #[test]
    fn rock_ridge_gives_unix_names_modes_and_symlinks() {
        let Some(tool) = xorriso() else {
            eprintln!("skipping: no ISO authoring tool");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let Some(path) = build(tool, tmp.path(), &["-R"]) else {
            eprintln!("skipping: the tool refused to build the fixture");
            return;
        };
        let image = Image::open(&path).unwrap();
        assert_eq!(image.flavour, "Rock Ridge");
        assert_eq!(
            names(&image, ""),
            ["a-long-lower-case-name.md", "point", "readme.txt", "subdir"]
        );
        assert_eq!(names(&image, "subdir"), ["deep.txt"]);

        let root = image.tree.get(Path::new("")).unwrap();
        let readme = root.iter().find(|e| e.name == "readme.txt").unwrap();
        assert_eq!(readme.size, 12);
        assert_eq!(readme.mode & 0o444, 0o444);
        let mut text = String::new();
        image
            .read(readme)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "on the disc\n");

        #[cfg(unix)]
        {
            let point = root.iter().find(|e| e.name == "point").unwrap();
            assert_eq!(point.link.as_deref(), Some("readme.txt"));
        }
    }

    #[test]
    fn joliet_carries_the_long_names_when_rock_ridge_is_absent() {
        let Some(tool) = xorriso() else {
            eprintln!("skipping: no ISO authoring tool");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let Some(path) = build(tool, tmp.path(), &["-J", "--norock"]) else {
            eprintln!("skipping: the tool refused to build the fixture");
            return;
        };
        let image = Image::open(&path).unwrap();
        assert_eq!(image.flavour, "Joliet");
        assert!(
            names(&image, "").contains(&"a-long-lower-case-name.md".to_string()),
            "{:?}",
            names(&image, "")
        );
        let deep = &image.tree.get(Path::new("subdir")).unwrap()[0];
        let mut text = String::new();
        image.read(deep).unwrap().read_to_string(&mut text).unwrap();
        assert_eq!(text, "further in\n");
    }

    #[test]
    fn the_base_format_still_lists_without_either_extension() {
        let Some(tool) = xorriso() else {
            eprintln!("skipping: no ISO authoring tool");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let Some(path) = build(tool, tmp.path(), &["--norock"]) else {
            eprintln!("skipping: the tool refused to build the fixture");
            return;
        };
        let image = Image::open(&path).unwrap();
        assert_eq!(image.flavour, "ISO 9660");
        let listed = names(&image, "");
        // 8.3 and shouted, but the ";1" version suffix must be gone
        assert!(listed.iter().all(|n| !n.contains(';')), "{listed:?}");
        assert!(
            listed.iter().any(|n| n.eq_ignore_ascii_case("readme.txt")),
            "{listed:?}"
        );
    }

    #[test]
    fn refuses_a_file_that_is_not_an_image() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("not.iso");
        std::fs::write(&path, vec![0u8; 40 * 2048]).unwrap();
        assert!(Image::open(&path).is_err());
    }

    #[test]
    fn record_time_reads_the_seven_byte_field() {
        // 2023-11-14 22:13:20 UTC, the epoch second 1_700_000_000
        assert_eq!(record_time(&[123, 11, 14, 22, 13, 20, 0]), 1_700_000_000);
        // an hour east of GMT is an hour earlier in absolute terms
        assert_eq!(record_time(&[123, 11, 14, 22, 13, 20, 4]), 1_699_996_400);
        assert_eq!(record_time(&[123, 0, 14, 22, 13, 20, 0]), 0);
    }
}
