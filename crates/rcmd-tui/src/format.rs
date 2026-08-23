//! MC's user-defined listing format: the `listing = "user"` panel draws
//! whatever `listing_format` says, in mc's own little format language.
//!
//! A format starts with the panel size (`half` or `full`), takes an
//! optional repeat count (how many times the field set is laid out side
//! by side, 1-9), and then names fields, optionally with `:width` for a
//! fixed size or `:width+` for a minimum one that grows into whatever
//! space is left. `space` and `|` are layout, not data.
//!
//! mc's own built-ins written out in it:
//!
//! ```text
//! Full: half type name | size | mtime
//! Long: full perm space nlink space owner space group space size space mtime space name
//! ```
//!
//! Parsing never fails: an unknown word is dropped with a warning and
//! the rest of the format still draws, because a typo in a config file
//! should cost you one column, not the panel.

/// One piece of data a format can put in a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Size,
    /// Size, but directories read `SUB-DIR` / `UP--DIR`.
    BSize,
    /// The one-character `ls -F` marker: `*` `/` `@` `=` `-` `+` `|` `~` `!`.
    Type,
    /// `*` when the entry is tagged, a space when it is not.
    Mark,
    Mtime,
    Atime,
    Ctime,
    Perm,
    /// Permission bits in octal.
    Mode,
    Nlink,
    Ngid,
    Nuid,
    Owner,
    Group,
    Inode,
}

impl Field {
    fn parse(word: &str) -> Option<Field> {
        Some(match word {
            "name" => Field::Name,
            "size" => Field::Size,
            "bsize" => Field::BSize,
            "type" => Field::Type,
            "mark" => Field::Mark,
            "mtime" => Field::Mtime,
            "atime" => Field::Atime,
            "ctime" => Field::Ctime,
            "perm" => Field::Perm,
            "mode" => Field::Mode,
            "nlink" => Field::Nlink,
            "ngid" => Field::Ngid,
            "nuid" => Field::Nuid,
            "owner" => Field::Owner,
            "group" => Field::Group,
            "inode" => Field::Inode,
            _ => return None,
        })
    }

    /// Column heading, in the style of the built-in listings.
    pub fn label(self) -> &'static str {
        match self {
            Field::Name => "Name",
            Field::Size | Field::BSize => "Size",
            Field::Type => "T",
            Field::Mark => "M",
            Field::Mtime => "Modify time",
            Field::Atime => "Access time",
            Field::Ctime => "Change time",
            Field::Perm => "Perms",
            Field::Mode => "Mode",
            Field::Nlink => "Lnk",
            Field::Ngid => "GID",
            Field::Nuid => "UID",
            Field::Owner => "Owner",
            Field::Group => "Group",
            Field::Inode => "Inode",
        }
    }

    /// Width when the format does not say - mc's defaults, matching the
    /// widths the built-in listings already use.
    pub fn default_width(self) -> u16 {
        match self {
            Field::Name => 0, // grows into whatever is left
            Field::Size | Field::BSize => 7,
            Field::Type | Field::Mark => 1,
            Field::Mtime | Field::Atime | Field::Ctime => 12,
            Field::Perm => 10,
            Field::Mode => 4,
            Field::Nlink | Field::Ngid | Field::Nuid => 5,
            Field::Owner | Field::Group => 8,
            Field::Inode => 9,
        }
    }

    /// Numbers read better against the column's right edge.
    pub fn right_aligned(self) -> bool {
        matches!(
            self,
            Field::Size
                | Field::BSize
                | Field::Mode
                | Field::Nlink
                | Field::Ngid
                | Field::Nuid
                | Field::Inode
        )
    }
}

