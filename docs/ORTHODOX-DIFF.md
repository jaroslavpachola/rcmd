# rcmd vs the rest of the orthodox family - what is worth taking

Baseline: rcmd 4.10.2 against Far Manager 3, DOS Navigator 1.51 / DN
OSP, Volkov Commander 4.99, Total Commander 11, Krusader and Double
Commander, the modern TUI generation (ranger, nnn, lf, vifm, broot,
yazi) and a few outsiders (Emacs dired, ncdu, Dolphin, zoxide, rclone).
Decided 2026-08-27. Same rule as [MC-DIFF.md](MC-DIFF.md): everything
below is from a *user's* seat, and every row was checked against the
tree before it was written down.

[MC-DIFF.md](MC-DIFF.md) is the sister document and the senior one. It
asks "where does rcmd differ from mc, and why", and its answer is
finished. This one asks a different question - "what did the other
orthodox managers work out that mc never did" - and its rows are not
parity work. Nothing here is owed to anybody.

## Standing policies

These five answers resolve every row that has no note of its own.

1. **Gaps are not debts.** An mc gap was parity work by policy. A Far or
   Total Commander gap is a *proposal*: each row earns its place on its
   own, and `Skip` is an ordinary answer rather than a failure.
2. **No muscle memory is spent.** An mc user's keys keep meaning what
   they mean. New verbs take unclaimed keys, and where a borrowed
   feature has a famous key of its own (`C-g`, `A-Del`, `C-b`) it gets
   that one only if it is free. Some of them are not: the terminal took
   `C-i`, `C-m` and the Ctrl-digits before rcmd had a say, which §10
   lists so it is not discovered halfway through building the feature.
3. **One config file, still.** New features are TOML in `config.toml`
   with what the UI changes going to `state.toml`, as §1 of MC-DIFF
   settled. Nothing here imports a foreign config format: Far's ini
   files and TC's `wincmd.ini` describe Windows programs.
4. **Where a row reopens a decision in MC-DIFF, it says so.** Three rows
   collide with the refusals in MC-DIFF §13 and two with the editor
   prunings in §8. They are marked `Open` and gathered in §9 rather than
   overturned in passing.
5. **The cheap borrow beats the faithful port.** Far's plugin ABI, DN's
   MDI desktop and TC's plugin types are all *architectures*. Where a
   single feature of one of them is what people actually use, this
   document takes the feature.

**Legend** - `Adopt`: worth building · `Adopt-later`: worth building,
behind the Adopt rows · `Keep`: rcmd already answers this its own way ·
`Skip`: non-goal · `Open`: reopens a decision in MC-DIFF, argued in §9.

## 1. File operations

| Feature | From | Decision | Note |
|---|---|---|---|
| **Directory synchronize**: compare two trees, then copy the differences from a per-row preview, either direction or newer-wins | DN, Krusader, TC | **Adopt - first** | the biggest hole in an otherwise complete feature set. `compare.rs` already computes the comparison in mc's three modes; what is missing is the dialog that *acts* on it. Today `C-x d` marks both sides and stops, and a plain F5 quietly does the wrong thing whenever the differences run both ways |
| **Pack the marked files into a new archive** (`A-F5`), unpack (`A-F6`) | VC, NC, TC, DN | **Adopt - first** | the one missing basic verb. rcmd browses ten formats, extracts from them and copies *into* an existing zip or tar, but nothing anywhere creates one. The zip and tar writers are already linked and only tests call them (`archive.rs:1148`); an external `7z`/`rar` covers what rcmd cannot write itself, which is VC's own design |
| **Undo the last file operation** (move, rename, delete) | Dolphin, Krusader | **Adopt** | an operation log hung on the job queue. F8 already trashes, so an undelete is a restore; move and rename reverse exactly; a copy reverses to deleting what was written. Neither mc nor Far offers this, so it is one of the few rows that would put rcmd *ahead* rather than level |
| Verify after copy: read the destination back and compare | DN, TC | Adopt | a checkbox on the copy form. Nothing in the tree hashes anything but sftp host keys, and this is what makes a copy to a failing stick or a remote target trustworthy |
| Create and verify `.sha256` / `.md5` files for the marked entries | TC | Adopt | a different job from the row above: this is the checksum you hand to someone else |
| **Apply a command once per marked file**, with progress and collected output | Far (`C-g`) | Adopt | `[[commands]]` expands `%s` to every marked file in one invocation. The per-file loop is a different tool, and it is the one you want at two hundred files |
| Wipe: overwrite, then unlink | Far (`A-Del`) | Adopt | beside F8-to-trash and `S-F8`-permanent, which is already a two-way choice this makes three |
| Pause and resume a running operation | DN, Double Commander | Adopt-later | the job queue has detach and Esc and nothing in between |
| Copy to several destinations in one pass | DN, Far | Adopt-later | one read, N writes |
| Split and combine a file | TC, DN | Adopt-later | still occasionally the only way past a size limit |
| Recursive attribute change over many files | Far, DN | Keep | rcmd's advanced chmod/chown is already a recursive job with a progress dialog (`app.rs:9153`) |

