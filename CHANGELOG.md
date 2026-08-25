# Changelog

## 3.50.0 - 2026-08-25

- **Skins** (4.0 S7): a theme can be a file now. rcmd's own format is
  TOML naming the fields it sets over an optional `base`, so a skin can
  be three lines rather than a full palette, and it is looked up by
  name in `~/.config/rcmd/themes/`.
- **mc's skins work as they are**: the lookup continues into
  `~/.local/share/mc/skins` and `/usr/share/mc/skins`, and an `.ini`
  found there is read as mc writes it - `[core]`, `[dialog]`,
  `[error]`, `[help]`, `[filehighlight]` and `[buttonbar]` mapped onto
  the same fields. `-S julia256` is all it takes. A skin is read for
  its colours; what rcmd draws for itself is not taken from it.
- Colours now include the 256-colour cube everywhere they are named -
  `colorN`, `rgbRGB` and `grayN` alongside mc's names and `#rrggbb`.
- **F9 > Options > Appearance** lists every theme found and switches on
  Enter, persisting the choice. It replaces the two-way theme radio in
  the options form, which could only ever hold `mc` and `dark` and
  would have overwritten a skin every time the form was accepted.
- A letter in a pick list now walks the rows starting with it from
  wherever the cursor is, rather than always jumping back to the first:
  a skin list has a dozen `modar...` in it, and a key that always lands
  on the same one has stopped working.
- A theme file that cannot be read, names a colour that is not one, or
  sets a field that does not exist is a warning on the status line and
  the mc palette - never a refusal to start.

## 3.49.0 - 2026-08-25

- **The other personalities** (4.0 S7): `rcmd -e FILE` and `rcmd -v FILE`
  bring the program up on **one screen instead of the panels**, and
  closing that screen ends the session - there is nothing underneath it
  to land on. Reached through a link named `rcedit`, `rcview` or
  `rcdiff` it does the same thing off argv[0], mc's own names included,
  since somebody's fingers are already typing `mcedit`.
- `rcedit a b` opens a screen per file with the first one in front, and
  ``Alt+` `` lists them as it always did; `rcdiff a b` is Compare files
  on two paths. Each goes through the panel that holds the file, so the
  `[[view]]` filters, the codepage and the size guard on a diff are the
  same ones F3 and F4 go through, and no subshell is started behind a
  screen nobody can reach it from.
- **mc's startup flags**: `-b` (black and white - the terminal's own
  colours with reverse video where something must stand out, and it
  overrides `-S`), `-c`, `-C keyword=fg,bg:...` laid over the loaded
  theme, `-S NAME`, `-d` (no mouse), `-u`/`-U` (subshell off/on for one
  run) and `-l FILE`.
- `-l FILE` writes every line of FTP and `fish://` dialogue to a file,
  which is what a server that will not list looks like from outside.
  The password is redacted, and control bytes are spelled out so a
  stray one cannot rearrange the log it lands in.
- `-C` names the mc keywords it has nowhere to put rather than dropping
  them in silence, and a theme switch mid-session keeps the overlay:
  it was asked for on the command line, and switching themes is not a
  retraction.
- **Shipped shell wrappers**: `contrib/rc.sh` (bash/zsh) and
  `contrib/rc.fish`, in the release tarballs, doing the one thing rcmd
  cannot do for itself - leaving the shell in the directory you were
  last looking at. A run that ends in a crash, or in a directory that
  has since gone away, leaves the shell exactly where it was.

## 3.48.0 - 2026-08-25

- **Compare files** (4.0 S6, and the last of it): F9 > Command puts the
  cursor file of each panel side by side, lined up by a **Myers diff** -
  the shortest edit script, which is what makes a diff read as a
  description of the change rather than as a list of every line that
  moved. Changed lines are marked on both sides, a line only one file
  has sits opposite a `~~~` gap, `n` and `p` walk the differences, and
  it opens on the first one rather than on whatever the two files
  happen to agree about at the top.
- The common prefix and suffix are taken off before the algorithm runs,
  so a one-line change in a twenty-thousand-line file costs almost
  nothing, and an edit script longer than twenty thousand lines is
  reported as "all of it changed" rather than proved line by line.
