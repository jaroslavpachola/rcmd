# rcmd

A Midnight Commander replacement in Rust: orthodox dual-pane file manager
with MC keybindings, built on ratatui. The original roadmap
([docs/PLAN.md](docs/PLAN.md)) is complete; the 2.0 roadmap is
[docs/PLAN2.md](docs/PLAN2.md).

## Status

All milestones of the original plan shipped, plus the debt list: marking
and F5–F8 operations with MC-style dialogs (mtimes preserved on copy),
command line + shell integration with real job control, F3 chunked
viewer with wrap and hex modes, F4 $EDITOR, F9 menu, F1 help, config
file with keymap presets/custom bindings, quick search, filter, hotlist,
themes, archive browsing (zip, tar, tar.gz, tar.xz, tar.bz2) with
extraction, and copying into zip archives. From the 2.0 roadmap: find
file / panelize / directory compare, non-blocking listings with
filesystem watching, **SFTP remote panels** (browse, transfer, edit on
servers), a **built-in editor** with syntax highlighting, and UX depth:
mouse support, per-panel directory history, quick view, and git status
in the panels — see below.

## Install & run

```sh
cargo install --path crates/rcmd-tui   # installs the `rcmd` binary
# or during development:
cargo run -p rcmd-tui                  # or: just run
```

Release binaries for Linux are attached to GitHub releases (built by
`.github/workflows/release.yml` on `v*` tags; macOS builds are
temporarily suspended).

```
usage: rcmd [-P FILE] [DIR1 [DIR2]]   (-V version, -h help)
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
| Enter | Enter directory or archive (zip, tar, tar.{gz,xz,bz2}) |
| Backspace | Parent directory / leave archive |
| F1 | Help |
| F3 | View file (internal viewer) |
| F4 | Edit file (built-in editor; `editor = "external"` for $EDITOR) |
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
| Alt+F7 | Find file (glob + optional content); results panelized |
| Alt+← / Alt+→ | Directory history back / forward (per panel) |
| Alt+↑ | Directory hotlist |
| Ctrl+X d | Compare directories (marks differences in both panels) |
| Ctrl+X q | Quick view: other panel previews the cursor file |
| Ctrl+Space | Directory size (background scan into the Size column) |
| Ctrl+R | Reload panel (also restores listing after find/panelize) |
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
F2 toggles soft-wrap, F4 toggles hex mode, F7 or `/` searches
(case-insensitive), `n` next match, F3/F10/Esc/q quit. Lines are indexed
lazily, so huge files open instantly; very long lines are broken at 4096
columns.

**Responsiveness**: directory listings that take longer than ~100 ms
(huge directories, cold network mounts) load in the background — the old
listing stays up with a spinner, typing never blocks, Esc cancels.
Panels also auto-reload when their directory changes on disk (debounced;
`watch = false` in the config disables it).

**Power tools**: Alt+F7 opens find file — a filename glob plus an
optional case-insensitive content substring; matches stream live into
the active panel as a *panelized* listing (paths relative to the panel
dir), where marking and F5/F6/F8 work as usual. *Panelize command…*
(F9 → Command) turns any command's stdout lines into such a listing
(`git ls-files -m`, `rg -l TODO`, …). *Compare directories* (Ctrl+X d)
marks files that are missing on the other side or differ in size/mtime
in both panels — then a plain F5 copies the marked differences across.

**Mouse**: click focuses a panel and moves the cursor, double-click
enters, the wheel scrolls whatever it hovers (panels, viewer, editor,
quick view), the bottom keybar and the F9 menu are clickable, and a
click in the editor places the cursor. All additive — every feature
stays keyboard-reachable. Hold Shift to select terminal text as usual;
`mouse = false` in the config turns capture off entirely.

**Panel history**: each panel remembers where it has been —
Alt+←/Alt+→ walk back and forward browser-style (sftp:// locations
reconnect through the connection cache), Alt+↑ opens the hotlist.

**Quick view** (Ctrl+X q): the other panel becomes a live preview of
the file under the cursor, updating as you move. It uses the viewer's
chunked reader, so previewing a multi-GB log is instant. Tab focuses
the preview for scrolling (arrows/PgUp/PgDn); Ctrl+X q turns it off.

**Git awareness**: inside a git work tree the panel title shows the
branch (`[main]`) and each entry gets a one-cell status column —
`M` modified, `A` added, `?` untracked, `!` ignored (ignored entries
are dimmed); changes deep inside a subdirectory mark the subdirectory.
Statuses are computed on a background thread so huge repositories never
block the UI. Built behind the default-on `git` cargo feature;
`git = false` in the config disables it at runtime.

**Archives**: Enter on a `.zip`, `.tar`, `.tar.gz`/`.tgz`,
`.tar.xz`/`.txz`, or `.tar.bz2`/`.tbz2` file browses it like a directory
(the panel title shows `archive://path`). F5 copies members out with the
usual progress/overwrite dialogs, F3 views them; move, delete, and mkdir
are disabled inside. Copying **into** an archive works for zip only
(members are appended in place — tar formats would need a full rewrite):
F5 with the destination panel inside a zip, or any destination written
as `path/to/archive.zip://dir`. The archive index loads once at open;
each member read decodes only that member.

