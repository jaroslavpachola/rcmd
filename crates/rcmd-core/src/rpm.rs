//! RPM packages: the lead, the two headers and where the payload
//! starts. The payload itself is a cpio stream under one of the usual
//! compressors, which is why [`crate::cpio`] came first.
//!
//! Only what a panel needs is decoded - the tags that describe the
//! package and the scriptlets it carries. Signatures are located and
//! stepped over, not checked: this is a file browser, and a listing is
//! not a claim that a package is authentic.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const LEAD: usize = 96;
const LEAD_MAGIC: [u8; 4] = [0xed, 0xab, 0xee, 0xdb];
const HEADER_MAGIC: [u8; 3] = [0x8e, 0xad, 0xe8];

/// A header that claims more entries than this is corrupt, not big.
const MAX_ENTRIES: u32 = 64 * 1024;
/// Likewise for the data store it indexes into.
const MAX_STORE: u32 = 256 * 1024 * 1024;

pub const NAME: u32 = 1000;
pub const VERSION: u32 = 1001;
pub const RELEASE: u32 = 1002;
pub const EPOCH: u32 = 1003;
pub const SUMMARY: u32 = 1004;
pub const DESCRIPTION: u32 = 1005;
pub const BUILDTIME: u32 = 1006;
pub const BUILDHOST: u32 = 1007;
pub const SIZE: u32 = 1009;
pub const LICENSE: u32 = 1014;
pub const PACKAGER: u32 = 1015;
pub const GROUP: u32 = 1016;
pub const URL: u32 = 1020;
pub const OS: u32 = 1021;
pub const ARCH: u32 = 1022;
pub const PREIN: u32 = 1023;
pub const POSTIN: u32 = 1024;
pub const PREUN: u32 = 1025;
pub const POSTUN: u32 = 1026;
pub const SOURCERPM: u32 = 1044;
pub const PAYLOADFORMAT: u32 = 1124;
pub const PAYLOADCOMPRESSOR: u32 = 1125;

/// One tag's value, in the shape the header stored it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Int(Vec<u64>),
    Str(Vec<String>),
    Bin(Vec<u8>),
}

impl Value {
    /// The first string, for the tags that hold exactly one.
    pub fn text(&self) -> Option<&str> {
        match self {
            Value::Str(list) => list.first().map(String::as_str),
            _ => None,
        }
    }

    pub fn int(&self) -> Option<u64> {
        match self {
            Value::Int(list) => list.first().copied(),
            _ => None,
        }
    }
}

pub struct Package {
    pub tags: HashMap<u32, Value>,
    /// Where the compressed payload begins.
    pub payload_at: u64,
    /// "cpio", in every package anyone has ever shipped.
    pub format: String,
    /// "gzip", "xz", "zstd", "bzip2", "lzma" or "none".
    pub compressor: String,
}

impl Package {
    pub fn text(&self, tag: u32) -> Option<&str> {
        self.tags.get(&tag).and_then(Value::text)
    }

    pub fn int(&self, tag: u32) -> Option<u64> {
        self.tags.get(&tag).and_then(Value::int)
    }

    /// Name-version-release, the way rpm prints it.
    pub fn nvr(&self) -> String {
        let name = self.text(NAME).unwrap_or("package");
        match (self.text(VERSION), self.text(RELEASE)) {
            (Some(v), Some(r)) => format!("{name}-{v}-{r}"),
            (Some(v), None) => format!("{name}-{v}"),
            _ => name.to_string(),
        }
    }
}

/// Read the lead and both headers, stopping where the payload starts.
pub fn open(path: &Path) -> io::Result<Package> {
    let mut file = File::open(path)?;
    let mut lead = [0u8; LEAD];
    file.read_exact(&mut lead)?;
    if lead[..4] != LEAD_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not an rpm package",
        ));
    }

    // the signature header is stepped over; the one after it describes
    // the package, and starts on the next 8-byte boundary
    let signature_end = read_header(&mut file)?.1;
    let aligned = signature_end.div_ceil(8) * 8;
    file.seek(SeekFrom::Start(aligned))?;
    let (tags, payload_at) = read_header(&mut file)?;

    let format = tags
        .get(&PAYLOADFORMAT)
        .and_then(Value::text)
        .unwrap_or("cpio")
        .to_string();
    let compressor = tags
        .get(&PAYLOADCOMPRESSOR)
        .and_then(Value::text)
        .unwrap_or("gzip")
        .to_string();
    Ok(Package {
        tags,
        payload_at,
        format,
        compressor,
    })
}

