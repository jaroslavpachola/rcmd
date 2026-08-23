# rcmd

A Midnight Commander replacement in Rust: orthodox dual-pane file manager
with MC keybindings, built on ratatui. The 1.0
([docs/PLAN.md](docs/PLAN.md)), 2.0 ([docs/PLAN2.md](docs/PLAN2.md)) and
3.0 ([docs/PLAN3.md](docs/PLAN3.md)) roadmaps are complete - 3.0's
flagship is the **persistent subshell** behind Ctrl+O. 4.0, the parity
release ([docs/PLAN4.md](docs/PLAN4.md)), is under way: its scope comes
from a decision-by-decision comparison against mc
([docs/MC-DIFF.md](docs/MC-DIFF.md)); its foundations phase shipped in
3.8 and the panel work has been landing since.

![rcmd demo: browsing, the syntax-highlighted viewer, marking and
copying, the persistent subshell](docs/demo.gif)

*(recorded by [tests/e2e/record_demo.py](tests/e2e/record_demo.py) -
the same pty harness that runs the test suite - and rendered with
[agg](https://github.com/asciinema/agg))*

## Status

**2.0** - complete MC-workflow parity and beyond: marking and F5–F8
operations with MC-style dialogs (mtimes preserved, F8 goes to trash),
command line + shell integration with real job control, F3 chunked
viewer with wrap and hex modes, F9 menu, F1 help, config file with
keymap presets/custom bindings, quick search, filter, hotlist, themes,
archive browsing (zip, tar, tar.{gz,xz,bz2}) with extraction and
copy-into-zip; find file / panelize / directory compare, non-blocking
listings with filesystem watching, **SFTP remote panels**, a **built-in
editor** with syntax highlighting, mouse support, per-panel directory
history, quick view, info panel, listing formats, git status in the
panels, **openers and an F2 user menu**, and MC's ESC-prefix - see
below.

**3.0** - the live commander: the persistent **subshell**
(Ctrl+O), SFTP auth depth (passphrase keys, keyboard-interactive),
bulk rename via the editor, viewer follow mode (tail&nbsp;-f) with
syntax highlighting and precise search-match highlighting, `[[view]]`
filters (F3 through `pdftotext` & co.), Tab path completion,
gitignore-aware find, recent directories in the hotlist, a **job
queue** with background transfers, chmod/chown/symlink dialogs, editor
soft-wrap + `$1` capture groups + block ops, copy *into* tar, **rar and
7z browsing** (via 7z/unrar), click-to-sort headers, and an MC alias
batch (S-F4/S-F5/S-F6, C-x t/p, M-c quick cd…).

**3.8** - toward parity with mc: `config.toml` is now yours alone, with
everything rcmd changes itself moved to a state file; F9 > Options >
Panel options is one grouped dialog covering mc's whole setting
surface; `[keys.viewer]` and `[keys.editor]` rebind inside the viewer
and the editor; `rcmd --import-mc` converts an existing mc
configuration; the command line gained mc's keys, macros and a
persistent history; and the panels gained the **Layout** settings
(horizontal split, adjustable ratio, optional bars), a **per-panel mini
status** and the **multi-column brief listing**.

**3.10** - the panels themselves: mc's **directory tree**, both as the
Command-menu dialog (Enter moves this panel) and as a panel listing mode
(Enter moves the other one and the tree stays), scanned on demand so
there is no tree cache to go stale; and the **user-defined listing
format**, where `listing = "user"` draws whatever `listing_format` names
in mc's own format language - `half type name | size | mtime` and the
other fifteen fields, with widths that grow.

**3.11** - `[[highlight]]` colour rules: entries are painted by glob or
by kind, which is mc's filehighlight without the second file.

**3.12** - mc's menu bar: **Left, File, Command, Options, Right**, where
the two panel menus act on their own panel whichever one has the focus.

## Install & run

```sh
cargo install --git https://github.com/jaroslavpachola/rcmd rcmd-tui
# from a checkout:
cargo install --path crates/rcmd-tui   # installs the `rcmd` binary
# or during development:
cargo run -p rcmd-tui                  # or: just run
```

Release binaries for Linux are attached to GitHub releases (built by
`.github/workflows/release.yml` on `v*` tags; macOS builds are
temporarily suspended). Two tarballs per release: the glibc build and a
**static musl build** (`x86_64-unknown-linux-musl`, C dependencies
vendored) that runs on any distro with no shared-library requirements.

```
usage: rcmd [-P FILE] [DIR1 [DIR2]]   (-V version, -h help)
       rcmd --import-mc [MC_CONFIG_DIR]
```

Coming from mc? `rcmd --import-mc` reads your `menu`, `mc.ext` and
`mc.keymap` and prints the equivalent rcmd config on stdout - user menu
entries, openers, view filters and panel key bindings. It never touches
your `config.toml`; review what it prints and paste what you want.
Anything with no rcmd equivalent (`type/` matchers, `%cd` commands,
unsupported macros) is reported on stderr rather than guessed at.

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
| F2 | User menu (`[[commands]]` from the config) |
| F3 | View file (internal viewer) |
| F4 | Edit file (built-in editor; `editor = "external"` for $EDITOR) |
| F9 | Pulldown menu (highlighted letters are hotkeys: `F9 o p` = Panel options) |
| Insert, Ctrl+T | Mark entry and advance |
| `+` / `-` (or `\`) | Select / unselect by glob |
| `*` | Invert marks |
| Ctrl+X l / s / v | Hard link / absolute symlink / relative symlink to the cursor entry |
| Ctrl+X Ctrl+S | Change where an existing symlink points |
| F5 | Copy marked (or cursor) entry - opens MC's form: source mask, destination, preserve attributes / follow links / dive into subdirs / stable symlinks, and OK / Background / Cancel |
| F6 | Move / rename |
| F7 | Make directory |
| F8 | Delete to trash |
| Shift+F8 | Delete permanently |
| Alt+N | Sort by name (again = reverse; other orders in the panel's own F9 → Left/Right menu) |
| Alt+T | Cycle listing format: brief (names in columns) / full / long (active long panel = full-width one-panel view) |
| Ctrl+U | Swap panels |
| Alt+. | Toggle hidden files |
| Ctrl+S, Alt+S | Quick search (type-ahead; Ctrl+S again = next match) |
| Ctrl+F | Filter shown files by glob (`*` or empty clears) |
| Ctrl+\ | Directory hotlist (Enter cd, `a` add current, `d` delete) |
| Alt+F7 | Find file (glob + optional content); results panelized |
| Alt+← / Alt+→ | Directory history back / forward (per panel) |
| Alt+↑ | Directory hotlist |
| Ctrl+X d | Compare directories (marks differences in both panels) |
| Ctrl+X ! | Panelize a command's output |
| Alt+H | Command history (kept across sessions); Alt+P / Alt+N walk it |
| Alt+A | Insert the panel's path on the command line |
| Ctrl+X q | Quick view: other panel previews the cursor file |
| Ctrl+X i | Info panel: other panel shows the cursor file's full stat |
| Alt+i / Alt+o | Other panel: same directory / directory under cursor |
| Ctrl+Space | Directory size (background scan into the Size column) |
| Ctrl+R | Reload panel (also restores listing after find/panelize) |
| Esc | Cancel dialog / running operation / clear command line |
| Esc *key* | MC meta prefix: Esc 1…0 = F1…F10, Esc x = Alt+X, Esc Esc = Esc |
| F10 | Quit |

Typing goes to the **command line** at the bottom; Enter runs it in the
active panel's directory (`cd` changes the panel instead, and `cd -`
goes back to where the panel came from; a relative `cd` that misses
locally also tries `$CDPATH`). MC's macros expand there too - `%f` the
cursor file, `%d` this directory, `%D` the other panel's, `%t` the
marked files. Alt+Enter
inserts the selected filename, Ctrl+P/Ctrl+N walk history, Ctrl+A/E are
readline-style and Esc clears the line (Ctrl+U swaps panels, like MC).
The `+`/`-`/`*`/`\` selection keys apply only while the command line is
empty.

**The subshell** (Ctrl+O): a persistent `$SHELL` runs on its own pty for
the whole session, exactly like MC's. Ctrl+O flips between the panels
and its screen - the last command's output is still there - and typed
commands run *inside* it, so aliases, functions, history and `$?`
survive between commands. cd sync goes both ways: the panels follow a
`cd` typed in the subshell, and the subshell is moved to the active
panel's directory before running anything. `exit` respawns it. bash,
zsh and fish get a prompt hook for precise tracking; plain POSIX `sh`
works with a `/proc`-based fallback. `subshell = false` in the config
restores the old one-shot execution (also the automatic fallback if the
shell cannot be spawned).

In dialogs: arrows/Tab move between buttons, Enter confirms, Esc cancels;
overwrite and error prompts also take hotkeys (o/a/s/S, r/s/S). The
The copy/move form takes MC's **source mask**: `*.tar.gz` with a
destination of `dir/*.tgz` copies `foo.tar.gz` to `dir/foo.tgz`, and
files the mask does not match stay where they are. The mask's wildcards
are numbered left to right - `*` in the destination is the first,
`\1`..`\9` any of them, `\0` the whole name - and `\u \l \U \L \E`
change case. (Regex renaming with capture groups lives in F9 > File >
Bulk rename, which is a better place for it than a one-line field.)

The overwrite prompt is MC's: both files' size and date on screen, then
**Overwrite / Append / Reget** for this file and **All / Update / Size
differs / None** for every remaining one (Up/Down switch rows). Append
and Reget - MC's resume - need a local file on both sides.

Esc doubles as MC's meta prefix everywhere: after a lone Esc, a digit is
an F-key (Esc 1 = F1 … Esc 0 = F10) and any other key gets Alt added -
handy on terminals without working F-keys or Alt. Esc Esc is a real
Escape; an unanswered Esc acts as one after a second.

**Viewer** (F3): arrows/PgUp/PgDn/Home/End scroll, ←→ horizontal scroll,
F2 toggles soft-wrap, F4 toggles hex mode, F3/F10/Esc/q quit. Lines are
indexed lazily, so huge files open instantly; very long lines are broken
at 4096 columns.

F7 or `/` opens mc's **search dialog**: the pattern, and the four
answers that change what it means. The pattern is read as **Normal**
(a literal), a **Regular expression**, or **Hexadecimal** bytes -
`7f454c46`, `7f 45 4c 46` and `0x7f 0x45 0x4c 0x46` are the same four,
and it is the only way to look for something that is not text. Alongside
it: **Case sensitive**, **Whole words** and **Backwards**. Tab and the
arrows move between rows, Space ticks, Enter searches; `n` repeats the
search with its options intact. Matches are highlighted in the line and
the found line is marked.

**Responsiveness**: directory listings that take longer than ~100 ms
(huge directories, cold network mounts) load in the background - the old
listing stays up with a spinner, typing never blocks, Esc cancels.
Panels also auto-reload when their directory changes on disk (debounced;
`watch = false` in the config disables it).

**Power tools**: Alt+F7 opens find file - a filename glob plus an
optional case-insensitive content substring; matches stream live into
the active panel as a *panelized* listing (paths relative to the panel
dir), where marking and F5/F6/F8 work as usual. *Panelize command…*
(F9 → Left/Right) turns any command's stdout lines into such a listing
(`git ls-files -m`, `rg -l TODO`, …). *Compare directories* (Ctrl+X d)
marks files that are missing on the other side or differ in size/mtime
in both panels - then a plain F5 copies the marked differences across.

**Mouse**: click focuses a panel and moves the cursor, double-click
enters, the wheel scrolls whatever it hovers (panels, viewer, editor,
quick view), the bottom keybar and the F9 menu are clickable, and a
click in the editor places the cursor. All additive - every feature
stays keyboard-reachable. Hold Shift to select terminal text as usual;
`mouse = false` in the config turns capture off entirely.

**Panel history**: each panel remembers where it has been -
Alt+←/Alt+→ walk back and forward browser-style (sftp:// locations
reconnect through the connection cache), Alt+↑ opens the hotlist.

**Quick view** (Ctrl+X q): the other panel becomes a live preview of
the file under the cursor, updating as you move. It uses the viewer's
chunked reader, so previewing a multi-GB log is instant. Tab focuses
the preview for scrolling (arrows/PgUp/PgDn); Ctrl+X q turns it off.

**Openers & user commands**: `[[open]]` rules in the config make Enter
open files by type - the first matching glob wins (case-insensitive):

```toml
[[open]]
match = "*.pdf"
run = "zathura %f >/dev/null 2>&1 &"
```

Openers run without a "press Enter" pause, so terminal programs (mpv,
less) feel native and GUI programs just need a trailing `&`. With
lynx-like motion on, Right still only enters directories - Enter opens.
`[[commands]]` are named shell templates listed in the **F2 user menu**
(first nine get digit hotkeys) and optionally bound directly:

```toml
[[commands]]
name = "git status"
run = "git status | less"
key = "ctrl+g"
```

Both expand macros before running in the active panel's directory:
`%f` the cursor file, `%d` this directory, `%D` the other panel's
directory, `%t` all marked files, `%%` a literal percent - everything
shell-quoted.

**File properties**: the info panel (Ctrl+X i) turns the other panel
into a live stat display of the file under the cursor - type, size,
permissions, owner and group (resolved locally, numeric on SFTP),
hard links, inode, and all three timestamps - plus the filesystem's
free space, which also shows in every local panel's footer. Listing
formats are switchable per panel from F9 → Left/Right: *brief* (names only,
full width), *full* (the classic name/size/mtime), and *long*
(ls-style perms/owner/group/size/name). A long listing needs room, so
while the *active* panel is long it takes the whole width and the
other panel is hidden - MC's one-panel view; Tab to the other panel
(or cycle the format back) and the split returns. The choice persists
in the config (`listing`).

**Git awareness**: inside a git work tree the panel title shows the
branch (`[main]`) and each entry gets a one-cell status column -
`M` modified, `A` added, `?` untracked, `!` ignored (ignored entries
are dimmed); changes deep inside a subdirectory mark the subdirectory.
Statuses are computed on a background thread so huge repositories never
block the UI. Built behind the default-on `git` cargo feature;
`git = false` in the config disables it at runtime.

**Archives**: Enter on a `.zip`, `.tar` or `.cpio` - plain or wrapped in
`.gz`, `.xz`, `.bz2` or `.zst`, with the usual short spellings
(`.tgz`, `.txz`, `.tbz2`, `.tzst`) - browses it like a directory (the
panel title shows `archive://path`). cpio is read in all three of its
header shapes - `newc`/`crc`, the portable octal `odc`, and the old
binary one in either byte order - and a hard link inside one lists and
opens as the file it shares its bytes with.

`ar` archives open too, which is how a `.a` static library lists its
members, and a **Debian package** (`.deb`, `.udeb`) opens as one tree
rather than three: `debian-binary` at the root, the metadata and
maintainer scripts under `CONTROL/`, and everything the package
installs under `CONTENTS/`.

**FISH**: `cd fish://[user@]host[:port][/path]` puts a panel on a
server that has a shell but no SFTP subsystem. It is the same SSH
connection, the same authentication and the same host-key dialog; what
differs is what happens after login. Every operation is one small
command, and the listing comes back as NUL-separated records rather than
`ls -l` output, so a filename containing a space, a newline or a `->`
survives - which `ls -l` cannot promise. `stat(1)` is used where the
server has it and an `ls`-based fallback where it does not.

**FTP**: `cd ftp://[user[:password]@]host[:port][/path]` connects a
panel to an FTP server - no user means the anonymous login. Listings
prefer `MLSD`, which says what everything is, and fall back to `LIST`
where the server is too old for it. Browsing, F3, F5 in both directions,
F6, F7 and F8 all work; FTP has no symlinks and no way to change
ownership, so those report that rather than pretending. Every transfer
needs a connection of its own, so a small pool of logged-in ones is kept
and reused: one login covers a whole session of listing and copying.

**Ctrl+X A** lists what the panels are sitting on that is not the local
filesystem - open archives and live SFTP connections, with the panel
each one belongs to. Enter goes there (a connection is reused, so no
second login), `f` frees one: the panel returns to a local directory and
an idle connection is forgotten.

**rar, 7z, lha/lzh, arj and cab** browse through an installed `7z`
(p7zip - rar needs its nonfree codec) or `unrar`, read-only and streamed
one member at a time. Without one of those tools installed, opening one
says which tool it wants rather than failing silently.

An **mbox** (`.mbox`, `.mbx`, plain or compressed) browses as the
messages in it, each numbered so name order is arrival order and named
by its subject - decoded, since real mail writes subjects as
`=?UTF-8?B?...?=`. Opening one gives an ordinary RFC 822 message,
without the `From ` separator line the mbox format puts between them.

A **patch** (`.patch`, `.diff`, plain or compressed) browses as the tree
it would apply to: one entry per file it touches, holding that file's
hunks and nothing else, filed under the directories its paths name.
Unified, git, context and Subversion headers all start a section.
Nothing is applied or reversed - this is a way of reading a patch, not
of using one.

An **ISO 9660 image** (`.iso`) browses in place. **Rock Ridge** names,
modes and symlinks are used where the disc carries them, **Joliet**'s
UTF-16 names where it does not, and the base format's shouted 8.3 names
(minus the `;1` version suffix) where it has neither.

An **RPM package** (`.rpm`, source packages included) takes the same
shape. `CONTROL/header` is the package's tags rendered as text - name,
version, license, summary, description, what the payload is wrapped in -
and any install scriptlets sit beside it as `prein`, `postin`, `preun`,
`postun`. `CONTENTS/` is the payload, which is a cpio stream under gzip,
xz, lzma, bzip2 or zstd. Signatures are stepped over, not checked: a
listing is not a claim that a package is authentic. F5 copies members out with the
usual progress/overwrite dialogs, F3 views them; move, delete, and mkdir
are disabled inside. Copying **into** an archive works for zip and tar: F5 with the
destination panel inside one, or any destination written as
`path/to/archive.zip://dir`. A member with the same name is **replaced**,
not shadowed by a second copy of the name. The archive index loads once
at open; each member read decodes only that member.

Inside a `.zip` or `.tar`, **F8 deletes**, **F6 renames** (type a bare
name - an absolute one would mean leaving the archive, which is a copy)
and **F7 makes a directory**. Each batch rewrites the container once, so
deleting five members costs one rewrite rather than five, and the
original is only replaced when the new one is complete. The other
formats - deb, rpm, iso, cpio and the 7z-backed ones - stay read-only.

**Editor** (F4): a built-in mcedit-style editor. F2 saves (atomically,
preserving permissions and CRLF line endings), F3 starts marking
(Shift+arrows also select), Ctrl+C/X/V copy/cut/paste, Ctrl+Z/Ctrl+Y
undo/redo (unlimited, with typing bursts grouped), F7 searches with a
smartcase regex and Shift+F7 repeats, F4 replaces interactively
(Replace / Skip / All / Quit), F8 deletes the selection or line, Enter
auto-indents, Ctrl+arrows hop words, and F10/Esc quits (asking
Save/Discard/Cancel when modified). Known file types get syntect syntax
colors (skipped for files over 2 MB - a 50 MB log still opens in about
0.2 s). On an SFTP panel F4 edits a local scratch copy and uploads it
back when you close the editor. Set `editor = "external"` in the config
to keep using $VISUAL/$EDITOR.

**Remote filesystems (SFTP)**: `cd sftp://[user@]host[:port][/path]`
(or F9 → Left/Right → SFTP link) connects a panel to a server - user
defaults to your login, path to the remote home. Authentication tries
your ssh-agent, then the default `~/.ssh/id_*` keys, then asks for a
password; host keys are checked against `~/.ssh/known_hosts`, and
unknown hosts show a fingerprint dialog before being saved. The panel
title shows the URL. Everything works panel-normally: F5/F6 transfer
between local and remote (or between two remote directories) with the
usual progress/overwrite dialogs, F7 creates server directories, F8
deletes on the server (permanently - there is no remote trash), F3
views, and F4 edits a local scratch copy that is uploaded back when the
editor saved it. `cd path` stays on the server; plain `cd` (or any `~`
path) returns the panel to the local filesystem, and closing the last
remote panel closes the connection. Both panels can share one
connection - put the same host on both sides, or compare a local tree
against a remote one with Ctrl+X d and F5 the differences across. The
hotlist stores sftp:// entries, so `Ctrl+\` + Enter reconnects.

## Configuration

`~/.config/rcmd/config.toml` is **yours** - rcmd only ever reads it, so
comments and hand formatting survive. Everything rcmd changes itself
(panel sort mode, hidden files, listing format, the hotlist, and every
options-form toggle) goes to `$XDG_STATE_HOME/rcmd/state.toml`
(`~/.local/state/rcmd/state.toml` by default) and takes precedence over
the config file. State is sparse - only keys you actually changed in the
UI are stored, so a config edit keeps working for everything else - and
writes merge into the on-disk file, so several rcmd instances never
clobber each other. Upgrading from 3.x: state keys still in your
`config.toml` (`show_hidden`, `sort_key`, `sort_reverse`, `listing`,
`[[hotlist]]`) are migrated once on first start; they stay honoured for
one release, then stop being read.

The settings live in one sectioned checkbox form under **F9 → Options →
Panel options**, applied live: *Layout* (split direction and size, the
per-panel mini status, and which of the menu bar / status line /
command line / key bar are drawn), *Panel* (hidden files, lynx-like motion,
mouse, auto-reload, git), *Confirmation* (ask before deleting /
overwriting / quitting), *Shell and editor* (persistent subshell,
internal or external editor) and *Appearance* (theme):

```toml
theme = "mc"        # or "dark" (truecolor); applied at startup
keymap = "mc"       # or "modern" (= lynx-like motion on by default)
lynx = false        # Left/Right = parent/enter; in the options form
watch = true        # auto-reload panels on external changes
mouse = true        # click/double-click/wheel support
git = true          # git status column + branch in panel titles
editor = "internal" # or "external" ($VISUAL/$EDITOR for F4)
subshell = true     # persistent $SHELL behind Ctrl+O (false = one-shot exec)
brief_columns = 2          # name columns in the brief listing (1..6)
split = "vertical"         # or "horizontal" (panels stacked)
split_ratio = 50           # percent for the left/top panel, 20..80
show_menubar = false       # MC's permanent menu bar (F9 works either way)
show_mini_status = false   # a status row inside each panel (MC's)
show_status = true         # the line describing the cursor entry
show_cmdline = true        # the command line
show_keybar = true         # the F1..F10 bar along the bottom
confirm_delete = true      # ask before F8 / Shift+F8
confirm_overwrite = true   # ask before overwriting during copy/move
confirm_exit = false       # ask before F10 quits (MC asks; rcmd does not)
confirm_hotlist_delete = true   # ask before dropping a hotlist entry
confirm_execute = false    # ask before Enter runs an [[open]] command
esc_timeout_ms = 250 # how long a lone Esc waits for its meta follow-up
                     # (1000 = MC's roomier window for typing Esc 1..0)
show_hidden = true
sort_key = "name"   # name | ext | size | mtime
sort_reverse = false
listing = "full"    # brief | full | long | tree | user
# "user" draws listing_format: a panel size (half/full), an optional
# repeat count 1-9, then fields with optional :width (:width+ grows) -
# name size bsize type mark mtime atime ctime perm mode nlink ngid nuid
# owner group inode, plus "space" and "|". MC's Full listing written out:
listing_format = "half type name | size | mtime"

[keys]              # custom bindings on top of the preset
"ctrl+y" = "swap-panels"     # bare entries bind in the panel
[keys.viewer]                # ...and these inside the F3 viewer
"ctrl+w" = "wrap"            # quit wrap hex search search-next follow
[keys.editor]                # ...and these inside the F4 editor
"ctrl+q" = "quit"            # save quit mark replace search search-next
                             # block-copy block-move delete-line undo
                             # redo copy cut paste select-all wrap
# key syntax:  [ctrl+][alt+][shift+]<key>  (f1..f20, letters, +, -, etc.)
# actions: help view edit copy move mkdir delete delete-perm select-group
#   unselect-group invert-selection quit shell reload swap-panels
#   toggle-hidden sort-name sort-ext sort-size sort-mtime sort-reverse
#   menu mark quick-search hotlist filter up-dir enter history-back
#   history-forward quick-view info-view user-menu listing-brief
#   listing-full listing-long listing-tree listing-user listing-cycle
#   other-same-dir other-open-dir sftp-link find-file panelize
#   compare-dirs dir-size dir-tree

[[highlight]]          # MC's filehighlight, as rules: first match wins
match = "*.tar.gz"     # a glob on the name...
color = "brightred"    # ...mc's colour names, #rrggbb or "default"

[[highlight]]
type = "exe"           # ...or what the entry is: dir linkdir exe link
color = "magenta"      #    broken file
bold = true            # optional; left out, the kind's own weight stands

[[hotlist]]
label = "projects"
path = "/home/you/git"

[[open]]                    # Enter on a matching file runs this
match = "*.pdf"
run = "zathura %f >/dev/null 2>&1 &"

[[commands]]                # F2 user menu; key = "..." binds directly
name = "git status"
run = "git status | less"
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
