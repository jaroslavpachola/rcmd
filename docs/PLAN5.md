# rcmd 5.0 - trust, then reach

**Status:** DRAFT - drafted 2026-09-17 from a code audit of 4.30.8, not
from the README. **Baseline:** 4.30.8 - the parity release (PLAN4), the
orthodox pass (4.11 to 4.27), the window build (4.28 onwards).

Both comparison documents are finished: every **Adopt** row of
[MC-DIFF.md](MC-DIFF.md) and [ORTHODOX-DIFF.md](ORTHODOX-DIFF.md) is
closed. This plan comes from a different question - not "what does mc
have that rcmd lacks on paper" but "what does the code actually do" -
and it found three kinds of thing: bugs, places where a parity row
shipped thinner than mc's, and places where mc never tried and rcmd is
one step from being plainly better.

How much each claim below was checked is part of the record:

- **[run]** reproduced by running code.
- **[read]** confirmed by reading the code on both sides of the claim.
- **[audit]** reported by the audit with file and line, not re-checked.

## Vision

4.0 made rcmd a drop-in. 5.0 makes it the one you *trust with the
files* and then the one that *reaches where mc cannot*. Trust first:
an orthodox manager that deletes what it skipped, or forgets an edit,
has no business growing features. Reach second: containers, the trash,
a recursive synchronize, a results list you can work through - the
places where mc stayed down to earth.

## Standing constraints (unchanged from 4.0)

- Keyboard-first; MC hands stay unbroken.
- `rcmd-core` stays TUI-free; threads + mpsc, no async.
- Every phase ends pty-verified, subshell on *and* off.
- External escape hatches never go away.
- Nothing in MC-DIFF §12 regresses.
- New for 5.0: **a phase that touches `fsops.rs` ships with a test that
  fails before the change.** The S0 bugs all lived where no test looked.

## Phases

### S0 - the bugs (blocks everything; a release of its own) - DONE (2026-09-18, 4.31.0)

- **A cross-device move deletes files it skipped.** [read]
  `move_one`'s fallback is `copy_tree(ctx, src, dst)?;
  delete_tree(ctx, src)?;` (`fsops.rs:1653-1654`), and `copy_tree`
  returns `Ok(())` when a copy error is answered Skip
  (`fsops.rs:1522`) or the overwrite prompt is answered Skip or None
  (`fsops.rs:1486-1488`). The stick fills, Skip all, and what never
  arrived is deleted from the source. Fix: delete only what was copied
  - the provider path already does this per file (`fsops.rs:888`) - or
  carry a "had skips" flag up and leave that subtree alone.
- **The editor forgets it is modified.** [run] Type, F2, keep typing,
  F10: no prompt, and the text typed after the save is gone. `splice`
  merges typing into the last undo group without checking that the
  group is the saved one (`rcmd-edit/src/lib.rs:559-599`), so
  `top_id() == saved_id` stays true. Fix: refuse the merge when the
  last group's id equals `saved_id`. The existing test
  `modified_tracking_survives_undo_past_save` does not cover it.
- **`C-x` chords are dead in the window build.** [read] egui-winit
  0.36.1 turns Ctrl+X/C/V presses into `Event::Cut/Copy/Paste` and
  returns before pushing a key event (its `lib.rs:1021-1035`);
  `rcmd-egui/src/keys.rs` has no arm for any of the three. Not run.
  Fix: map the three events back to the keys they were, and hand
  `Paste` to S1's paste path.
- **Hex goto-offset lands 16 times too far.** [read] `hex_top` is a
  row index (`ui.rs` multiplies it by 16) and `viewer_goto` assigns it
  a byte offset (`app/mod.rs:4767`).
- **A hex search leaves hex mode.** [read] Every hit sets
  `v.hex = false` (`app/mod.rs:4786`) and never positions the hex view.
- **`cd ftp://user:pass@host` is saved to the command history.** [read]
  `submit_command` pushes the raw line before anything inspects it
  (`app/panel.rs:2361`). `vfslog::redact` exists and only the FTP log
  uses it.