- It is a **screen** like the viewer and the editor, so ``Alt+` `` lists
  it and it can be left open while you do something else.
- Fixed: `Screen list...` and `Compare files` both claimed `l` in the
  Command menu, which would have left the second one unreachable by
  letter. With this, **S6 is complete**.

## 3.47.0 - 2026-08-25

- **Panelize keeps a list** (4.0 S6): the commands worth running twice
  now sit above the field, as `[[panelize]]` entries in the config or
  saved while running with Ctrl+S. Tab moves between the list and the
  field, F8 drops one, and Enter runs whichever is in front of you.
  Saved ones go to the state file, so they outlive the session that
  made them.
- **The output streams into the panel** rather than arriving all at
  once when the command exits. A `git ls-files` over a large tree, or
  an `rg -l` that takes a few seconds, fills the listing while it runs,
  says how many it has so far, and stops on Esc.
- The command's failure is only reported when nothing came of it: a
  command that printed a hundred paths and then exited non-zero has
  still panelized a hundred paths.

## 3.46.0 - 2026-08-25

- **Compare directories asks how** (4.0 S6): Ctrl+X d now offers mc's
  three answers - **Quick** (size and date, which is what rcmd always
  did), **Size only**, and **Thorough**.
- **Thorough reads the files.** Two files with the same size and the
  same date can still be different files, and nothing but their bytes
  will say so. It runs on a worker thread, marks each pair as it finds
  it rather than at the end, and Esc stops it part way - a directory of
  large files marks the first difference long before the last pair is
  read.
- **Size only** is for a tree whose timestamps were never going to
  survive the trip: an unzip, an rsync without `-t`, a copy through a
  filesystem that rounds them.
- Thorough works through whatever the panel is on, so a local
  directory can be compared against an SFTP or FTP one byte for byte.
  Archives are still refused, as they were.

## 3.45.0 - 2026-08-25

- **An idle rcmd stops repainting.** The event loop wakes on a timer to
  poll jobs, watches and the like, and it drew a frame on every one of
  those wakeups - eighteen a second, forever, for a screen that was not
  moving. That is a stream of escape sequences down every ssh
  connection and a process waking twenty times a second to say nothing.
  A frame is now drawn when something changed, when something is
  moving, or once every two seconds regardless - the last of those
  being insurance against a change that forgot to say so.
- **...and stops spinning behind a dialog.** A directory change picked
  up by the watcher cannot be acted on while a dialog is open, but the
  pending flag still put the loop in its 50 ms polling mode - so
  opening any dialog after any file change left rcmd busy-waiting until
  the dialog closed. The flag now only counts while the reload could
  actually fire.
- The e2e suite reads the screen rather than the clock: **288 s for a
  full run, down from 447 s**, with the same checks. Most of what it
  used to wait for was rcmd's own idle repainting - the two fixes above
  are what made the difference, and 159 keystrokes that nothing looks
  at in between now go out together.
- Several checks in that suite were **passing vacuously** and are
  fixed: the subshell test asked whether the panels were back by
  looking for the key bar, which the screen still shows while a shell
  owns the terminal (the alternate screen is invisible to the test's
  renderer), so it had been one screen-toggle out of step for a long
  time; and the viewer's search-kind helper pressed Space again before
  the answer to the last one had landed, which could cycle past what it
  was asked for.

## 3.44.0 - 2026-08-25

- **The find results window** (4.0 S6): matches now land in a list of
  their own as they are found, with mc's six buttons - **Chdir**
  (Enter on a row) takes the panel to the file and puts the cursor on
  it, **Again** reopens the dialog on the same question, **Panelize**
  turns the list into the panel listing, **View** and **Edit** open
  the match, **Quit** closes. F3 and F4 work on the row directly.
- **The panel is left alone until you say so.** Streaming into it,
  which is what rcmd did before, spends the listing you were looking at
  on a search you might be about to redo - and there is no way back to
  the list once you have moved. The old shape is `find_window = false`
  and behaves exactly as it did.
- Closing the window stops the walk, and a walk still running keeps
  filling the list while you read it.

## 3.43.0 - 2026-08-25

- **The find dialog grew mc's answers** (4.0 S6): a **start
  directory**, and beside the two patterns **whole words**, **case
  sensitive**, **regular expression**, **all charsets**, **skip
  hidden** and **follow symlinks** - with rcmd's **skip gitignored**
  kept where it was.
- **Content can be a regular expression now**, matched line by line -
  which is what a regular expression means anyway, since `.` does not
  cross a line. Reading a line at a time also bounds what a search can
  pull into memory, however binary the file turns out to be.
- **All charsets looks for the word as another machine spelled it.**
  A file written on a KOI8-R box holds different bytes for "Привет"
  than a UTF-8 one; with the switch on, every codepage's spelling of
  the word is looked for. It costs nothing for an ASCII search, where
  every codepage spells it the same way and the duplicates collapse.
- **Fixed: a non-ASCII content search never matched.** The needle was
  lowercased in full while the haystack was folded a byte at a time,
  which only touches ASCII - so "Привет" was looked for as "привет" in
  a file that spelled it with a capital. The word is now looked for as
  typed as well as lowered wherever the fold cannot reach.
- **Follow symlinks is off by default**, and stays that way: a link
  pointing at its own ancestor is a walk that never ends.
- A pattern that will not compile stops before the walk starts, with
  the dialog still open on what was typed.

## 3.42.0 - 2026-08-25

- **The select, unselect and filter dialogs** (4.0 S6): `+`, `-` and
  `Ctrl+F` now ask mc's question - the pattern, plus **Files only**,
  **Case sensitive** and **Shell patterns**. Unticking the last one
  makes the pattern a **regular expression**, which is the thing a glob
  cannot do: `^\d+\.txt$` selects the numbered files and nothing else.
- One form serves all three, because in mc they are one dialog with a
  different title, and one type in the core (`pattern::Pattern`)
  carries the answers to whoever is matching.
- **Files only leaves directories alone.** A filter that hid them would
  strand you in a directory with no way down, which is why the switch
  is on by default; unticking it lets a pattern take directories too,
  which is what "select every backup" sometimes means.
- **A regex that will not compile is reported**, quoting what the
  regex crate said about the pattern you typed, rather than quietly
  matching nothing. A listing never reports it a second time: the
  dialog has already refused it.
- The panel names the filter it is under along its bottom edge, with
  the options that are not the usual ones spelled out - `*.log (regex)
  (any case)` rather than a pattern that does not look like one.
- Select and unselect now say how many entries they moved.

## 3.41.0 - 2026-08-25

- **Per-panel codepages** (4.0 S5, completing it): `Alt+E` on a panel -
  or Left/Right > Character set, which is the one entry the panel menus
  were still missing from S1 - says what the filenames in this
  directory are written in. Unix names are bytes; a directory made on a
  CP1251 or KOI8-R machine shows as replacement characters until the
  panel is told.
- **The whole panel reads that way**, not just the drawing: the
  listing, the sort order, the glob filter, select/unselect by pattern
  and the quick search all work on the name as the codepage spells it,
  because a name you can read is a name you can look for.
- **Names typed on that panel are written back in it**, so creating
  `Мир` on a KOI8-R panel makes the file the panel then shows rather
  than a second unreadable one beside it. One funnel does it - every
  dialog that turns typed text into a path goes through the same place.
- The panel title names the codepage while one is set, and the dialogs
  that quote a filename (rename, chmod, chown, delete, links) quote it
  as the panel shows it.
- Highlight rules and `[[open]]` globs still match the name read as
  UTF-8: those patterns are extensions, and extensions are ASCII
  whatever the rest of the name is.

## 3.40.0 - 2026-08-25

- **Codepages in the viewer and the editor** (4.0 S5): `Alt+E` picks
  what the bytes mean - UTF-8, the Latin, Cyrillic, Greek and Baltic
  single-byte sets, KOI8-R/U, CP866, and Shift_JIS, EUC-JP, GBK, Big5
  and EUC-KR. A file is bytes and nothing in it says which of those it
  is, which is why mc asks and rcmd now does too.
- **The viewer re-reads at once, and the search follows**: with a
  codepage chosen, searching looks through the text on the screen
  rather than the bytes underneath it, which is the only reading that
  can match what you type.
- **The editor writes back in the codepage it read**, so editing a
  KOI8-R file leaves a KOI8-R file rather than quietly converting it to
  UTF-8. Changing the codepage re-reads, so it asks you to save first
  instead of dropping an edit - mc re-reads too.
- The title bar names the codepage whenever it is not UTF-8, since a
  file read in the wrong one looks like a file that is simply broken.
- The list is `encoding_rs`, pure Rust and both ways, so the static
  musl build is unaffected. It is the set every browser implements,
  which is a couple of DOS codepages short of mc's - a list of what can
  actually be decoded beats a longer one where some rows do nothing.
- Panel codepages and non-UTF-8 filenames are the rest of S5 and come
  next; this is the half that only touches file *contents*.

## 3.39.0 - 2026-08-25

- **Several editors and viewers open at once** (4.0 S4, and the last of
  it): ``Alt+` `` lists them - the panels first, then every open screen
  with what it is on and whether it has unsaved changes - and Enter
  switches. It is mc's screen list, reachable from wherever you are,
  and it is in F9 > Command and the editor's own File menu too.