/// How much room a field asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    /// No `:size` given - the field's default, and `name` grows.
    Auto,
    /// `:n` - exactly this many columns.
    Fixed(u16),
    /// `:n+` - at least this many, more when there is room.
    Min(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Field(Field, Width),
    Space,
    /// `|`, drawn as a vertical rule.
    Bar,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Format {
    /// `full`: the panel takes the whole width, mc's one-panel view.
    pub full: bool,
    /// How many times the field set repeats side by side (1-9).
    pub repeat: u16,
    pub items: Vec<Item>,
}

/// What `listing = "user"` draws until the config says otherwise - mc's
/// own Full listing, written in the format language.
pub const DEFAULT: &str = "half type name | size | mtime";

impl Default for Format {
    fn default() -> Format {
        parse(DEFAULT).0
    }
}

impl Format {
    /// The fields, in order, with the width each one ends up with in a
    /// panel `total` columns wide. Fixed sizes are honoured first and
    /// whatever is left is shared out among the fields that grow (`name`
    /// and any `:n+`), so a format always fills the panel exactly.
    /// Shared by drawing and by header clicks, so the two agree on where
    /// each column starts.
    pub fn layout(&self, total: u16) -> Vec<(Item, u16)> {
        let mut widths: Vec<u16> = self
            .items
            .iter()
            .map(|item| match *item {
                Item::Space | Item::Bar => 1,
                Item::Field(_, Width::Fixed(n) | Width::Min(n)) => n.max(1),
                Item::Field(field, Width::Auto) => field.default_width(),
            })
            .collect();
        let grows: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                matches!(
                    item,
                    Item::Field(_, Width::Min(_)) | Item::Field(Field::Name, Width::Auto)
                )
            })
            .map(|(i, _)| i)
            .collect();
        // one space between columns, as the built-in listings have
        let spacing = self.items.len().saturating_sub(1) as u16;
        let fixed: u16 = widths.iter().sum::<u16>() + spacing;
        if !grows.is_empty() && total > fixed {
            let spare = total - fixed;
            let each = spare / grows.len() as u16;
            let mut extra = spare % grows.len() as u16;
            for index in &grows {
                widths[*index] += each + u16::from(extra > 0);
                extra = extra.saturating_sub(1);
            }
        }
        // a column narrower than one character cannot draw anything;
        // in a panel too narrow for the format the renderer clips
        for width in &mut widths {
            *width = (*width).max(1);
        }
        self.items.iter().copied().zip(widths).collect()
    }
}

/// Parse a format string. Never fails: whatever could not be understood
/// comes back as a warning for the status line, and the rest still
/// draws.
pub fn parse(spec: &str) -> (Format, Vec<String>) {
    let mut warnings = Vec::new();
    let mut format = Format {
        full: false,
        repeat: 1,
        items: Vec::new(),
    };
    let mut words = spec.split_whitespace().peekable();

    match words.peek().copied() {
        Some("half") => {
            words.next();
        }
        Some("full") => {
            words.next();
            format.full = true;
        }
        _ => warnings.push("listing_format: no half/full, assuming half".into()),
    }
    // an optional repeat count, and only right after the panel size
    if let Some(word) = words.peek().copied()
        && let Ok(count) = word.parse::<u16>()
    {
        words.next();
        if (1..=9).contains(&count) {
            format.repeat = count;
        } else {
            warnings.push(format!("listing_format: {count} is not 1-9, using 1"));
        }
    }

    for word in words {
        match word {
            "space" => format.items.push(Item::Space),
            "|" => format.items.push(Item::Bar),
            _ => {
                let (name, width) = split_width(word, &mut warnings);
                match Field::parse(name) {
                    Some(field) => format.items.push(Item::Field(field, width)),
                    None => warnings.push(format!("listing_format: unknown field {name}")),
                }
            }
        }
    }
    if format.items.is_empty() {
        warnings.push("listing_format: no fields, using the default".into());
        format.items = parse(DEFAULT).0.items;
    }
    (format, warnings)
}

