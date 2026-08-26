//! Reading Midnight Commander's own config files (PLAN4 S0).
//!
//! TOML stays canonical: this module never writes `config.toml`. It
//! parses mc's `menu`, `mc.ext` / `mc.ext.ini` and keymap files and
//! emits an rcmd config fragment on stdout (`rcmd --import-mc`), so the
//! user decides what to keep. Anything that cannot be expressed in
//! rcmd's vocabulary becomes a warning rather than a silent drop.

use std::collections::BTreeMap;
use std::path::Path;

use crate::config::{OpenRule, UserCommand};

#[derive(Default, Debug)]
pub struct Imported {
    pub commands: Vec<UserCommand>,
    pub open: Vec<OpenRule>,
    pub view: Vec<OpenRule>,
    pub keys: BTreeMap<String, String>,
    pub warnings: Vec<String>,
}

/// mc's config directory, `~/.config/mc` unless told otherwise.
pub fn default_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(Path::new(&dir).join("mc"));
    }
    std::env::var_os("HOME").map(|home| Path::new(&home).join(".config/mc"))
}

/// Import every mc file found in `dir`. Missing files are not an error:
/// an mc setup rarely has all of them.
pub fn import_dir(dir: &Path) -> Imported {
    let mut out = Imported::default();
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();

    match read("menu") {
        Some(text) => {
            let (commands, warnings) = parse_menu(&text);
            out.commands = commands;
            out.warnings.extend(warnings);
        }
        None => out.warnings.push("no menu file".into()),
    }

    // mc 4.8.28+ ships mc.ext.ini; older versions the line-based mc.ext
    if let Some(text) = read("mc.ext.ini") {
        let (open, view, warnings) = parse_ext_ini(&text);
        out.open.extend(open);
        out.view.extend(view);
        out.warnings.extend(warnings);
    } else if let Some(text) = read("mc.ext") {
        let (open, view, warnings) = parse_ext(&text);
        out.open.extend(open);
        out.view.extend(view);
        out.warnings.extend(warnings);
    } else {
        out.warnings.push("no mc.ext / mc.ext.ini".into());
    }

    if let Some(text) = read("mc.keymap") {
        let (keys, warnings) = parse_keymap(&text);
        out.keys = keys;
        out.warnings.extend(warnings);
    }
    out
}

/// mc's user menu: an entry is a hotkey character in column 0 followed
/// by its title, then indented shell lines. `#` comments, `+`/`=`
/// condition lines. rcmd has no conditions yet, so a condition is kept
/// as a comment on the emitted entry.
pub fn parse_menu(text: &str) -> (Vec<UserCommand>, Vec<String>) {
    let mut commands: Vec<UserCommand> = Vec::new();
    let mut warnings = Vec::new();
    let mut body: Vec<String> = Vec::new();
    let mut pending_condition: Option<String> = None;
    // mc's default; `shell_patterns=0` at the top means the patterns in
    // the conditions are regexes instead of globs
    let mut shell_patterns = true;
    let mut current: Option<(String, Option<String>)> = None; // title, condition

    let flush = |current: &mut Option<(String, Option<String>)>,
                 body: &mut Vec<String>,
                 commands: &mut Vec<UserCommand>| {
        if let Some((title, condition)) = current.take() {
            let run = body.join("\n");
            body.clear();
            if run.trim().is_empty() {
                return;
            }
            commands.push(UserCommand {
                name: title,
                run,
                key: None,
                // mc's condition language is rcmd's too now, so it
                // comes across as itself rather than as a note in the
                // entry's name
                when: condition,
                entries: Vec::new(),
            });
        } else {
            body.clear();
        }
    };

    for line in text.lines() {
        if line.trim_start().starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let first = line.chars().next().expect("non-empty");
        if first.is_whitespace() {
            if current.is_some() {
                body.push(line.trim().to_string());
            }
            continue;
        }
        if let Some(value) = line.trim().strip_prefix("shell_patterns=") {
            shell_patterns = value.trim() != "0";
            continue;
        }
        if first == '+' || first == '=' {
            // condition for the entry that follows. `=` marks mc's
            // default entry, which rcmd has no notion of - the entry
            // is kept, the "start here" is not.
            let condition = line[1..].trim_start_matches('=').trim();
            pending_condition = Some(match shell_patterns {
                true => condition.to_string(),
                false => regex_condition(condition, &mut warnings),
            });
            continue;
        }
        flush(&mut current, &mut body, &mut commands);
        let title = line[first.len_utf8()..].trim().to_string();
        current = Some((
            if title.is_empty() {
                format!("menu entry {first}")
            } else {
                title
            },
            pending_condition.take(),
        ));
    }
    flush(&mut current, &mut body, &mut commands);

    for cmd in &commands {
        warnings.extend(macro_warnings(&cmd.run, &cmd.name));
    }
    (commands, warnings)
}

