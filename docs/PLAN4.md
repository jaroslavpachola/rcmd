# rcmd 4.0 - the parity release

**Status:** COMPLETE - drafted 2026-08-22 from the decisions in
[MC-DIFF.md](MC-DIFF.md), every phase done and shipped as 4.0.0 on
2026-08-26. **Prerequisite:** the 3.0.0 tag (R1 dogfooding window).
Baseline: 2.5.0 - subshell, SFTP, built-in editor, job queue, archive
browsing, git awareness.

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

### S0 - foundations (blocks most of the rest) - DONE (2026-08-22)

- **Config/state split**: `config.toml` becomes read-only from rcmd's
  side (user comments and formatting survive verbatim); panel state,
  sort, hotlist, command history and find/panelize presets move to an
  rcmd-owned `state.toml`. Merge-on-write semantics stay.
- **Grouped options dialog**: one sectioned form covering mc's whole
  setting surface - panel, layout, confirmations, appearance, VFS. Exit
  confirmation exists, default off; delete/overwrite confirmations exist,
  default on.
- **mc import layer**: done, as `rcmd --import-mc` rather than a
  first-run importer - `config.toml` is the user's file now, so the
  conversion prints to stdout for them to paste instead of writing it.
  Covers `menu`, `mc.ext` / `mc.ext.ini` and `mc.keymap`. Per-directory
  `.mc.menu` is not read yet; it belongs with the user-menu conditions
  in a later phase.
- **Per-context keymaps**: `[keys.panel|viewer|editor]` - done. The
  `dialog` context is deferred: rebinding OK/Cancel/next-field means
  routing every dialog's keys through a table first, which belongs with
  the dialog work in S2/S6 rather than here.
- **Small keys batch**: Esc prefix timeout to ~250 ms, `M-p`/`M-n`
  history aliases + `M-h` list, `M-a`, `cd -`/CDPATH, `C-x !`,
  command-line `%` macros, persisted command history.

### S1 - panel & layout (first user-visible milestone) - DONE (2026-08-23)

- Layout dialog: split direction, unequal split, hide
  menubar/keybar/command line/hint bar.
- Per-panel mini-status.
- Directory tree: panel listing mode **and** the Command-menu dialog.
- Multi-column brief listing; user-defined listing format.
- Per-extension/type file highlighting rules.
- Per-panel Left/Right menus (listing mode, sort, filter, panelize,
  rescan, encoding, links) - the mc menu structure. Encoding joined
  them in S5 as "Character set", with the codepage work.

### S2 - file-operation dialogs - DONE (2026-08-23)

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

### S3 - VFS breadth - DONE (2026-08-23)

- `fish://` and `ftp://` (the 3.0 refusal is overturned for these two).
- extfs-class formats: deb, rpm, iso9660, cpio, lha/arj/cab, patchfs,
  mailfs - read first, write where the format allows.
- Writable archives: move/delete/mkdir inside, and zip member *replace*
  instead of today's shadowing append.
- Active VFS list dialog + VFS settings.

### S4 - viewer & editor depth - DONE (2026-08-24)

- Viewer: goto line/offset, regex search with case/backwards/whole-word
  options, raw/parsed toggle, format/unformat, ruler, next/previous
  file, bookmarks, `%` jump, hex editing with save.
- Editor, first: **F9 menu bar + editor options dialog** (tab size,
  auto-indent, wrap column, backspace-through-tabs, syntax picker),
  then goto line, bookmarks, line numbers, `~` backups, system
  clipboard, `C-u` undo alias.
- Editor, later: macros, insert file, sort block, pipe block through a
  command, column/rectangular blocks. **Pruned** - this is the list the
  Risks section put here to be pruned, and nothing in it blocks 4.0. A
  block is one shell command away through the panel's own tools, and
  macros are a feature to design rather than to port.
- Screens: several editors/viewers open at once with mc's screen list.
  Done: `M-\`` lists the panels and every open editor and viewer,
  Enter switches, closing one lands back on the panels. Each screen
  carries its own follow-up (a remote upload, a bulk rename), which is
  what makes more than one safe.

### S5 - encoding - DONE (2026-08-25)

Full parity, all three surfaces: per-panel codepage (`M-e`), recoding on
read/write in viewer and editor, and correct round-tripping of non-UTF-8
filenames (no more lossy display). The list is `encoding_rs`, which is
the set every browser implements - a couple of DOS codepages short of
mc's, and every row of it actually decodes.

