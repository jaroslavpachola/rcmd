# rcmd

A Midnight Commander replacement in Rust: orthodox dual-pane file manager
with MC keybindings, built on ratatui. All four roadmaps are complete -
1.0 ([docs/PLAN.md](docs/PLAN.md)), 2.0
([docs/PLAN2.md](docs/PLAN2.md)), 3.0
([docs/PLAN3.md](docs/PLAN3.md)) and now **4.0, the parity release**
([docs/PLAN4.md](docs/PLAN4.md)), whose scope came from a
decision-by-decision comparison against mc
([docs/MC-DIFF.md](docs/MC-DIFF.md)). Every row that comparison marked
**Adopt** is closed, and the places where rcmd still differs on purpose
are written down there rather than left to be discovered.

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

**3.51** - the wider world (4.0 S7): `-e` and `-v` and the
**rcedit / rcview / rcdiff** aliases, mc's startup flags
(`-b -c -C -S -d -u -U -l`), the shipped shell wrappers, **skins** -
rcmd's own theme files and mc's skin files read where they lie - and
**macOS builds back** in CI and in the releases.

**4.0** - the parity release, and the leftovers that finished it: mc's
**quick search** with an input field of its own, **Learn keys**,
`[keys.dialog]`, a **hotlist with groups** and a label prompt and
editing, **user-menu conditions and submenus** plus the per-directory
`.mc.menu`, mc's full **macro set**, dialog **input history**, **mouse**
and **underlined hotkeys**, the mc **clipboard file**, and **user syntax
files**. What is left in [docs/MC-DIFF.md](docs/MC-DIFF.md) is the
divergences - Tab completing rather than switching panels, F8 to the
trash, one grouped options dialog instead of five - each of them a
decision with a reason next to it.

## Install & run

```sh
cargo install --git https://github.com/jaroslavpachola/rcmd rcmd-tui
# from a checkout:
cargo install --path crates/rcmd-tui   # installs the `rcmd` binary
# or during development:
cargo run -p rcmd-tui                  # or: just run
```

Release binaries are attached to GitHub releases (built by
`.github/workflows/release.yml` on `v*` tags): the glibc Linux build, a
**static musl build** (`x86_64-unknown-linux-musl`, C dependencies
vendored) that runs on any distro with no shared-library requirements,
and macOS on both architectures (`x86_64-apple-darwin`,
`aarch64-apple-darwin`, OpenSSL vendored so nothing has to be installed
alongside).

```
usage: rcmd [OPTIONS] [DIR1 [DIR2]]
       rcedit FILE...    rcview FILE    rcdiff FILE1 FILE2
       rcmd --import-mc [MC_CONFIG_DIR]

  -e, --edit FILE     start in the editor on FILE (repeatable)
  -v, --view FILE     start in the viewer on FILE
  -P, --printwd FILE  write the last active directory to FILE on exit
  -S, --skin NAME     theme: mc, dark, bw
  -b, --nocolor       black and white
  -c, --color         colour (the default)
  -C, --colors SPEC   mc colour spec: keyword=fg,bg:keyword=fg,bg
  -d, --nomouse       no mouse
  -u / -U             subshell off / on for this run
  -l, --ftplog FILE   log the FTP/fish dialogue to FILE
```

`-e` and `-v` bring rcmd up on **one screen instead of the panels**, and
closing it ends the session - that is mc's `mcedit` / `mcview`, and the
same thing happens when the binary is reached through a link named
`rcedit`, `rcview` or `rcdiff` (mc's names work too, if that is what
your fingers type):

```sh
ln -s "$(command -v rcmd)" ~/.local/bin/rcedit    # and rcview, rcdiff
rcedit notes.txt draft.txt   # two editor screens; Alt+` lists them
rcdiff old.rs new.rs         # the two files side by side
```

`-b` is the one to reach for when the colours are not arriving - it
drops to the terminal's own foreground and background, with reverse
video where something has to stand out, and it overrides `-S`. `-C`
takes mc's colour spec (`normal=brightgreen,black:directory=white`) and
lays it over whatever theme is loaded; keywords rcmd has nowhere to put
are named on the status line rather than dropped in silence. `-l` writes
every line of FTP and `fish://` dialogue to a file - the transcript is
what a server that will not list looks like from outside - with the
password redacted.