## 2. Marking, selecting, finding

| Feature | From | Decision | Note |
|---|---|---|---|
| **Include-exclude masks** (`*.c,*.h\|*_test.*`) as one language everywhere | Far | **Adopt - first** | Far's real trick is not the syntax, it is that select, filter, highlighting, associations and sort groups all speak it. `pattern.rs` has the glob and regex halves already; adding comma lists and the `\|` exclusion upgrades `+`, `-`, `C-f`, `[[highlight]]` and `[[open]]` in one change, and every row below leans on it |
| **Named filter sets**: several per panel, each with include and exclude masks, toggled independently, saved | Far (`C-i`) | Adopt | `panel.filter` is a single anonymous glob (`panel.rs:80`). This is the largest day-to-day difference between using Far and using rcmd. Far's key is Tab in a terminal (§10), so this one needs a key of its own |
| Select by size, date range and attributes, not only by mask | DN, dired's `%` | Adopt | the select dialog is a mask plus three checkboxes. "Everything over 100 MB" or "everything touched since yesterday" means `A-F7` and panelize today |
| Restore the previous selection | Far (`C-m`) | Adopt | marks die to `%u`, to a reload and to every operation; one key that brings the last set back is a handful of lines against the panel's mark storage. Far's key is Enter in a terminal (§10) |
| **Yank registers for files**: `yy` / `dd` / `p`, named | vifm | Adopt-later | collect from four directories into register `a`, paste once. A `HashMap<char, Vec<PathBuf>>` and three keys, and it subsumes most of what the temporary panel below is for |
| Temporary collector panel | Far's TmpPanel, DN's file lists, TC | Adopt-later | rcmd's panelize is one-shot from a command's stdout. Decide this together with the registers row: they solve the same problem and only one of them is needed |
| Persistent letter tags, separate from the selection | ranger | Adopt-later | selections are transient by design; a tag is for the files you are working through today, and it should survive a restart |
| Find inside archives | Far, DN, Krusader | Adopt-later | rcmd browses ten formats and searches file contents, but `A-F7` does not descend into them |
| Duplicate finder: group by size, then by hash, results panelized | Directory Opus, TC | Adopt-later | `find.rs` already streams results into a panel as they arrive |

## 3. Panels and navigation

