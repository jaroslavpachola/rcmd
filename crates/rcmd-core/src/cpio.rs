//! The cpio archive format, in the shapes GNU cpio writes: the "newc"
//! and "crc" ASCII headers (`070701` / `070702`), the portable octal
//! "odc" header (`070707`), and the old binary one in either byte
//! order. Each member's header carries its own magic, so a stream that
//! mixes formats still reads.
//!
//! cpio has no index and a compressed one cannot seek, so the stream is
//! read start to finish: the caller takes a member's bytes when its
//! header comes past, or leaves them and the reader skips over them on
//! the way to the next header.
//!
//! This lives outside the archive VFS because an rpm's payload is a
//! cpio stream too.

use std::ffi::OsString;
use std::io::{self, Read};
use std::path::PathBuf;

/// The name of the record that ends every well-formed stream.
const TRAILER: &str = "TRAILER!!!";

/// A name longer than this is a corrupt header, not a filename - the
/// limit stops one from asking for an arbitrary allocation.
const MAX_NAME: u64 = 64 * 1024;

const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFREG: u32 = 0o100_000;
const S_IFLNK: u32 = 0o120_000;

/// One member's header. `mode` is the full `st_mode`, file-type bits
/// included; `dev` and `ino` together are the identity hard links share.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub path: PathBuf,
    pub mode: u32,
    pub size: u64,
    pub mtime: u64,
    pub nlink: u64,
    pub dev: u64,
    pub ino: u64,
}