Coming from mc? `rcmd --import-mc` reads your `menu`, `mc.ext` and
`mc.keymap` and prints the equivalent rcmd config on stdout - user menu
entries, openers, view filters and panel key bindings. It never touches
your `config.toml`; review what it prints and paste what you want.
Anything with no rcmd equivalent (`type/` matchers, `%cd` commands,
unsupported macros) is reported on stderr rather than guessed at.

To make your shell follow rcmd's last directory on exit (the mc-wrapper
trick), source one of the shipped wrappers - [`contrib/rc.sh`](contrib/rc.sh)
for bash/zsh, [`contrib/rc.fish`](contrib/rc.fish) for fish. They come
with the release tarballs, and are one function each if you would rather
copy it into your shell config than source a file:

```sh
. /path/to/rc.sh                              # bash/zsh: in ~/.bashrc
cp rc.fish ~/.config/fish/functions/rc.fish   # fish
rc                                            # rcmd, and cd where it ended
```

Both are the same idea: rcmd writes its last active directory to a file
on exit (`-P`), the function reads it and `cd`s there. A run that ends
in a crash, or in a directory that has since gone away, leaves the shell
exactly where it was.

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
| Ctrl+S, Alt+S | Quick search: matches anywhere in the name, `*`/`?` glob, smartcase; Ctrl+S or ↓/↑ walks the matches |
| Ctrl+F | Filter shown files by glob (`*` or empty clears) |
| Alt+letter | In a dialog: press the button whose underlined letter it is |
| Ctrl+\ | Directory hotlist: Enter goes (or walks into a group), `a` adds this directory, `g` makes a group, `e` renames, `m` moves an entry into another group, `d` drops, Alt+↑/↓ reorders |
| Alt+F7 | Find file (glob + optional content); results panelized |
| Alt+← / Alt+→ | Directory history back / forward (per panel) |
| Alt+Shift+H | Directory history as a list; Enter goes there |
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
locally also tries `$CDPATH`). Dialog fields remember what was typed
into them before - Alt+P and Alt+N walk a field's own history, kept per
kind of question (destinations, `mkdir`, `cd`, `chown`…) and saved
between sessions. MC's macros expand there too (the same
set the user menu gets, below - so `%%s` is how you type a literal
`%s`). Alt+Enter
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
at 4096 columns. The bottom bar names what each key does *now*, as mc's
does: F2 says Unwrap once wrapping is on.

**F5**, `Alt+L` or `:` opens **goto**, which takes all three of mc's
destinations in one field, told apart by how the number is written: a
bare `201` is a line, `0x3e8` or `1000b` a byte offset, and `50%` a
share of the file. **`m`** followed by a digit sets one of ten marks at
the current position and **`r`** followed by the same digit returns to
it. `Alt+R` toggles a column ruler under the title, which counts from
the leftmost column on screen rather than from the start of the line,
so it keeps telling the truth when the view is scrolled sideways.

F7 or `/` opens mc's **search dialog**: the pattern, and the four
answers that change what it means. The pattern is read as **Normal**
(a literal), a **Regular expression**, or **Hexadecimal** bytes -
`7f454c46`, `7f 45 4c 46` and `0x7f 0x45 0x4c 0x46` are the same four,
and it is the only way to look for something that is not text. Alongside
it: **Case sensitive**, **Whole words** and **Backwards**. Tab and the
arrows move between rows, Space ticks, Enter searches; `n` repeats the
search with its options intact. Matches are highlighted in the line and
the found line is marked.

**F8** turns nroff formatting on: `_^Ht` and `t^Ht` - a character, a
backspace and another character, which is how a formatted man page has
said "underline" and "bold" since printers could only move forward -
are read as the attributes they stand for instead of showing up as
control bytes. The search follows the mode, so with formatting on the
word you can see is the word you can look for.

**F6** swaps the `[[view]]` filter in and out under the same file: the
parsed text from `pdftotext`, `tr`, `unzip -l` or whatever the rule
runs, or the file as it actually is. It is the in-viewer form of the
choice Shift+F3 makes when opening.

**Alt+!** (F9 > File > Filtered view) is the same thing for a command
typed on the spot: the field starts as the file name, the command goes
in front of it (`head -50 `, `strings `, `xxd | less`-style pipes are
fine, `%f` works too), and the output opens in the viewer as a filter
would, so F6 swaps the file itself back in.