/// The classic line-based `mc.ext`: a matcher line at column 0, then
/// indented `Open=` / `View=` / `Edit=` lines.
pub fn parse_ext(text: &str) -> (Vec<OpenRule>, Vec<OpenRule>, Vec<String>) {
    let mut open = Vec::new();
    let mut view = Vec::new();
    let mut warnings = Vec::new();
    let mut matchers: Vec<OpenRule> = Vec::new();

    for line in text.lines() {
        if line.trim_start().starts_with('#') || line.trim().is_empty() {
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            let (rules, warning) = matcher_to_rules(line.trim());
            warnings.extend(warning);
            matchers = rules;
            continue;
        }
        let body = line.trim();
        let Some((verb, command)) = body.split_once('=') else {
            continue;
        };
        let Some(run) = convert_command(command.trim()) else {
            warnings.push(format!("skipped '{}': no rcmd equivalent", body.trim()));
            continue;
        };
        for matcher in &matchers {
            let rule = OpenRule {
                run: run.clone(),
                ..matcher.clone()
            };
            match verb.trim() {
                "Open" => open.push(rule),
                "View" => view.push(rule),
                "Edit" => {} // rcmd's F4 is the editor; nothing to bind
                other => warnings.push(format!("unknown mc.ext verb '{other}'")),
            }
        }
    }
    (open, view, warnings)
}

