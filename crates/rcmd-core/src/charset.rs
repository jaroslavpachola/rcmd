//! The codepages a file can be read in - mc's "Select codepage", which
//! it needs because a file is bytes and nothing in it says what those
//! bytes mean. UTF-8 is what everything is now; the rest is what
//! everything was, and a file written in 1998 has not changed since.

pub use encoding_rs::Encoding;

/// The list offered in the pickers, in mc's order: the one everything
/// is in, then the western European sets, then Cyrillic, then the East
/// Asian multi-byte ones. Labels are the names people know them by
/// rather than the WHATWG spellings. The set is the one every browser
/// implements, which is a couple of DOS codepages short of mc's - a
/// list of what can actually be decoded beats a longer one where some
/// rows do nothing.
pub const CHARSETS: &[(&str, &str)] = &[
    ("UTF-8", "utf-8"),
    // one row, because they are one encoding: the standard everything
    // implements says iso-8859-1 means windows-1252, and a picker with
    // both would be offering the same thing twice
    ("CP1252 / ISO-8859-1 (Western)", "windows-1252"),
    ("ISO-8859-2 (Latin-2)", "iso-8859-2"),
    ("ISO-8859-5 (Cyrillic)", "iso-8859-5"),
    ("ISO-8859-7 (Greek)", "iso-8859-7"),
    ("ISO-8859-15 (Latin-9)", "iso-8859-15"),
    ("CP1250 (Central European)", "windows-1250"),
    ("CP1251 (Cyrillic)", "windows-1251"),
    ("CP1253 (Greek)", "windows-1253"),
    ("CP1257 (Baltic)", "windows-1257"),
    ("CP866 (DOS Cyrillic)", "ibm866"),
    ("KOI8-R (Russian)", "koi8-r"),
    ("KOI8-U (Ukrainian)", "koi8-u"),
    ("Shift_JIS (Japanese)", "shift_jis"),
    ("EUC-JP (Japanese)", "euc-jp"),
    ("GBK (Simplified Chinese)", "gbk"),
    ("Big5 (Traditional Chinese)", "big5"),
    ("EUC-KR (Korean)", "euc-kr"),
];

/// Look a row of [`CHARSETS`] up. `None` for a label that is not one,
/// so a config typo is reported rather than silently meaning UTF-8.
pub fn by_label(label: &str) -> Option<&'static Encoding> {
    let (_, name) = CHARSETS.iter().find(|(shown, name)| {
        shown.eq_ignore_ascii_case(label) || name.eq_ignore_ascii_case(label)
    })?;
    Encoding::for_label(name.as_bytes())
}

/// The label for an encoding, for a title bar or a saved setting.
pub fn label_of(encoding: &'static Encoding) -> &'static str {
    CHARSETS
        .iter()
        .find(|(_, name)| Encoding::for_label(name.as_bytes()) == Some(encoding))
        .map(|(shown, _)| *shown)
        .unwrap_or("UTF-8")
}

/// Bytes to text in a given codepage. `None` means UTF-8, which is
/// what everything already did - and is decoded lossily, so a file
/// that is nearly UTF-8 still reads.
pub fn decode(bytes: &[u8], encoding: Option<&'static Encoding>) -> String {
    match encoding {
        None => String::from_utf8_lossy(bytes).into_owned(),
        Some(enc) => enc.decode(bytes).0.into_owned(),
    }
}

/// ...and back, for writing a file out in the codepage it was read in.
/// Characters the codepage cannot hold become its own replacement -
/// that is the codepage's answer, not ours to improve on.
pub fn encode(text: &str, encoding: Option<&'static Encoding>) -> Vec<u8> {
    match encoding {
        None => text.as_bytes().to_vec(),
        Some(enc) => enc.encode(text).0.into_owned(),
    }
}

/// A filename as text, read in a codepage. On Unix a name is bytes and
/// nothing in it says what they mean, so a panel can be told - and
/// until it is, the bytes are read as UTF-8 and whatever is not UTF-8
/// shows as the replacement character, which is what every file
/// manager does and what nobody can read.
pub fn decode_name(name: &std::ffi::OsStr, encoding: Option<&'static Encoding>) -> String {
    use std::os::unix::ffi::OsStrExt;
    match encoding {
        None => name.to_string_lossy().into_owned(),
        Some(enc) => enc.decode(name.as_bytes()).0.into_owned(),
    }
}

/// ...and back: text typed into a dialog becomes the bytes the
/// filesystem will hold, so a name created on a codepage panel is
/// spelled the way the names already there are.
pub fn encode_name(text: &str, encoding: Option<&'static Encoding>) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    match encoding {
        None => std::ffi::OsString::from(text),
        Some(enc) => std::ffi::OsString::from_vec(enc.encode(text).0.into_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_codepage_resolves() {
        for (label, name) in CHARSETS {
            let enc = by_label(label).unwrap_or_else(|| panic!("{label} does not resolve"));
            assert_eq!(by_label(name), Some(enc));
            // and the label comes back, so a title says what was picked
            assert_eq!(label_of(enc), *label);
        }
        assert_eq!(by_label("no such codepage"), None);
        // no two rows may be the same encoding under another name, or
        // the list would offer one thing twice and label_of would have
        // to guess which row was meant
        let mut seen: Vec<&'static Encoding> = Vec::new();
        for (label, _) in CHARSETS {
            let enc = by_label(label).unwrap();
            assert!(!seen.contains(&enc), "{label} duplicates another row");
            seen.push(enc);
        }
    }

    #[test]
    fn names_round_trip_through_a_codepage() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let koi = by_label("KOI8-R (Russian)").unwrap();
        // a name written by a machine that spoke KOI8-R: six bytes,
        // and not a valid UTF-8 string
        let raw = OsString::from_vec(encode("Привет", Some(koi)));
        assert!(raw.to_str().is_none());
        assert_eq!(decode_name(&raw, Some(koi)), "Привет");
        // read as UTF-8 it is unreadable, which is the state of things
        // before anyone is asked
        assert!(decode_name(&raw, None).contains('\u{FFFD}'));
        // and typing it back produces the same bytes, so the file the
        // panel shows is the file the panel makes
        assert_eq!(encode_name("Привет", Some(koi)), raw);
        assert_eq!(encode_name("plain", None), OsString::from("plain"));
    }

    #[test]
    fn round_trips_through_a_single_byte_codepage() {
        // "Привет" in KOI8-R is six bytes, and is not valid UTF-8
        let koi = by_label("KOI8-R (Russian)").unwrap();
        let bytes = encode("Привет", Some(koi));
        assert_eq!(bytes.len(), 6);
        assert!(String::from_utf8(bytes.clone()).is_err());
        assert_eq!(decode(&bytes, Some(koi)), "Привет");
        // read as UTF-8 instead, the same bytes are nonsense - which is
        // the whole reason the picker exists
        assert_ne!(decode(&bytes, None), "Привет");
    }
}
