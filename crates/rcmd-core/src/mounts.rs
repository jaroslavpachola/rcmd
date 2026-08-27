//! What is mounted, and how much room is left on it. `df -P` is the one
//! answer that is the same on Linux and macOS - `/proc/mounts` is not
//! there on the second, `getmntinfo` is not there on the first, and
//! neither of them carries the free space without a second call. One
//! process, asked when the list is opened, is cheaper than two code
//! paths that can disagree.

use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// What is mounted: a device, or whatever the filesystem calls
    /// itself (`tmpfs`, `proc`).
    pub source: String,
    /// Where it is mounted.
    pub point: String,
    /// Bytes free and bytes in total; both 0 where `df` said nothing
    /// useful, which is what a pseudo-filesystem says.
    pub free: u64,
    pub total: u64,
}

/// Everything mounted, in the order `df` lists it, deduplicated by
/// mount point. An empty list means `df` could not be run at all, which
/// is not worth an error of its own: the list simply has no mounts in
/// it.
pub fn mounts() -> Vec<Mount> {
    let out = match Command::new("df").args(["-P", "-k"]).output() {
        Ok(out) => out,
        Err(_) => return Vec::new(),
    };
    parse_df(&String::from_utf8_lossy(&out.stdout))
}

/// `df -P` promises one line per filesystem and the mount point last,
/// which is the only reason a mount point with a space in it survives
/// this.
fn parse_df(text: &str) -> Vec<Mount> {
    let mut out: Vec<Mount> = Vec::new();
    for line in text.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 6 {
            continue;
        }
        // 1K blocks, as -k asked for
        let kb = |at: usize| fields[at].parse::<u64>().unwrap_or(0) * 1024;
        let point = fields[5..].join(" ");
        if out.iter().any(|m| m.point == point) {
            continue;
        }
        out.push(Mount {
            source: fields[0].to_string(),
            point,
            total: kb(1),
            free: kb(3),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn df_output_becomes_mounts() {
        let text = "Filesystem     1024-blocks      Used Available Capacity Mounted on\n\
                    /dev/sda2         98559220  60125148  33395356      65% /\n\
                    tmpfs                16384         0     16384       0% /dev/shm\n\
                    /dev/sdb1          1048576    524288    524288      50% /media/my disk\n\
                    proc                     0         0         0        - /proc\n";
        let mounts = parse_df(text);
        assert_eq!(mounts.len(), 4);
        assert_eq!(mounts[0].point, "/");
        assert_eq!(mounts[0].source, "/dev/sda2");
        assert_eq!(mounts[0].free, 33_395_356 * 1024);
        // the mount point is what is left of the line, spaces and all
        assert_eq!(mounts[2].point, "/media/my disk");
        // a pseudo-filesystem says nothing about room, and that is not
        // a parse failure
        assert_eq!(mounts[3].total, 0);
    }

    #[test]
    fn a_mount_point_is_listed_once() {
        let text = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                    /dev/sda1 100 50 50 50% /boot\n\
                    /dev/sda1 100 50 50 50% /boot\n";
        assert_eq!(parse_df(text).len(), 1);
    }

    #[test]
    fn the_real_system_has_a_root() {
        // the one thing every unix agrees on
        let mounts = mounts();
        assert!(
            mounts.is_empty() || mounts.iter().any(|m| m.point == "/"),
            "{mounts:?}"
        );
    }
}