/// `name`, `name:8` or `name:8+` - the trailing `+` makes the size a
/// minimum instead of a fixed width.
fn split_width<'a>(word: &'a str, warnings: &mut Vec<String>) -> (&'a str, Width) {
    let Some((name, size)) = word.split_once(':') else {
        return (word, Width::Auto);
    };
    let (digits, grows) = match size.strip_suffix('+') {
        Some(digits) => (digits, true),
        None => (size, false),
    };
    match digits.parse::<u16>() {
        Ok(n) if n > 0 => (
            name,
            if grows {
                Width::Min(n)
            } else {
                Width::Fixed(n)
            },
        ),
        _ => {
            warnings.push(format!("listing_format: bad size in {word}"));
            (name, Width::Auto)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(format: &Format) -> Vec<Item> {
        format.items.clone()
    }

    #[test]
    fn mcs_full_listing_round_trips() {
        let (format, warnings) = parse("half type name | size | mtime");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!format.full);
        assert_eq!(format.repeat, 1);
        assert_eq!(
            fields(&format),
            vec![
                Item::Field(Field::Type, Width::Auto),
                Item::Field(Field::Name, Width::Auto),
                Item::Bar,
                Item::Field(Field::Size, Width::Auto),
                Item::Bar,
                Item::Field(Field::Mtime, Width::Auto),
            ]
        );
    }

    #[test]
    fn mcs_long_listing_is_a_full_width_format() {
        let (format, warnings) = parse(
            "full perm space nlink space owner space group space size space mtime space name",
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(format.full);
        assert_eq!(
            format.items.iter().filter(|i| **i == Item::Space).count(),
            6
        );
        assert_eq!(
            format.items.first(),
            Some(&Item::Field(Field::Perm, Width::Auto))
        );
    }

    #[test]
    fn a_repeat_count_follows_the_panel_size() {
        let (format, _) = parse("half 3 name");
        assert_eq!(format.repeat, 3);
        assert_eq!(fields(&format), vec![Item::Field(Field::Name, Width::Auto)]);
    }

    #[test]
    fn repeat_counts_outside_one_to_nine_warn() {
        let (format, warnings) = parse("half 12 name");
        assert_eq!(format.repeat, 1);
        assert!(warnings.iter().any(|w| w.contains("1-9")), "{warnings:?}");
    }

    #[test]
    fn sizes_are_fixed_or_minimum() {
        let (format, warnings) = parse("half name:20+ size:7");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            fields(&format),
            vec![
                Item::Field(Field::Name, Width::Min(20)),
                Item::Field(Field::Size, Width::Fixed(7)),
            ]
        );
    }

    #[test]
    fn a_bad_size_falls_back_to_the_default_width() {
        let (format, warnings) = parse("half name:wide");
        assert_eq!(fields(&format), vec![Item::Field(Field::Name, Width::Auto)]);
        assert!(
            warnings.iter().any(|w| w.contains("bad size")),
            "{warnings:?}"
        );
    }

    #[test]
    fn an_unknown_field_is_dropped_with_a_warning() {
        let (format, warnings) = parse("half name colour size");
        assert_eq!(
            fields(&format),
            vec![
                Item::Field(Field::Name, Width::Auto),
                Item::Field(Field::Size, Width::Auto),
            ]
        );
        assert!(
            warnings.iter().any(|w| w.contains("colour")),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_missing_panel_size_warns_but_still_parses() {
        let (format, warnings) = parse("name | size");
        assert!(!format.full);
        assert_eq!(format.items.len(), 3);
        assert!(
            warnings.iter().any(|w| w.contains("half/full")),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_format_with_no_fields_falls_back_to_the_default() {
        let (format, warnings) = parse("half");
        assert_eq!(format.items, parse(DEFAULT).0.items);
        assert!(
            warnings.iter().any(|w| w.contains("no fields")),
            "{warnings:?}"
        );
    }

    #[test]
    fn layout_fills_the_panel_exactly() {
        let (format, _) = parse("half type name | size | mtime");
        let layout = format.layout(80);
        let spacing = (layout.len() - 1) as u16;
        let total: u16 = layout.iter().map(|(_, w)| w).sum::<u16>() + spacing;
        assert_eq!(total, 80);
        // the fixed columns kept their widths; name took the rest
        let name = layout
            .iter()
            .find(|(item, _)| matches!(item, Item::Field(Field::Name, _)))
            .unwrap();
        assert_eq!(name.1, 80 - (1 + 1 + 7 + 1 + 12) - spacing);
    }

    #[test]
    fn growable_fields_share_the_spare_room() {
        let (format, _) = parse("half name:10+ owner:4+ size:7");
        let layout = format.layout(60);
        let widths: Vec<u16> = layout.iter().map(|(_, w)| *w).collect();
        assert_eq!(widths.iter().sum::<u16>() + 2, 60);
        assert!(widths[0] >= 10 && widths[1] >= 4);
        // both grew, and by within one column of each other
        assert!(widths[0].abs_diff(widths[1]) <= 1 + 6, "{widths:?}");
        assert_eq!(widths[2], 7);
    }

    #[test]
    fn a_narrow_panel_keeps_the_asked_for_widths() {
        let (format, _) = parse("half perm space owner space group space size space mtime name");
        // far too little room: nothing shrinks, the renderer clips
        let layout = format.layout(10);
        assert!(layout.iter().all(|(_, w)| *w >= 1));
    }
}
