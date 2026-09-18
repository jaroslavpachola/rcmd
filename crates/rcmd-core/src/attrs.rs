//! The file flags `lsattr` shows and `chattr` sets - append-only,
//! immutable, no-dump, no copy-on-write and the rest - read and written
//! through the ext2 ioctls that ext4, btrfs, xfs and f2fs all answer.
//! mc has had a dialog for them since 4.8.25.

use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

/// `_IOR('f', 1, long)` and `_IOW('f', 2, long)`; the kernel reads and
/// writes an int through them whatever the macro says.
const FS_IOC_GETFLAGS: libc::c_ulong = 0x8008_6601;
const FS_IOC_SETFLAGS: libc::c_ulong = 0x4008_6602;

/// The flags worth a checkbox, in `lsattr`'s letters: the letter, the
/// bit, and what it does.
pub const FLAGS: &[(char, u32, &str)] = &[
    ('a', 0x0000_0020, "append only"),
    ('i', 0x0000_0010, "immutable"),
    ('d', 0x0000_0040, "no dump"),
    ('A', 0x0000_0080, "no atime updates"),
    ('S', 0x0000_0008, "synchronous updates"),
    ('D', 0x0001_0000, "synchronous directory updates"),
    ('c', 0x0000_0004, "compressed"),
    ('C', 0x0080_0000, "no copy on write"),
    ('s', 0x0000_0001, "secure deletion"),
    ('u', 0x0000_0002, "undeletable"),
    ('j', 0x0000_4000, "data journalling"),
    ('t', 0x0000_8000, "no tail merging"),
    ('T', 0x0002_0000, "top of directory hierarchy"),
];

/// The file's flags. A FIFO or a device is opened without blocking; a
/// filesystem that has no such flags says so as an error.
pub fn get(path: &Path) -> io::Result<u32> {
    let file = open(path)?;
    let mut flags: libc::c_int = 0;
    if unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_GETFLAGS, &mut flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(flags as u32)
}

/// Set the file's flags. Some need root (`i`, `a`); the error says so.
pub fn set(path: &Path, flags: u32) -> io::Result<()> {
    let file = open(path)?;
    let flags = flags as libc::c_int;
    if unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_SETFLAGS, &flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn open(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
}

/// The extended attributes a file carries, by name - `user.*`,
/// `security.selinux`, and the POSIX ACLs, which live here as
/// `system.posix_acl_access` and `system.posix_acl_default`.
pub fn xattr_names(path: &Path) -> Vec<String> {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return Vec::new();
    };
    let size = unsafe { libc::llistxattr(c.as_ptr(), std::ptr::null_mut(), 0) };
    if size <= 0 {
        return Vec::new();
    }
    let mut buf = vec![0u8; size as usize];
    let got = unsafe { libc::llistxattr(c.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    if got <= 0 {
        return Vec::new();
    }
    buf[..got as usize]
        .split(|b| *b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect()
}

/// The flags as `lsattr` writes them: a letter where one is set, a dash
/// where it is not, in [`FLAGS`]' order.
pub fn letters(flags: u32) -> String {
    FLAGS
        .iter()
        .map(|&(letter, bit, _)| if flags & bit != 0 { letter } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extended_attributes_are_listed_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f");
        std::fs::write(&path, b"x").unwrap();
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        let set = unsafe {
            libc::setxattr(
                c.as_ptr(),
                c"user.rcmd".as_ptr(),
                b"1".as_ptr().cast(),
                1,
                0,
            )
        };
        if set != 0 {
            return; // this filesystem takes no user xattrs
        }
        assert!(xattr_names(&path).contains(&"user.rcmd".to_string()));
    }

    #[test]
    fn a_flag_set_is_a_flag_read_back() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f");
        std::fs::write(&path, b"x").unwrap();
        let Ok(before) = get(&path) else {
            return; // this filesystem has no such flags
        };
        let nodump = 0x40;
        set(&path, before | nodump).unwrap();
        assert_ne!(get(&path).unwrap() & nodump, 0);
        assert!(letters(get(&path).unwrap()).contains('d'));
        set(&path, before).unwrap();
        assert_eq!(get(&path).unwrap() & nodump, 0);
    }
}
