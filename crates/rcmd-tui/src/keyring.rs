//! Passwords in the desktop's keyring, or nowhere. rcmd never writes a
//! password to a file of its own: a saved connection that wants one
//! kept keeps it where the desktop keeps the rest - the Secret Service
//! through `secret-tool` on a Linux desktop, the login keychain through
//! `security` on a Mac. Without either, a password is asked for every
//! time, which is the honest alternative to a plain-text file.

use std::io::Write;
use std::process::{Command, Stdio};

/// What the keyring calls rcmd's entries.
const SERVICE: &str = "rcmd";

/// A password kept for `account`, if the keyring has one.
pub fn lookup(account: &str) -> Option<String> {
    let out = if cfg!(target_os = "macos") {
        Command::new("security")
            .args(["find-generic-password", "-s", SERVICE, "-a", account, "-w"])
            .stderr(Stdio::null())
            .output()
    } else {
        Command::new("secret-tool")
            .args(["lookup", "service", SERVICE, "account", account])
            .stderr(Stdio::null())
            .output()
    }
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let secret = String::from_utf8(out.stdout).ok()?;
    let secret = secret.strip_suffix('\n').unwrap_or(&secret).to_string();
    (!secret.is_empty()).then_some(secret)
}

/// Keep a password for `account`. False = there is no keyring to keep
/// it in, or it refused.
pub fn store(account: &str, secret: &str) -> bool {
    if cfg!(target_os = "macos") {
        // `security` takes the secret on its command line; it has no
        // other way in that does not need a terminal
        return Command::new("security")
            .args([
                "add-generic-password",
                "-U",
                "-s",
                SERVICE,
                "-a",
                account,
                "-w",
                secret,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
    }
    // the secret goes in on stdin, never on a command line `ps` shows
    let child = Command::new("secret-tool")
        .args([
            "store",
            "--label",
            &format!("rcmd: {account}"),
            "service",
            SERVICE,
            "account",
            account,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(secret.as_bytes());
    }
    child.wait().is_ok_and(|s| s.success())
}

/// Drop what is kept for `account`.
pub fn forget(account: &str) {
    let _ = if cfg!(target_os = "macos") {
        Command::new("security")
            .args(["delete-generic-password", "-s", SERVICE, "-a", account])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    } else {
        Command::new("secret-tool")
            .args(["clear", "service", SERVICE, "account", account])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    };
}
