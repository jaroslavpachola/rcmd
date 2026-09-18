//! What `~/.ssh/config` says about a host, so that `sftp://box` reaches
//! the machine `ssh box` does: the name to dial, the user, the port and
//! the keys. mc's sftpfs reads the same four keywords; `Include`,
//! `Match` and `ProxyJump` are beyond it, and beyond this.
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
}

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

/// The settings for `alias` in the text of a config file.
pub fn parse(text: &str, alias: &str, home: &Path) -> HostConfig {
    let mut out = HostConfig::default();
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
            _ => {}
        }
    }
    out
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
}
