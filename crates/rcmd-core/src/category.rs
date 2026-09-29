//! Named classes of file by extension: `@pictures`, `@videos`, `@ebooks`
//! and the rest, so a mask list can say what kind of file it means
//! instead of spelling out every extension a camera or a phone writes.
//!
//! A category is a mask in any mask list - `@pictures,*.xcf` is either
//! of them - and matches in any case whatever the list around it does,
//! because `IMG_0001.JPG` is as much a picture as `scan.png`. It knows
//! nothing of contents: a category is what the name says, as every
//! mask is.

pub struct Category {
    /// What follows the `@`.
    pub key: &'static str,
    /// What a sort group made of it is called when nobody names it.
    pub label: &'static str,
    pub exts: &'static [&'static str],
}

pub const CATEGORIES: &[Category] = &[
    Category {
        key: "pictures",
        label: "Pictures",
        exts: &[
            "jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "heic", "heif", "avif",
            "jxl", "svg", "ico", "psd", "xcf", "raw", "dng", "cr2", "cr3", "nef", "arw", "orf",
            "rw2", "raf",
        ],
    },
    Category {
        key: "videos",
        label: "Videos",
        exts: &[
            "mp4", "m4v", "mkv", "webm", "avi", "mov", "wmv", "flv", "mpg", "mpeg", "m2ts", "mts",
            "3gp", "ogv", "vob",
        ],
    },
    Category {
        key: "audio",
        label: "Audio",
        exts: &[
            "mp3", "flac", "ogg", "oga", "opus", "m4a", "aac", "wav", "wma", "aif", "aiff", "ape",
            "mka", "mid", "midi",
        ],
    },
    Category {
        key: "ebooks",
        label: "Ebooks",
        exts: &["epub", "mobi", "azw", "azw3", "fb2", "djvu", "cbz", "cbr"],
    },
    Category {
        key: "documents",
        label: "Documents",
        exts: &[
            "pdf", "doc", "docx", "odt", "rtf", "txt", "md", "tex", "xls", "xlsx", "ods", "csv",
            "ppt", "pptx", "odp",
        ],
    },
    Category {
        key: "archives",
        label: "Archives",
        exts: &[
            "zip", "tar", "gz", "tgz", "bz2", "tbz", "xz", "txz", "zst", "lz", "lzma", "7z", "rar",
            "cab", "iso", "deb", "rpm", "jar",
        ],
    },
    Category {
        key: "sources",
        label: "Sources",
        exts: &[
            "c", "h", "cc", "cpp", "cxx", "hpp", "rs", "go", "py", "js", "ts", "jsx", "tsx",
            "java", "kt", "swift", "rb", "php", "cs", "sh", "fish", "lua", "pl", "hs", "ml",
            "scala", "zig",
        ],
    },
];

/// The category `@key` names, in any case.
pub fn find(key: &str) -> Option<&'static Category> {
    CATEGORIES.iter().find(|c| c.key.eq_ignore_ascii_case(key))
}

/// The `@names` in a mask list that are not categories, to be told
/// about rather than left to match a file called `@picturs`.
pub fn unknown(masks: &str) -> Vec<String> {
    masks
        .split([',', '|'])
        .map(str::trim)
        .filter_map(|mask| mask.strip_prefix('@'))
        .filter(|key| find(key).is_none())
        .map(|key| format!("@{key}"))
        .collect()
}

/// What a group of exactly one category is called: `@pictures` alone
/// is Pictures, and anything more is for its author to name.
pub fn label_of(masks: &str) -> Option<&'static str> {
    find(masks.trim().strip_prefix('@')?).map(|c| c.label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_are_found_in_any_case_and_the_rest_are_reported() {
        assert_eq!(find("Pictures").map(|c| c.label), Some("Pictures"));
        assert_eq!(unknown("@pictures,*.rs|@picturs"), ["@picturs"]);
        assert_eq!(label_of(" @ebooks "), Some("Ebooks"));
        assert_eq!(label_of("@ebooks,*.pdf"), None);
        // no extension in two categories: the first group to claim a
        // file would decide it silently otherwise
        let mut seen = std::collections::HashSet::new();
        for c in CATEGORIES {
            for ext in c.exts {
                assert!(seen.insert(ext), "{ext} is in two categories");
            }
        }
    }
}
