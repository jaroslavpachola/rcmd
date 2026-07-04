# rcmd

A Midnight Commander replacement in Rust: orthodox dual-pane file manager
with MC keybindings, built on ratatui. See [docs/PLAN.md](docs/PLAN.md) for
the roadmap.

## Status

M5 — all milestones of the original plan shipped. Marking and F5–F8
operations with MC-style dialogs, command line + shell integration, F3
chunked viewer, F4 $EDITOR, F9 menu, F1 help, config file with keymap
presets/custom bindings, quick search, filter, hotlist, themes, and
read-only archive browsing (zip, tar, tar.gz) with extraction.

## Run

```sh
cargo run -p rcmd-tui     # or: just run
```

```
usage: rcmd [-P FILE] [DIR1 [DIR2]]
```

To make your shell follow rcmd's last directory on exit (the mc-wrapper
trick), add this to your shell config:

```sh
# bash/zsh
rc() {
    local tmp; tmp="$(mktemp)"
    rcmd -P "$tmp" "$@"
    local dir; dir="$(cat -- "$tmp" 2>/dev/null)"
    rm -f -- "$tmp"
    [ -n "$dir" ] && [ -d "$dir" ] && cd -- "$dir"
}
```

```fish
# fish (~/.config/fish/functions/rc.fish)
function rc
    set -l tmp (mktemp)
    rcmd -P $tmp $argv
    set -l dir (cat $tmp 2>/dev/null)
    rm -f $tmp
    test -n "$dir" -a -d "$dir"; and cd $dir
end
```

## Keys

| Key | Action |
|-----|--------|
| Tab | Switch panel |
| ↑ ↓ PgUp PgDn Home End | Move cursor |
| Enter | Enter directory or archive (.zip/.tar/.tar.gz/.tgz) |
| Backspace | Parent directory / leave archive |
| F1 | Help |
| F3 | View file (internal viewer) |
| F4 | Edit file in $VISUAL / $EDITOR (fallback vi) |
| F9 | Pulldown menu |
| Insert, Ctrl+T | Mark entry and advance |
| `+` / `-` (or `\`) | Select / unselect by glob |
| `*` | Invert marks |
| F5 | Copy marked (or cursor) entry |
| F6 | Move / rename |
| F7 | Make directory |
| F8 | Delete to trash |
| Shift+F8 | Delete permanently |
| Alt+N / E / S / T | Sort by name / ext / size / mtime (again = reverse) |
| Alt+. | Toggle hidden files |
| Ctrl+S | Quick search (type-ahead; Ctrl+S again = next match) |
| Ctrl+F | Filter shown files by glob (`*` or empty clears) |
| Ctrl+\ | Directory hotlist (Enter cd, `a` add current, `d` delete) |
| Ctrl+R | Reload panel |
| Esc | Cancel dialog / running operation / clear command line |
| F10 | Quit |

Typing goes to the **command line** at the bottom; Enter runs it in the
active panel's directory (`cd` changes the panel instead). Alt+Enter
inserts the selected filename, Ctrl+P/Ctrl+N walk history, Ctrl+A/E/U are
readline-style. Ctrl+O suspends to a full shell — `exit` to come back.
The `+`/`-`/`*`/`\` selection keys apply only while the command line is
empty.

In dialogs: arrows/Tab move between buttons, Enter confirms, Esc cancels;
overwrite and error prompts also take hotkeys (o/a/s/S, r/s/S).

**Viewer** (F3): arrows/PgUp/PgDn/Home/End scroll, ←→ horizontal scroll,
F4 toggles hex mode, F7 or `/` searches (case-insensitive), `n` next
match, F3/F10/Esc/q quit. Lines are indexed lazily, so huge files open
instantly; very long lines are broken at 4096 columns.

**Archives**: Enter on a `.zip`, `.tar`, `.tar.gz`/`.tgz` file browses it
like a directory (read-only — the panel title shows `archive://path`).
F5 copies members out with the usual progress/overwrite dialogs, F3
views them; move, delete, and mkdir are disabled inside. The archive
index loads once at open; each member read decodes only that member.

## Configuration

`~/.config/rcmd/config.toml` (created/rewritten on exit — panel sort
mode, hidden-file setting, and the hotlist persist automatically):

```toml
theme = "mc"        # or "dark" (truecolor); applied at startup
keymap = "mc"       # or "modern": Left/Right = parent/enter (lynx style)
show_hidden = true
sort_key = "name"   # name | ext | size | mtime
sort_reverse = false

[keys]              # custom bindings on top of the preset
"ctrl+y" = "swap-panels"
# key syntax:  [ctrl+][alt+][shift+]<key>  (f1..f20, letters, +, -, etc.)
# actions: help view edit copy move mkdir delete delete-perm select-group
#   unselect-group invert-selection quit shell reload swap-panels
#   toggle-hidden sort-name sort-ext sort-size sort-mtime sort-reverse
#   menu mark quick-search hotlist filter up-dir enter

[[hotlist]]
label = "projects"
path = "/home/you/git"
```

## Development

```sh
cargo test --workspace                              # tests (rcmd-core is TUI-free)
cargo clippy --workspace --all-targets -- -D warnings
```

Workspace layout: `crates/rcmd-core` (panel/fs logic, no TUI deps),
`crates/rcmd-tui` (ratatui frontend, binary `rcmd`).
