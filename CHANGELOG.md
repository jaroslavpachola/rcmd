# Changelog

## Unreleased

- **Per-context key bindings** (4.0 S0): `[keys.viewer]` and
  `[keys.editor]` rebind keys inside the F3 viewer and the F4 editor,
  which were hardcoded until now; bare `[keys]` entries still bind in
  the panel (and `[keys.panel]` says so explicitly). Viewer actions:
  quit, wrap, hex, search, search-next, follow. Editor actions: save,
  quit, mark, replace, search, search-next, block-copy, block-move,
  delete-line, undo, redo, copy, cut, paste, select-all, wrap. Unknown
  contexts, keys and action names warn in the status line instead of
  stopping the program.

## Unreleased

- **One grouped options dialog** (4.0 S0): F9 > Options > Panel options
  is now a sectioned form - Panel, Confirmation, Shell and editor,
  Appearance - covering MC's whole setting surface in one screen rather
  than its five dialogs. Arrow keys skip the headings.
- **Confirmation settings** (MC parity): *Ask before deleting* and *Ask
  before overwriting* (both on, as before) and *Ask before quitting*
  (off, keeping rcmd's instant F10). Turning the overwrite question off
  answers "overwrite all" for every job; turning the delete question off
  makes F8 act at once.

## Unreleased

- **MC command-line keys** (4.0 S0): `M-h` opens the command history as
  a pick list (Enter puts a line back on the command line), `M-p`/`M-n`
  walk it like MC (`C-p`/`C-n` still work), `M-a` inserts the panel
  path, `C-x !` panelizes a command's output, and `cd -` returns to the
  panel's previous directory - a relative `cd` that misses locally now
  also tries `$CDPATH`. The command line expands MC's macros (`%f %d %D
  %t %%`); unknown percent sequences are left alone, so `printf "%s"`
  still works. **The history survives sessions**, in the state file
  (last 100 lines).
- **Esc meta prefix is quicker**: a lone Esc now resolves after 250 ms
  instead of a second, so Esc-to-clear feels immediate. Typing the
  prefix by hand (Esc 1..0 for F1..F10) needs the follow-up key inside
  that window - `esc_timeout_ms = 1000` in the config restores MC's
  older, roomier feel.

## Unreleased

- **Config/state split** (4.0 S0): `~/.config/rcmd/config.toml` is now
  read-only from rcmd's side - comments and hand formatting survive
  because nothing writes it back. Everything rcmd changes itself (panel
  sort/hidden/listing state, the hotlist, every options-form toggle)
  lives in `$XDG_STATE_HOME/rcmd/state.toml` and takes precedence over
  the config. State is sparse, so a config edit still decides anything
  you never touched in the UI. Existing state keys in `config.toml` are
  migrated once on first start and stay honoured for one release.

## 3.0.0 - 2026-08-22

The live commander ([docs/PLAN3.md](docs/PLAN3.md), R1–R5 complete): the
persistent subshell shipped in R1 and has been dogfooded since, joined
by SFTP auth depth, the workflow bells, the depth-debt menu including
the job queue, and the packaging work.

- **Packaging** (3.0 R5): release binaries are now thin-LTO'd and
  stripped, and every release ships a second, fully static
  `x86_64-unknown-linux-musl` tarball (C dependencies vendored) that
  runs on any distro. The README opens with a demo GIF recorded by the
  project's own pty harness.
- **rar and 7z browsing**: Enter on a `.rar` or `.7z` opens it like any
  archive - read-only listing, F3 views members, F5 copies out,
  Ctrl+Space sizes directories. Served by the first working external
  tool (`7z`/`7zz`/`7za`, or `unrar` for rar when 7z lacks the codec),
  with a clear status message when none is installed. Listings are
  parsed from the machine-readable `-slt` / `vt` outputs under
  `LC_ALL=C`; members stream out per read.
- **View filters** (`[[view]]` in the config): F3 can now pipe a file
  through a command and show its stdout in the internal viewer -
  `match = "*.pdf"` / `run = "pdftotext %f -"` - with search, wrap and
  hex working on the filtered text. First matching glob wins, local
  panels only; a failing filter falls back to the raw bytes with a
  status note, and Shift+F3 always views raw.

- **Viewer highlighting**: the F3 viewer now syntax-colors files with a
  recognized syntax under the editor's 2 MB ceiling (same syntect
  machinery, plain and instant above it), and search matches are
  highlighted precisely - every visible occurrence gets the selection
  style, the current found line keeps its bold marker. Works in wrap
  mode and survives tab expansion; follow mode invalidates the parse
  cache only when the file shrinks (rotation).

- **Job queue** (3.0 R4): `b` in a copy/move/delete/pack progress
  dialog sends the job to the background - the panels come back, the
  status line shows aggregate progress, and more jobs can start
  meanwhile. C-x j (or F9 > Command > Jobs) lists running jobs: Enter
  brings one to the foreground, `c` cancels. A job that needs an
  answer (overwrite/error) pulls itself back up; quitting is refused
  while jobs run.
- **Editor depth** (3.0 R4): `$1`–`$9` capture groups in replace,
  mcedit-style F5/F6 block copy/move, and soft-wrap on Alt+W (wrapped
  segments keep selection, tabs and syntax colors; clicks and the
  viewport are wrap-aware).
- **chmod / chown / symlink dialogs** (3.0 R4): C-x c (octal mode),
  C-x o (`user[:group]`, names resolved locally, numeric ids over
  sftp), C-x s (link to the cursor entry) - all work on remote panels
  through the new `FsWrite::set_owner` verb.
- **Copy into tar** (3.0 R4): a tar destination (plain/.gz/.xz/.bz2)
  is rewritten in full - old entries stream across, new trees append,
  a temp file renames over the archive. Zip keeps in-place append.
- **Quick-view hex mode** (3.0 R4): F4 while the preview pane is
  focused flips it to a hex dump.
- **Click-to-sort** (3.0 R4): clicking a panel column header sorts by
  that column; clicking again reverses.
- **Ctrl+Space everywhere** (3.0 R4): directory size now also works on
  sftp and archive panels via provider traversal.

- **Bulk rename via the editor** (3.0 R3): F9 > File > Bulk rename
  opens the marked names (or the cursor entry) as a numbered text
  buffer in the built-in editor - edit names to rename (swaps and
  chains are fine: renames go through temp names in two phases),
  delete lines to delete (to trash, via the job engine), then confirm
  the preview. Occupied targets are refused and restored, and a buffer
  that doesn't parse applies nothing.
- **Viewer follow mode** (3.0 R3): `f` in the F3 viewer toggles
  tail&nbsp;-f - appended data is picked up every loop tick and the
  view sticks to the bottom; truncation or rotation re-indexes from
  scratch. `[follow]` shows in the title.
- **Command-line Tab completion** (3.0 R3): with text on the line, Tab
  completes the path under the cursor (files and directories only) -
  unique matches get a trailing `/` or space, ambiguous ones advance
  to the common prefix and list candidates in the status line. An
  empty line still switches panels; Alt+Tab always completes.
- **Gitignore-aware find** (3.0 R3): inside a git work tree, Alt+F7
  now skips ignored trees and `.git` by default; a checkbox in the
  dialog searches everything again.
- **Recent directories in the hotlist** (3.0 R3): the hotlist dialog
  lists both panels' visited directories (newest first, deduped,
  pinned entries excluded) below the pinned rows; Enter cds, sftp
  URLs reconnect through the connection cache.
- **MC alias batch** (3.0 R3): M-y/M-u history back/forward, M-? find
  file, M-c quick cd dialog, C-l repaint, C-x t / C-x p paste tagged
  names / the panel path to the command line, S-F4 edit a new file
  (created on first save), S-F5/S-F6 copy/rename the cursor file in
  place with the name prefilled. All remappable via `[keys]`.
- **SFTP auth depth** (3.0 R2): the connect worker now asks the server
  which auth methods it accepts and tries only those, in OpenSSH order
  (publickey, keyboard-interactive, password). Passphrase-protected
  keys get a masked prompt (3 attempts, empty input skips the key)
  instead of silently falling through to password auth; encryption is
  detected for both PEM and OpenSSH-format key files.
  Keyboard-interactive servers work: each challenge becomes its own
  dialog (several per round supported), masked or echoed as the server
  requests. e2e drives both against paramiko - an encrypted key with a
  wrong-then-right passphrase, and a two-prompt kbd-interactive round.

- **Persistent subshell** (3.0 R1, the flagship): a long-lived `$SHELL`
  on its own pty, like MC's. Ctrl+O toggles panels ↔ the shell's screen
  (the last output survives the trip), typed commands run *inside* the
  shell - aliases, functions, history and `$?` persist - and cd syncs
  both ways (panels follow a subshell `cd` on return; the shell is
  moved to the panel directory before a command runs). `exit` respawns
  the shell with a note. bash/zsh/fish get prompt hooks over a pipe for
  cwd + prompt-idle tracking; other shells (dash, POSIX sh) fall back
  to `/proc/<pid>/cwd` and foreground-process-group probing. While
  hidden, output is buffered for replay and a small shim answers
  blocking terminal queries (DA1/DSR - fish probes at startup).
  `subshell = false` (or any spawn failure) restores the pre-3.0
  one-shot execution. The e2e suite now runs twice in CI - subshell on
  and off - plus per-shell scenarios for sh, bash, zsh and fish.
- **Panel options form** (MC parity): F9 > Options > Panel options is
  an MC-style checkbox dialog over the everyday toggles - hidden files,
  lynx-like motion, mouse, auto-reload, git status, persistent
  subshell - plus editor (internal/external) and theme (mc/dark)
  radios. OK applies everything live (the subshell spawns or stops,
  the theme switches in place, the keymap rebuilds) and writes the
  config file immediately.
- **bugfix: config saves no longer clobber each other.** Every save is
  now a read-modify-write of the on-disk file: options and hotlist
  changes write through when they happen, exit only overlays panel
  state (sort/hidden/listing). Previously each exiting instance dumped
  its whole in-memory config, so with two rcmd sessions open the later
  exit silently reverted settings the earlier one had saved.
- **Menu hotkey letters** (MC parity): every F9 menu title and entry
  has a highlighted hotkey - `F9 o p` opens Panel options. Entries of
  the open menu win over titles; arrows and Enter work as before.
- **Lynx-like motion** (MC parity): Left = parent, Right = enter (dirs
  only - Enter opens files), now switchable from the options form and
  persisted as `lynx = true|false`; the `modern` keymap preset just
  means "lynx on by default".
- **One-panel view for long listings** (MC parity): while the *active*
  panel is in the *long* format it takes the whole screen width and the
  other panel is hidden; Tab to the other panel (or cycling the format
  back) restores the split. Previously the six ls-style columns were
  squeezed into a half-width panel.

## 2.0.0 - 2026-07-06

The 2.0 roadmap (docs/PLAN2.md) is complete: rcmd now owns the
workflows that used to require leaving it. One-command install:
`cargo install --git https://github.com/jaroslavpachola/rcmd rcmd-tui`

- **MC keybinding parity**: Alt+S = quick search, Alt+T = cycle listing
  format, Ctrl+U = swap panels - their Midnight Commander meanings
  (sort by ext/size/mtime moved to F9 → Sort or custom `[keys]`;
  Alt+E freed); ESC works as MC's meta prefix (Esc 1…0 = F1…F10,
  Esc key = Alt+key, Esc Esc = Escape, 1 s timeout)