/// mc 4.8.28+ `mc.ext.ini`: `[section]` with `Regex=`/`Shell=`/`Type=`
/// plus the same Open/View/Edit verbs.
pub fn parse_ext_ini(text: &str) -> (Vec<OpenRule>, Vec<OpenRule>, Vec<String>) {
    let mut open = Vec::new();
    let mut view = Vec::new();
    let mut warnings = Vec::new();
    let mut matchers: Vec<OpenRule> = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            matchers.clear();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "Regex" | "Shell" | "Type" | "Directory" => {
                let matcher = format!("{}/{value}", key.to_lowercase());
                let (rules, warning) = matcher_to_rules(&matcher);
                warnings.extend(warning);
                matchers = rules;
            }
            // the ini spells the i/ flag as its own key, which may come
            // after the pattern it applies to
            "RegexIgnoreCase" | "TypeIgnoreCase" | "ShellIgnoreCase"
                if value.eq_ignore_ascii_case("true") =>
            {
                for rule in &mut matchers {
                    if let Some(re) = rule.regex.as_mut().filter(|re| !re.starts_with("(?i)")) {
                        re.insert_str(0, "(?i)");
                    }
                    if let Some(re) = rule.kind.as_mut().filter(|re| !re.starts_with("(?i)")) {
                        re.insert_str(0, "(?i)");
                    }
                }
            }
            "Open" | "View" | "Edit" => {
                let Some(run) = convert_command(value) else {
                    warnings.push(format!("skipped '{key}={value}': no rcmd equivalent"));
                    continue;
                };
                for matcher in &matchers {
                    let rule = OpenRule {
                        run: run.clone(),
                        ..matcher.clone()
                    };
                    match key {
                        "Open" => open.push(rule),
                        "View" => view.push(rule),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    (open, view, warnings)
}

/// An mc.ext matcher line to rule templates (everything but `run`).
/// `shell/.txt` is an extension glob; `regex/...` becomes globs when it
/// is simple enough to read as one and a `regex =` otherwise; `type/`
/// and `directory/` are the same regexes on `file -b` and the path.
fn matcher_to_rules(line: &str) -> (Vec<OpenRule>, Option<String>) {
    let (kind, value) = match line.split_once('/') {
        Some(pair) => pair,
        None => return (Vec::new(), Some(format!("unknown matcher '{line}'"))),
    };
    // mc allows a case-insensitivity flag: shell/i/.txt
    let (value, fold) = match value.strip_prefix("i/") {
        Some(rest) => (rest, true),
        None => (value, false),
    };
    let re = |value: &str| {
        if fold {
            format!("(?i){value}")
        } else {
            value.to_string()
        }
    };
    let blank = OpenRule::default();
    match kind {
        "shell" => {
            let glob = if let Some(ext) = value.strip_prefix('.') {
                format!("*.{ext}")
            } else {
                value.to_string()
            };
            (vec![OpenRule::by_glob(&glob, "")], None)
        }
        "regex" => match regex_to_globs(value) {
            Some(globs) => (
                globs.iter().map(|g| OpenRule::by_glob(g, "")).collect(),
                None,
            ),
            None => (
                vec![OpenRule {
                    regex: Some(re(value)),
                    ..blank
                }],
                None,
            ),
        },
        "type" => (
            vec![OpenRule {
                kind: Some(re(value)),
                ..blank
            }],
            None,
        ),
        "directory" => (
            vec![OpenRule {
                directory: Some(re(value)),
                ..blank
            }],
            None,
        ),
        other => (Vec::new(), Some(format!("unknown matcher kind '{other}'"))),
    }
}

/// Convert the simple, common mc.ext regexes to globs: anchors, escaped
/// dots, `.` wildcards and one alternation group. Anything else is
/// refused rather than mistranslated.
pub fn regex_to_globs(re: &str) -> Option<Vec<String>> {
    let anchored_end = re.ends_with('$');
    let anchored_start = re.starts_with('^');
    let core = re.trim_start_matches('^').trim_end_matches('$').to_string();

    // expand a single (a|b|c) group
    let variants: Vec<String> = match (core.find('('), core.find(')')) {
        (Some(open), Some(close)) if open < close => {
            let inner = &core[open + 1..close];
            if inner.contains('(') || inner.contains(')') {
                return None;
            }
            inner
                .split('|')
                .map(|alt| format!("{}{alt}{}", &core[..open], &core[close + 1..]))
                .collect()
        }
        (None, None) => vec![core.clone()],
        _ => return None,
    };

    let mut globs = Vec::new();
    for variant in variants {
        let mut glob = String::new();
        let mut chars = variant.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    // an escaped literal: keep it, unless glob-special
                    // an escaped glob metacharacter cannot survive the trip
                    Some('*' | '?' | '[' | ']') => return None,
                    Some(esc) => glob.push(esc),
                    None => return None,
                },
                '.' => {
                    if chars.peek() == Some(&'*') {
                        chars.next();
                        glob.push('*');
                    } else {
                        glob.push('?');
                    }
                }
                '[' | ']' | '{' | '}' | '+' | '*' | '?' | '|' | '^' | '$' => return None,
                other => glob.push(other),
            }
        }
        if glob.is_empty() {
            return None;
        }
        // an unanchored end matches anywhere: mc.ext patterns are
        // overwhelmingly "ends with", so anchor accordingly
        let glob = match (anchored_start, anchored_end) {
            (true, true) => glob,
            (true, false) => format!("{glob}*"),
            (false, true) => format!("*{glob}"),
            (false, false) => format!("*{glob}*"),
        };
        globs.push(glob);
    }
    Some(globs)
}

