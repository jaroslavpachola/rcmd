# rcmd

A Midnight Commander replacement in Rust: orthodox dual-pane file manager
with MC keybindings, built on ratatui. See [docs/PLAN.md](docs/PLAN.md) for
the roadmap.

## Status

M3 — view & edit. The M1 daily-driver cut (marking, copy/move/mkdir/delete
with MC-style dialogs, sort modes), M2 shell integration (command line,
Ctrl+O, exit-to-cwd), plus the F3 viewer (chunked — opens multi-GB files
instantly; text and hex modes, search), F4 editing via $EDITOR, and the
F9 pulldown menu.

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
| Enter | Enter directory |
| Backspace | Parent directory |
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

## Development

```sh
cargo test --workspace                              # tests (rcmd-core is TUI-free)
cargo clippy --workspace --all-targets -- -D warnings
```

Workspace layout: `crates/rcmd-core` (panel/fs logic, no TUI deps),
`crates/rcmd-tui` (ratatui frontend, binary `rcmd`).