### S6 - search, compare, panelize - DONE (2026-08-25)

- Select/unselect and filter dialogs: files only, case sensitive, shell
  patterns or regex.
- Find dialog: regex content, case, whole words, skip hidden, follow
  symlinks, start directory, charsets - keeping the gitignore checkbox.
- Find **results window** (Chdir/Again/Panelize/View/Edit) as the
  default; today's stream-into-the-panel stays behind a setting
  (`find_window = false`).
- Panelize presets, saved and async.
- Compare directories: quick / size-only / thorough.
- Internal diff viewer + "Compare files".

### S7 - packaging & the wider world - DONE (2026-08-25)

- `-e FILE` / `-v FILE`, argv0 dispatch (`rcedit`/`rcview`/`rcdiff`
  symlinks, and mc's own names), the missing mc flags
  (`-b -c -C -S -d -u/-U -l`). The alias modes come up on one screen
  with no panels underneath, so closing it ends the session; each goes
  through the panel that holds the file, which is what keeps the view
  filters, the codepage and the diff's size guard in one place.
- Shipped shell wrappers: `contrib/rc.sh`, `contrib/rc.fish`, in the
  release tarballs and covered by the e2e suite.
- Skins: rcmd's TOML theme files over an optional base, **and mc's own
  skin files read where they lie** - the import was optional and turned
  out to be cheaper than a converter. F9 > Options > Appearance picks
  one.
- macOS builds restored to CI and releases (both architectures,
  OpenSSL vendored). The pty layer needed the three terminal ioctls
  spelled out (libc has them for Linux only), `pipe` instead of
  `pipe2`, and `proc_pidinfo` where Linux reads `/proc/<pid>/cwd`.

### S8 - the leftovers - DONE (2026-08-26)

S0-S7 were done, but a sweep of [MC-DIFF.md](MC-DIFF.md) afterwards
found **Adopt** rows that no phase had claimed - each one small enough
to fall between two milestones, which is exactly how they did. The
policy is still MC-DIFF's, so they were parity work rather than a shrug:

- **Quick search** (§2): still prefix-only, still swallowing a
  character that matches nothing. mc's is a substring/wildcard search
  with an input field of its own.
- **Learn keys** (§2): no dialog at all. The keys a terminal sends and
  the keys rcmd expects are not always the same, and the answer cannot
  be "edit the config until it works".
- **`[keys.dialog]`** (§2, deferred out of S0): rebinding OK / Cancel /
  next-field. It was parked for "the dialog work in S2/S6", which came
  and went without it.
- **Hotlist** (§4): groups, a label prompt, edit and move. Today `a`
  adds with a label it made up and `d` drops; that is the whole of it.
- **User menu** (§11): conditions and submenus in the TOML, and the
  per-directory `.mc.menu` that S0 left for "the user-menu conditions
  in a later phase".
- **Macros** (§11): five of mc's (`%f %d %D %t %%`) against its full
  set - `%F %T %s %S %u %U %q` and `%{prompt}`.
- **Dialogs** (§11): input history (`M-p`), mouse, and mc's
  underlined-hotkey scheme in place of today's ad-hoc single letters.
- **Editor odds** (§8): the mc clipboard file, and user syntax files.

All of them landed. Two carry a divergence worth writing down:

- A `.mc.menu` in a directory **adds** to your `[[commands]]` rather
  than replacing them, where mc's local menu shadows the user's own. A
  project's menu is an addition to yours.
- The **mouse in dialogs** reaches the list-shaped ones (hotlist, user
  menu, jobs, history, the pickers): a click selects, a double-click is
  Enter. Dialogs with fields and checkboxes stay keyboard-only, and
  **Learn keys** answers "what does rcmd see?" rather than rewriting a
  keymap, because rcmd cannot reprogram a terminal and should not
  pretend to.

The rest of §9's tail was already in: `lha`/`arj`/`cab` browse through
their own tools alongside `rar` and `7z`.

**So 4.0 is what it set out to be** - a drop-in where an mc user's
hands, config files and expectations all land somewhere, with the
deliberate divergences written down rather than quietly closed. Every
**Adopt** row in MC-DIFF is closed.

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