impl Header {
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }

    pub fn is_file(&self) -> bool {
        self.mode & S_IFMT == S_IFREG
    }

    pub fn is_symlink(&self) -> bool {
        self.mode & S_IFMT == S_IFLNK
    }

    /// Permission bits alone, for a listing's mode column.
    pub fn perm(&self) -> u32 {
        self.mode & 0o7777
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Format {
    /// `070701` and `070702`: 110-byte header of 8-digit hex fields.
    Newc,
    /// `070707`: 76-byte header of octal fields.
    Odc,
    /// 26-byte header of 16-bit words, `swapped` when the writer's
    /// byte order was the other one.
    Bin { swapped: bool },
}

/// Reads members out of a cpio stream, one after another.
pub struct Reader<R> {
    inner: R,
    /// The current member's bytes, still unread.
    pending: u64,
    /// Padding after them, skipped with the data.
    pad: u64,
    done: bool,
}

impl<R: Read> Reader<R> {
    pub fn new(inner: R) -> Self {
        Reader {
            inner,
            pending: 0,
            pad: 0,
            done: false,
        }
    }

    /// The next member, or `None` at the trailer or the end of the
    /// stream. Any bytes left unread from the previous one are skipped.
    pub fn next_member(&mut self) -> io::Result<Option<Header>> {
        if self.done {
            return Ok(None);
        }
        let skip = self.pending + self.pad;
        self.pending = 0;
        self.pad = 0;
        if skip > 0 {
            io::copy(&mut self.inner.by_ref().take(skip), &mut io::sink())?;
        }

        let mut magic = [0u8; 6];
        if !self.read_full(&mut magic)? {
            self.done = true;
            return Ok(None);
        }
        let header = match detect(&magic) {
            Some(Format::Newc) => self.read_newc()?,
            Some(Format::Odc) => self.read_odc()?,
            Some(Format::Bin { swapped }) => self.read_bin(&magic, swapped)?,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "not a cpio header",
                ));
            }
        };
        if header.path.as_os_str() == TRAILER {
            self.done = true;
            return Ok(None);
        }
        Ok(Some(header))
    }

    /// The current member's bytes. Reading them twice yields the second
    /// call an empty vector, since the stream has moved on.
    pub fn data(&mut self) -> io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.inner
            .by_ref()
            .take(self.pending)
            .read_to_end(&mut buf)?;
        if buf.len() as u64 != self.pending {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "cpio member ends early",
            ));
        }
        self.pending = 0;
        Ok(buf)
    }

    /// `Ok(false)` when the stream ended cleanly on the first byte -
    /// trailing NUL padding after the trailer looks like that too.
    fn read_full(&mut self, buf: &mut [u8]) -> io::Result<bool> {
        let mut filled = 0;
        while filled < buf.len() {
            match self.inner.read(&mut buf[filled..])? {
                0 if filled == 0 => return Ok(false),
                0 => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "cpio header ends early",
                    ));
                }
                n => filled += n,
            }
        }
        Ok(true)
    }

    fn read_newc(&mut self) -> io::Result<Header> {
        let mut rest = [0u8; 104];
        self.read_full(&mut rest)?;
        let field = |i: usize| number(&rest[i * 8..i * 8 + 8], 16);
        let size = field(6)?;
        let namesize = field(11)?;
        let header = Header {
            path: self.read_name(namesize, (4 - (110 + namesize) % 4) % 4)?,
            mode: field(1)? as u32,
            size,
            mtime: field(5)?,
            nlink: field(4)?,
            dev: field(7)? << 32 | field(8)?,
            ino: field(0)?,
        };
        self.pending = size;
        self.pad = (4 - size % 4) % 4;
        Ok(header)
    }

    fn read_odc(&mut self) -> io::Result<Header> {
        let mut rest = [0u8; 70];
        self.read_full(&mut rest)?;
        let field = |from: usize, len: usize| number(&rest[from..from + len], 8);
        let size = field(59, 11)?;
        let namesize = field(53, 6)?;
        let header = Header {
            path: self.read_name(namesize, 0)?,
            mode: field(12, 6)? as u32,
            size,
            mtime: field(42, 11)?,
            nlink: field(30, 6)?,
            dev: field(0, 6)?,
            ino: field(6, 6)?,
        };
        self.pending = size;
        self.pad = 0;
        Ok(header)
    }

    fn read_bin(&mut self, magic: &[u8; 6], swapped: bool) -> io::Result<Header> {
        let mut raw = [0u8; 26];
        raw[..6].copy_from_slice(magic);
        self.read_full(&mut raw[6..])?;
        let short = |at: usize| {
            let pair = [raw[at], raw[at + 1]];
            u64::from(if swapped {
                u16::from_be_bytes(pair)
            } else {
                u16::from_le_bytes(pair)
            })
        };
        // longs are two shorts, the high half first, whatever the byte
        // order inside each half
        let long = |at: usize| short(at) << 16 | short(at + 2);
        let size = long(22);
        let namesize = short(20);
        let header = Header {
            path: self.read_name(namesize, namesize % 2)?,
            mode: short(6) as u32,
            size,
            mtime: long(16),
            nlink: short(12),
            dev: short(2),
            ino: short(4),
        };
        self.pending = size;
        self.pad = size % 2;
        Ok(header)
    }

    /// `namesize` counts the terminating NUL; `pad` is the filler the
    /// format puts after the name to reach its alignment.
    fn read_name(&mut self, namesize: u64, pad: u64) -> io::Result<PathBuf> {
        if namesize == 0 || namesize > MAX_NAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cpio member name is not a length",
            ));
        }
        let mut buf = vec![0u8; namesize as usize];
        self.read_full(&mut buf)?;
        if pad > 0 {
            io::copy(&mut self.inner.by_ref().take(pad), &mut io::sink())?;
        }
        while buf.last() == Some(&0) {
            buf.pop();
        }
        Ok(PathBuf::from(os_string(buf)))
    }
}

fn detect(magic: &[u8; 6]) -> Option<Format> {
    match magic {
        b"070701" | b"070702" => Some(Format::Newc),
        b"070707" => Some(Format::Odc),
        // 0o070707 as a 16-bit word, written either way round
        [0xc7, 0x71, ..] => Some(Format::Bin { swapped: false }),
        [0x71, 0xc7, ..] => Some(Format::Bin { swapped: true }),
        _ => None,
    }
}

/// A fixed-width ASCII number field. Some writers pad with spaces
/// rather than zeros, so both are tolerated.
fn number(field: &[u8], radix: u32) -> io::Result<u64> {
    let text = std::str::from_utf8(field)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "cpio field is not ASCII"))?;
    let text = text.trim_matches(|c: char| c == ' ' || c == '\0');
    u64::from_str_radix(text, radix)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "cpio field is not a number"))
}

#[cfg(unix)]
fn os_string(bytes: Vec<u8>) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(bytes)
}

