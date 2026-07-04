# Changelog

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