**Ctrl+F** and **Ctrl+B** move to the next and previous file of the
panel without leaving the viewer, keeping wrap, hex, the ruler, the
formatting mode and the search - so reading through a directory is one
key per file. The panel cursor follows, which is where you land when
you quit.

In **hex mode** (F4), **F2** puts a cursor on the bytes. Hex digits type
over the byte it is on, two halves to a byte; Tab switches to the text
column, where a character stands for itself; arrows, PgUp/PgDn and
Home/End move it; **F6** writes the changed bytes into the file and Esc
stops editing. Nothing reaches the file until F6, changed bytes are
marked until then, and leaving with any still unwritten asks first.
Bytes are replaced, never inserted or deleted, so the file's length
never moves - which is what makes writing a handful of bytes into a
multi-GB file instant. Editing needs the file itself: on an archive
member, a remote file or a `[[view]]` filter's output the viewer is on a
copy, and it says so rather than writing to something about to be
deleted.

**Responsiveness**: directory listings that take longer than ~100 ms
(huge directories, cold network mounts) load in the background - the old
listing stays up with a spinner, typing never blocks, Esc cancels.
Panels also auto-reload when their directory changes on disk (debounced;
`watch = false` in the config disables it).

**Selecting and filtering**: `+` and `-` select and unselect by
pattern, and `Ctrl+F` filters what the panel shows at all. All three
are mc's one dialog: the pattern, then **Files only** (directories are
left alone, so a filter can never strand you), **Case sensitive**, and
**Shell patterns** - unticked, the pattern is a regular expression
instead of a glob. A regex that will not compile is quoted back at you
rather than silently matching nothing. The panel names the filter it is
under along its bottom edge, options included.

**Power tools**: Alt+F7 opens **find file** - where to start, a
filename pattern, and the text to look for inside the files, with mc's
answers beside them: **whole words**, **case sensitive**, **regular
expression** (matched line by line), **all charsets** (the same word as
another machine spelled it - KOI8-R, CP1251, Shift_JIS and the rest),
**skip hidden**, **follow symlinks**, and rcmd's own **skip
gitignored**. Matches arrive in a **results window** of their own as
they are found, with mc's six buttons: **Chdir** (Enter on a row) takes
the panel to the file and puts the cursor on it, **Again** reopens the
dialog on the same question, **Panelize** turns the list into the panel
listing, **View** and **Edit** open the match, and **Quit** closes.
`find_window = false` restores the pre-4.0 shape, where matches stream
straight into the panel as a *panelized* listing (paths relative to the
search root), where marking and F5/F6/F8 work as usual. *Panelize command…*
(F9 → Left/Right) turns any command's stdout lines into such a listing
(`git ls-files -m`, `rg -l TODO`, …). Its output **streams in as it
arrives**, so a slow command fills the panel while it runs and Esc
stops it. Commands worth keeping sit above the field as a saved list:
Tab moves between the list and the field, Ctrl+S saves what you typed
under a name, F8 drops one. They live in `[[panelize]]` entries in the
config, and the ones you save while running go to the state file. *Compare directories* (Ctrl+X d) asks
mc's question first - **Quick** (size and date), **Size only**, or
**Thorough** - and marks what differs on both sides, so a plain F5
copies the differences across. Thorough reads the files, which is the
only way to tell two files with the same size and date apart; it runs
in the background, marks each pair as it finds it, and Esc stops it.
Size only is for a tree whose timestamps were never going to survive
the trip.

**Compare files** (F9 → Command) puts the cursor file of each panel
side by side, lined up by a Myers diff: changed lines are highlighted
on both sides, a line only one file has shows opposite a `~~~` gap, and
`n` and `p` walk from one difference to the next (it opens on the first
one). `q` closes it. It is a screen like the viewer and the editor, so
``Alt+` `` lists it and you can leave it open while you do something
else.

**Mouse**: click focuses a panel and moves the cursor, double-click
enters, the wheel scrolls whatever it hovers (panels, viewer, editor,
quick view), the bottom keybar and the F9 menu are clickable, and a
click in the editor places the cursor. All additive - every feature
stays keyboard-reachable. Hold Shift to select terminal text as usual;
`mouse = false` in the config turns capture off entirely.

**Panel history**: each panel remembers where it has been -
Alt+←/Alt+→ walk back and forward browser-style (sftp:// locations
reconnect through the connection cache), Alt+Shift+H lists the whole
history with `*` on where the panel is now and Enter moving the cursor
there, Alt+↑ opens the hotlist.

**Quick view** (Ctrl+X q): the other panel becomes a live preview of
the file under the cursor, updating as you move. It uses the viewer's
chunked reader, so previewing a multi-GB log is instant. Tab focuses
the preview for scrolling (arrows/PgUp/PgDn); Ctrl+X q turns it off.

**Openers & user commands**: `[[open]]` rules in the config make Enter
open files by type - the first matching rule wins:

```toml
[[open]]
match = "*.pdf"              # a glob on the name, case-insensitive
run = "zathura %f >/dev/null 2>&1 &"