#[cfg(not(unix))]
fn os_string(bytes: Vec<u8>) -> OsString {
    OsString::from(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::path::Path;

    /// One member to write into a test fixture.
    struct M<'a> {
        name: &'a str,
        mode: u32,
        data: &'a [u8],
        nlink: u64,
        ino: u64,
    }

    fn m<'a>(name: &'a str, mode: u32, data: &'a [u8]) -> M<'a> {
        M {
            name,
            mode,
            data,
            nlink: 1,
            ino: 0,
        }
    }

    fn pad_to(out: &mut Vec<u8>, align: usize) {
        while !out.len().is_multiple_of(align) {
            out.push(0);
        }
    }

    fn ino_of(mem: &M, i: usize) -> u64 {
        if mem.ino != 0 { mem.ino } else { i as u64 + 1 }
    }

    fn write_newc(members: &[M]) -> Vec<u8> {
        let mut out = Vec::new();
        let trailer = m(TRAILER, 0, b"");
        for (i, mem) in members.iter().chain([&trailer]).enumerate() {
            let name = format!("{}\0", mem.name);
            out.extend_from_slice(b"070701");
            for value in [
                ino_of(mem, i),
                u64::from(mem.mode),
                0,
                0,
                mem.nlink.max(1),
                1_700_000_000,
                mem.data.len() as u64,
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
            pad_to(&mut out, 4);
            out.extend_from_slice(mem.data);
            pad_to(&mut out, 4);
        }
        out
    }

    fn write_odc(members: &[M]) -> Vec<u8> {
        let mut out = Vec::new();
        let trailer = m(TRAILER, 0, b"");
        for (i, mem) in members.iter().chain([&trailer]).enumerate() {
            let name = format!("{}\0", mem.name);
            out.extend_from_slice(b"070707");
            for (value, width) in [
                (7, 6),
                (ino_of(mem, i), 6),
                (u64::from(mem.mode), 6),
                (0, 6),
                (0, 6),
                (mem.nlink.max(1), 6),
                (0, 6),
                (1_700_000_000, 11),
                (name.len() as u64, 6),
                (mem.data.len() as u64, 11),
            ] {
                out.extend_from_slice(format!("{value:0width$o}").as_bytes());
            }
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(mem.data);
        }
        out
    }

    fn write_bin(members: &[M], big_endian: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let trailer = m(TRAILER, 0, b"");
        let short = |out: &mut Vec<u8>, value: u16| {
            out.extend_from_slice(&if big_endian {
                value.to_be_bytes()
            } else {
                value.to_le_bytes()
            });
        };
        for (i, mem) in members.iter().chain([&trailer]).enumerate() {
            let name = format!("{}\0", mem.name);
            let size = mem.data.len() as u32;
            short(&mut out, 0o070707);
            short(&mut out, 7);
            short(&mut out, ino_of(mem, i) as u16);
            short(&mut out, mem.mode as u16);
            short(&mut out, 0);
            short(&mut out, 0);
            short(&mut out, mem.nlink.max(1) as u16);
            short(&mut out, 0);
            // longs go high half first, whatever the order inside a half
            short(&mut out, (1_700_000_000u32 >> 16) as u16);
            short(&mut out, 1_700_000_000u32 as u16);
            short(&mut out, name.len() as u16);
            short(&mut out, (size >> 16) as u16);
            short(&mut out, size as u16);
            out.extend_from_slice(name.as_bytes());
            pad_to(&mut out, 2);
            out.extend_from_slice(mem.data);
            pad_to(&mut out, 2);
        }
        out
    }

    /// Every member with its bytes, so a fixture can be checked whole.
    fn drain(bytes: &[u8]) -> Vec<(Header, Vec<u8>)> {
        let mut reader = Reader::new(Cursor::new(bytes.to_vec()));
        let mut out = Vec::new();
        while let Some(header) = reader.next_member().unwrap() {
            let data = reader.data().unwrap();
            out.push((header, data));
        }
        out
    }

    fn fixture() -> Vec<M<'static>> {
        vec![
            m("dir", S_IFDIR | 0o755, b""),
            m("dir/hello.txt", S_IFREG | 0o644, b"hello cpio\n"),
            m("link", S_IFLNK | 0o777, b"dir/hello.txt"),
            m("odd-size.bin", S_IFREG | 0o600, b"abc"),
        ]
    }

    #[test]
    fn reads_all_three_header_formats() {
        let members = fixture();
        for (label, bytes) in [
            ("newc", write_newc(&members)),
            ("odc", write_odc(&members)),
            ("bin", write_bin(&members, false)),
            ("bin-swapped", write_bin(&members, true)),
        ] {
            let got = drain(&bytes);
            assert_eq!(got.len(), 4, "{label}");

            assert_eq!(got[0].0.path, Path::new("dir"), "{label}");
            assert!(got[0].0.is_dir(), "{label}");
            assert_eq!(got[0].0.perm(), 0o755, "{label}");
            assert_eq!(got[0].0.mtime, 1_700_000_000, "{label}");

            assert_eq!(got[1].0.path, Path::new("dir/hello.txt"), "{label}");
            assert!(got[1].0.is_file(), "{label}");
            assert_eq!(got[1].0.size, 11, "{label}");
            assert_eq!(got[1].1, b"hello cpio\n", "{label}");

            assert!(got[2].0.is_symlink(), "{label}");
            assert_eq!(got[2].1, b"dir/hello.txt", "{label}");

            // 3 bytes: the one length that needs padding in every format
            assert_eq!(got[3].1, b"abc", "{label}");
            assert_eq!(got[3].0.perm(), 0o600, "{label}");
        }
    }

    #[test]
    fn skips_over_data_the_caller_never_asked_for() {
        let bytes = write_newc(&fixture());
        let mut reader = Reader::new(Cursor::new(bytes));
        let mut names = Vec::new();
        while let Some(header) = reader.next_member().unwrap() {
            names.push(header.path.to_string_lossy().into_owned());
        }
        assert_eq!(names, ["dir", "dir/hello.txt", "link", "odd-size.bin"]);
    }

    #[test]
    fn stops_at_the_trailer_and_ignores_what_follows() {
        let mut bytes = write_newc(&[m("only.txt", S_IFREG | 0o644, b"x")]);
        bytes.extend_from_slice(&[0u8; 512]); // block padding after the trailer
        assert_eq!(drain(&bytes).len(), 1);
    }

    #[test]
    fn carries_the_identity_a_hard_link_shares() {
        let bytes = write_newc(&[
            M {
                name: "a.txt",
                mode: S_IFREG | 0o644,
                data: b"",
                nlink: 2,
                ino: 42,
            },
            M {
                name: "b.txt",
                mode: S_IFREG | 0o644,
                data: b"shared\n",
                nlink: 2,
                ino: 42,
            },
        ]);
        let got = drain(&bytes);
        assert_eq!(got[0].0.ino, got[1].0.ino);
        assert_eq!(got[0].0.dev, got[1].0.dev);
        assert_eq!(got[0].0.nlink, 2);
        assert_eq!(got[0].0.size, 0);
        assert_eq!(got[1].1, b"shared\n");
    }

    #[test]
    fn refuses_a_stream_that_is_not_cpio() {
        let mut reader = Reader::new(Cursor::new(b"not a cpio archive at all".to_vec()));
        assert!(reader.next_member().is_err());
    }

    #[test]
    fn refuses_a_truncated_header() {
        let mut bytes = write_newc(&[m("a.txt", S_IFREG | 0o644, b"data")]);
        bytes.truncate(40);
        let mut reader = Reader::new(Cursor::new(bytes));
        assert!(reader.next_member().is_err());
    }

    #[test]
    fn refuses_an_impossible_name_length() {
        let mut bytes = write_newc(&[m("a.txt", S_IFREG | 0o644, b"data")]);
        // namesize field: the twelfth 8-hex field after the magic
        bytes[6 + 11 * 8..6 + 12 * 8].copy_from_slice(b"7FFFFFFF");
        let mut reader = Reader::new(Cursor::new(bytes));
        assert!(reader.next_member().is_err());
    }

    #[test]
    fn number_tolerates_space_padding() {
        assert_eq!(number(b"0000000A", 16).unwrap(), 10);
        assert_eq!(number(b"     12 ", 8).unwrap(), 10);
        assert!(number(b"        ", 8).is_err());
        assert!(number(b"zzzzzzzz", 16).is_err());
    }
}