- **Openers & user commands** (P6): `[[open]]` config rules make Enter
  open files by glob (first match wins, no pause - append `&` for GUI
  apps; lynx-motion Right stays dirs-only); `[[commands]]` shell
  templates with `%f %d %D %t` macros in a new F2 user menu (digit
  hotkeys), each optionally bound to its own key
- **File properties & listing formats** (P7 MC depth): Ctrl+X i info
  panel (full stat of the cursor file on the other panel: type, size,
  perms, owner/group, links, inode, mtime/atime/ctime), free-space
  display in local panel footers and the info panel, per-panel listing
  formats brief/full/long via F9 → View (persisted as `listing`),
  Alt+i / Alt+o point the other panel at this directory / the directory
  under the cursor

## 1.1.0 - 2026-07-05

2.0-roadmap phases P1–P5 (docs/PLAN2.md):

- **UX depth** (P5): mouse support (click to focus/select, double-click
  to enter, wheel scrolling everywhere, clickable keybar/menu, click
  places the editor cursor; `mouse = false` disables), per-panel
  directory history (Alt+←/→ back/forward incl. sftp:// stops, Alt+↑
  hotlist), quick view (Ctrl+X q - the other panel live-previews the
  cursor file via the chunked viewer), and git awareness (branch in the
  panel title, M/A/?/! status column with ignored entries dimmed,
  computed in the background; `git` cargo feature, on by default)
