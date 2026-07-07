# Changelog

## Unreleased

- **Persistent subshell** (3.0 R1, the flagship): a long-lived `$SHELL`
  on its own pty, like MC's. Ctrl+O toggles panels ↔ the shell's screen
  (the last output survives the trip), typed commands run *inside* the
  shell — aliases, functions, history and `$?` persist — and cd syncs
  both ways (panels follow a subshell `cd` on return; the shell is
  moved to the panel directory before a command runs). `exit` respawns
  the shell with a note. bash/zsh/fish get prompt hooks over a pipe for
  cwd + prompt-idle tracking; other shells (dash, POSIX sh) fall back
  to `/proc/<pid>/cwd` and foreground-process-group probing. While
  hidden, output is buffered for replay and a small shim answers
  blocking terminal queries (DA1/DSR — fish probes at startup).
  `subshell = false` (or any spawn failure) restores the pre-3.0
  one-shot execution. The e2e suite now runs twice in CI — subshell on
  and off — plus per-shell scenarios for sh, bash, zsh and fish.
- **Lynx-like motion is now a UI toggle** (MC parity): F9 > Options >
  Lynx-like motion switches Left/Right = parent/enter at runtime and
  persists as `lynx = true|false` in the config; the `modern` keymap
  preset now just means "lynx on by default". Right stays dirs-only —
  Enter opens files.
- **One-panel view for long listings** (MC parity): while the *active*
  panel is in the *long* format it takes the whole screen width and the
  other panel is hidden; Tab to the other panel (or cycling the format
  back) restores the split. Previously the six ls-style columns were
  squeezed into a half-width panel.

## 2.0.0 — 2026-07-06

The 2.0 roadmap (docs/PLAN2.md) is complete: rcmd now owns the
workflows that used to require leaving it. One-command install:
`cargo install --git https://github.com/jaroslavpachola/rcmd rcmd-tui`

- **MC keybinding parity**: Alt+S = quick search, Alt+T = cycle listing
  format, Ctrl+U = swap panels — their Midnight Commander meanings
  (sort by ext/size/mtime moved to F9 → Sort or custom `[keys]`;
  Alt+E freed); ESC works as MC's meta prefix (Esc 1…0 = F1…F10,
  Esc key = Alt+key, Esc Esc = Escape, 1 s timeout)
- **Openers & user commands** (P6): `[[open]]` config rules make Enter
  open files by glob (first match wins, no pause — append `&` for GUI
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

## 1.1.0 — 2026-07-05

2.0-roadmap phases P1–P5 (docs/PLAN2.md):

- **UX depth** (P5): mouse support (click to focus/select, double-click
  to enter, wheel scrolling everywhere, clickable keybar/menu, click
  places the editor cursor; `mouse = false` disables), per-panel
  directory history (Alt+←/→ back/forward incl. sftp:// stops, Alt+↑
  hotlist), quick view (Ctrl+X q — the other panel live-previews the
  cursor file via the chunked viewer), and git awareness (branch in the
  panel title, M/A/?/! status column with ignored entries dimmed,
  computed in the background; `git` cargo feature, on by default)
- **Built-in editor** (P4): F4 opens an mcedit-style editor (new
  `rcmd-edit` crate) — unlimited grouped undo/redo, F3 marking and
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

## 1.0.0 — 2026-07-04

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
- Archives as read-only VFS: zip, tar, tar.gz, tar.xz, tar.bz2 — browse,
  extract (F5), view (F3); copy *into* zip archives (append)
- F9 pulldown menu, F1 help, directory hotlist (Ctrl+\)
- `~/.config/rcmd/config.toml`: mc/modern keymap presets, custom key
  bindings, mc/dark themes, persisted sort/hidden/hotlist
