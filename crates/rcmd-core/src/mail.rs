//! An mbox, browsed as the messages in it. Each entry is one message -
//! headers and body, without the `From ` separator line the mbox format
//! puts between them - so opening one gives you an ordinary RFC 822
//! message rather than a fragment.
//!
//! Names come from the `Subject:` header, which in real mail is often
//! RFC 2047 encoded (`=?UTF-8?B?...?=`); a listing full of that is no
//! listing at all, so encoded words are decoded.

use std::path::PathBuf;

/// One message: where it starts in the mbox and how far it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// The name a panel shows: an index and the decoded subject.
    pub name: PathBuf,
    pub at: usize,
    pub len: usize,
    pub subject: String,
    pub from: String,
    pub date: String,
}

/// Split an mbox into messages. Returns empty for anything that is not
/// one, which is how the caller knows to refuse the file.
pub fn split(text: &str) -> Vec<Message> {
    let mut starts = Vec::new();
    let mut at = 0usize;
    let mut previous_blank = true; // the first line may be a separator
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        if previous_blank && is_separator(body) {
            starts.push(at);
        }
        previous_blank = body.is_empty();
        at += line.len();
    }

    let mut out = Vec::new();
    for (index, start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(text.len());
        // the "From " separator belongs to the mbox, not the message
        let body_at = text[*start..end]
            .find('\n')
            .map(|n| start + n + 1)
            .unwrap_or(end);
        if body_at >= end {
            continue;
        }
        let raw = &text[body_at..end];
        let subject = decode(&header(raw, "subject").unwrap_or_default());
        let from = decode(&header(raw, "from").unwrap_or_default());
        let date = header(raw, "date").unwrap_or_default();
        out.push(Message {
            name: PathBuf::from(name_for(index + 1, &subject)),
            at: body_at,
            len: end - body_at,
            subject,
            from,
            date,
        });
    }
    out
}

/// An mbox separates messages with `From sender date`, and a body line
/// that happens to begin "From " is supposed to be escaped - but plenty
/// of mail is not. Requiring a sender with no spaces in it and a
/// four-digit year further along tells the two apart without needing
/// the escaping to have happened.
fn is_separator(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("From ") else {
        return false;
    };
    let mut tokens = rest.split_whitespace();
    let Some(sender) = tokens.next() else {
        return false;
    };
    if sender.is_empty() {
        return false;
    }
    tokens.any(|token| {
        token.len() == 4
            && token.chars().all(|c| c.is_ascii_digit())
            && (1900..2200).contains(&token.parse::<u32>().unwrap_or(0))
    })
}

/// "0001 Re: the thing" - numbered so the panel's name order is the
/// mailbox's order, and sanitised so a subject with a slash in it stays
/// one entry.
fn name_for(index: usize, subject: &str) -> String {
    let cleaned: String = subject
        .chars()
        .map(|c| if c == '/' || c < ' ' { ' ' } else { c })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let cleaned = if cleaned.chars().count() > 60 {
        cleaned.chars().take(60).collect()
    } else {
        cleaned
    };
    if cleaned.is_empty() {
        format!("{index:04} (no subject)")
    } else {
        format!("{index:04} {cleaned}")
    }
}

/// One header's value, continuation lines folded back in. `name` is
/// lower case; header names are not case sensitive.
fn header(message: &str, name: &str) -> Option<String> {
    let mut value: Option<String> = None;
    for line in message.lines() {
        if line.is_empty() {
            break; // headers end at the first blank line
        }
        if line.starts_with([' ', '\t']) {
            // a folded continuation of the header before it
            if let Some(value) = value.as_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if value.is_some() {
            break; // a new header started: the one we wanted is done
        }
        if let Some((key, rest)) = line.split_once(':')
            && key.trim().eq_ignore_ascii_case(name)
        {
            value = Some(rest.trim().to_string());
        }
    }
    value
}

/// RFC 2047 encoded words: `=?charset?B?base64?=` or `=?charset?Q?qp?=`.
/// Anything that is not one is passed through untouched.
pub fn decode(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("=?") {
        let (before, tail) = rest.split_at(start);
        let Some(end) = tail[2..].find("?=") else {
            break;
        };
        let word = &tail[2..2 + end];
        let mut parts = word.splitn(3, '?');
        let decoded = match (parts.next(), parts.next(), parts.next()) {
            (Some(charset), Some(encoding), Some(payload)) => {
                match encoding.to_ascii_uppercase().as_str() {
                    "B" => base64(payload).map(|bytes| to_text(charset, bytes)),
                    "Q" => Some(to_text(charset, quoted_printable(payload, true))),
                    _ => None,
                }
            }
            _ => None,
        };
        match decoded {
            Some(text) => {
                // whitespace between two encoded words is not content
                if !(out.ends_with(|c: char| !c.is_whitespace()) && before.trim().is_empty()) {
                    out.push_str(before);
                }
                out.push_str(&text);
            }
            None => {
                out.push_str(before);
                out.push_str("=?");
                out.push_str(word);
                out.push_str("?=");
            }
        }
        rest = &tail[2 + end + 2..];
    }
    out.push_str(rest);
    out
}

/// Bytes to a string, for the charsets mail actually uses. Anything
/// else is read as Latin-1, which never fails and never invents a
/// character that was not there.
fn to_text(charset: &str, bytes: Vec<u8>) -> String {
    match charset.to_ascii_uppercase().as_str() {
        "UTF-8" | "UTF8" | "US-ASCII" | "ASCII" => String::from_utf8_lossy(&bytes).into_owned(),
        _ => bytes.iter().map(|b| *b as char).collect(),
    }
}

fn base64(text: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut have = 0u32;
    let mut out = Vec::new();
    for c in text.bytes() {
        let value = match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\r' | b'\n' => continue,
            _ => return None,
        };
        bits = bits << 6 | value;
        have += 6;
        if have >= 8 {
            have -= 8;
            out.push((bits >> have) as u8);
        }
    }
    Some(out)
}

