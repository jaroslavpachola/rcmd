//! What `~/.ssh/config` says about a host, so that `sftp://box` reaches
//! the machine `ssh box` does: the name to dial, the user, the port, the
//! keys, and the way there when it is not direct - `ProxyJump` and
//! `ProxyCommand`. `Include` pulls other files in, as ssh does. mc's
//! sftpfs reads four of these keywords; `Match` is beyond both.
//!
//! OpenSSH's rules: keywords are case-insensitive and take `Key value`
//! or `Key=value`; for each keyword the first value found wins, reading
//! top to bottom through every `Host` block that matches - except
//! `IdentityFile`, whose values all count, in order.

use std::path::{Path, PathBuf};

/// The settings that apply to one host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostConfig {
    /// `HostName`: the address to dial, `%h` already the alias.
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    /// `IdentityFile`s, `~` expanded; `%` tokens are left for the caller,
    /// which knows the user.
    pub identities: Vec<String>,
    /// `ProxyJump`: the hosts to go through, as ssh's `-J` takes them.
    /// `none` switches it off, and is kept, so a later value cannot win.
    pub proxy_jump: Option<String>,
    /// `ProxyCommand`, its `%` tokens not yet filled in; `none` as above.
    pub proxy_command: Option<String>,
}

/// How deep `Include`s may nest, as in OpenSSH: a file that includes
/// itself stops here rather than going round for ever.
const MAX_INCLUDE_DEPTH: usize = 16;

/// The settings for `alias` in the user's own `~/.ssh/config`.
pub fn lookup(alias: &str) -> HostConfig {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return HostConfig::default();
    };
    match std::fs::read_to_string(home.join(".ssh/config")) {
        Ok(text) => parse(&text, alias, &home),
        Err(_) => HostConfig::default(),
    }
}

/// The settings for `alias` in the text of a config file. An `Include`
/// is read from the disk, relative to `~/.ssh`.
pub fn parse(text: &str, alias: &str, home: &Path) -> HostConfig {
    let mut out = HostConfig::default();
    read(text, alias, home, &mut out, 0);
    out
}

/// One file's lines into `out`. Its `Host` blocks are its own: an
/// included file starts with everything applying and hands back to the
/// block that included it, which is how ssh reads one.
fn read(text: &str, alias: &str, home: &Path, out: &mut HostConfig, depth: usize) {
    // before the first Host line, everything applies
    let mut applies = true;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match line.split_once(|c: char| c == '=' || c.is_whitespace()) {
            Some((key, value)) => (key.trim(), value.trim().trim_start_matches('=').trim()),
            None => continue,
        };
        let value = value.trim_matches('"');
        match key.to_ascii_lowercase().as_str() {
            "host" => applies = host_matches(value, alias),
            // a Match block's conditions are more than this reads; its
            // settings are skipped rather than applied to every host
            "match" => applies = false,
            _ if !applies => {}
            "hostname" if out.hostname.is_none() => {
                out.hostname = Some(value.replace("%h", alias));
            }
            "user" if out.user.is_none() => out.user = Some(value.to_string()),
            "port" if out.port.is_none() => out.port = value.parse().ok(),
            "identityfile" => out.identities.push(expand_home(value, home)),
            "proxyjump" if out.proxy_jump.is_none() => out.proxy_jump = Some(value.to_string()),
            "proxycommand" if out.proxy_command.is_none() => {
                out.proxy_command = Some(value.to_string());
            }
            "include" if depth < MAX_INCLUDE_DEPTH => {
                for file in value.split_whitespace().flat_map(|p| included(p, home)) {
                    if let Ok(text) = std::fs::read_to_string(&file) {
                        read(&text, alias, home, out, depth + 1);
                    }
                }
            }
            _ => {}
        }
    }
}

/// The files an `Include` names: `~` expanded, a relative path taken
/// from `~/.ssh`, and a wildcard in the file name matched in its
/// directory, the matches in name order.
fn included(pattern: &str, home: &Path) -> Vec<PathBuf> {
    let path = PathBuf::from(expand_home(pattern.trim_matches('"'), home));
    let path = match path.is_absolute() {
        true => path,
        false => home.join(".ssh").join(path),
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !name.contains(['*', '?']) {
        return vec![path];
    }
    let Some(dir) = path.parent() else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| crate::glob::glob_match(&name, &e.file_name().to_string_lossy()))
        .map(|e| e.path())
        .collect();
    found.sort();
    found
}