- **Built-in editor** (P4): F4 opens an mcedit-style editor (new
  `rcmd-edit` crate) - unlimited grouped undo/redo, F3 marking and
  Shift+arrow selection with an internal clipboard, smartcase regex
  search (F7) and interactive replace (F4), auto-indent, atomic save
  preserving permissions and CRLF, syntect syntax highlighting for
  known file types (`syntax` feature, on by default), instant on huge
  files (50 MB log ≈ 0.2 s). Works on SFTP panels via scratch-copy
  upload. `editor = "external"` restores $VISUAL/$EDITOR.
- **SFTP remote panels** (P3): `cd sftp://[user@]host[:port][/path]` or
  F9 → Command → SFTP link; agent/key/password auth with known_hosts
  checking and a fingerprint dialog for unknown hosts; upload, download
  and remote↔remote F5/F6 through the usual job dialogs; F7 mkdir and
  F8 delete on the server; F3 view; F4 edits a scratch copy and uploads
  it back on save; hotlist remembers sftp:// entries; both panels can
  share a connection. Threads-not-async confirmed (decision D1).
- Find file (Alt+F7) with streamed results, panelize command output,
  quick directory compare (Ctrl+X d) + F5 sync (P1)
- Non-blocking directory loads with spinner and Esc cancel, Ctrl+Space
  directory size, notify-based auto-reload, 100k-entry benchmark (P2)

