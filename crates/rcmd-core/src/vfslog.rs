//! `rcmd -l FILE`: mc's ftpfs dialogue log, one line per thing said.
//!
//! A remote panel that will not list, or lists something odd, is nearly
//! always the server answering something the client did not expect, and
//! the only way to see that from outside is a transcript. The sink is a
//! process-wide file because the connections are pooled and threaded:
//! whoever is talking writes into the same log, tagged with which way
//! the line went.

use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

static SINK: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// Start logging into `path`, appending to whatever is there: two runs
/// against the same server in one session read as one story.
pub fn open(path: &Path) -> std::io::Result<()> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    *SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(file);
    line("---", "rcmd session start");
    Ok(())
}

/// Whether anything is listening - callers that would have to build the
/// text first ask this before paying for it.
pub fn is_on() -> bool {
    SINK.lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some()
}

/// One line of dialogue. `tag` is the direction: `>` sent, `<` received.
/// Control characters are spelled out so a stray byte cannot rearrange
/// the log it lands in.
pub fn line(tag: &str, text: &str) {
    let mut sink = SINK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(file) = sink.as_mut() else { return };
    let _ = writeln!(
        file,
        "{tag} {}",
        escape(text.trim_end_matches(['\r', '\n']))
    );
    let _ = file.flush();
}

fn escape(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            let escaped = match c {
                '\t' => Some("\\t"),
                '\r' => Some("\\r"),
                '\n' => Some("\\n"),
                c if (c as u32) < 0x20 || c == '\u{7f}' => Some("?"),
                _ => None,
            };
            match escaped {
                Some(text) => text.chars().collect::<Vec<_>>(),
                None => vec![c],
            }
        })
        .collect()
}

/// A password is dialogue too, but not the kind that belongs in a file
/// the user is about to paste into a bug report.
pub fn redact(line: &str) -> &str {
    match line.split(' ').next() {
        Some("PASS") => "PASS ***",
        _ => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_bytes_cannot_rearrange_the_log() {
        assert_eq!(escape("220 hi\tthere"), "220 hi\\tthere");
        assert_eq!(escape("a\u{1}b"), "a?b");
    }

    #[test]
    fn the_password_never_lands_in_the_file() {
        assert_eq!(redact("PASS hunter2"), "PASS ***");
        assert_eq!(redact("USER anonymous"), "USER anonymous");
        // not a prefix match: a filename starting with PASS is not one
        assert_eq!(redact("RETR PASSWORDS.txt"), "RETR PASSWORDS.txt");
    }
}