| Feature | From | Decision | Note |
|---|---|---|---|
| **Panel tabs** | TC, Krusader, DC, nnn's contexts | **Open** | refused in MC-DIFF §13; argued in §9 |
| **Frecency-ranked fuzzy directory jump** | zoxide, fzf | Adopt | recent directories sit in the hotlist in visit order. Ranking them by frequency-and-recency and matching fragments turns `C-\` from a list you browse into one key and three letters. Cheap, and it changes the feel of ordinary navigation more than anything else here |
| **Session persistence**: both panels' directory and listing mode, and the open screens, restored on start | DN's saved desktop | Adopt | `state.rs` carries thirty-odd keys and not one of them is where the panels were |
| Drive and mount menu: mount points with free space, and the open VFS sessions in the same list | Far (`A-F1`/`A-F2`), DN | Adopt | `C-x a` already lists the VFS half. Nothing in the tree reads `/proc/mounts` |
| Folder shortcuts on ten numbered slots | Far (`C-0`..`C-9`) | Adopt | the hotlist is a list you browse; shortcuts are muscle memory, and the two do not replace each other. Ctrl with a digit mostly does not reach a terminal program (§10), so the slots need a prefix - `C-x 0`..`C-x 9` is free and already the shape of rcmd's other chords |
| A history of viewed and edited *files* | Far (`A-F11`) | Adopt | rcmd keeps per-panel directory history and command history. This is the one you want right after closing an editor screen. F11 is often eaten by the terminal emulator (§10), so it belongs on the `A-h` family of keys or in the F9 menu |
| Copy the selected names, and full paths, to the clipboard | Far (`C-Ins`, `C-A-Ins`) | Adopt | the editor already talks to `wl-copy`/`xclip`/`pbcopy`; the panel does not |
| Hide one panel outright | VC (`C-F1`/`C-F2`) | Adopt | the long listing forces the one-panel view and the layout dialog sets a ratio, but nothing simply asks for the whole screen |
| Directory sizes for every directory in the panel at once | TC | Adopt | `C-space` sizes the cursor directory only, one scan at a time (`app.rs:8688`) |
| **Fuzzy tree filter**: type, and the tree collapses to the matching paths with their context | broot | Adopt-later | rcmd has a tree mode and a quick search; this is the two of them fused, and it beats both at "where did I put that" |
| Flat / branch view of a whole subtree in the panel | TC (`C-b`), DN | Adopt-later | find-and-panelize covers it awkwardly and with a different mental model |
| Disk usage mode: the tree sorted by size, drill in, delete from inside it | ncdu | Adopt-later | the numbers are already computed by `C-space`; this is a listing mode and a sort over them |
| Process panel: `/proc` as a listing, F8 signals, columns for CPU and RSS | Far's ProcList | Adopt-later | the panel abstraction and the VFS layer would carry it nearly unchanged |
| Panel scrollbars | DN, mc | Adopt-later | mouse support is already there |
| Many panels as windows, tiled and cascaded | DN's MDI desktop | **Skip** | the two-panel layout is the point of an orthodox manager. The useful subset - several *saved panel pairs* reachable from the ``A-` `` screen list - is the tabs row above, and should be decided there rather than twice |

## 4. Columns, sorting, listing

| Feature | From | Decision | Note |
|---|---|---|---|
| More sort keys: atime, ctime, owner, group, and **unsorted** (on-disk order) | Far, DN | Adopt | `SortKey` is Name/Ext/Size/Mtime (`panel.rs:36`) while `format.rs:33` already parses atime, ctime, owner and group as *columns*. Unsorted matters most on a panelized listing, where the command's own order was the information and sorting throws it away |
| **Sort groups**: masks that pin classes of file to the top whatever the sort key | Far | Adopt-later | shares its configuration with file highlighting, which rcmd already has as `[[highlight]]`, so half the data model exists |
| Ten numbered listing formats | Far, TC (`C-1`..`C-0`) | Adopt-later | one `listing_format` generalized to a numbered set; mostly config plumbing over `format.rs`. Same key problem as the folder shortcuts (§10), and the same answer |
| **Content columns**: fields computed from inside the file (EXIF date, media duration, line count, hash), sortable like any other | TC's WDX plugins, vifm's viewcolumns | Adopt-later | one more field kind in `format.rs`, computed by a provider and cached. The plugin ABI behind TC's version is not needed to get the feature |
| File descriptions: a column, and a key that edits them | DN, Far (`C-z`) | Adopt-later | on Unix the better spelling is a `user.*` extended attribute, with `descript.ion` read where it is found. `C-z` is free in the panel but means undo one screen over and SIGTSTP everywhere else (§10) |

## 5. Remote filesystems

| Feature | From | Decision | Note |
|---|---|---|---|
| **rclone as one provider** | rclone | Adopt | one integration reaches S3, Drive, Dropbox, B2, WebDAV and forty more, through the config the user already has, over `rclone lsjson` and friends. `vfs.rs` defines the shape already |
| WebDAV and S3 implemented natively | Far's NetBox | Skip | superseded by the row above at a fraction of the surface |
| Saved connection manager: host, user, key, start directory, under a name | Far's NetBox sessions, DN's phone book | Adopt-later | today an `sftp://` target lives in the hotlist as a bare URL |

## 6. Editor and viewer