/// mc's command macros to rcmd's. `%cd` (an internal VFS chdir) has no
/// equivalent, so such a rule is dropped rather than half-translated.
fn convert_command(command: &str) -> Option<String> {
    if command.contains("%cd") {
        return None;
    }
    // "%view{ascii}" means "show it in the viewer" - that is exactly
    // what a [[view]] rule does, so the prefix just goes away
    let mut out = String::new();
    let mut rest = command.trim();
    if let Some(after) = rest.strip_prefix("%view") {
        rest = after.trim_start();
        if rest.starts_with('{') {
            {
                let end = rest.find('}')?;
                rest = rest[end + 1..].trim_start()
            }
        }
    }
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // %p and %f are both "the current file"
            Some('f' | 'p') => out.push_str("%f"),
            Some('d') => out.push_str("%d"),
            Some('D') => out.push_str("%D"),
            Some('s' | 't') => out.push_str("%t"),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    let out = out.trim().to_string();
    (!out.is_empty()).then_some(out)
}

/// A condition from a `shell_patterns=0` file: the patterns in it are
/// regexes, and rcmd's conditions are globs. Each term is converted on
/// its own; one that will not convert is left as it is and reported,
/// which is better than a menu entry that quietly never shows up.
fn regex_condition(condition: &str, warnings: &mut Vec<String>) -> String {
    let mut out = String::new();
    let mut rest = condition;
    while !rest.is_empty() {
        let at = rest.find(['|', '&']).unwrap_or(rest.len());
        let (term, tail) = rest.split_at(at);
        let (joiner, next) = match tail.chars().next() {
            Some(c) => (c.to_string(), &tail[c.len_utf8()..]),
            None => (String::new(), ""),
        };
        let term = term.trim();
        let converted = match term.split_once(char::is_whitespace) {
            // only the name conditions carry a pattern; `t` takes
            // letters and `x` takes a path
            Some((head, pattern))
                if head
                    .trim_start_matches('!')
                    .starts_with(['f', 'F', 'd', 'D']) =>
            {
                match regex_to_globs(pattern.trim()).and_then(|globs| globs.into_iter().next()) {
                    Some(glob) => format!("{head} {glob}"),
                    None => {
                        warnings.push(format!(
                            "menu condition '{term}': this regex has no glob, left as it is"
                        ));
                        term.to_string()
                    }
                }
            }
            _ => term.to_string(),
        };
        out.push_str(&converted);
        if !joiner.is_empty() {
            out.push(' ');
            out.push_str(&joiner);
            out.push(' ');
        }
        rest = next;
    }
    out.trim().to_string()
}

/// Macros rcmd does not expand, so the user can fix them by hand.
fn macro_warnings(run: &str, where_: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = run.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            continue;
        }
        match chars.next() {
            Some('f' | 'F' | 'd' | 'D' | 't' | 'T' | 'u' | 'U' | 's' | 'S' | 'q' | '{' | '%') => {}
            Some(other) => out.push(format!("'{where_}': macro %{other} is not supported")),
            None => {}
        }
    }
    out.sort();
    out.dedup();
    out
}

/// mc keymap ini: `[panel]` sections with `Action = key; key`. Only the
/// actions rcmd has are mapped; the rest are reported.
pub fn parse_keymap(text: &str) -> (BTreeMap<String, String>, Vec<String>) {
    let mut keys = BTreeMap::new();
    let mut warnings = Vec::new();
    let mut in_panel = false;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            in_panel = line.eq_ignore_ascii_case("[panel]");
            continue;
        }
        if !in_panel {
            continue;
        }
        let Some((action, binding)) = line.split_once('=') else {
            continue;
        };
        let Some(rcmd_action) = mc_action(action.trim()) else {
            warnings.push(format!("no rcmd action for mc's '{}'", action.trim()));
            continue;
        };
        for key in binding.split(';') {
            match mc_key(key.trim()) {
                Some(key) => {
                    keys.insert(key, rcmd_action.to_string());
                }
                None => warnings.push(format!("cannot map mc key '{}'", key.trim())),
            }
        }
    }
    (keys, warnings)
}

