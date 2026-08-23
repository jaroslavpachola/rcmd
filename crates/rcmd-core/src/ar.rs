//! The `ar` archive: the container a `.deb` rides in, and the one a
//! static library (`.a`) *is*. A flat list of members with 60-byte
//! ASCII headers, no compression and no directories.
//!
//! The file is on disk and seekable, so members are located rather than
//! copied: each one is recorded as an offset and a length, and reading
//! it means seeking there.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

const MAGIC: &[u8; 8] = b"!<arch>\n";
const HEADER: usize = 60;

/// One member, located inside the file rather than held in memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    /// Where the member's bytes start.
    pub at: u64,
    pub size: u64,
    pub mtime: u64,
    /// What the header says, which is the full `st_mode` - `ar` writes
    /// "100644", not "644".
    pub mode: u32,
}

/// Read the member table. Both housekeeping members - the symbol table
/// (`/` or `__.SYMDEF`) and the GNU long-name table (`//`) - are read
/// and then left out of the result: nothing browses them, and the long
/// names they hold have already been handed to the members that use
/// them.
pub fn members(path: &std::path::Path) -> io::Result<Vec<Member>> {
    let mut file = File::open(path)?;
    let mut magic = [0u8; 8];
    file.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not an ar archive",
        ));
    }

    let mut out = Vec::new();
    let mut names = Vec::new();
    let mut at = MAGIC.len() as u64;
    loop {
        let mut header = [0u8; HEADER];
        if !read_full(&mut file, &mut header)? {
            break;
        }
        if &header[58..60] != b"\x60\n" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ar member header is not terminated",
            ));
        }
        // what the header says the member's data is, before a BSD long
        // name eats the front of it
        let stored = number(&header[48..58], 10)?;
        let raw = String::from_utf8_lossy(&header[0..16])
            .trim_end()
            .to_string();
        let mut start = at + HEADER as u64;
        let mut size = stored;

        let name = if raw == "//" {
            names = vec![0u8; stored as usize];
            file.read_exact(&mut names)?;
            String::new()
        } else if raw == "/" || raw == "__.SYMDEF" || raw == "__.SYMDEF SORTED" {
            String::new()
        } else if let Some(index) = raw.strip_prefix("/").and_then(|d| d.parse::<usize>().ok()) {
            long_name(&names, index)
        } else if let Some(len) = raw.strip_prefix("#1/").and_then(|d| d.parse::<u64>().ok()) {
            // BSD keeps a long name in the first bytes of the data
            let mut buf = vec![0u8; len.min(size) as usize];
            file.read_exact(&mut buf)?;
            start += buf.len() as u64;
            size -= buf.len() as u64;
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            String::from_utf8_lossy(&buf[..end]).into_owned()
        } else {
            // GNU pads a short name with a trailing slash
            raw.strip_suffix('/').unwrap_or(&raw).to_string()
        };

        if !name.is_empty() {
            out.push(Member {
                name,
                at: start,
                size,
                mtime: number(&header[16..28], 10).unwrap_or(0),
                mode: number(&header[40..48], 8).unwrap_or(0o644) as u32,
            });
        }
        // members are padded to an even offset with a newline
        at += HEADER as u64 + stored + stored % 2;
        file.seek(SeekFrom::Start(at))?;
    }
    Ok(out)
}

/// A member's bytes, read from where the table said they were.
pub fn read_member(path: &std::path::Path, member: &Member) -> io::Result<Box<dyn Read + Send>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(member.at))?;
    Ok(Box::new(file.take(member.size)))
}

/// The GNU long-name table is one NUL- or newline-terminated name after
/// another, addressed by byte offset.
fn long_name(names: &[u8], index: usize) -> String {
    let rest = names.get(index..).unwrap_or_default();
    let end = rest
        .iter()
        .position(|b| *b == b'\n' || *b == 0)
        .unwrap_or(rest.len());
    let name = String::from_utf8_lossy(&rest[..end]).into_owned();
    name.strip_suffix('/').unwrap_or(&name).to_string()
}

/// `Ok(false)` when the archive ended cleanly at a member boundary.
fn read_full(file: &mut File, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..])? {
            0 if filled == 0 => return Ok(false),
            0 => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "ar member header ends early",
                ));
            }
            n => filled += n,
        }
    }
    Ok(true)
}

/// A fixed-width ASCII field, space padded on the right.
fn number(field: &[u8], radix: u32) -> io::Result<u64> {
    let text = String::from_utf8_lossy(field);
    u64::from_str_radix(text.trim(), radix)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ar field is not a number"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// An `ar` archive with the plain short-name headers a .deb uses.
    fn write_ar(dir: &std::path::Path, members: &[(&str, &[u8])]) -> std::path::PathBuf {
        let path = dir.join("box.a");
        let mut file = File::create(&path).unwrap();
        file.write_all(MAGIC).unwrap();
        for (name, data) in members {
            let header = format!(
                "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}\x60\n",
                name,
                1_700_000_000u64,
                0,
                0,
                "100644",
                data.len()
            );
            assert_eq!(header.len(), HEADER);
            file.write_all(header.as_bytes()).unwrap();
            file.write_all(data).unwrap();
            if data.len() % 2 == 1 {
                file.write_all(b"\n").unwrap();
            }
        }
        path
    }

    #[test]
    fn lists_members_and_finds_their_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        // the first member has an odd length, so the second only lands
        // where the table says if the padding byte is accounted for
        let path = write_ar(
            tmp.path(),
            &[
                ("debian-binary", b"2.0\n"),
                ("odd/", b"abc"),
                ("after/", b"last\n"),
            ],
        );
        let members = members(&path).unwrap();
        let names: Vec<_> = members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["debian-binary", "odd", "after"]);
        assert_eq!(members[0].mode & 0o7777, 0o644);
        assert_eq!(members[0].mtime, 1_700_000_000);

        let mut text = String::new();
        read_member(&path, &members[2])
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "last\n");
    }

    #[test]
    fn refuses_a_file_that_is_not_an_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("not.a");
        std::fs::write(&path, b"just some bytes here").unwrap();
        assert!(members(&path).is_err());
    }

    #[test]
    fn reads_gnu_long_names_from_the_name_table() {
        let tmp = tempfile::tempdir().unwrap();
        let table = b"a-very-long-member-name.o/\n";
        let path = write_ar(tmp.path(), &[("//", table), ("/0", b"body\n")]);
        let members = members(&path).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].name, "a-very-long-member-name.o");
        assert_eq!(members[0].size, 5);
    }

    #[test]
    fn reads_a_bsd_long_name_from_the_front_of_the_data() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_ar(tmp.path(), &[("#1/8", b"long.txtbody\n")]);
        let members = members(&path).unwrap();
        assert_eq!(members[0].name, "long.txt");
        assert_eq!(members[0].size, 5);
        let mut text = String::new();
        read_member(&path, &members[0])
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "body\n");
    }

    #[test]
    fn round_trips_against_the_system_ar() {
        if std::process::Command::new("ar")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .status()
            .is_err()
        {
            eprintln!("skipping: no ar binary to build the fixture");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("one.txt"), b"first\n").unwrap();
        std::fs::write(tmp.path().join("two.txt"), b"second\n").unwrap();
        let status = std::process::Command::new("ar")
            .args(["rc", "box.a", "one.txt", "two.txt"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        assert!(status.success());

        let path = tmp.path().join("box.a");
        let members = members(&path).unwrap();
        let names: Vec<_> = members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["one.txt", "two.txt"]);
        let mut text = String::new();
        read_member(&path, &members[1])
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "second\n");
    }
}