[[open]]
type = "^ELF"                # a regex over what `file -b` says of it
run = "objdump -d %f | less"

[[open]]
regex = "^[a-z]+[0-9]+\\.log$"  # a regex on the name ((?i) folds case)
directory = "^/var/log/"       # ...and one on the panel's path
run = "less %f"
```

Those are mc.ext's four matchers (`match`, `regex`, `type`,
`directory`); every one a rule gives must hold. `file` is only asked
when a rule has `type =`, and `[[view]]` rules take the same keys.

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

[[commands]]                     # only offered where it makes sense
name = "extract here"
run = "tar xf %f"
when = "f *.tar.gz | f *.tgz"

[[commands]]                     # a submenu: Enter walks in, ← walks out
name = "Tools"
entries = [
  { name = "line count", run = "wc -l %s" },
]
```

`when` is **mc's user-menu condition language**: `f`/`F` the cursor file
here or in the other panel, `d`/`D` the directory, `t`/`T` the file's
type (`r` regular, `d` directory, `l` link, `x` executable, `n` not a
directory, `t` something is marked), `x` a program that must exist,
`!` to negate, `|` and `&` to join - evaluated left to right, as mc
evaluates them. Patterns are globs.

A **`.mc.menu` in the panel's directory** is read too, in mc's own
format (`shell_patterns=0` files have their regexes converted). Its
entries come first and the configured ones stay after them: a project's
menu is an addition to yours, where mc's would have replaced it.

Both expand **mc's macros** before running in the active panel's
directory, everything shell-quoted:

| Macro | This panel | Other panel |
|---|---|---|
| cursor file | `%f` | `%F` |
| directory | `%d` | `%D` |
| marked files | `%t` | `%T` |
| marked files, and drop the marks | `%u` | `%U` |
| marked files, or the cursor file if none | `%s` | `%S` |