/// mc panel action names to rcmd's.
fn mc_action(name: &str) -> Option<&'static str> {
    Some(match name {
        "Help" => "help",
        "View" => "view",
        "ViewRaw" => "view-raw",
        "ViewFiltered" => "filtered-view",
        "Edit" => "edit",
        "Copy" => "copy",
        "Move" => "move",
        "MakeDir" => "mkdir",
        "Delete" => "delete",
        "Menu" => "menu",
        "Quit" | "QuitQuiet" => "quit",
        "Select" => "select-group",
        "Unselect" => "unselect-group",
        "SelectInvert" => "invert-selection",
        "Reread" => "reload",
        "Swap" => "swap-panels",
        "ShowHidden" => "toggle-hidden",
        "Search" => "quick-search",
        "Filter" => "filter",
        "DirSize" => "dir-size",
        // in mc's [panel] section History is the *directory* history
        // (M-H); the command-line one lives in [main], which is not read
        "History" => "dir-history",
        "HistoryNext" => "history-forward",
        "HistoryPrev" => "history-back",
        "Find" => "find-file",
        "CompareDirs" => "compare-dirs",
        "PanelListingSwitch" => "listing-cycle",
        "QuickView" => "quick-view",
        "Info" => "info-view",
        "Shell" => "shell",
        "UserMenu" => "user-menu",
        "HotList" => "hotlist",
        "Sort" => "sort-name",
        _ => return None,
    })
}

/// mc key syntax ("ctrl-f5", "alt-o", "shift-f3") to rcmd's ("ctrl+f5").
fn mc_key(key: &str) -> Option<String> {
    if key.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    for part in key.split('-') {
        let part = part.trim().to_lowercase();
        let mapped = match part.as_str() {
            "ctrl" | "control" => "ctrl",
            "alt" | "meta" => "alt",
            "shift" => "shift",
            // mc spells punctuation out; rcmd wants the character
            "dot" | "period" => ".",
            "question" => "?",
            "exclamation" => "!",
            "slash" => "/",
            "backslash" => "\\",
            "comma" => ",",
            "semicolon" => ";",
            "colon" => ":",
            "star" | "asterisk" => "*",
            "plus" => "+",
            "minus" | "dash" => "-",
            "equal" | "equals" => "=",
            "escape" => "esc",
            "return" => "enter",
            "" => return None,
            other => other,
        };
        parts.push(mapped.to_string());
    }
    let spec = parts.join("+");
    // reuse rcmd's own parser as the validator
    crate::keymap::parse_key(&spec).is_some().then_some(spec)
}

/// The imported settings as an rcmd config fragment.
fn rule_toml(table: &str, rule: &OpenRule) -> String {
    let mut out = format!("\n[[{table}]]\n");
    for (key, value) in [
        ("match", &rule.pattern),
        ("regex", &rule.regex),
        ("type", &rule.kind),
        ("directory", &rule.directory),
    ] {
        if let Some(value) = value {
            out.push_str(&format!("{key} = {}\n", toml_str(value)));
        }
    }
    out.push_str(&format!("run = {}\n", toml_str(&rule.run)));
    out
}

pub fn to_toml(imported: &Imported) -> String {
    let mut out = String::new();
    out.push_str("# Imported from Midnight Commander by `rcmd --import-mc`.\n");
    out.push_str("# Review it, then paste what you want into your config.\n");

    if !imported.keys.is_empty() {
        out.push_str("\n[keys]\n");
        for (key, action) in &imported.keys {
            out.push_str(&format!("{} = {}\n", toml_str(key), toml_str(action)));
        }
    }
    for rule in &imported.open {
        out.push_str(&rule_toml("open", rule));
    }
    for rule in &imported.view {
        out.push_str(&rule_toml("view", rule));
    }
    for cmd in &imported.commands {
        out.push_str(&format!(
            "\n[[commands]]\nname = {}\nrun = {}\n",
            toml_str(&cmd.name),
            toml_str(&cmd.run)
        ));
        if let Some(when) = &cmd.when {
            out.push_str(&format!("when = {}\n", toml_str(when)));
        }
    }
    out
}