- **Remote-edit and remote-view temp files are predictable.** [read]
  `$TMPDIR/rcmd-edit-<pid>-<name>` through a plain `File::create`
  (`app/editor.rs:49-55`, and the viewer's twin): no `O_EXCL`, default
  mode, symlinks followed, leaked on a crash. `tempfile` is already a
  workspace dependency.
- From the audits, to confirm and then fix: [audit]
  - `alt+n` = sort-name is a dead binding; history-next eats the key
    before the keymap lookup (`app/panel.rs:2316`, `keymap.rs:69`).
  - `Alt+N` on a prefilled dialog field wipes it, and `Alt+P` then
    `Alt+N` loses the typed draft (`app/dialog.rs:1404-1414`); the
    command line keeps its draft, so the two disagree.
  - A CR or LF in an FTP filename injects commands
    (`ftp.rs:478,567-608`).
  - 7z member names are passed without `--` (`archive.rs:764-771`).
  - A FIFO in a copied tree reaches `File::open` and blocks with the
    cancel flag unchecked; a device node is read as data
    (`fsops.rs:1428-1478` has no branch for either).
  - Same-file and into-itself guards are lexical only
    (`fsops.rs:1429,1455,1480,1623`): a hard link or a symlinked path
    gets past them, and Overwrite then truncates the source.
  - A mid-file I/O error or a Verify failure leaves the partial file
    behind (`fsops.rs:1596,1600`); only Esc cleans up.
  - The Panelize "Save as" field and the SFTP password field reset the
    cursor on every key (`app/dialog.rs:398`, `app/connect.rs:234`).
  - Extracted members keep setuid/setgid bits (`mode & 0o7777`).

### S1 - the field (the row that started this plan) - DONE (2026-09-18, 4.32.0)

One text-field widget, so that every form gets all of it at once
instead of each dialog growing its own.

- **History in every field.** Today only `Dialog::Input` has it
  (`InputAction::history`, `app/mod.rs:133`). The main F5/F6
  `TransferDialog` never calls `remember_field`, so the "destination"
  ring is fed only by Shift+F5/F6 and Checksum. Find (start, name,
  content), select/unselect/filter, panelize, link, chmod, editor
  search and replace-with, viewer search: nothing persisted. [audit]
- **The history popup**: mc's `Alt+H` inside any field, a list rather
  than blind `Alt+P`/`Alt+N` stepping. The list-dialog machinery is
  `Dialog::History`'s already.
- **Find reopens on the last question**, across sessions, not only
  through the Again button (`open_find` builds a fresh dialog,
  `app/search.rs:230-253`).
- **Line editing**: `edit_line` (`app/mod.rs:5011-5044`) knows Ctrl+A/E,
  Home/End, Backspace/Delete and the arrows. Add Ctrl+W, Alt+B/F and
  Ctrl+arrows, Ctrl+K, Ctrl+Y, Alt+D, Alt+Backspace, Ctrl+Q, and a
  paste key. `field_history_step` re-reads `state.toml` on every
  keypress; load once per dialog.
- **Completion inside fields**, and more than paths: `complete.rs`
  says "no command completion" in its header. Commands from `$PATH`,
  `$VARS`, `~user`, and a candidate list instead of 76 characters of
  status line (`app/panel.rs:2467-2472`).
- **Bracketed paste.** No `EnableBracketedPaste`, and `Event::Paste`
  falls through (`app/mod.rs:3283-3288`): a multi-line paste executes
  line by line, a leading `+ - *` on an empty line fires the select
  actions, and pasted code staircases in the editor. One event, routed
  to whatever has the focus.

### S2 - find, and a panelized listing that lasts - DONE (2026-09-18, 4.33.0)

Shipped as below, except saving a panelized listing under a name: the
external panelize's saved commands (`cat list.txt`) already reload one,
and Ctrl+Insert puts the names on the clipboard. The fuzzy tree
(ORTHODOX-DIFF §3) stays Adopt-later; the finder shipped on its own.


- **Hits carry their position.** The matchers return a bool at the
  first hit (`find.rs:221-293`) and `FindEvent::Match` carries only an
  `Entry`. Results rows become `file:line:text`, View and Edit jump to
  the match with the search pre-seeded, and mc's **First hit** becomes
  a switch instead of the only behaviour.
- **The criteria `Pattern` already has.** `size` and `newer` are
  fields of `Pattern` (`pattern.rs:27-34`) and find builds its pattern
  with `..Default::default()`. Wire them, then add mc's ignore-
  directories list, a non-recursive switch and a max depth.
- **Separate case switches** for name and content, as mc has
  (`app/search.rs:157,166` share one).
- **Operate from the results window**: mark, F5, F6, F8 without
  Panelize first (`app/dialog.rs:283-338`).
- **A panelized listing survives a job.** `panelized = None` on every
  load and reload (`rcmd-core/src/panel.rs:230,430`) and every finished
  job reloads both panels (`app/mod.rs:3538-3541`), so the listing is
  gone after the first F5. Re-stat the list instead of dropping it, and
  let one be saved and reloaded.
- **Walk through the hits**: next and previous match across files from
  inside the viewer and the editor - a quickfix list, which mc has
  never had.
- **An incremental fuzzy finder** over the subtree on the `jwalk` pool
  that 4.30.8 already built. ORTHODOX-DIFF §3's fuzzy tree is the
  sibling row; decide them together.

### S3 - the first run, and mc's leftovers - DONE (2026-09-18, 4.34.0)

Shipped as below, with these left: SFTP and FISH reconnect after a
dropped connection (the keepalive makes it rarer; FTP already logs in
again), `ProxyJump` and `Include` in `~/.ssh/config`, mouse clicks in
the dialogs beyond find, copy/move, select and link, and the subshell
prompt for a shell that has wandered from the panel (the `dir$` form is
shown then, rather than a prompt naming the wrong place).


- **Ship default `[[open]]` / `[[view]]` rules.** All three tables
  default to `Vec::new()` (`config.rs:471-473`), so a fresh install's
  F3 on a PDF or a `.gz` shows bytes. mc ships `mc.ext` with hundreds.
  A small built-in set, overridable, each rule skipped when its tool
  is missing.
- **Transparent single-file decompression**: `foo.log.gz` is not an
  archive (`archive.rs:157`) and the viewer reads raw bytes
  (`view.rs:280`). The decoders are linked already.
- **`--import-mc` reads what mc actually ships**: `Include=` keys and
  `[Include/...]` sections are dropped in silence (`mcimport.rs:253`)
  and `/etc/mc` is never read (`mcimport.rs:24-35`).
- **`~/.ssh/config`**: never read; host, user and port come from the
  URL alone (`sftp.rs:49-73`) and the key list is three hardcoded
  names (`sftp.rs:259-264`). Host, HostName, User, Port, IdentityFile
  is what mc's sftpfs parses; ProxyJump is the stretch.
- **Keepalive and reconnect** for SFTP and FISH; FTP already logs in
  again on the next call. IPv6 literals break the URL split
  (`sftp.rs:59`, `ftp.rs:60`). `.netrc` for FTP.
- **Sort options**: mix files with directories, case-sensitive,
  **version sort** (`file10` sorts before `file2` today,
  `rcmd-core/src/panel.rs:980-982`), SI units, a date format setting
  (`"%b %e %H:%M"` is hardcoded, `ui.rs:1460,1938`).
- **Per-panel state**: one shared hidden/sort/listing set is saved
  from the active panel and applied to both
  (`state.rs:243-245`, `app/mod.rs:2916-2922`).
- **Keys mc has**: `Alt+G/R/J`, `C-x h`, `Alt+,`.
- **Mouse where mc has it**: right-click marks, buttons and checkboxes
  click (`app/mod.rs:3650-3652` says field dialogs "stay
  keyboard-only"), the wheel in lists.
- **Help that knows where it is**: one static 410-line page
  (`ui.rs:526-936`), opened only from the panels. F1 in the viewer,
  the editor and dialogs; search inside it. S7's palette is the other
  half of this.
- **The subshell's real prompt** on the command line
  (`ui.rs:2265-2268` draws a fixed one), and the terminal title.
- **Editor**: Save As; a literal search mode with whole words and
  backwards (it is regex-only, `rcmd-edit/src/lib.rs:851`); Replace
  All as one undo group (`app/editor.rs:813`); Tab and Shift+Tab
  indent a selection instead of replacing it
  (`rcmd-edit/src/lib.rs:623-624`); word completion; bracket jump; a
  warning when the file changed on disk; position remembered per file
  (mc's "save file position"); mixed line endings left alone
  (`lib.rs:211` rewrites them all as CRLF).
- **Viewer**: `N` for the previous match, wrap-around, a match count;
  `cmd | rcview -` (`main.rs:233,247` takes `-` as a path).
- **Tree**: F5/F6/F7/F8 from the tree panel, deferred in a comment at
  `app/panel.rs:259-285`.
- **Config**: `--print-config` with every key commented, the whole
  TOML error rather than its first line (`config.rs:508-519`), a man
  page and shell completions in `contrib/`.
- Stale text: `rcmd-egui/src/exec.rs:16-21` and the `--help` at
  `rcmd-egui/src/main.rs:243` still say the window has no subshell;
  `open_diff` carries a "Quick compare" comment and `run_panelize`
  says "Synchronous" above a streaming body.

### S4 - copies you can trust - DONE (2026-09-18, 4.35.0)

Shipped as below, with these left: zip members are still read into
memory whole (the zip crate wants a seekable reader per member, and
only tar members stream), FISH uploads still buffer the file before
sending it, and reflinks are only tried for a local-to-local copy.
ACLs travel as the `system.posix_acl_*` xattrs they are; there is no
separate ACL code.


mc copies with a read/write loop onto the final name. So does rcmd
(`fsops.rs:1571,1587-1604`, 256 KiB). This is where being written in
2026 should show.

- **Write to a temporary name and rename**, so an overwrite that fails
  halfway has not already truncated the file it was replacing.
- **Reflink, then `copy_file_range`, then the loop**: a copy inside one
  btrfs or xfs volume becomes instant and free. Sparse files stay
  sparse (`SEEK_HOLE`); today they are inflated.
- **Free space checked before the first byte**: `statvfs` is only used
  for the panel footer (`app/mod.rs:4832-4836`), and the pre-scan
  already knows the total.
- **Same file by device and inode**, and into-itself by canonical
  path, replacing S0's lexical guards for good.
- **What mc preserves and rcmd drops**: owner when root, atime,
  directory modes (`create_dir` and no `set_permissions`,
  `fsops.rs:1432`), hard links within a tree (mc tracks inodes),
  FIFOs and device nodes. Then what mc drops too: xattrs and ACLs.
- **Provider transfers honour the form.** `start_vfs_transfer` takes no
  `TransferOpts` (`app/panel.rs:1810-1822`), so preserve, follow links
  and the masks mean nothing on a remote copy.
- **A report, not a count.** "N processed, M skipped"
  (`app/mod.rs:3531-3535`) becomes a list of what was skipped and why,
  viewable after the job.
- **Resume on remotes**: `FsProvider` has no seek or offset
  (`vfs.rs:13-25`), so Reget is local-only. One `open_read_at` /
  `open_append` pair buys SFTP and FTP `REST`.
- **FISH and archive members stream** instead of buffering whole files
  in RAM (`fish.rs:233-238,318-328`, `archive.rs:851-887`), and a
  `tar.gz` extraction stops rescanning from the start per member.
- `fsync` behind a switch; `chattr` beside chmod (mc has had the
  dialog since 4.8.25).

### S5 - the trash as a place, and a queue that is one - DONE (2026-09-18, 4.36.0)

Shipped as below, with these left: only copies and moves take part in
the queue (a delete or a wipe is never held, and never holds one back);
a pause waits for the job's next check, which is at most a chunk of a
file away; notices reach tmux and screen as the bell alone; and the
trash's `directorysizes` cache is not updated when rcmd restores or
purges, which only costs the desktop a recount.


- **`trash://`**: F8 to the trash is rcmd's flagship divergence
  (MC-DIFF §6) and nothing in rcmd can list, restore from or empty it
  - the only `trash` in the tree is the `trash::delete` call
  (`fsops.rs:429`). A panel over the XDG trash: Enter on an entry says
  where it came from, F6 restores, F8 deletes for good, and `C-x u`
  after an F8 becomes the restore ORTHODOX-DIFF §12 left open.
- **Undo as a stack**, not one slot (`app/mod.rs:2755-2759`), and an
  undo of a swap-style bulk rename that works (it refuses to overwrite
  and so does nothing today, `fsops.rs:304-311`).
- **Queue behind**: jobs are a `Vec<Job>`, one thread each, all at
  once (`app/mod.rs:2753`). A third button on the F5 form - run after
  the job already writing to that device - is Total Commander's F2 and
  the difference between two copies to one USB stick finishing and
  fighting.
- **Pause and resume** (ORTHODOX-DIFF §1, Adopt-later).
- **Say when it is done**: a finished background job is a status line
  (`app/mod.rs:3526-3536`). The bell, OSC 9 / 777 where the terminal
  takes it, a desktop notification from the window build, and progress
  in the title.

### S6 - synchronize, recursively, and a diff worth opening - DONE (2026-09-18, 4.37.0)

Shipped as below, with these left: `C-x d` stays mc's one-directory
compare (the recursion lives in Synchronize, where a plan can show it);
archives are still refused as a synchronize side; a merge is saved in
the charset the file was read in, but a file that mixed line endings
comes back with the one it had most of; and the diff's word highlight
is by words, not characters.


- **Recursive compare and synchronize.** Both filter directories out
  (`compare.rs:65-72`, `app/panel.rs:1509-1548`); the README says so
  itself. A tree walk under the same plan dialog.
- **Remote sides.** Compare already reads through `FsProvider`
  (`app/panel.rs:1632-1635`); synchronize demands two local panels
  (`app/panel.rs:1493-1496`). Lifted, it is rsync with a preview, over
  SFTP, FTP and FISH, on servers that have no rsync.
- **Mirror mode**: a row can mean *delete on that side*. Masks on the
  plan. **F3 on a row** opens the diff of that pair.
- **The diff viewer** (`diff.rs`, `app/mod.rs:4505-4525`) scrolls and
  steps. Add intra-line highlighting, ignore whitespace / case / blank
  lines, search and goto, a binary guard, and mcdiff's merge-a-hunk
  and save. Move it off the UI thread and bound its memory: the trace
  clones `V` per edit distance (`diff.rs:136`), multi-GB near the
  20,000-edit cap.
- **Diff against HEAD** for the cursor file - `git2` is linked, the
  viewer is there (ORTHODOX-DIFF §7, Adopt-later).

### S7 - reach - DONE (2026-09-18, 4.38.0)

Shipped as below, with these left: `sudo://` asks for no password (`-n`:
`sudo -v` first), and podman and kubectl are reached exactly as docker
is, without a login step of their own; a `[[vfs]]` filesystem is
read-only, and its members are copied out to a temporary file to be
read; rclone cannot set a file's time after an upload; the kitty
protocol is asked for only in the terminals known to answer, and only
its disambiguating flag is used; the keyring is reached through
`secret-tool` and `security`, with no library of its own, and on a Mac
`security` takes the password on its command line.


- **One shell transport, many panels.** `fish.rs` touches ssh2 in four
  places (`:81-102,107,125-153,189`); everything else goes through
  `run` and gets `Output { stdout, stderr, status }` back. Behind a
  `ShellTransport` trait with that one method, `docker exec -i`,
  `podman`, `kubectl exec`, `adb shell` and `sudo sh` are each a
  `Command` - container, cluster, phone and root panels. Needs S4's
  streaming first, a URL type of its own (fish reuses `SftpUrl`), and
  the scheme added in the three places that hardcode the list
  (`app/connect.rs:12-45`, `is_remote_url` at `app/mod.rs:5049`,
  `app/panel.rs:1737-1746`) - which is the moment to make that a
  registry.
- **User-defined VFS**: mc's extfs. `[[vfs]]` with a `list` and a
  `copyout` command is enough for read-only, and the transport above
  is most of the machinery.
- **rclone, writable**: `rcat`, `deletefile`, `mkdir`, `moveto` behind
  `FsWrite`; the module doc deferred it (`rclone.rs:9-11`).
- **The socket becomes a protocol** (`app/mod.rs:4219-4262`,
  `remote.rs:91-105`): an event direction (subscribe to cd, cursor,
  marks), a verb that pushes a list of paths into a panel, a prompt
  and a menu a script can raise, and a `marked` that survives a space
  in a name - it joins with spaces today.
- **A command palette.** Every action already has a name, because the
  socket needed one. A fuzzy list of them with the bound key beside
  each is the discoverability `HELP_TEXT` cannot give, and the honest
  answer to ORTHODOX-DIFF §10: a feature with no key to spare needs
  no key.
- **The kitty keyboard protocol, where offered.** §10 of
  ORTHODOX-DIFF refused to *depend* on it; asking the terminal and
  pushing the flags when it says yes depends on nothing. `C-i` apart
  from Tab, the Ctrl-digits, an Esc that needs no timeout.
- **Saved connections** (ORTHODOX-DIFF §5, Adopt-later), with
  passwords in the desktop keyring or nowhere.

### S8 - the window - DONE (2026-09-18, 4.39.0)

Shipped as below, with these left: dragging files *out* of the window,
which egui has no way to start; hover beyond what the drop target
says; and pictures load on a thread of their own, so the first frame
of one is a spinner. The title already named the directory (S3).


`rcmd-egui` paints the grid, draws a menu bar, picks a font and hosts a
terminal pane. It handles text, key press, pointer press and wheel
(`keys.rs:47-139`) and nothing else.

- **Images in quick view and F3** - see Open decisions below.
- **Drag and drop**, both ways; **egui's clipboard** instead of
  shelling out to `wl-copy` (`app/mod.rs:4903-4935`).
- **Window size and position** remembered (eframe's `persistence` is
  off, `Cargo.toml:33-38`); the **title** is the static string
  "rcmd-egui" (`main.rs:96`) and should be the directory.
- Pointer move and release: drag-select in the editor, hover, a
  context menu on right click, smooth wheel scrolling.
- IME and compose input.

### S9 - small wins (any order, none blocks a release) - DONE (2026-09-18, 4.40.0)

Shipped: display width in `fit`, `tail`, fields and the editor; rows
built only for what is on screen; the editor's trim, final newline,
visible whitespace, margin, line ending, indent detection and
`.editorconfig`; the info panel's xattrs, ACL, flags, device, mount,
filesystem type and inodes; git's `U`, `S` and ahead/behind; Alt+F6
extract; the hex inspector and hex undo.

Left, each still worth doing and none blocking: graphemes (not only
cells) in the line editor; `marked_stats` is still a walk per draw;
changed-line marks in the editor gutter and ORTHODOX-DIFF §7's git
actions; zstd and 7z writing, archive passwords, a compression level
on the pack form, archives on remote panels and inside one another,
and zip names in a chosen codepage.

**i18n, decided: not now.** Every string stays an English literal.
Translating means a catalogue, a lookup on every string drawn and a
translator per language, and nobody has asked for one; the day someone
does, the strings are all in two files (`ui.rs`, `app/`) and the work
is mechanical. Recorded so the question is closed rather than open.


- **Display width.** Nothing uses `unicode-width`: `fit()`, `tail()`,
  `field_row`, the brief and user columns and the editor's
  `screen_col` count chars, so CJK and emoji names misalign and the
  cursor drifts. Graphemes in the line editor with it.
- **Rows for what is on screen.** Full and Long build a `Row` for every
  entry on every draw (`ui.rs:1414-1427`), date formatting and
  highlight matching included; `marked_stats` is O(n) per draw too.
  Brief and User already touch only what is visible.
- **Editor hygiene mcedit never had**: `.editorconfig`, trim trailing
  whitespace, final newline, indent detection, visible whitespace, a
  right margin, the line ending in the status row.
- **A hex data inspector**: the bytes under the cursor as u8 to u64,
  float and timestamp, either endianness. Hex undo.
- **Git**: staged apart from unstaged, conflicts apart from `M`
  (`git.rs:107-115`), ahead/behind beside the branch, changed-line
  marks in the editor gutter; then ORTHODOX-DIFF §7's actions.
- **Archives**: zstd and 7z writing, passwords (the `zip` crate is
  built with `deflate` alone), a compression level on the pack form,
  **extract here / to the other panel** as a verb (`Alt+F6`), archives
  on a remote panel and inside one another (`panel.rs:539` wants
  local), zip names in a chosen codepage.
- **The info panel**: filesystem type, device, mount point, free
  inodes, xattrs and ACLs.
- **i18n**: every string is a literal. A decision more than a task;
  written down here so it is one.

## Open decisions

Policy 4 of ORTHODOX-DIFF: a decision recorded in MC-DIFF is not
overturned in passing. This plan reopens one and leaves four where
they were.

- **Images, in the window only.** MC-DIFF §13 refused in-*terminal*
  images, and ORTHODOX-DIFF §9 kept the refusal for its real reason:
  protocol detection, cell geometry, terminals that lie. None of that
  exists in `rcmd-egui`, where an image is a texture and a rectangle.
  **Recommendation: reopen for the window build alone**; the terminal
  refusal stands.
- **Panel tabs**, **column blocks**, **editor macros**, **a Lua
  runtime**: untouched. S7's protocol and palette are further
  arguments for leaving the runtime refused.

## Sequencing

**S0 alone, first, released on its own** - nothing else in this plan
is worth more than not losing a file. Then S1, because it is the
smallest phase that touches every dialog and the one a user feels in
the first minute. S2 and S3 in either order. S4 before S5, S6 and S7:
the trash panel, a recursive synchronize and the container panels all
move files, and should move them through the engine S4 leaves behind.
S8 and S9 ride along with whatever release cuts.

## Risks

- **S4 rewrites the code S0 just fixed.** Mitigation: S0's tests stay
  and S4 has to keep them green; temp-and-rename changes what Esc and
  an error leave behind, which is exactly what those tests pin down.
- **Reflink and `copy_file_range` fail in interesting ways** across
  filesystems, FUSE mounts and old kernels. Every fast path falls back
  to the loop on any error, and Verify stays what it is.
- **S1's shared field touches every dialog at once.** Land the widget
  first with behaviour unchanged, then move dialogs onto it one green
  commit at a time; the e2e suite drives most of them already.
- **S7's transport is a long tail** - busybox `stat`, `adb`'s shell,
  `sudo` wanting a password on a tty. Ship docker first, read-only
  first, each transport its own commit.
- **Default `[[open]]` rules are opinions.** Keep the set small, skip
  a rule whose program is missing, and let one line switch them off.
- **Scope gravity.** S9 and the second half of S3 are lists to prune
  from, not promises. Nothing in either blocks 5.0.

## What 5.0 still refuses to do

Windows, a Lua or plugin runtime, in-terminal image rendering,
many-panel MDI desktops, ext2 undelete. Browser-style tabs, column
blocks and editor macros stay **Open** in ORTHODOX-DIFF §9 and are not
decided here.