- Closing a screen lands back on the panels, which is where mc puts you
  as well. Quitting rcmd while an editor still holds unsaved changes
  says how many and asks, whether or not the ordinary exit question is
  switched on - the changes are in a screen you cannot see, so the
  question is the only place they can be mentioned.
- **Each screen carries its own follow-up.** Uploading the scratch copy
  of a remote file and turning a bulk-rename buffer into renames used
  to be App-wide slots; with two editors open, closing one would have
  run the other's. They belong to the editor that was opened for them.
- **Options > Syntax** in the editor picks the highlighting by hand -
  every syntax syntect knows, or plain text - for a file whose name
  does not say what it is. A letter jumps to the first syntax starting
  with it, which is the only way to walk two hundred of them.
- With this, **S4 is complete**: the viewer and editor depth the plan
  asked for. The editor's "later" list - macros, insert file, sort
  block, pipe block, column blocks - is pruned, as the plan's Risks
  section put it there to be.

## 3.38.0 - 2026-08-24

- **Getting around the editor** (4.0 S4, completing its editor list):
  `Alt+L` goes to a line by number, `Alt+K` bookmarks the line the
  cursor is on, `Alt+J` and `Alt+I` walk to the next and previous
  bookmark and `Alt+O` drops them all - mc's keys.
- **A bookmark follows its text.** Inserting or deleting lines above
  one moves it with what it marked; a bookmark that stayed on its line
  *number* would end up pointing at whatever slid into that place,
  which is not what was bookmarked.
- **`Alt+N` draws mc's line-number gutter**, with a `*` beside a
  bookmarked line - the bookmarks are worth seeing and not only
  jumping to. A click in the gutter lands on the start of its line
  rather than eight characters into it.
- **The desktop clipboard**: Ctrl+C and Ctrl+X - and the F5/F6 block
  ops, which fill the same clipboard - also put the text on the system
  clipboard, and Ctrl+V reads it, through `wl-copy`, `xclip`, `xsel` or
  `pbcopy`, whichever is installed. With none of them there, or with no
  desktop to have a clipboard at all (over ssh the tools are not even
  tried), the editor's own clipboard stands, so copy and paste inside
  rcmd work either way and nothing has to be configured for them to.
- **`file~` backups** on save, off by default: the previous contents
  are copied aside before anything is written, which is mc's "Do
  backups" - one step back rather than a history.
- **`Ctrl+U` undoes**, as it does in mc, beside rcmd's `Ctrl+Z`.
- The bottom bar of the editor and the viewer now gives the **note**
  the room it needs and cuts the key list instead. It was the other way
  round, so a longer key list quietly clipped the message the program
  had just printed - exactly when there was something to say.
- The editor options form gained the three switches these need - line
  numbers, backups and the shared clipboard - and the menu bar gained
  the entries, so every one of them can be found without knowing it was
  there.

## 3.37.0 - 2026-08-24

- **The editor has a menu bar** (4.0 S4): **F9** opens File, Edit,
  Search and Options over the title row, with mc's hands - the title
  letters pick a menu, the entry letters run an entry, the arrows walk
  and Esc closes. Every entry names the key that already does it,
  because the menu is how you find the key rather than a second way of
  working.
- **Editor options** (Options > General), which is where the settings
  mc keeps in its own dialog now live: **tab size**, **fill tabs with
  spaces**, **return does autoindent**, **backspace through tabs** and
  the **wrap column**. Left/Right nudge the numbers, Space ticks the
  switches, OK applies them to the open editor and writes them through
  to the state file. They stay out of the panel's grouped options
  dialog on purpose: they belong to the editor, they are set while
  editing, and that form is already a screenful.
- **Tab size is now a setting rather than the number 8** in four
  different renderers. What a tab is worth is decided in one place -
  `rcmd_edit::screen_col` - and the line drawing, the cursor and the
  mouse all ask it rather than each knowing.
- **Fill tabs with spaces** inserts spaces up to the next stop, not a
  fixed run of them: the point of the option is that the file has no
  tabs in it, not that Tab always moves the same distance.
- **Backspace through tabs** takes the whole stop inside an indent made
  of spaces, which is what makes a space indent behave like the tab it
  stands in for. Outside one it is a single character, as it was.
- **The wrap column** pins the soft wrap where you say instead of at
  the window's edge; `window` - the default - is mc's dynamic wrapping.

## 3.36.0 - 2026-08-24

