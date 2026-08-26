# rcmd vs mc - every difference, and what we decided about it

Baseline: rcmd 2.5.0 (the 3.0 feature set) against GNU Midnight
Commander 4.8.x, decided 2026-08-22; re-read against 4.0.0 on
2026-08-26. Everything below is from a *user's* seat: if it can't be
noticed while using the program, it isn't here.

## Standing policies

These four answers resolve every row that has no note of its own.

1. **Gaps → adopt.** An mc feature rcmd lacks is parity work, planned in
   [PLAN4.md](PLAN4.md) - not a shrug. The exceptions are the four
   Refused rows in §13.
2. **Divergences → keep, and say so.** Where rcmd does the same job
   differently on purpose, rcmd's behaviour stands and this document is
   where it is written down. No mc-compat knob unless a row asks for one.
3. **mc's config files → import, don't adopt.** TOML stays canonical;
   `menu`, `mc.ext` and mc keymap files are read/imported so an mc user's
   setup carries over.
4. **Refusals overturned for VFS only.** `ftp://` and `fish://` are back
   on the menu. Windows, Lua/plugins, in-terminal images and tabs stay
   refused.

**Legend** - `Keep`: rcmd's behaviour stands · `Keep+`: rcmd's default
stands, mc's variant added alongside · `Adopt`: parity work, PLAN4 ·
`Change`: neither, a new decision · `Refused`: non-goal.

## 1. Startup, CLI, config plumbing

| Difference | Decision | Note |
|---|---|---|
| No `-e file` / `-v file` (start in editor/viewer) | Adopt | PLAN4 S7 |
| No `mcedit`/`mcview`/`mcdiff` argv aliases | Adopt | argv0 dispatch + installed symlinks |
| Flags missing: `-b -c -C -S skin -d -u/-U -l` | Adopt | mirror the config toggles as flags |
| No shipped shell wrapper (mc-wrapper.sh) | Adopt | ship `rc.sh` / `rc.fish` from the README |
| `-P FILE` last-dir export | Keep | same idea as mc's `-P` |
| config.toml regenerated on save - comments lost | **Change** | split: user `config.toml` read-only, rcmd-owned `state.toml` for panel/sort/hotlist/history |
| One TOML vs mc's ini + panels.ini + hotlist + history + menu + mc.ext + filehighlight + keymap | Keep+ | TOML canonical, mc files importable (policy 3) |
| No skins | Adopt | theme files; mc skin import optional |
| Options/hotlist saved instantly, merge-on-write | Keep | rcmd-only; beats mc's "Save setup" |

## 2. Input model and keys

| Difference | Decision | Note |
|---|---|---|
| Tab completes when the command line has text (mc: Tab always switches panels) | **Keep** | the flagship divergence; `M-Tab` also completes, empty line still switches |
| Lone Esc resolves after a 1 s meta-prefix timeout | **Change** | shorten to ~250 ms; `Esc Esc` stays the instant escape |
| History on `C-p`/`C-n`; no `M-p`/`M-n`, no `M-h` list | Keep+ | add mc's aliases and the `M-h` history dialog |
| Command history not persisted between sessions | Adopt | into the new state file |
| Quick search is prefix-only, silently rejects non-matches | Adopt | mc-style substring/wildcard matching with its own input field |
| No Learn keys dialog | Adopt | |
| Only the panel context is rebindable | Adopt | `[keys.panel|editor|viewer|dialog]` + mc keymap import |
| Missing chords `C-x l`, `C-x C-s`, `C-x a`, `C-x !` | Adopt | land with their features (§6, §5) |
| Extra legacy aliases (F15–F20, `ctrl+4`) | Keep | rcmd-only, harmless |
| Every panel action rebindable by action name; `[[commands]]` can claim a key | Keep | rcmd-only |

## 3. Panel display and layout

| Difference | Decision | Note |
|---|---|---|
| Split is always vertical 50/50 | Adopt | layout dialog: direction, unequal split, hide menubar/keybar/cmdline/hintbar |
| No per-panel mini-status (one global status row, active panel only) | Adopt | |
| Brief listing is one full-width column | Adopt | multi-column brief |
| No user-defined listing format | Adopt | mc's format string |
| No tree listing mode | Adopt | with §4's tree dialog |
| Active long listing auto-forces the one-panel view | Keep | automatic where mc is manual |
| Panel title shows the full path | Keep | |
| Free space / marked totals live in the panel frame | Keep+ | `show_free_space` in the options dialog (4.3.0); the marked total is not a toggle, it is only there while something is marked |
| Fixed type colours (`/ ~ @ ! *`), no per-extension highlighting | Adopt | filehighlight-equivalent rules in TOML |
| No codepage/encoding anywhere; lossy non-UTF-8 filenames | Adopt | **full parity**: per-panel codepage + viewer/editor recoding + correct filename round-trip |
| Git branch + status column, ignored entries dimmed | Keep | rcmd-only |
| Slow listings load in background with spinner + Esc cancel | Keep | rcmd-only |
| Panels auto-reload on filesystem change | Keep | rcmd-only; mc needs `C-r` |