## 1.0.0 - 2026-07-04

First release. Complete MC-workflow parity per the original plan
(docs/PLAN.md), all milestones M0–M5 plus the debt list:

- Dual-pane browser with MC keybindings, colors, and F-key bar
- Marking (Insert, glob select/unselect, invert), sort modes, hidden
  toggle, per-panel file filter, quick search (Ctrl+S)
- F5/F6/F7/F8 file operations on a cancellable worker-job engine with
  MC-style progress, overwrite (o/a/s/S) and Retry/Skip/Abort dialogs;
  F8 trashes, Shift+F8 deletes permanently; mtimes preserved
- Command line with history, `cd`, Alt+Enter filename insert; Ctrl+O
  full shell; `rcmd -P FILE` exit-to-cwd; shell-style job control so
  Ctrl+C/Ctrl+Z never take down rcmd
- F3 viewer: lazy line indexing (instant on huge files), soft-wrap (F2),
  hex mode (F4), case-insensitive search; F4 edits via $VISUAL/$EDITOR
- Archives as read-only VFS: zip, tar, tar.gz, tar.xz, tar.bz2 - browse,
  extract (F5), view (F3); copy *into* zip archives (append)
- F9 pulldown menu, F1 help, directory hotlist (Ctrl+\)
- `~/.config/rcmd/config.toml`: mc/modern keymap presets, custom key
  bindings, mc/dark themes, persisted sort/hidden/hotlist