/// One header: a fixed preamble, an index of 16-byte entries, and the
/// data store they point into. Returns the tags and the offset just
/// past the store.
fn read_header(file: &mut File) -> io::Result<(HashMap<u32, Value>, u64)> {
    let mut preamble = [0u8; 16];
    file.read_exact(&mut preamble)?;
    if preamble[..3] != HEADER_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "rpm header magic is missing",
        ));
    }
    let entries = be32(&preamble[8..12]);
    let store_len = be32(&preamble[12..16]);
    if entries > MAX_ENTRIES || store_len > MAX_STORE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "rpm header is not a header",
        ));
    }

    let mut index = vec![0u8; entries as usize * 16];
    file.read_exact(&mut index)?;
    let mut store = vec![0u8; store_len as usize];
    file.read_exact(&mut store)?;
    let end = file.stream_position()?;

    let mut tags = HashMap::new();
    for entry in index.as_chunks::<16>().0 {
        let tag = be32(&entry[0..4]);
        let kind = be32(&entry[4..8]);
        let at = be32(&entry[8..12]) as usize;
        let count = be32(&entry[12..16]) as usize;
        if let Some(value) = decode(kind, &store, at, count) {
            tags.insert(tag, value);
        }
    }
    Ok((tags, end))
}

/// One index entry's value, or `None` when it points outside the store
/// or names a type with nothing to show.
fn decode(kind: u32, store: &[u8], at: usize, count: usize) -> Option<Value> {
    let rest = store.get(at..)?;
    match kind {
        // CHAR and INT8 through INT64
        2..=5 => {
            let width = match kind {
                2 | 3 => 1 << (kind - 2),
                4 => 4,
                _ => 8,
            };
            let bytes = rest.get(..count.checked_mul(width)?)?;
            Some(Value::Int(
                bytes
                    .chunks_exact(width)
                    .map(|c| c.iter().fold(0u64, |acc, b| acc << 8 | u64::from(*b)))
                    .collect(),
            ))
        }
        // STRING, STRING_ARRAY, I18NSTRING: NUL-terminated, back to back
        6 | 8 | 9 => {
            let wanted = if kind == 6 { 1 } else { count };
            let mut out = Vec::with_capacity(wanted);
            let mut slice = rest;
            for _ in 0..wanted {
                let end = slice.iter().position(|b| *b == 0)?;
                out.push(String::from_utf8_lossy(&slice[..end]).into_owned());
                slice = slice.get(end + 1..)?;
            }
            Some(Value::Str(out))
        }
        // BIN
        7 => Some(Value::Bin(rest.get(..count)?.to_vec())),
        _ => None,
    }
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// The package's tags rendered as the text file a panel can view.
pub fn header_text(pkg: &Package) -> String {
    let mut out = String::new();
    let mut line = |label: &str, value: &str| {
        if !value.is_empty() {
            out.push_str(&format!("{label:<14}{value}\n"));
        }
    };
    line("Name", pkg.text(NAME).unwrap_or(""));
    if let Some(epoch) = pkg.int(EPOCH) {
        line("Epoch", &epoch.to_string());
    }
    line("Version", pkg.text(VERSION).unwrap_or(""));
    line("Release", pkg.text(RELEASE).unwrap_or(""));
    line("Architecture", pkg.text(ARCH).unwrap_or(""));
    line("OS", pkg.text(OS).unwrap_or(""));
    line("Group", pkg.text(GROUP).unwrap_or(""));
    line("License", pkg.text(LICENSE).unwrap_or(""));
    line("URL", pkg.text(URL).unwrap_or(""));
    line("Packager", pkg.text(PACKAGER).unwrap_or(""));
    line("Build host", pkg.text(BUILDHOST).unwrap_or(""));
    if let Some(size) = pkg.int(SIZE) {
        line("Size", &size.to_string());
    }
    line("Source RPM", pkg.text(SOURCERPM).unwrap_or(""));
    line("Payload", &format!("{} / {}", pkg.format, pkg.compressor));
    line("Summary", pkg.text(SUMMARY).unwrap_or(""));
    if let Some(description) = pkg.text(DESCRIPTION) {
        out.push_str("\nDescription\n");
        for text in description.lines() {
            out.push_str(&format!("  {text}\n"));
        }
    }
    out
}

/// The scriptlets a package carries, as `(filename, body)`.
pub fn scriptlets(pkg: &Package) -> Vec<(&'static str, String)> {
    [
        ("prein", PREIN),
        ("postin", POSTIN),
        ("preun", PREUN),
        ("postun", POSTUN),
    ]
    .into_iter()
    .filter_map(|(name, tag)| {
        let body = pkg.text(tag)?;
        (!body.is_empty()).then(|| (name, format!("{body}\n")))
    })
    .collect()
}

#[cfg(test)]
pub(crate) mod fixture {
    use super::*;

    /// One tag to write into a test package.
    pub enum Tag {
        Str(u32, &'static str),
        Int(u32, u32),
    }

    fn header(tags: &[Tag]) -> Vec<u8> {
        let mut index = Vec::new();
        let mut store: Vec<u8> = Vec::new();
        for tag in tags {
            let (id, kind, at, count) = match tag {
                Tag::Str(id, text) => {
                    let at = store.len();
                    store.extend_from_slice(text.as_bytes());
                    store.push(0);
                    (*id, 6u32, at, 1usize)
                }
                Tag::Int(id, value) => {
                    // INT32 is aligned to four bytes in a real header
                    while !store.len().is_multiple_of(4) {
                        store.push(0);
                    }
                    let at = store.len();
                    store.extend_from_slice(&value.to_be_bytes());
                    (*id, 4u32, at, 1usize)
                }
            };
            index.extend_from_slice(&id.to_be_bytes());
            index.extend_from_slice(&kind.to_be_bytes());
            index.extend_from_slice(&(at as u32).to_be_bytes());
            index.extend_from_slice(&(count as u32).to_be_bytes());
        }
        let mut out = Vec::new();
        out.extend_from_slice(&HEADER_MAGIC);
        out.push(1);
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&(tags.len() as u32).to_be_bytes());
        out.extend_from_slice(&(store.len() as u32).to_be_bytes());
        out.extend_from_slice(&index);
        out.extend_from_slice(&store);
        out
    }

    /// A whole package: lead, signature header, main header, payload.
    pub fn build(tags: &[Tag], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&LEAD_MAGIC);
        out.resize(LEAD, 0);
        // the signature header carries nothing a browser reads, but it
        // has to be stepped over, and the next one is 8-byte aligned
        out.extend_from_slice(&header(&[Tag::Int(1000, 0)]));
        while !out.len().is_multiple_of(8) {
            out.push(0);
        }
        out.extend_from_slice(&header(tags));
        out.extend_from_slice(payload);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{Tag, build};
    use super::*;

    fn sample() -> Vec<Tag> {
        vec![
            Tag::Str(NAME, "hello"),
            Tag::Str(VERSION, "1.0"),
            Tag::Str(RELEASE, "3.fc42"),
            Tag::Str(ARCH, "noarch"),
            Tag::Str(SUMMARY, "a fixture"),
            Tag::Str(DESCRIPTION, "two lines\nof description"),
            Tag::Str(LICENSE, "MIT"),
            Tag::Int(SIZE, 4096),
            Tag::Str(POSTIN, "#!/bin/sh\nexit 0"),
            Tag::Str(PAYLOADFORMAT, "cpio"),
            Tag::Str(PAYLOADCOMPRESSOR, "zstd"),
        ]
    }

    fn write(dir: &Path, payload: &[u8]) -> std::path::PathBuf {
        let path = dir.join("hello-1.0-3.fc42.noarch.rpm");
        std::fs::write(&path, build(&sample(), payload)).unwrap();
        path
    }

    #[test]
    fn reads_the_tags_and_finds_the_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), b"PAYLOAD-GOES-HERE");
        let pkg = open(&path).unwrap();

        assert_eq!(pkg.text(NAME), Some("hello"));
        assert_eq!(pkg.text(ARCH), Some("noarch"));
        assert_eq!(pkg.int(SIZE), Some(4096));
        assert_eq!(pkg.nvr(), "hello-1.0-3.fc42");
        assert_eq!(pkg.format, "cpio");
        assert_eq!(pkg.compressor, "zstd");

        // the payload offset has to land exactly where the bytes start,
        // which is the one thing the whole reader exists to get right
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[pkg.payload_at as usize..], b"PAYLOAD-GOES-HERE");
    }

    #[test]
    fn renders_the_tags_as_something_readable() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = open(&write(tmp.path(), b"")).unwrap();
        let text = header_text(&pkg);
        assert!(text.contains("Name          hello"), "{text}");
        assert!(text.contains("License       MIT"), "{text}");
        assert!(text.contains("Payload       cpio / zstd"), "{text}");
        assert!(text.contains("  of description"), "{text}");
        // a tag the package does not carry leaves no empty line behind
        assert!(!text.contains("Packager"), "{text}");
    }

    #[test]
    fn lists_only_the_scriptlets_that_are_there() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = open(&write(tmp.path(), b"")).unwrap();
        let scripts = scriptlets(&pkg);
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].0, "postin");
        assert_eq!(scripts[0].1, "#!/bin/sh\nexit 0\n");
    }

    #[test]
    fn refuses_a_file_that_is_not_a_package() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("not.rpm");
        std::fs::write(&path, vec![0u8; 400]).unwrap();
        assert!(open(&path).is_err());
    }

    #[test]
    fn refuses_a_header_that_claims_an_impossible_size() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bytes = build(&sample(), b"");
        // the entry count of the signature header, straight after the lead
        bytes[LEAD + 8..LEAD + 12].copy_from_slice(&0x7fff_ffffu32.to_be_bytes());
        let path = tmp.path().join("bad.rpm");
        std::fs::write(&path, bytes).unwrap();
        assert!(open(&path).is_err());
    }

    #[test]
    fn an_index_entry_pointing_outside_the_store_is_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let tags = vec![Tag::Str(NAME, "hello"), Tag::Str(SUMMARY, "fine")];
        let mut bytes = build(&tags, b"");
        // walk to the main header and push the second entry's offset
        // past the end of the data store
        let at = bytes
            .windows(3)
            .rposition(|w| w == HEADER_MAGIC)
            .expect("main header");
        let entry = at + 16 + 16;
        bytes[entry + 8..entry + 12].copy_from_slice(&0x0000_7fffu32.to_be_bytes());
        let path = tmp.path().join("odd.rpm");
        std::fs::write(&path, bytes).unwrap();

        let pkg = open(&path).unwrap();
        assert_eq!(pkg.text(NAME), Some("hello"));
        assert_eq!(pkg.text(SUMMARY), None);
    }
}