## 4. Navigation

| Difference | Decision | Note |
|---|---|---|
| `[[open]]` globs instead of mc.ext's matching language | Keep+ | extend TOML to mc.ext's power, import mc.ext |
| No directory tree (panel mode or dialog) | Adopt | |
| History is browser back/forward, no list popup | Keep+ | add mc's `M-H` history list |
| Hotlist flat: auto label, no groups, no edit | Adopt | groups, label prompt, edit/move |
| "Recent directories" merged into the hotlist | Keep | rcmd-only |
| Bare `cd` → home; `cd`/`cd ~` on a remote panel drops back to local | Keep | documented rule |
| No `cd -`, no CDPATH, no `M-a` | Adopt | cheap; `M-a` aliases `C-x p` |

## 5. Marking, filtering, finding

| Difference | Decision | Note |
|---|---|---|
| `+ - \` take a plain glob and also mark directories | Adopt | mc's dialog: files only / case sensitive / shell patterns or regex |
| Filter is a glob; directories always shown | Adopt | same option set as mc's filter dialog |
| Find: glob + literal case-insensitive substring only | Adopt | regex, case, whole words, skip hidden, follow symlinks, start directory, charsets |
| Find streams straight into the panel as a panelized listing | **Keep+** | mc's results window (Chdir/Again/Panelize/View/Edit) becomes the default; direct-panelize stays as a setting |
| Find has a gitignore-skip checkbox | Keep | rcmd-only |
| Panelize asks for a command every time, runs synchronously | Adopt | saved presets + `C-x !`; async run |
| Compare directories has one mode (name+size+mtime, 2 s tolerance) | Adopt | quick / size-only / thorough |
| No diff viewer, no "Compare files" | Adopt | internal diff, mcdiff-equivalent |

## 6. File operations

| Difference | Decision | Note |
|---|---|---|
| **F8 deletes to trash**, `S-F8` permanent (mc: F8 is permanent) | **Keep** | the confirm dialog names which one it is; remote F8 is always permanent |
| Copy/move dialog is a single destination field | Adopt | file masks, preserve attributes, dive into subdirs, follow links, stable symlinks, Background button |
| Attribute policy fixed and implicit | Adopt, **safest defaults** | preserve attributes on, follow links off, stable symlinks on - whichever loses least information, mc-compatible or not |
| Overwrite prompt: Overwrite/All/Skip/Skip all/Abort, no file stats shown | Adopt | add Update (if newer), Size differs, Append, Reget, and both files' size + date |
| Error prompt: Retry/Skip/Skip all/Abort | Keep | already equivalent |
| Progress dialog: one gauge, no ETA or throughput | Adopt | ETA, speed, per-file bar |
| Job queue: any number detached, `C-x j`, asks auto-foreground, quit refused while running | Keep | rcmd-only superset of mc's background jobs |
| chmod is an octal text field | Adopt | mc's permission-bit matrix (octal field kept alongside) |
| chown is a `user[:group]` text field | Adopt | user/group pick lists + advanced chown (perms+owner, recursive) |
| No hard link, no edit-symlink, no relative-symlink option | Adopt | `C-x l`, `C-x C-s`, relative option on `C-x s` |
| `mkdir` creates parents | Keep | rcmd-only convenience |
| Bulk rename through the editor (numbered buffer, preview) | Keep | rcmd-only |
| Cross-device move degrades to copy+delete with recalculated totals | Keep | |
| No ext2 undelete | **Refused** | obsolete on modern filesystems; §13 |

## 7. Viewer (F3)

| Difference | Decision | Note |
|---|---|---|
| No goto line/offset | Adopt | |
| Search is literal, case-insensitive, forward-only | Adopt | regex + case/backwards/whole-words options |
| No raw/parsed toggle, no format/unformat, no ruler | Adopt | |
| No next/previous file, no bookmarks, no `%` position jump | Adopt | |
| Hex mode is read-only (mc can edit and save in hex) | Adopt | |
| Syntax highlighting, follow mode (`f`), lazy index (instant on huge files), precise match highlighting | Keep | rcmd-only |
| `[[view]]` filters on F3, `S-F3` for raw bytes | Keep+ | plus mc's "Filtered view" menu entry |

## 8. Editor (F4)

| Difference | Decision | Note |
|---|---|---|
| No editor menu bar, no editor options (tab size, auto-indent, wrap column…) | **Adopt - first** | the chosen head of the editor milestone |
| No goto line, no bookmarks | Adopt | |
| No macros, insert-file, sort block, pipe-block-through-command | **Keep** | pruned in 4.0 S4: a block is one shell command away through the panel's own tools, and macros are a feature to design rather than to port |
| Stream marking only, no column/rectangular blocks | **Keep** | pruned in 4.0 S4, same reason |
| No line numbers, no syntax picker, no user syntax files | Adopt | |
| No backup file on save | Adopt | mc-style `~` backup, toggleable |
| Clipboard is internal only | Adopt | system clipboard + mc clipboard file |
| Undo/redo on `C-z`/`C-y` (mcedit: `C-u`) | Keep+ | add the `C-u` alias |
| CUA extras (`C-c/x/v/a/s`) | Keep | rcmd-only |
| Replace is smartcase regex with `$1`–`$9` | Keep | superset of mcedit |
| Binary files refused; >2 MB opens without highlighting | Keep | |
| One editor/viewer at a time, no screen list | Adopt | mc's screens (`M-\``) |
| Bulk rename always uses the internal editor | Keep | `$EDITOR` can't signal "session over" |