- **Hex editing** (4.0 S4, and the last of the viewer's list): in hex
  mode **F2** puts a cursor on the bytes, which is what mc's button bar
  spends F2 on there. Hex digits type over the byte it is on, two
  halves to a byte; **Tab** switches to the text column, where a
  character stands for itself; the arrows, PgUp/PgDn and Home/End move
  it; **F6** writes and Esc stops editing.
- **Nothing reaches the file until F6.** Changed bytes are marked where
  they will land, the title counts them, and leaving with any still
  unwritten asks - Save, Discard or Cancel - rather than dropping them
  quietly. Stepping to another file with C-f says so too.
- **Bytes are replaced, never inserted or deleted**, so the file's
  length never moves. That is mc's rule as well, and it is what makes
  changing four bytes of a multi-GB file a couple of writes into the
  file that is already there rather than a rewrite of it.
- **Editing needs the file itself.** On an archive member, a remote
  file or a `[[view]]` filter's output the viewer is on a scratch copy
  that is deleted when it closes; F2 says which of those it is instead
  of writing bytes into something about to go away.
- In the text column every printable key is a byte, `q` included, which
  is why Esc leaves editing before `q` means quit again.

## 3.35.0 - 2026-08-24

- **nroff formatting in the viewer** (4.0 S4): **F8** reads the
  overstrikes a formatted file is written with - `_`, a backspace and a
  character for underline, a character over itself for bold, which is
  what a printer that could only move forward understood - as the
  attributes they stand for, instead of showing the control bytes.
  Off by default, because a file full of `^H` is rare and a file with a
  stray one is not.
- **The search follows the mode.** With formatting on, the word on the
  screen is the word a search looks for; the bytes underneath spell it
  with a backspace between every letter and nothing would ever match
  them.
- **F6 swaps the `[[view]]` filter in and out** under the same file:
  the parsed text - `pdftotext`, `tr`, `unzip -l`, whatever the rule
  runs - or the file as it is. It is the in-viewer form of the choice
  Shift+F3 makes when opening one, and it is the same file either way:
  the line numbers, the found line and the marks are dropped, because
  they pointed into the other text.
- **Ctrl+F and Ctrl+B are the next and previous file** of the panel,
  read in the same viewer with the same wrap, hex, ruler, formatting
  and search. The panel cursor follows along, so quitting leaves you on
  what you were reading rather than back where you started. Directories
  are stepped over; the ends of the listing say so rather than wrapping
  around.
- The bottom bar now names what each key does *now*, as mc's button bar
  does: F2 is Unwrap once wrapping is on, F6 is Parse once the filter
  is out, F8 is Unform once formatting is on.

## 3.34.0 - 2026-08-23

- **Getting around a file in the viewer** (4.0 S4): **F5**, `Alt+L` or
  `:` opens goto. mc splits line, offset and percentage across a radio
  button; rcmd takes all three in one field and tells them apart by how
  the number is written - `201` is a line, `0x3e8` and `1000b` are byte
  offsets, `50%` is a share of the file. Anything else says so instead
  of jumping somewhere.
- **Ten numbered marks**: `m` then a digit sets one where you are, `r`
  then the same digit returns. An unset mark says it is unset rather
  than moving you to the top of the file.
- **`Alt+R` toggles a column ruler** under the title. It counts from
  the leftmost column actually on screen, not from the start of the
  line, so it still tells the truth when the view is scrolled sideways.

## 3.33.0 - 2026-08-23

- **The viewer's search dialog** (4.0 S4): F7 or `/` now opens mc's
  dialog rather than a bare prompt. The pattern is read as **Normal**
  (a literal), a **Regular expression**, or **Hexadecimal** bytes, and
  three answers sit beside it: **Case sensitive**, **Whole words** and
  **Backwards**. Tab and the arrows move, Space ticks, Enter searches.
- **`n` repeats the search with its options**, backwards included -
  which is what makes a backwards search usable at all rather than a
  one-shot.
- **Hexadecimal search finds bytes, not text.** `7f454c46`,
  `7f 45 4c 46` and `0x7f 0x45 0x4c 0x46` all name the same four, the
  scan crosses its own chunk boundaries, and the viewer reports the
  line holding the offset it found. It is the only way to look for
  something that is not text, which is why it belongs next to the hex
  view.
- **Whole words means the word, not the letters.** `\b` would not do
  it: that anchors on the pattern's own edges, and a pattern starting
  with punctuation has no word boundary there.
- A broken regular expression is reported **against the pattern that
  was typed**, not against the wrapper rcmd puts around it for whole
  words - the error is about your pattern and should quote your pattern.
- Match highlighting follows the search: a regular expression paints
  what it actually matched, where before every search painted plain
  substrings.

## 3.32.0 - 2026-08-23

- **`fish://` panels** (4.0 S3, and the last of it): `cd
  fish://[user@]host[:port][/path]` puts a panel on a server that has a
  shell but no SFTP subsystem. Same SSH connection, same
  authentication, same host-key dialog - what differs is only what
  happens after login. Browsing, F3, F5 both ways, F6, F7 and F8 all
  work, and so do symlinks, hard links, chmod and chown, because on the
  far end they are just commands.
- **The listing is NUL-separated records, not `ls -l`.** mc's fish
  parses an `ls -l` variant, which cannot survive a filename with a
  space, a newline or a `->` in it. rcmd asks the remote shell for one
  record per entry with each field NUL-terminated, so all three do.
  `stat(1)` is used where the server has it and an `ls`-based fallback
  where it does not, which is the split between ordinary boxes and the
  busybox ones.
- **Every path reaches the shell as one quoted word.** A file called
  `$(rm -rf /)` is a filename, and it is treated as one.
- Each operation is one `exec` on the shared session rather than mc's
  persistent helper shell. More round trips, and much easier to be sure
  of: nothing can be left half-said on a channel that the next command
  then reads as its own output.
- **4.0 S3 is complete**: every extfs format mc shipped, writable zip
  and tar archives, the active VFS list, and all three remote
  protocols.

## 3.31.0 - 2026-08-23

- **`ftp://` panels** (4.0 S3, overturning the 3.0 refusal):
  `cd ftp://[user[:password]@]host[:port][/path]`, or F9 → Command →
  Remote link, puts a panel on an FTP server. Browsing, F3, F5 in both
  directions, F6, F7 and F8 all work, and the connection shows up in
  `C-x a` beside any SFTP ones. No user means the anonymous login.
- **Listings prefer `MLSD`**, which is machine-readable and says what
  everything is, and fall back to `LIST` - whose output is `ls -l` by
  convention and by nothing else - on a server too old for it. The
  refusal is remembered, so the next listing does not pay for it again.
  A name with spaces in it survives either way.
- **A small pool of logged-in connections.** FTP opens a second
  connection for every transfer and cannot use the control connection
  meanwhile, so a transfer takes one from the pool for its whole life
  and hands it back when the reader or writer is dropped. One login
  covers a whole session of listing and copying, and a listing can
  still happen while a copy is running.
- FTP has no symlinks and no way to change ownership; both say so
  rather than pretending. Permissions and timestamps go through
  `SITE CHMOD` and `MFMT`, which are conventions rather than protocol -
  a server without them just leaves the attribute alone.
- The connect machinery is now protocol-agnostic (`rcmd-core::remote`),
  which is what lets one password prompt, one connection cache and one
  active-VFS list serve both schemes. `fish://` is next and rides the
  same rails.

## 3.30.0 - 2026-08-23

- **Archives are writable** (4.0 S3): inside a `.zip` or a `.tar`
  (plain or compressed), **F8 deletes**, **F6 renames** and **F7 makes a
  directory**. Deleting a directory takes what is inside it; renaming
  one takes its whole subtree along.
- **One rewrite per batch, not per file.** An archive has no way to
  remove a member in place, so deleting five of them one at a time
  would rewrite the container five times. Every change in a batch is
  applied in a single pass into a temp file that renames over the
  original, which also means the archive on disk is never a half-written
  one: either the old archive or the new, nothing between.
- **F6 inside an archive takes a bare name**, and prefills one. An
  absolute destination would mean leaving the archive, which is a copy
  followed by a delete and is better asked for as those two things -
  the status line says so rather than doing something surprising.
- **Copying into a zip replaces instead of shadowing.** It used to
  append in place, which was cheaper and left two members carrying one
  name for readers to resolve however they each do. Now the zip is
  rewritten, surviving members copied across still compressed so nothing
  is decoded and re-encoded on the way.
- deb, rpm, iso, cpio and the 7z-backed formats stay read-only. Not for
  want of code: rewriting a package or a disc image is not what a panel
  is for.

## 3.29.0 - 2026-08-23

- **Active VFS list** (4.0 S3): `C-x a`, or F9 → Command → Active VFS
  list, shows what the panels are sitting on that is not the local
  filesystem - open archives and live SFTP connections - with the panel
  each one belongs to, so the list says which side an action will move.
  Enter goes there, and a connection is reused rather than dialled
  again.
- **`f` frees a row**, which is mc's word for it: the panels on it go
  back to a local directory and, if it was a connection, it is dropped
  from the cache. An archive belongs to the panel that opened it; a
  connection outlives one, which is why a connection can be listed as
  idle and an archive never can.
- **VFS settings are not here yet, on purpose.** mc's are all ftp ones -
  timeouts, the anonymous password, the proxy - and rcmd has no ftp yet.
  They will arrive with it rather than as an empty section now.

## 3.28.0 - 2026-08-23

- **lha/lzh, arj and cab browse** (4.0 S3, finishing the extfs list):
  they go through the same external-tool path rar and 7z already used,
  because the 7z family reads all five and rcmd has no reason to write
  a fifth decompressor. With no `7z` installed, opening one says which
  tool it wants and names the format it was asked about, rather than
  reporting an unsupported type.
- Every format mc's extfs helpers covered is now readable: deb, rpm,
  iso9660, cpio, patchfs, mailfs, and these four through 7z.

## 3.27.0 - 2026-08-23

- **Mailboxes browse as their messages** (4.0 S3, and the last of mc's
  extfs formats rcmd was missing): Enter on a `.mbox` or `.mbx` - plain
  or compressed - and each message is an entry, numbered so the panel's
  name order is the order they arrived in and named by its subject.
  Opening one gives an ordinary RFC 822 message, without the `From `
  separator line the mbox format puts between them, which is to say a
  file you can hand to anything that reads mail.
- **Subjects are decoded.** Real mail writes them as
  `=?UTF-8?B?4bmt...?=`, and a listing full of that is no listing at
  all, so RFC 2047 encoded words are unwrapped - both the base64 and
  quoted-printable forms, and the folded headers that carry long ones.
- **A body line beginning "From " does not start a new message.** Mail
  agents are supposed to escape those and plenty do not, so a separator
  has to look like one: a sender with no spaces in it and a four-digit
  year further along. "From now on nothing works." has neither.

## 3.26.0 - 2026-08-23

- **Patches browse as directories** (4.0 S3): Enter on a `.patch` or
  `.diff` - plain or compressed - and it lists as the tree it would
  apply to. Each entry is one file's slice of the diff, so `src/main.rs`
  inside a four-thousand-line patch opens as just the hunks that touch
  it, and because the names are paths they file themselves under the
  directories they name rather than crowding into one flat list.
- Unified diffs, git's own `diff --git`, context diffs and Subversion's
  `Index:` headers all start a section. A deleted file is named by its
  old path, since `+++ /dev/null` has no name to give.
- F5 on an entry writes that file's hunks out as a patch of its own,
  which is the fastest way to split one patch into several.
- **Nothing is applied or reversed.** This is a way of reading a patch,
  not of using one: `patch` is one shell command away and it is better
  at its job than a file manager would be.

## 3.25.0 - 2026-08-23

- **ISO 9660 images** (4.0 S3): Enter on a `.iso` and the disc browses
  in place, with F3 on files and F5 to extract. Nothing is unpacked at
  open - the image is a plain seekable file, so each entry just records
  the sector its data starts at.
- **All three naming schemes are read.** The base format shouts its
  names in 8.3 with a `;1` version suffix, so two extensions exist to
  carry real ones. **Rock Ridge** wins where a disc has it, because it
  brings Unix modes and symlinks along with the names; **Joliet**'s
  UTF-16 names are used where it does not; and the plain 8.3 names,
  version suffix stripped, are the fallback. Which one a disc got is
  worked out by looking at its root directory's own record rather than
  by guessing from the extension.
- Symlinks on a Rock Ridge disc list as symlinks, target and all - the
  component records are reassembled, including the ones that mean "."
  and ".." and "/".

## 3.24.0 - 2026-08-23

- **RPM packages** (4.0 S3): Enter on a `.rpm` and it opens in the same
  shape a `.deb` does. `CONTROL/header` is the package's tags rendered
  as text - name, version, license, summary, description, what the
  payload is wrapped in - and the install scriptlets sit beside it as
  `prein`, `postin`, `preun`, `postun`, since they are shell scripts and
  reading them is the point. `CONTENTS/` is the payload.
- The payload is a **cpio stream**, which is why `rcmd-core::cpio` was
  built first and on its own. It arrives under gzip, xz, lzma, bzip2 or
  zstd, and all five are read - lzma through the same library as xz,
  told to work out which of the two it has.
- **Signatures are stepped over, not checked.** The signature header has
  to be located to find where the next one starts, and that is all rcmd
  does with it: this is a file browser, and a listing is not a claim
  that a package is authentic.
- A header that claims more entries than could fit, or an index entry
  pointing outside its own data store, is refused or dropped rather than
  trusted. Untrusted files get parsed here, and a length field is only a
  suggestion.

## 3.23.0 - 2026-08-23

- **Debian packages** (4.0 S3): Enter on a `.deb` or `.udeb` and the
  package opens as **one tree** instead of the three archives it
  really is - `debian-binary` at the root, the metadata and maintainer
  scripts under `CONTROL/`, everything the package installs under
  `CONTENTS/`. F3 reads a control file, F5 extracts an installed one.
  Opening a package to see what is in it should not mean opening two
  more archives to get there.
- **`ar` archives** browse in their own right, so a `.a` static library
  lists its members. GNU long names (the `//` table) and BSD long names
  (`#1/NN`) both resolve; the symbol table is read and left out of the
  listing, since nothing browses it.
- **zstd**: `.tar.zst`, `.tzst` and `.cpio.zst` join the wrappers, which
  is also what a modern `.deb` needs - Debian and Ubuntu have shipped
  `data.tar.zst` for years now. The decoder is `ruzstd`: pure Rust and
  decode-only, which is all a read-only VFS wants, and it keeps the musl
  static build free of another C library.
- Members of an `ar` archive are located rather than copied: the file is
  on disk and seekable, so the index records an offset and a length and
  reading one means seeking there.

## 3.22.0 - 2026-08-23

- **cpio archives** (4.0 S3): Enter on a `.cpio` - plain or `.gz`,
  `.xz`, `.bz2` - browses it like any other archive, with F3 on members
  and F5 to extract them. All three header shapes are read: the
  `newc`/`crc` hex ASCII one, the portable octal `odc`, and the old
  binary one **in either byte order**, since that format was whatever
  the machine that wrote it happened to be. Each member carries its own
  magic, so a stream that mixes them still reads.
- **Hard links inside a cpio resolve.** cpio writes the bytes once and
  gives the other names an empty record, so a naive reader hands you a
  zero-byte file. The index pairs the empty aliases with the member
  that shares their inode: the listing shows the real size and opening
  the alias gives the real bytes.
- The reader lives in `rcmd-core::cpio` rather than inside the archive
  VFS, because an rpm's payload is a cpio stream and this is the half
  of that work that stands on its own.
- Compression is now an axis of the archive kind rather than a variant
  per combination, which is what let `.cpio.gz` arrive without a fourth
  copy of the gz/xz/bz2 wrapper.
- Device nodes, fifos and sockets in a cpio are dropped from the
  listing, the same as tar has always dropped them - there is nothing a
  panel can do with one.

## 3.21.0 - 2026-08-23

- **Recursive chmod and chown** (4.0 S2, completes it): both windows
  gained a **recurse into directories** box - mc keeps this in a third
  "advanced chown" dialog, rcmd puts it where the change is made. A
  recursive change runs as a **job**, with the progress dialog, a
  Cancel button and the same Retry/Skip questions on error that a copy
  gets: a deep tree is not something to walk between two frames of the
  UI, and halfway down one is exactly when a Cancel button earns its
  place.
- **A directory is changed after what is inside it.** Take the execute
  bit off a directory and nothing under it can be reached any more, so
  the walk goes in first and the door closes behind it. There is a test
  that recursively removes `+x` and then has to put it back before it
  can even look at the file it just changed.
- The box is its own focus stop in both windows, reached with Tab and
  ticked with Space. It briefly answered to `r` in the chown window,
  which the test suite caught within the hour: names get typed at those
  lists, and "jarda" has an r in it. A letter key must not flip a switch
  that turns one chmod into a thousand.
- A recursive chmod writes one mode over the tree: "add these bits" and
  "take these away" are answers about one file's own mode, and a tree
  has no single mode to add them to.

## 3.20.0 - 2026-08-23

- **MC's four link commands** (4.0 S2): `C-x l` makes a **hard link**,
  `C-x s` a symlink holding the entry's **full path**, `C-x v` one
  holding just its **name**, and `C-x C-s` changes where an existing
  link points. All four share one small form - what to point at on top,
  what to call it below - both of which are yours to edit, as they are
  in mc.
- **`C-x s` changed meaning**: it used to write a name-only target, and
  now writes the full path, because that is what mc's absolute symlink
  command does and `C-x v` is the one for the short form. The short one
  is what you want when the link and its target travel together; the
  long one when the target stays put.
- Hard links are local only. The SFTP protocol's hardlink is an OpenSSH
  extension and an archive has nowhere to keep one, so the write trait
  says so rather than pretending - the panel reports it instead of
  failing obscurely.
- Editing a link replaces it: there is no atomic retarget on Unix, so
  the old link is removed and a new one written in its place.

## 3.19.0 - 2026-08-23

- **The rest of MC's confirmations** (4.0 S2): mc asks about six things,
  rcmd asked about three. Two more are now wired, each with its own
  toggle in F9 > Options > Panel options > Confirmation:
  - **Dropping a hotlist entry** (`confirm_hotlist_delete`, on by
    default). `d` in the hotlist removed a line with no way back; it
    asks first now. rcmd has one dialog slot, so the question replaces
    the hotlist and puts it back afterwards either way - answering No
    leaves you where you were, not staring at a panel.
  - **Enter running an `[[open]]` command** (`confirm_execute`, off by
    default, as mc has it). You wrote the rule, so the usual answer is
    "just run it"; the toggle is there for anyone who opens files they
    did not put there.
- mc's sixth, history cleanup, has nothing to confirm: rcmd has no
  clear-history command to guard.
- Checked the other prompts while wiring these. Bulk rename already
  refuses to rename onto an existing file rather than overwriting it,
  and archive extraction goes through the same overwrite question as a
  copy, so `confirm_overwrite` covers it.

## 3.18.0 - 2026-08-23

- **MC's chown pick lists** (4.0 S2): Ctrl+X o asked for `user[:group]`
  as text, which assumes you can name the account you want. It now
  shows the system's users and groups as two lists side by side, with
  the entry's own owner and group preselected and the file's details
  beside them. Tab walks users to groups to the buttons, arrows and
  Page keys move, Home/End jump to the ends.
- The names come from `getpwent`/`getgrent` rather than by reading
  `/etc/passwd`, so accounts that live in LDAP or SSSD are listed too.
  The list is capped at 4096 entries: a directory service will happily
  return far more names than belong in a pick list.
- **On an SFTP panel it stays a typed spec.** Our `/etc/passwd` has
  nothing to say about the server's accounts, and a pick list there
  would be offering confident wrong answers.

## 3.17.0 - 2026-08-23

- **MC's chmod bit matrix** (4.0 S2): Ctrl+X c was a one-line octal
  prompt, which is fine if you already know that 0754 is what you want
  and no help at all otherwise. It is now mc's window: the twelve
  attribute bits as check boxes on the left, the entry's name, mode,
  owner and group on the right, and the octal field kept underneath.
  Space flips a box and the octal follows; type an octal and the boxes
  follow. Neither is the source of truth - they are two views of the
  same mode.
- The **octal field has the focus** when the window opens, so the old
  Ctrl+X c, type a mode, Enter still works exactly as it did; the boxes
  are there for everyone who does not already know the number.
- The buttons are mc's three ways of spending the bits: **Set** writes
  the mode exactly, **Set marked** adds the checked bits to each entry's
  own mode, **Clear marked** takes them away. The last two leave every
  other bit of every file alone, which is the whole point of chmod'ing a
  group at once - and the boxes start from the cursor entry, so what
  gets added or removed is what you can see.

## 3.16.0 - 2026-08-23

- **Progress with throughput, time left and a per-file bar** (4.0 S2):
  the copy dialog now says how fast it is going and how long is left,
  and carries a second bar for the file in hand. On one big file the
  total bar barely moves for minutes, which reads as a hang; on a
  thousand small ones the per-file bar is the one that flickers and the
  total tells the story. Both are there because neither answers the
  question on its own.
- The rate is smoothed over quarter-second windows, since a reading
  taken over a few milliseconds says either zero or gigabytes. Before
  the first window closes the average since the job started stands in -
  the opening seconds of a copy are exactly when someone is watching.
- The per-file numbers come from the worker, so they are the bytes
  actually written rather than a guess, and remote transfers report
  them too.

## 3.15.0 - 2026-08-23

- **MC's copy/rename masks** (4.0 S2, completes the copy dialog): the
  copy/move form starts with a **source mask**, and the destination may
  answer it with wildcards. `*.tar.gz` into `dir/*.tgz` copies
  `foo.tar.gz` to `dir/foo.tgz`; files the mask does not match are left
  where they are, as mc leaves them. The mask's `*` and `?` are capture
  groups numbered left to right, and the destination spends them: `*`
  is the first, `\1`..`\9` any of them, `\0` the whole name. `\u` and
  `\l` change the case of the next character, `\U` and `\L` of
  everything up to `\E`, and `\` quotes itself or a wildcard. Matching
  is greedy, like the regex mc compiles the pattern into, so `*.*`
  against `a.b.c` captures `a.b` and `c`.
- The regex form behind mc's "use shell patterns" switch is **not**
  here, deliberately: rcmd already does regex renaming with capture
  groups in the F9 > File > Bulk rename editor, which is a better place
  for it than a one-line field in a dialog.
- The form still opens on the **destination**, with the mask one row
  above it: the destination is what anyone types, and a form that opens
  somewhere else costs every user a keystroke to save a rare one.
- Masks apply to local copies. The SFTP and archive routes build their
  own targets, so asking for a mask there says so rather than quietly
  ignoring it.

## 3.14.0 - 2026-08-23

- **MC's copy/move form** (4.0 S2): F5 and F6 open a form rather than a
  bare destination line. Under the destination are the four switches
  that change what a copy *means* - **preserve attributes**, **follow
  links**, **dive into subdirs**, **stable symlinks** - and the buttons
  are **OK / Background / Cancel**, where Background starts the job
  detached instead of making you press `b` once it is already running.
  Space flips a box, Up/Down move between rows.
- The defaults are rcmd's careful ones, deliberately not mc's:
  attributes preserved, links recreated rather than followed, symlinks
  kept stable, and a directory copied onto an existing directory of its
  own name lands **inside** it. mc merges the two directories there,
  which is the one behaviour that can silently mix two trees together;
  turning "dive into subdirs" off gives you mc's version.
- **Stable symlinks got a refinement mc does not have**, and it is
  probably why mc ships the option off: a link pointing *inside* the
  tree being copied is left alone. Rewriting those would aim the fresh
  copy back at the original tree - which the user may be about to
  delete - so only links reaching outside the copy, the ones that would
  otherwise break, are recomputed.
- Paths that do not go through the form (Shift+F5, copying into an
  archive, transfers over SFTP) keep the defaults, which is what they
  did before there was a form.

## 3.13.0 - 2026-08-23

- **MC's overwrite prompt** (4.0 S2): "Target already exists -
  overwrite?" was a question nobody could answer, so the prompt now
  shows **both files' size and date** and offers what mc offers. For
  this file: **Overwrite**, **Append** (the source goes on the end of
  the target) and **Reget** (resume - whatever is on disk is taken to
  be the head of the source, so only the rest is copied). For every
  remaining one: **All**, **Update** (only where the source is newer),
  **Size differs**, **None**. The sticky answers decide the file they
  were given on as well, not just the ones after it, and an unknown
  modification time never counts as newer - Update leaves such a target
  alone rather than clobbering it. Up/Down move between the two rows of
  buttons; the old o/a/s/S hotkeys still work. Append and Reget appear
  only where both sides are local files: a VFS provider hands out a
  writer, not a file to seek in, so there is nothing to append to.
  Cancelling an append or a resume no longer deletes the target - that
  file was there before the copy started.

## 3.12.0 - 2026-08-23

- **Per-panel Left and Right menus** (4.0 S1, completes it): the menu
  bar is mc's - **Left, File, Command, Options, Right** - and the two
  panel menus act on *their own* panel whichever one has the focus.
  Each carries what mc puts there: the listing formats (brief, full,
  long, user defined, tree), quick view, info, the sort orders and
  reverse, filter, panelize, rescan and the SFTP link - all of mc's
  per-panel entries except Encoding, which joins them in S5 when there
  is a codepage to pick. rcmd's global Sort and View menus are gone
  into them, and Command keeps what works on both panels at once.
  Using a panel menu **moves the focus to that panel**, which mc does
  not do: several of these entries open a dialog that only lands later,
  and a filter or a panelize prompt acting on a panel other than the
  focused one is how you end up surprised. An entry letter beats a menu
  title's, so the panel-menu entries deliberately avoid `f`, `c`, `o`
  and `r`: File, Command, Options and Right stay one keystroke away
  from an open panel menu, and the documented `F9 o p` still reaches
  the options form. That is also why the filter entry reads **Glob
  filter...** - every letter of "Filter" was already spoken for. With a
  horizontal split the menus are still Left and Right, as in mc, and
  mean top and bottom.
- **Fixed: the permanent menu bar's titles now sit where clicks expect
  them.** `show_menubar = true` drew the bar with different spacing from
  the title row an open menu draws, while clicks were hit-tested against
  the second one - so clicking a title on the permanent bar could open
  its neighbour. The two are drawn the same way now, which also stops
  the bar shifting as a menu opens.

## 3.11.0 - 2026-08-23

- **File highlighting rules** (4.0 S1): `[[highlight]]` colours entries
  by name or by kind - `match = "*.tar.gz"` for a glob, `type = "exe"`
  for what the entry is (`dir linkdir exe link broken file`), plus
  `color` (mc's own colour names, `#rrggbb`, or `default`) and an
  optional `bold`. The first matching rule wins, as with `[[open]]` and
  `[[view]]`. mc splits this over two files - the groups in
  `filehighlight.ini`, their colours in the skin - which only ever made
  sense while skins shipped separately, so a rcmd rule carries both. A
  rule that cannot be understood (a colour typo, a type nobody knows,
  or both `match` and `type` at once) is dropped with a warning in the
  status line rather than taking the listing down, and with no rules
  configured the listing costs exactly what it did before.

## 3.10.0 - 2026-08-23

S1 turns to the panels themselves: both of mc's directory-tree forms,
and the user-defined listing format that lets a panel draw whatever
fields you name.

- **User-defined listing format** (4.0 S1, completes the listing work):
  `listing = "user"` draws whatever `listing_format` says, in mc's own
  format language - a panel size (`half` or `full`, where `full` takes
  the one-panel view), an optional repeat count 1-9 laying the field
  set out side by side, then the fields: `name size bsize type mark
  mtime atime ctime perm mode nlink ngid nuid owner group inode`, plus
  `space` and `|`, each with an optional `:width` (`:width+` grows into
  whatever room is left). mc's own listings are expressible in it -
  Full is `half type name | size | mtime`, Long is `full perm space
  nlink space owner space group space size space mtime space name`.
  Column headers, click-to-sort and the marked/cursor colours all work
  as in the built-in listings. `type` draws the marker rcmd already
  uses (`/ ~ @ ! *`); mc's socket, device and pipe marks need the entry
  model to learn those kinds first. A word the parser does not know costs
  that one column and says so in the status line, rather than taking
  the panel down with it.
- **Directory tree** (4.0 S1): mc's tree figure, in both of its forms.
  F9 > Command > Directory tree opens it as a dialog where Enter takes
  *this* panel to the selected directory and closes; F9 > View > Tree
  (or `listing = "tree"`) turns the panel itself into the figure, where
  Enter opens the selection in the **other** panel and the tree stays
  put - mc's split of the two, and what makes the panel mode a
  navigator rather than a one-shot chooser. Up/Down walk the figure,
  Left/Right go to parent/child, F4 switches between mc's dynamic
  navigation (the default: the figure re-shapes itself around the
  cursor) and static (everything scanned stays visible), Ctrl+R/F2
  rescans a branch that has gone stale and F3 forgets one. Typing in
  the dialog jumps to the next matching directory. Nothing is scanned
  until it is opened - there is no tree cache to go stale, which is the
  one thing mc's own figure warns you about. While a panel shows the
  tree, the actions that mean "the entry under the cursor" (F3-F8,
  marking, directory size) say so instead of acting on the listing
  hidden underneath.

## 3.8.0 - 2026-08-23

Toward parity ([docs/PLAN4.md](docs/PLAN4.md)): S0's foundations are
complete - the config/state split, one grouped options dialog,
per-context key bindings and the mc import layer - and S1 has begun
turning the panels into mc's, with the Layout dialog, the per-panel
mini status and the multi-column brief listing.

- **Multi-column brief listing** (4.0 S1): the brief listing shows names
  in two columns by default, as MC does, filled column by column so
  Down still lands on the file drawn underneath. `brief_columns` sets
  the count (1 keeps the old single full-width column, up to 6); paging
  moves by whole screens of names and a click finds the right column.
- **Mini status** (4.0 S1): each panel can carry its own status row
  describing the entry under *its* cursor (permissions, size, name,
  symlink target), MC-style. Off by default - rcmd's single status line
  already covers the active panel, so the row earns its space mainly by
  showing what the *other* panel is sitting on. Switch it on under
  F9 > Options > Panel options > Layout.
- **Layout settings** (4.0 S1): MC's Layout dialog arrives as a section
  of the options form. The panels can be **stacked horizontally**
  instead of side by side, the **split size** is adjustable (20-80%,
  Left/Right on the ratio row), and the menu bar, status line, command
  line and key bar are each optional. The menu bar is new: MC shows one
  permanently, rcmd only had F9, and it is clickable like the rest.
  With the command line hidden, plain characters only trigger key
  bindings - there is nowhere for them to be typed.
- **Import from Midnight Commander** (4.0 S0, completes it):
  `rcmd --import-mc [DIR]` reads mc's `menu`, `mc.ext` (or the newer
  `mc.ext.ini`) and `mc.keymap` and prints an rcmd config fragment on
  stdout - user menu entries become `[[commands]]`, `Open=` becomes
  `[[open]]`, `View=` becomes `[[view]]`, and panel key bindings become
  `[keys]`. It never writes `config.toml`: that file is yours, so the
  conversion is yours to paste. Simple `regex/` matchers convert to
  globs (including one alternation group, so `\.(png|jpg)$` becomes two
  rules); `type/` matchers, `%cd` commands, unsupported macros and
  unmappable keys are reported on stderr instead of being guessed at.
- **Per-context key bindings** (4.0 S0): `[keys.viewer]` and
  `[keys.editor]` rebind keys inside the F3 viewer and the F4 editor,
  which were hardcoded until now; bare `[keys]` entries still bind in
  the panel (and `[keys.panel]` says so explicitly). Viewer actions:
  quit, wrap, hex, search, search-next, follow. Editor actions: save,
  quit, mark, replace, search, search-next, block-copy, block-move,
  delete-line, undo, redo, copy, cut, paste, select-all, wrap. Unknown
  contexts, keys and action names warn in the status line instead of
  stopping the program.
- **One grouped options dialog** (4.0 S0): F9 > Options > Panel options
  is now a sectioned form - Panel, Confirmation, Shell and editor,
  Appearance - covering MC's whole setting surface in one screen rather
  than its five dialogs. Arrow keys skip the headings.
- **Confirmation settings** (MC parity): *Ask before deleting* and *Ask
  before overwriting* (both on, as before) and *Ask before quitting*
  (off, keeping rcmd's instant F10). Turning the overwrite question off
  answers "overwrite all" for every job; turning the delete question off
  makes F8 act at once.
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
