# rcmd 4.0 - the parity release

**Status:** drafted 2026-08-22 from the decisions in
[MC-DIFF.md](MC-DIFF.md). **Prerequisite:** the 3.0.0 tag (R1 dogfooding
window). Baseline: 2.5.0 - subshell, SFTP, built-in editor, job queue,
archive browsing, git awareness.

## Vision

2.0 made rcmd complete, 3.0 made it alive, 4.0 makes it a *drop-in*: an
mc user's hands, config files and expectations all land somewhere. The
policy behind every phase is MC-DIFF's: **gaps get closed, deliberate
divergences stay and are documented**. Where mc's shape is a historical
accident (five options dialogs, a menu-file scripting language as the
only way to script), rcmd keeps its own shape and imports mc's data.

## Standing constraints (unchanged from 3.0)

- Keyboard-first; MC hands stay unbroken.
- `rcmd-core` stays TUI-free; threads + mpsc, no async.
- Every phase ends pty-verified, subshell on *and* off.
- External escape hatches never go away.
- Nothing in MC-DIFF §12 regresses to buy parity.

## Phases

### S0 - foundations (blocks most of the rest)

- **Config/state split**: `config.toml` becomes read-only from rcmd's
  side (user comments and formatting survive verbatim); panel state,
  sort, hotlist, command history and find/panelize presets move to an
  rcmd-owned `state.toml`. Merge-on-write semantics stay.
- **Grouped options dialog**: one sectioned form covering mc's whole
  setting surface - panel, layout, confirmations, appearance, VFS. Exit
  confirmation exists, default off; delete/overwrite confirmations exist,
  default on.
- **mc import layer**: `menu` / `.mc.menu`, `mc.ext`, mc keymap files
  read and converted into TOML; a one-shot importer on first run.
- **Per-context keymaps**: `[keys.panel|viewer|editor]` - done. The
  `dialog` context is deferred: rebinding OK/Cancel/next-field means
  routing every dialog's keys through a table first, which belongs with
  the dialog work in S2/S6 rather than here.
- **Small keys batch**: Esc prefix timeout to ~250 ms, `M-p`/`M-n`
  history aliases + `M-h` list, `M-a`, `cd -`/CDPATH, `C-x !`,
  command-line `%` macros, persisted command history.

### S1 - panel & layout (first user-visible milestone)

- Layout dialog: split direction, unequal split, hide
  menubar/keybar/command line/hint bar.
- Per-panel mini-status.
- Directory tree: panel listing mode **and** the Command-menu dialog.
- Multi-column brief listing; user-defined listing format.
- Per-extension/type file highlighting rules.
- Per-panel Left/Right menus (listing mode, sort, filter, panelize,
  rescan, encoding, links) - the mc menu structure.

### S2 - file-operation dialogs

- Copy/move dialog: source file masks, preserve attributes, dive into
  subdirs, follow links, stable symlinks, Background button.
  Defaults are **safest-by-default**, not mc-identical: preserve
  attributes on, follow links off, stable symlinks on.
- Overwrite prompt: Update (if newer), Size differs, Append, Reget, and
  both files' size + date on screen.
- Progress: ETA, throughput, per-file bar.
- chmod bit matrix (octal field kept), chown pick lists, advanced chown
  with recursion.
- `C-x l` hard link, `C-x C-s` edit symlink, relative-symlink option.
- Wire the confirmation toggles from S0 through every prompt.

### S3 - VFS breadth

- `fish://` and `ftp://` (the 3.0 refusal is overturned for these two).
- extfs-class formats: deb, rpm, iso9660, cpio, lha/arj/cab, patchfs,
  mailfs - read first, write where the format allows.
- Writable archives: move/delete/mkdir inside, and zip member *replace*
  instead of today's shadowing append.
- Active VFS list dialog + VFS settings.

### S4 - viewer & editor depth

- Viewer: goto line/offset, regex search with case/backwards/whole-word
  options, raw/parsed toggle, format/unformat, ruler, next/previous
  file, bookmarks, `%` jump, hex editing with save.
- Editor, first: **F9 menu bar + editor options dialog** (tab size,
  auto-indent, wrap column, backspace-through-tabs, syntax picker),
  then goto line, bookmarks, line numbers, `~` backups, system
  clipboard, `C-u` undo alias.
- Editor, later: macros, insert file, sort block, pipe block through a
  command, column/rectangular blocks.
- Screens: several editors/viewers open at once with mc's screen list.

### S5 - encoding

Full parity, all three surfaces: per-panel codepage (`M-e`), recoding on
read/write in viewer and editor, and correct round-tripping of non-UTF-8
filenames (no more lossy display).

### S6 - search, compare, panelize

- Select/unselect and filter dialogs: files only, case sensitive, shell
  patterns or regex.
- Find dialog: regex content, case, whole words, skip hidden, follow
  symlinks, start directory, charsets - keeping the gitignore checkbox.
- Find **results window** (Chdir/Again/Panelize/View/Edit) as the
  default; today's stream-into-the-panel stays behind a setting.
- Panelize presets, saved and async.
- Compare directories: quick / size-only / thorough.
- Internal diff viewer + "Compare files".

### S7 - packaging & the wider world

- `-e FILE` / `-v FILE`, argv0 dispatch (`rcedit`/`rcview`/`rcdiff`
  symlinks), the missing mc flags (`-b -c -C -S -d -u/-U -l`).
- Shipped shell wrappers (`rc.sh`, `rc.fish`).
- Skins (theme files; mc skin import optional).
- macOS builds restored to CI and releases.

## Sequencing

S0 first - it unblocks the options surface everything else needs. Then
**S1, S2 and S3 in parallel-ish order** (the chosen first milestone),
followed by S4, S5, S6, S7. S5 and S6 do not block anything; S7 rides
along with whatever release cuts.

## Risks

- **S0's config split is a migration.** Existing `config.toml` files
  carry state keys that must move without a user noticing. Mitigation:
  read both locations for one release, write only the new one.
- **S3 is the biggest surface in the whole plan** - extfs-class formats
  are a long tail. Ship read-only support format by format; each one is
  its own green commit, none of them block a release.
- **S5 touches every string path in the program.** Do filenames last,
  behind tests that use deliberately broken byte sequences.
- **Scope gravity.** S4's "later" list and S3's format tail exist to be
  pruned; nothing in either blocks 4.0.

## What 4.0 still refuses to do

Windows, Lua/plugin systems, in-terminal image rendering, browser-style
tabs, ext2 undelete (obsolete). Everything else mc does is in scope.