| Feature | From | Decision | Note |
|---|---|---|---|
| Undo for a *completed* bulk rename | TC's multi-rename | Adopt | `rename.rs:111` rolls back a batch that failed halfway; a batch that succeeded is final. Pairs with the operation-undo row in §1 and should share its log |
| Column / rectangular blocks in the editor | Far, DN, TC | **Open** | pruned as `Keep` in MC-DIFF §8; argued in §9 |
| Keyboard macro recording | Far (`C-.`, which no terminal sends - §10) | **Open** | MC-DIFF §8 kept "no macros" as a decision, calling it a feature to design rather than port. Far is the design worth copying if any is; argued in §9 |
| Image preview in quick view and the viewer | ranger, yazi | **Open** | refused in MC-DIFF §13; argued in §9 |
| An expression evaluator on the command line (a `=` prefix) | DN's calculators | Adopt-later | the habit behind DN's built-in tools, without the tools |
| Raw device editing | DN's disk editor | Keep | tested while writing this row, and it found a bug rather than a feature: a block device reports a length of zero, so the viewer showed it empty, and so it showed every `/proc` and `/sys` file. Fixed in 4.10.3. A device is a file on Linux and the F2 hex editor writes bytes in place, so what is left is a README line and whatever permissions the device itself wants |
| Spreadsheet, terminal, modem dialer, print manager, screen saver | DN | Skip | period pieces. The dialer's live descendant is the connection manager in §5 |

## 7. Extensibility

| Feature | From | Decision | Note |
|---|---|---|---|
| **A remote-control socket**: `rcmd -remote 'cd /tmp'`, select, reload | lf | Adopt | one mechanism, and the one that matters. An external script can drive the running instance, which is also what turns `[[commands]]` from "run a command" into a real plugin protocol |
| Plugins as ordinary shell scripts against that protocol | nnn | Adopt, with the row above | no ABI, no embedded runtime, no versioned interface to maintain. MC-DIFF §13 refused a plugin *runtime*, and this is not one |
| A Lua plugin API | Far, yazi, xplr | **Skip** | the refusal stands. The two rows above buy most of what people use plugins for at a small fraction of the surface |
| Git actions: stage and unstage the marked files, diff the cursor file against HEAD, switch branch | Krusader, magit, the modern managers | Adopt-later | `git2` is already linked and `compare.rs` already draws the side-by-side diff, so this is mostly wiring |

## 8. Where the borrowed idea is already rcmd's answer

Far's user-menu conditions and Krusader's user actions are
`[[commands]]` with `when =`. dired's wdired and TC's multi-rename are
rcmd's bulk rename through the editor, which is the better of the two
ergonomics and only wants the undo in §6. Far's file highlighting is
`[[highlight]]`. Far's find-results window, its dialog input history,
its underlined hotkeys and its screen list all arrived with 4.0. FarColorer
is syntect. Krusader's queue manager is the job queue, minus the pause
in §1. VC's whole reason for existing - a fast, small, single binary -
is the static musl build, and the only thing worth adding for it is a
startup-time number in CI so the claim stays honest as this list gets
built.

## 9. Reopened decisions

Five rows collide with decisions already written down. Each is argued
here rather than overturned quietly; each is still `Open`, and the
answer belongs to whoever plans the next milestone.

| Decision | Where | The case for reopening | The case against | Recommendation |
|---|---|---|---|---|
| **Browser-style tabs** | MC-DIFF §13, Refused | Every orthodox manager built or maintained since 1995 has them: TC, Krusader, Double Commander, and nnn's contexts are the same idea with a smaller ambition. rcmd already runs the machinery under ``A-` ``, where several editors and viewers coexist and the panels are one fixed row in the list. Tabs are that list gaining panel entries | The refusal was against browser chrome in an orthodox layout, and a tab bar costs a screen row that the layout dialog has been fighting for | **Reopen**, in the form the screen list already suggests: several saved *panel pairs*, switched from ``A-` `` and optionally from a bar that the layout dialog can hide. That is DN's desktop and TC's tabs meeting where rcmd already stands |
| **In-terminal images** | MC-DIFF §13, Refused | It is the single visible difference between rcmd and a manager written in the 2020s, and quick view is exactly the place for it | Protocol detection (kitty, sixel, iterm2), cell geometry, and redraw discipline under ratatui - the honest estimate is three times any other row here, and it fails ugly on the terminals that lie about their support | **Keep refused for now**, and revisit once the §7 socket exists: a preview handed to an external previewer is how ranger does it anyway, and that path costs rcmd nothing |
| **Lua / plugin runtime** | MC-DIFF §13, Refused | Far, yazi and xplr all have one, and Far's macro engine is the deep version of the macro row | An embedded runtime is a permanent interface to maintain and the largest surface on this page | **Keep refused.** §7's socket plus shell-script plugins is the borrow that pays |
| **Editor column blocks** | MC-DIFF §8, Keep | Three of the four programs surveyed have them. The reason recorded in §8 - that a block operation is one shell command away through the panel's own tools - is true of sort-block and pipe-through-command, and simply does not apply to selecting a rectangle | It is real work inside `rcmd-edit`'s selection model, which is linear today | **Reopen.** The recorded reason does not cover this case |
| **Editor macros** | MC-DIFF §8, Keep | §8 deferred it as "a feature to design rather than to port", and Far's recorded-keys design is exactly the design that was missing | Overlaps the §7 socket for anything scriptable | **Reopen at low priority**, as recorded keys only, and only after the socket lands so the two do not solve the same problem twice |