`%q` is the clipboard file (`~/.cache/mc/mcedit/mcedit.clip`, shared
with mcedit - rcmd's editor writes it too), `%%` a literal percent, and
`%{Some question}` **asks** before the command runs, putting the answer
in unquoted, which is how options get passed. Anything else is left
alone, so `printf '%%s'` needs its percent doubled like everywhere else.

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

**F9 in the editor** opens its own menu bar - File, Edit, Search,
Options - over the title row, with every entry naming the key that
already does it: the menu is how you find the key, not a second way of
working. **Options > General** is mc's editor options dialog: **tab
size**, **fill tabs with spaces** (Tab inserts spaces up to the next
stop, so the file has no tabs in it), **return does autoindent**,
**backspace through tabs** (inside an indent one Backspace takes the
whole stop rather than one space of it) and the **wrap column** the
soft wrap folds at - `window` means the window's width, which is mc's
dynamic wrapping. Left/Right nudge the numbers, Space ticks the
switches, and OK applies them to the open editor and remembers them for
the next session. They are `edit_tab_size`, `edit_fill_tabs`,
`edit_auto_indent`, `edit_backspace_tabs`, `edit_wrap_column`,
`edit_line_numbers`, `edit_backups` and `edit_clipboard` in the config
file. **Options > Syntax** picks the highlighting by hand - every
syntax syntect knows, or plain text - for a file whose name does not
say what it is.

**Getting around, and bookmarks**: `Alt+L` goes to a line by number,
`Alt+K` bookmarks the line the cursor is on, `Alt+J` and `Alt+I` walk
to the next and previous bookmark, and `Alt+O` drops them all. A
bookmark follows its text: inserting or deleting lines above one moves
it with what it marked, rather than leaving it pointing at whatever
slid into that line number. `Alt+N` draws mc's line-number gutter, with
a `*` beside a bookmarked line so the bookmarks can be seen and not
only jumped to. `Ctrl+U` undoes, as it does in mc, beside rcmd's
`Ctrl+Z`.

**The desktop clipboard**: Ctrl+C and Ctrl+X also put the text on the
system clipboard and Ctrl+V reads it, through `wl-copy`, `xclip`,
`xsel` or `pbcopy` - whichever is installed. With none of them there,
or with nothing in the clipboard, the editor's own clipboard stands, so
copy and paste inside rcmd work either way. `edit_clipboard = false`
keeps it to the editor.

**Backups**: with `edit_backups` on, every save first copies what is on
disk to `file~` - mc's "Do backups", one step back rather than a
history.

**Codepages** (`Alt+E`): a file is bytes, and nothing in it says what
they mean - so `Alt+E` in the viewer, the editor **or a panel** picks
the codepage to read it in: UTF-8, the Latin and Cyrillic and Greek and Baltic
single-byte sets, KOI8-R/U, CP866, and Shift_JIS, EUC-JP, GBK, Big5 and
EUC-KR. The viewer re-reads at once (and the search follows, because it
reads what you can see); the editor re-reads and **writes back in the
same codepage**, so editing a KOI8-R file leaves a KOI8-R file. Since
changing it means re-reading, the editor asks you to save first rather
than dropping an edit. The title bar names the codepage whenever it is
not UTF-8.

On a **panel** (`Alt+E`, or Left/Right → Character set) the codepage is
what the *filenames* are read in. Unix names are bytes, so a directory
written on a CP1251 or KOI8-R machine shows as replacement characters
until the panel is told - and then it reads, sorts, filters and quick-
searches as text. Names typed into a dialog on that panel are written
back in the same codepage, so the file you make is the file the panel
then shows. The panel title names the codepage while one is set.

**Screens** (``Alt+` ``): several editors and viewers can be open at
once. ``Alt+` `` lists them - the panels are the first row, then every
open screen with what it is on and whether it has unsaved changes - and
Enter switches. Closing a screen (F10 in the editor, q in the viewer)
lands back on the panels, which is where mc puts you too. Quitting rcmd
with an editor still holding unsaved changes says how many and asks,
whether or not the ordinary exit question is switched on.

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
clobber each other. The state keys (`show_hidden`, `sort_key`,
`sort_reverse`, `listing`, `[[hotlist]]`) are still read from
`config.toml` as your defaults; what the UI changes goes to the state
file on top of them.

The settings live in one sectioned checkbox form under **F9 → Options →
Panel options**, applied live: *Layout* (split direction and size, the
per-panel mini status, and which of the menu bar / status line /
command line / key bar are drawn), *Panel* (hidden files, lynx-like motion,
mouse, auto-reload, git), *Confirmation* (ask before deleting /
overwriting / quitting) and *Shell and editor* (persistent subshell,
internal or external editor). The theme has a list of its own under
**F9 → Options → Appearance**, because a skin is one of however many
files are installed rather than a two-way switch:

```toml
theme = "mc"        # "dark", "bw", or the name of a theme file
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
show_free_space = true     # free space in each local panel's footer
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
edit_tab_size = 8          # the built-in editor, F9 > Options > General
edit_fill_tabs = false     # Tab inserts spaces up to the next stop
edit_auto_indent = true    # Enter copies the line's leading whitespace
edit_backspace_tabs = false  # in an indent, Backspace takes a whole stop
edit_wrap_column = 0       # column the soft wrap folds at; 0 = the window
find_window = true         # find file: matches in a window of their own
                           # (false = straight into the panel listing)
edit_line_numbers = false  # the line-number gutter (Alt+N toggles it)
edit_backups = false       # keep the previous contents as file~ on save
edit_clipboard = true      # share the desktop clipboard (wl-copy/xclip/...)
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
                             # goto set-mark go-mark ruler nroff raw
                             # next-file prev-file hex-edit hex-save
[keys.dialog]                # ...and these wherever a dialog is open
"ctrl+j" = "ok"              # ok cancel next prev - a bound key stands
                             # in for Enter / Esc / Tab / Shift+Tab
[keys.editor]                # ...and these inside the F4 editor
"ctrl+q" = "quit"            # save quit mark replace search search-next
                             # block-copy block-move delete-line undo
                             # redo copy cut paste select-all wrap menu
                             # goto bookmark bookmark-next bookmark-prev
                             # bookmark-clear line-numbers
# key syntax:  [ctrl+][alt+][shift+]<key>  (f1..f20, letters, +, -, etc.)
# actions: help view edit copy move mkdir delete delete-perm select-group
#   unselect-group invert-selection quit shell reload swap-panels
#   toggle-hidden sort-name sort-ext sort-size sort-mtime sort-reverse
#   menu mark quick-search hotlist filter up-dir enter history-back
#   history-forward quick-view info-view user-menu listing-brief
#   listing-full listing-long listing-tree listing-user listing-cycle
#   other-same-dir other-open-dir sftp-link find-file panelize
#   compare-dirs dir-size dir-tree appearance learn-keys edit-config

[[highlight]]          # MC's filehighlight, as rules: first match wins
match = "*.tar.gz"     # a glob on the name...
color = "brightred"    # ...mc's colour names, #rrggbb or "default"

[[highlight]]
type = "exe"           # ...or what the entry is: dir linkdir exe link
color = "magenta"      #    broken file
bold = true            # optional; left out, the kind's own weight stands

[[hotlist]]                 # Ctrl+\ - a tree, as in mc
label = "projects"
path = "/home/you/git"

[[hotlist]]                 # an entry with `entries` is a group to
label = "Work"              # walk into rather than a place to go
entries = [
  { label = "api", path = "/srv/api" },
]

[[open]]                    # Enter on a matching file runs this
match = "*.pdf"             # match (glob) / regex (name) / type (file -b)
run = "zathura %f >/dev/null 2>&1 &"   # / directory (path): all given must hold

[[panelize]]                # saved panelize commands (Ctrl+S adds one)
name = "modified"
run = "git ls-files -m"

[[commands]]                # F2 user menu; key = "..." binds directly
name = "git status"
run = "git status | less"
```

### Syntax files

The editor highlights with syntect, which speaks **`.sublime-syntax`**.
Drop your own into `~/.config/rcmd/syntax/` and they join the built-in
list - by extension, and in F9 > Options > Syntax inside the editor.
A file that will not parse costs itself and a note on the editor's
status line, never the highlighting of everything else. (mc's own
syntax format is a different language and is not read.)

### Skins

A theme that is not one of the three built in (`mc`, `dark`, `bw`) is a
file, looked up by name in `~/.config/rcmd/themes/` and then in mc's
skin directories - `~/.local/share/mc/skins`, `/usr/local/share/mc/skins`,
`/usr/share/mc/skins`. rcmd's own format is TOML naming the fields it
sets, over an optional base, so a skin can be three lines:

```toml
base = "dark"            # mc | dark | bw - the palette to patch (default mc)
dir_fg = "brightblue"
panel_bg = "#1e222a"
header_fg = "color214"
```

The fields are `panel_fg` `panel_bg` `dir_fg` `exec_fg` `broken_fg`
`header_fg` `mark_fg` `select_fg` `select_bg` `dialog_fg` `dialog_bg`
`error_fg` `error_bg` `help_fg` `help_bg` `help_header_fg` `prompt_fg`
`key_fg` `key_bg` `label_fg` `label_bg`. A colour is one of mc's names
(`black`, `brightgreen`, `brown`, `lightgray`, ...), `#rrggbb`,
`colorN` (0-255), `rgbRGB` (three digits 0-5), `grayN` (0-23), or
`default` for the terminal's own.

**mc's skins work as they are**: `-S julia256` reads
`/usr/share/mc/skins/julia256.ini` and maps its `[core]`, `[dialog]`,
`[error]`, `[help]`, `[filehighlight]` and `[buttonbar]` sections onto
those fields. What rcmd draws for itself - the frames, the menus - is
not taken from the skin, so a skin is read for its colours and nothing
else. **F9 → Options → Appearance** lists everything found and switches
on Enter, and the choice outlives the session.

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
`rcmd`). CI runs fmt, clippy and the unit tests on Linux and macOS, and
the pty e2e suite on Linux - it drives the binary through `/dev/pts`
and installs shells to test them, which is a Linux job. Licensed MIT.