/// Quoted-printable. In an encoded word `_` stands for a space; in a
/// message body it does not.
fn quoted_printable(text: &str, underscore_is_space: bool) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'_' if underscore_is_space => out.push(b' '),
            b'=' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 2;
                    }
                    Err(_) => out.push(b'='),
                }
            }
            byte => out.push(byte),
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MBOX: &str = "\
From alice@example.com Mon Aug 23 10:00:00 2026
From: Alice <alice@example.com>
To: bob@example.com
Subject: the first message
Date: Mon, 23 Aug 2026 10:00:00 +0000

Hello Bob,
this is the body.

From bob@example.com Mon Aug 23 11:00:00 2026
From: Bob <bob@example.com>
Subject: =?UTF-8?B?w6FydsOtem7DrWs=?=
Date: Mon, 23 Aug 2026 11:00:00 +0000

A reply, with a From-looking line in it:
From here on it is just text.
";

    #[test]
    fn splits_an_mbox_into_its_messages() {
        let messages = split(MBOX);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].name, PathBuf::from("0001 the first message"));
        assert_eq!(messages[0].from, "Alice <alice@example.com>");
        assert_eq!(messages[0].date, "Mon, 23 Aug 2026 10:00:00 +0000");

        // the separator line is the mbox's, not the message's
        let body = &MBOX[messages[0].at..messages[0].at + messages[0].len];
        assert!(body.starts_with("From: Alice"), "{body}");
        assert!(body.contains("this is the body."));
        assert!(!body.contains("Bob <bob@"));
    }

    #[test]
    fn a_from_line_inside_a_body_does_not_start_a_message() {
        let messages = split(MBOX);
        let second = &MBOX[messages[1].at..messages[1].at + messages[1].len];
        assert!(second.contains("From here on it is just text."), "{second}");
    }

    #[test]
    fn an_unescaped_from_line_after_a_blank_is_still_body_text() {
        // mail agents are supposed to escape this and often do not: a
        // separator has a sender and a year, and a sentence has neither
        let text = "\
From x@example Mon Aug 23 10:00:00 2026
Subject: a warning

Do not do this:

From now on nothing works.
";
        let messages = split(text);
        assert_eq!(messages.len(), 1);
        let body = &text[messages[0].at..messages[0].at + messages[0].len];
        assert!(body.contains("From now on nothing works."), "{body}");
    }

    #[test]
    fn the_separator_rule_wants_a_sender_and_a_year() {
        assert!(is_separator(
            "From alice@example.com Mon Aug 23 10:00:00 2026"
        ));
        assert!(is_separator("From alice Fri Jan  1 00:00:00 1999"));
        assert!(!is_separator("From here on it is just text."));
        assert!(!is_separator("From: Alice <alice@example.com>"));
        assert!(!is_separator("From "));
        assert!(!is_separator("Subject: From 2026 onwards"));
    }

    #[test]
    fn an_encoded_subject_becomes_readable() {
        let messages = split(MBOX);
        assert_eq!(messages[1].subject, "árvízník");
        assert_eq!(messages[1].name, PathBuf::from("0002 árvízník"));
    }

    #[test]
    fn folded_headers_are_read_as_one_value() {
        let text = "\
From x@example Mon Aug 23 10:00:00 2026
Subject: a subject that runs
 onto a second line
From: x@example

body
";
        let messages = split(text);
        assert_eq!(
            messages[0].subject,
            "a subject that runs onto a second line"
        );
        assert_eq!(messages[0].from, "x@example");
    }

    #[test]
    fn a_message_with_no_subject_is_still_named() {
        let text = "From x@example Mon Aug 23 10:00:00 2026\nFrom: x\n\nbody\n";
        assert_eq!(split(text)[0].name, PathBuf::from("0001 (no subject)"));
    }

    #[test]
    fn a_slash_in_a_subject_does_not_become_a_directory() {
        let text = "From x@e Mon Aug 23 10:00:00 2026\nSubject: one/two\n\nbody\n";
        assert_eq!(split(text)[0].name, PathBuf::from("0001 one two"));
    }

    #[test]
    fn text_that_is_not_an_mbox_yields_nothing() {
        assert!(split("just some notes\n").is_empty());
        assert!(split("").is_empty());
    }

    #[test]
    fn decodes_both_encoded_word_forms() {
        assert_eq!(decode("=?UTF-8?B?aGVsbG8=?="), "hello");
        assert_eq!(decode("=?utf-8?q?a_b=C3=A9?="), "a bé");
        assert_eq!(decode("=?ISO-8859-1?Q?caf=E9?="), "café");
        // the whitespace between two encoded words is not content
        assert_eq!(decode("=?UTF-8?B?YQ==?= =?UTF-8?B?Yg==?="), "ab");
        // plain text, and a broken word, are passed through
        assert_eq!(decode("plain subject"), "plain subject");
        assert_eq!(decode("=?UTF-8?X?zz?="), "=?UTF-8?X?zz?=");
        assert_eq!(decode("prefix =?UTF-8?B?aGk=?= suffix"), "prefix hi suffix");
    }
}