**Editor** (F4): a built-in mcedit-style editor. F2 saves (atomically,
preserving permissions and CRLF line endings), F3 starts marking
(Shift+arrows also select), Ctrl+C/X/V copy/cut/paste, Ctrl+Z/Ctrl+Y
undo/redo (unlimited, with typing bursts grouped), F7 searches with a
smartcase regex and Shift+F7 repeats, F4 replaces interactively
(Replace / Skip / All / Quit), F8 deletes the selection or line, Enter
auto-indents, Ctrl+arrows hop words, and F10/Esc quits (asking
Save/Discard/Cancel when modified). Known file types get syntect syntax
colors (skipped for files over 2 MB — a 50 MB log still opens in about
0.2 s). On an SFTP panel F4 edits a local scratch copy and uploads it
back when you close the editor. Set `editor = "external"` in the config
to keep using $VISUAL/$EDITOR.

**Remote filesystems (SFTP)**: `cd sftp://[user@]host[:port][/path]`
(or F9 → Command → SFTP link) connects a panel to a server — user
defaults to your login, path to the remote home. Authentication tries
your ssh-agent, then the default `~/.ssh/id_*` keys, then asks for a
password; host keys are checked against `~/.ssh/known_hosts`, and
unknown hosts show a fingerprint dialog before being saved. The panel
title shows the URL. Everything works panel-normally: F5/F6 transfer
between local and remote (or between two remote directories) with the
usual progress/overwrite dialogs, F7 creates server directories, F8
deletes on the server (permanently — there is no remote trash), F3
views, and F4 edits a local scratch copy that is uploaded back when the
editor saved it. `cd path` stays on the server; plain `cd` (or any `~`
path) returns the panel to the local filesystem, and closing the last
remote panel closes the connection. Both panels can share one
connection — put the same host on both sides, or compare a local tree
against a remote one with Ctrl+X d and F5 the differences across. The
hotlist stores sftp:// entries, so `Ctrl+\` + Enter reconnects.

## Configuration

`~/.config/rcmd/config.toml` (created/rewritten on exit — panel sort
mode, hidden-file setting, and the hotlist persist automatically):

```toml
theme = "mc"        # or "dark" (truecolor); applied at startup
keymap = "mc"       # or "modern": Left/Right = parent/enter (lynx style)
watch = true        # auto-reload panels on external changes
mouse = true        # click/double-click/wheel support
git = true          # git status column + branch in panel titles
editor = "internal" # or "external" ($VISUAL/$EDITOR for F4)
show_hidden = true
sort_key = "name"   # name | ext | size | mtime
sort_reverse = false

[keys]              # custom bindings on top of the preset
"ctrl+y" = "swap-panels"
# key syntax:  [ctrl+][alt+][shift+]<key>  (f1..f20, letters, +, -, etc.)
# actions: help view edit copy move mkdir delete delete-perm select-group
#   unselect-group invert-selection quit shell reload swap-panels
#   toggle-hidden sort-name sort-ext sort-size sort-mtime sort-reverse
#   menu mark quick-search hotlist filter up-dir enter history-back
#   history-forward quick-view sftp-link find-file panelize compare-dirs
#   dir-size

[[hotlist]]
label = "projects"
path = "/home/you/git"
```

## Development

```sh
cargo test --workspace                              # unit tests (rcmd-core is TUI-free)
cargo clippy --workspace --all-targets -- -D warnings
python3 tests/e2e/run.py                            # drives the real binary in a pty
just check                                          # all of the above
```

The e2e suite includes an SFTP scenario that spins up a local paramiko
server (`pip install paramiko`; skipped when unavailable).

Workspace layout: `crates/rcmd-core` (panel/fs logic, no TUI deps),
`crates/rcmd-edit` (editor buffer/undo/search, TUI-free; syntect behind
the `syntax` feature), `crates/rcmd-tui` (ratatui frontend, binary
`rcmd`). CI runs fmt,
clippy, unit tests, and the pty e2e suite on Linux (macOS temporarily
suspended). Licensed MIT.