/// Whether a `Host` line's patterns take in `alias`: any positive
/// pattern matching, and no negated one (`!pattern`).
fn host_matches(patterns: &str, alias: &str) -> bool {
    let alias = alias.to_ascii_lowercase();
    let mut matched = false;
    for pattern in patterns.split_whitespace() {
        let (negated, pattern) = match pattern.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, pattern),
        };
        if crate::glob::glob_match(&pattern.to_ascii_lowercase(), &alias) {
            if negated {
                return false;
            }
            matched = true;
        }
    }
    matched
}

fn expand_home(path: &str, home: &Path) -> String {
    let path = path.replace("%d", &home.to_string_lossy());
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest).to_string_lossy().into_owned(),
        None => path,
    }
}

/// An `IdentityFile` with the tokens that need the connection filled in:
/// `%h` the alias, `%r` the remote user, `%u` the local one.
pub fn identity_path(template: &str, alias: &str, user: &str) -> PathBuf {
    let local = std::env::var("USER").unwrap_or_default();
    PathBuf::from(
        template
            .replace("%h", alias)
            .replace("%r", user)
            .replace("%u", &local),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "\
# the top applies to all
IdentityFile ~/.ssh/common

Host box work-*
    HostName %h.example.com
    User alice
    Port 2222
    IdentityFile ~/.ssh/box_key

Host box
    User ignored-first-one-won

Host * !secret
    User=everyone
    IdentityFile %d/.ssh/fallback

Match host box
    User from-a-match
";

    #[test]
    fn a_host_gets_the_first_value_of_each_and_every_identity() {
        let home = Path::new("/home/me");
        let cfg = parse(CONFIG, "box", home);
        assert_eq!(cfg.hostname.as_deref(), Some("box.example.com"));
        assert_eq!(cfg.user.as_deref(), Some("alice"));
        assert_eq!(cfg.port, Some(2222));
        assert_eq!(
            cfg.identities,
            [
                "/home/me/.ssh/common",
                "/home/me/.ssh/box_key",
                "/home/me/.ssh/fallback"
            ]
        );
    }

    #[test]
    fn patterns_and_negations() {
        let home = Path::new("/home/me");
        assert_eq!(parse(CONFIG, "work-db", home).port, Some(2222));
        assert_eq!(
            parse(CONFIG, "other", home).user.as_deref(),
            Some("everyone")
        );
        assert_eq!(parse(CONFIG, "secret", home).user, None);
        assert_eq!(parse(CONFIG, "BOX", home).user.as_deref(), Some("alice"));
    }

    #[test]
    fn identity_tokens() {
        assert_eq!(
            identity_path("/k/%r@%h", "box", "alice"),
            PathBuf::from("/k/alice@box")
        );
    }

    #[test]
    fn include_reads_other_files_where_it_stands() {
        let home = tempfile::tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        std::fs::create_dir_all(ssh.join("config.d")).unwrap();
        std::fs::write(
            ssh.join("config.d/10-work"),
            "Host work\n    HostName work.example.com\n    ProxyJump gate\n",
        )
        .unwrap();
        std::fs::write(
            ssh.join("config.d/20-lab"),
            "Host lab\n    ProxyCommand nc %h %p\n",
        )
        .unwrap();
        std::fs::write(ssh.join("config.d/notes.txt"), "Host work\n    User nope\n").unwrap();
        // includes itself: the depth limit stops it
        std::fs::write(ssh.join("loop"), "Include loop\nUser looped\n").unwrap();
        let config = "\
Include config.d/*-*
Host work
    User alice
Host other
    Include ~/.ssh/loop
";
        let work = parse(config, "work", home.path());
        assert_eq!(work.hostname.as_deref(), Some("work.example.com"));
        assert_eq!(work.proxy_jump.as_deref(), Some("gate"));
        assert_eq!(work.user.as_deref(), Some("alice"));
        let lab = parse(config, "lab", home.path());
        assert_eq!(lab.proxy_command.as_deref(), Some("nc %h %p"));
        assert_eq!(lab.proxy_jump, None);
        // an Include inside a block that does not apply is not read
        assert_eq!(parse(config, "lab", home.path()).user, None);
        assert_eq!(
            parse(config, "other", home.path()).user.as_deref(),
            Some("looped")
        );
    }

    #[test]
    fn a_proxy_set_to_none_stays_none() {
        let config = "Host box\n    ProxyJump none\nHost *\n    ProxyJump gate\n";
        let home = Path::new("/nonexistent");
        assert_eq!(
            parse(config, "box", home).proxy_jump.as_deref(),
            Some("none")
        );
        assert_eq!(
            parse(config, "else", home).proxy_jump.as_deref(),
            Some("gate")
        );
    }
}