## 10. Keys the terminal has already taken

Policy 2 keeps an mc user's keys meaning what they mean. The terminal
removes a second set before rcmd gets a say, and several rows above ask
for keys that cannot exist in one. They are written down here rather
than discovered when the feature is half built.

| Key | Wanted by | What it actually is |
|---|---|---|
| `C-i` | Far's filter menu (§2) | Tab. The same byte (HT), and Tab is the one key rcmd has already spent, on its flagship divergence |
| `C-m` | Far's restore selection (§2) | Enter (CR) |
| `C-0`..`C-9` | folder shortcuts (§3), numbered listing formats (§4) | mostly nothing: `C-2` is NUL, `C-3`..`C-8` are ESC, the file separators and DEL, and `C-1`, `C-9`, `C-0` arrive as the bare digit. rcmd's legacy `ctrl+4` alias (MC-DIFF §2) is a survivor of exactly this |
| `C-.` | Far's macro recording (§6) | nothing at all, for the same reason |
| `C-z` | DN and Far's edit-description (§4) | free in the panel, but undo in rcmd's own editor (`keymap.rs:536`) and SIGTSTP everywhere else in a terminal, so taking it teaches a habit that misfires one screen over |
| `F11` | the viewed/edited history (§3) | frequently eaten by the terminal emulator for fullscreen before the program sees it |

Three ways out, in order of preference: pick a free key and record which
key it was in Far, so someone arriving from there can find it; put the
feature in the F9 menu, where it needs no key at all; or leave it to
`[keys.panel]`, which is also part of why the §7 socket is worth landing
early. The kitty keyboard protocol would make most of this table go
away, and depending on it would trade a program that works on every
terminal for a tidier keymap on some.

## 11. Skipped

Windows-shaped or obsolete, and not coming back: Far's registry and
network browsers, drive letters, elevation prompts and its plugin ABI ·
TC's button bar, which is a toolbar for a mouse · DN's spreadsheet,
terminal, dialer, print manager and screen saver · undelete, already
refused in MC-DIFF §13 and no less obsolete here · DN's format and
diskcopy tools · anything whose value was that DOS had no other program
that did it.

## 12. Suggested order

The first five, in this order, on the argument that each is either a
hole in the existing feature set or disproportionately cheap:

1. **Directory synchronize** (§1) - the biggest hole, and half of it is
   already computed by `compare.rs`.
2. **Archive creation, `A-F5`** (§1) - the missing basic verb, with the
   writers already linked.
3. **Include-exclude masks** (§2) - one change that upgrades five
   features, and every other §2 row leans on it.
4. **Undo for file operations** (§1) - the row that puts rcmd ahead of
   both mc and Far rather than level with them.
5. **Frecency jump** (§3) - the cheapest thing here that changes how the
   program feels every day.

Then, in a second pass: panel tabs if §9 reopens them, the remote socket
and script plugins (§7), session persistence and the mount menu (§3),
the named filter sets and richer select criteria (§2), and the sort keys
(§4).

---

This document is decisions, in the shape MC-DIFF settled on. A roadmap
for the rows marked `Adopt` would be `PLAN5.md`, and does not exist yet:
unlike MC-DIFF's, none of these rows is owed to anybody, so the roadmap
should start from the five above rather than from the whole page.