## 9. Archives and remote filesystems

| Difference | Decision | Note |
|---|---|---|
| Formats: zip, tar(.gz/.xz/.bz2) native; rar/7z read-only via external tool | Adopt more | extfs-class: deb, rpm, iso9660, cpio, lha/arj/cab, patchfs, mailfs |
| No `fish://` | **Adopt** | refusal overturned |
| No `ftp://` | **Adopt** | refusal overturned |
| SFTP exists in both; rcmd's auth ladder (agent → keys → passphrase → kbd-interactive → password), fingerprint dialog, shared connections | Keep | rcmd's implementation stands |
| No Active VFS list, no VFS settings dialog | Adopt | |
| Move/delete/mkdir disabled inside archives | Adopt | writable where the format allows |
| zip append *shadows* a same-named member instead of replacing it | Adopt (fix) | |
| `archive.zip://dir` destination syntax | Keep | rcmd-only |
| Remote panels show numeric uid/gid; typed commands run in `$HOME` | Keep | documented |

## 10. Shell integration

| Difference | Decision | Note |
|---|---|---|
| Subshell (`C-o`, two-way cd sync, respawn on exit) | Keep | mc-equivalent |
| A command typed while the shell is busy is refused with a message | Keep | |
| `subshell = false` fallback: `Press Enter to return`, no job control | Keep | escape hatch, stays forever |
| No `%f/%d/%t` macros on the command line itself | Adopt | mc expands them on the shell line |
| No `C-x !` | Adopt | with panelize presets (§5) |
| No command-history dialog | Adopt | `M-h` (same row as §2) |

## 11. Menus, user menu, dialogs

| Difference | Decision | Note |
|---|---|---|
| Menus are File/Command/Sort/View/Options; no per-panel Left/Right menus | Adopt | mc's Left/File/Command/Options/Right structure |
| Missing entries: tree, compare files, command history, screen list, VFS list, edit extension/menu files | Adopt | each follows its feature; rcmd has one config file, so one "Edit config file" entry |
| No "Undelete files" / "Save setup" entries | Keep | undelete is refused (§13); options and hotlist save instantly (§1), so there is nothing to save |
| Options is one 8-checkbox form; mc has five dialogs | **Adopt, one grouped dialog** | full mc setting coverage including confirmation toggles, but a single sectioned dialog instead of five |
| F10 quits with no confirmation | Keep | exit-confirm toggle exists, default off |
| Delete/overwrite always confirmed, not configurable | Keep+ | toggles exist, default on |
| F2 user menu is flat TOML `[[commands]]` | Adopt | TOML gains conditions + submenus; mc `menu` and per-directory `.mc.menu` imported |
| Only 5 macros (`%f %d %D %t %%`) | Adopt | mc's full macro set |
| Dialogs are keyboard-only, no input history | Adopt | field history (`M-p`) + mouse in dialogs |
| Ad-hoc single-letter hotkeys in dialogs | Adopt | mc's underlined-hotkey scheme everywhere |
| F9 menu already uses `&`-marked hotkeys | Keep | |

## 12. rcmd-only, staying as-is

Git status column and branch · auto-reloading panels · non-blocking
listings · trash as the F8 default · bulk rename through the editor ·
viewer follow mode and syntax highlighting · quick-view hex · free space
in the panel footer · the `C-x i` info panel · the job queue with
detach/reattach · click-to-sort headers · focus-preserving wheel scroll ·
`[[view]]` filters · gitignore-aware find · recent directories in the
hotlist · merge-on-write config · a single static musl binary.

## 13. Platform and refusals

| Item | Decision |
|---|---|
| macOS builds (currently suspended) | Adopt - restore CI and release artifacts |
| Windows | **Refused** |
| Lua / plugin system | **Refused** |
| In-terminal image rendering | **Refused** |
| Browser-style tabs | **Refused** |
| ext2 undelete | **Refused** - obsolete on modern filesystems |

---

Roadmap for everything marked Adopt/Change: [PLAN4.md](PLAN4.md).
It is complete: every **Adopt** row above either shipped in 4.0.0
(2026-08-26) or was re-decided as Keep with its reason next to it.
What is left in this document is the decisions - the `Keep`, `Keep+`,
`Change` and `Refused` rows - which are where rcmd differs from mc on
purpose.

Four **Keep+** rows promised mc's variant alongside rcmd's and 4.0 did
not deliver that half; they are listed under "Left for 4.x" in PLAN4:
the `M-H` directory history list (§4), the "Filtered view" menu entry
(§7), the free-space/marked-totals toggle (§3), and `[[open]]` growing
to mc.ext's matching power (§4).