/// A TOML string literal: multi-line commands get a triple-quoted one.
fn toml_str(value: &str) -> String {
    if value.contains('\n') {
        format!("\"\"\"\n{}\"\"\"", value.replace('\\', "\\\\"))
    } else {
        format!("{:?}", value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // shaped like a real ~/.config/mc/menu
    const MENU: &str = r#"# comment line
shell_patterns=0
+ f \.tar\.gz$ | f \.tgz$
a       Extract the tar file
        tar xzf %f

b       Count lines in the tagged files
        wc -l %t
        echo done

c       Uses an unsupported macro
        echo %q
"#;

    #[test]
    fn menu_entries_become_commands() {
        let (commands, warnings) = parse_menu(MENU);
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0].name, "Extract the tar file");
        // the condition comes across as rcmd's own, and this file said
        // its patterns were regexes, so they arrive as globs
        assert_eq!(commands[0].when.as_deref(), Some("f *.tar.gz | f *.tgz"));
        assert_eq!(commands[0].run, "tar xzf %f");
        // multi-line bodies stay multi-line
        assert_eq!(commands[1].run, "wc -l %t\necho done");
        assert!(commands[1].name.starts_with("Count lines"));
        // %q is one rcmd expands now, so it is no longer reported; a
        // macro rcmd really does not have still is
        assert!(!warnings.iter().any(|w| w.contains("%q")), "{warnings:?}");
        let (_, warnings) = parse_menu("a Entry\n        echo %i\n");
        assert!(warnings.iter().any(|w| w.contains("%i")), "{warnings:?}");
    }

    #[test]
    fn ext_lines_become_open_and_view_rules() {
        let text = "\
shell/.txt\n\
\tOpen=less %f\n\
\tView=%view{ascii} cat %f\n\
regex/\\.(png|jpg)$\n\
\tOpen=feh %f\n\
type/^ELF\n\
\tOpen=objdump -d %f\n";
        let (open, view, warnings) = parse_ext(text);
        assert_eq!(open[0].pattern.as_deref(), Some("*.txt"));
        assert_eq!(open[0].run, "less %f");
        // %view{...} is what a [[view]] rule means, so the prefix goes
        assert_eq!(view[0].run, "cat %f");
        // one alternation regex expands into two globs
        let pngs: Vec<&str> = open.iter().filter_map(|r| r.pattern.as_deref()).collect();
        assert!(
            pngs.contains(&"*.png") && pngs.contains(&"*.jpg"),
            "{pngs:?}"
        );
        // type/ is a regex over file -b, carried as such
        let elf = open.iter().find(|r| r.kind.is_some()).expect("type rule");
        assert_eq!(elf.kind.as_deref(), Some("^ELF"));
        assert_eq!(elf.run, "objdump -d %f");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn regexes_a_glob_cannot_say_are_kept_as_regexes() {
        let text = "\
regex/i/^[a-z]+[0-9]+\\.log$\n\
\tOpen=less %f\n\
directory/^/srv/\n\
\tOpen=echo srv %f\n";
        let (open, _, warnings) = parse_ext(text);
        assert_eq!(open[0].regex.as_deref(), Some("(?i)^[a-z]+[0-9]+\\.log$"));
        assert!(open[0].pattern.is_none());
        assert_eq!(open[1].directory.as_deref(), Some("^/srv/"));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn ext_ini_ignore_case_key_reaches_the_pattern_before_it() {
        let text = "\
[logs]\n\
Regex=^[a-z]+\\.log$\n\
RegexIgnoreCase=true\n\
Open=less %f\n";
        let (open, _, _) = parse_ext_ini(text);
        assert_eq!(open[0].regex.as_deref(), Some("(?i)^[a-z]+\\.log$"));
    }

    #[test]
    fn ext_ini_is_understood_too() {
        let text = "\
[pdf]\n\
Regex=\\.pdf$\n\
Open=zathura %f\n\
View=pdftotext %f -\n";
        let (open, view, warnings) = parse_ext_ini(text);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].pattern.as_deref(), Some("*.pdf"));
        assert_eq!(view[0].run, "pdftotext %f -");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn cd_rules_are_dropped_not_mistranslated() {
        let text = "shell/.tar\n\tOpen=%cd %p#utar\n";
        let (open, _, warnings) = parse_ext(text);
        assert!(open.is_empty());
        assert!(warnings.iter().any(|w| w.contains("no rcmd equivalent")));
    }

    #[test]
    fn regexes_convert_only_when_unambiguous() {
        assert_eq!(regex_to_globs("\\.txt$"), Some(vec!["*.txt".into()]));
        assert_eq!(
            regex_to_globs("\\.(gz|xz)$"),
            Some(vec!["*.gz".into(), "*.xz".into()])
        );
        assert_eq!(regex_to_globs("^README$"), Some(vec!["README".into()]));
        // a character class or a quantifier is a refusal, not a guess
        assert_eq!(regex_to_globs("\\.[ch]$"), None);
        assert_eq!(regex_to_globs("a+b"), None);
    }

    #[test]
    fn keymap_maps_known_panel_actions() {
        let text = "\
[panel]\n\
Copy = f5; ctrl-c\n\
ShowHidden = alt-dot\n\
Reread = ctrl-r\n\
SomethingElse = f12\n\
[editor]\n\
Save = f2\n";
        let (keys, warnings) = parse_keymap(text);
        assert_eq!(keys["f5"], "copy");
        assert_eq!(keys["ctrl+c"], "copy");
        assert_eq!(keys["ctrl+r"], "reload");
        // [editor] is not the panel section, so Save is not picked up
        assert!(!keys.values().any(|v| v == "save"));
        assert!(warnings.iter().any(|w| w.contains("SomethingElse")));
        // mc spells punctuation out: alt-dot is rcmd's alt+.
        assert_eq!(keys["alt+."], "toggle-hidden");
    }

    #[test]
    fn mc_key_names_convert() {
        assert_eq!(mc_key("f5").as_deref(), Some("f5"));
        assert_eq!(mc_key("ctrl-r").as_deref(), Some("ctrl+r"));
        assert_eq!(mc_key("alt-question").as_deref(), Some("alt+?"));
        assert_eq!(mc_key("ctrl-backslash").as_deref(), Some("ctrl+\\"));
        assert_eq!(mc_key("alt-dot").as_deref(), Some("alt+."));
        assert_eq!(mc_key("shift-f3").as_deref(), Some("shift+f3"));
        // something rcmd has no key for stays unmapped
        assert_eq!(mc_key("kpplus"), None);
        assert_eq!(mc_key(""), None);
    }

    #[test]
    fn emitted_toml_parses_back_as_config() {
        let imported = Imported {
            commands: vec![UserCommand {
                name: "two lines".into(),
                run: "echo one\necho two\n".into(),
                key: None,
                when: Some("f *.tar.gz".into()),
                entries: Vec::new(),
            }],
            open: vec![
                OpenRule::by_glob("*.pdf", "zathura %f &"),
                OpenRule {
                    kind: Some("^ELF".into()),
                    directory: Some("^/opt/".into()),
                    run: "objdump -d %f".into(),
                    ..OpenRule::default()
                },
            ],
            ..Imported::default()
        };
        let text = to_toml(&imported);
        let config: crate::config::Config = toml::from_str(&text).expect("emits valid TOML");
        assert_eq!(config.open[0].pattern.as_deref(), Some("*.pdf"));
        assert_eq!(config.open[1].kind.as_deref(), Some("^ELF"));
        assert_eq!(config.open[1].directory.as_deref(), Some("^/opt/"));
        assert!(config.open[1].pattern.is_none());
        assert_eq!(config.commands[0].run, "echo one\necho two\n");
        // mc's condition comes across as rcmd's `when`, not as a note
        assert_eq!(config.commands[0].when.as_deref(), Some("f *.tar.gz"));
    }
}
