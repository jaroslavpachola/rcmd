//! Running a command from a window.
//!
//! This is the one place where the two front ends genuinely cannot do
//! the same thing. The terminal build leaves the alternate screen and
//! hands the real tty to the child: `less` pages, `make` scrolls, and
//! Ctrl+O drops you into a shell that is still there when you come
//! back. There is no tty behind a window to hand over, so:
//!
//! * `Exec::Quiet` - the openers and the `[[open]]` rules, which rcmd
//!   already documents as wanting a trailing `&` for GUI programs - is
//!   spawned detached, which is exactly right here and needs no tty.
//! * `Exec::Command` and `Exec::Shell` want a terminal, so one is
//!   asked for: `$TERMINAL` first, then the usual emulators. Without
//!   one the command still runs, detached and silent, and says so.
//!
//! The honest alternative is a terminal emulator inside the window, and
//! that is a bigger thing than this crate: rcmd's subshell already owns
//! a pty (`libc::openpty`) but pumps its bytes straight at the terminal
//! rather than interpreting them, so rendering it means implementing
//! the interpreting half. Until then this build runs with the subshell
//! switched off, and Ctrl+O has nothing to show.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Result, bail};
use rcmd_tui::app::Exec;

/// Terminal emulators worth trying, and the flag each one takes before
/// a command. `-e` is near-universal; the exceptions are listed by name.
const TERMINALS: &[(&str, &str)] = &[
    ("x-terminal-emulator", "-e"),
    ("alacritty", "-e"),
    ("kitty", "-e"),
    ("wezterm", "-e"),
    ("foot", "-e"),
    ("ghostty", "-e"),
    ("konsole", "-e"),
    ("gnome-terminal", "--"),
    ("xfce4-terminal", "-x"),
    ("urxvt", "-e"),
    ("xterm", "-e"),
];

/// Run what a key press queued. `Ok(Some(note))` is something worth
/// putting on the status line.
pub fn run(exec: &Exec, cwd: &Path) -> Result<Option<String>> {
    match exec {
        // no pause, no terminal wanted: this is the opener path, and a
        // detached child is what an opener has always been
        Exec::Quiet(cmd) => {
            spawn_detached(cmd, cwd)?;
            Ok(None)
        }
        Exec::Command(cmd) => match terminal_for(cmd) {
            Some((program, args)) => {
                spawn_in(&program, &args, cwd)?;
                Ok(None)
            }
            None => {
                spawn_detached(cmd, cwd)?;
                Ok(Some(
                    "no terminal emulator found - ran it detached, with no output".into(),
                ))
            }
        },
        Exec::Shell => match terminal_for("") {
            Some((program, args)) => {
                spawn_in(&program, &args, cwd)?;
                Ok(None)
            }
            None => bail!("no terminal emulator found (set $TERMINAL)"),
        },
    }
}

/// The emulator to use and the arguments to give it. An empty `cmd`
/// asks for a plain interactive shell.
fn terminal_for(cmd: &str) -> Option<(String, Vec<String>)> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    // The command runs, then the shell stays: without the second half
    // a window that only prints an error closes before it can be read -
    // this is the window's answer to the terminal build's "press Enter"
    let script = match cmd.is_empty() {
        true => None,
        false => Some(format!(
            "{cmd}; printf '\\n[rmut] press Enter to close '; read _",
        )),
    };

    let mut candidates: Vec<(String, String)> = Vec::new();
    if let Ok(preferred) = std::env::var("TERMINAL")
        && !preferred.is_empty()
    {
        candidates.push((preferred, "-e".into()));
    }
    candidates.extend(
        TERMINALS
            .iter()
            .map(|(name, flag)| ((*name).to_string(), (*flag).to_string())),
    );

    let (program, flag) = candidates.into_iter().find(|(name, _)| which(name))?;
    let args = match &script {
        Some(script) => vec![flag, shell, "-c".into(), script.clone()],
        None => vec![flag, shell],
    };
    Some((program, args))
}

/// Is `name` on the PATH (or an absolute path that exists)?
fn which(name: &str) -> bool {
    let path = Path::new(name);
    if path.is_absolute() {
        return path.is_file();
    }
    let Ok(var) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&var).any(|dir| dir.join(name).is_file())
}

fn spawn_in(program: &str, args: &[String], cwd: &Path) -> Result<()> {
    Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

fn spawn_detached(cmd: &str, cwd: &Path) -> Result<()> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    spawn_in(&shell, &["-c".to_string(), cmd.to_string()], cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_gets_a_pause_and_a_shell_does_not() {
        // whichever emulator this machine has, the shape is the same
        if let Some((_, args)) = terminal_for("ls") {
            let script = args.last().expect("a script");
            assert!(script.starts_with("ls;"));
            assert!(script.contains("press Enter"));
        }
        if let Some((_, args)) = terminal_for("") {
            assert!(!args.iter().any(|a| a == "-c"));
        }
    }

    #[test]
    fn which_finds_what_is_there_and_not_what_is_not() {
        assert!(which("sh"));
        assert!(!which("a-program-nobody-has-installed-here"));
        assert!(which("/bin/sh") || which("/usr/bin/sh"));
        assert!(!which("/nonexistent/binary"));
    }
}
