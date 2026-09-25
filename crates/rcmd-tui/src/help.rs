//! The F1 help: pages on topics, links between them, a contents page,
//! Back, and an About page - mc's help viewer, over rcmd's own text.
//!
//! The text is one list of lines. A line starting with `# ` starts a
//! page and is its heading; the heading's title is what comes before a
//! double space or a parenthesis, and `{{Title}}` anywhere in a line is
//! a link to that page, drawn as the title alone.

use std::sync::LazyLock;

use crate::field::TextField;

const TEXT: &[&str] = &[
    "# Contents",
    "  rcmd is mc's two panels, keys and menus, with a viewer, an editor",
    "  and panels on archives, servers and the trash. Pick a page:",
    "",
    "  {{Using the help}}          the keys of this screen",
    "  {{Panels}}                  moving about, the other panel, history",
    "  {{Listing}}                 formats, sort order, filters, colours",
    "  {{Directories}}             the hotlist, numbered places, the tree",
    "  {{Finding}}                 find file, fuzzy find, panelize",
    "  {{Comparing}}               two files, two directories, synchronize",
    "  {{Marking}}                 Insert, select and unselect by pattern",
    "  {{File operations}}         copy, move, delete, the trash, undo",
    "  {{Jobs}}                    progress, background, the queue",
    "  {{Attributes and links}}    chmod, chattr, chown, links",
    "  {{Archives}}                browsing, packing, writing into them",
    "  {{Remote panels}}           SFTP, FISH, FTP, rclone, containers",
    "  {{Openers and commands}}    what Enter and F3 run, the user menu",
    "  {{Command line}}            running commands, history, completion",
    "  {{Editing a line}}          the keys of every field",
    "  {{Viewer}}                  F3",
    "  {{Editor}}                  F4",
    "  {{Mouse}}                   clicks, the wheel",
    "  {{Menus and options}}       F9, and the options form",
    "  {{Other keys}}              the meta prefix, the palette, scripting",
    "  {{Config}}                  config.toml",
    "  {{About}}                   version, license, where files live",
    "",
    "# Using the help",
    "  Each part of rcmd has a page; {{Contents}} lists them all. A",
    "  highlighted name is a link to another page.",
    "  Tab / S-Tab     the next / previous link",
    "  Enter, Right    follow the picked link (a click follows one too);",
    "                  with no link on screen Enter closes the help",
    "  Left, Backspace back to the page you came from (F3 too); with",
    "                  nowhere to go back to, the contents",
    "  F2, c           the contents",
    "  / or F7         search every page; n finds the next match,",
    "                  going on into the pages after this one",
    "  Up/Down, PgUp/PgDn, Home/End   scroll",
    "  F1              this page",
    "  Esc, F10, q     close the help",
    "  F1 in the viewer, the editor or a dialog opens the help at the",
    "  part about it, and closing it lands back there.",
    "",
    "# Panels",
    "  Tab             switch active panel",
    "  Up/Down, PgUp/PgDn, Home/End   move the cursor",
    "  M-g / M-r / M-j cursor to the top / middle / bottom of the screen",
    "  Enter           enter dir or archive (zip/tar/cpio/deb/rpm/iso)",
    "  Backspace       go to parent directory / leave the archive",
    "  C-s, M-s        quick search (type to jump, C-s again = next)",
    "  C-u             swap the two panels",
    "  M-,             panels side by side, or one above the other",
    "  C-F1 / C-F2     hide the left / right panel: the other takes the",
    "                  screen, and the hidden one keeps everything it had",
    "  M-Left/Right    walk the panel's directory history (back/forward)",
    "  M-y / M-u       history back / forward (same as M-Left/Right)",
    "  M-H             the same history as a list; Enter goes, * is here",
    "  M-i             other panel switches to this panel's directory",
    "  M-o             other panel opens the directory under the cursor",
    "  M-c             quick cd dialog   M-?  find file   C-l  redraw",
    "  C-x q           quick view: the other panel previews the cursor",
    "                  file live (Tab focuses it for scrolling; again = off)",
    "  C-x i           info panel: the other panel shows the full stat of",
    "                  the cursor file (owner, times, inode, xattrs, ACL,",
    "                  flags, the filesystem and its mount; again = off)",
    "  C-spc           directory size (background scan, fills Size column);",
    "                  C-x spc does every directory in the panel, one",
    "                  after another",
    "  C-Ins           the marked names on the clipboard; C-A-Ins their",
    "                  whole paths (the clipboard file always, a desktop",
    "                  clipboard where a tool for one is installed)",
    "  C-r             reload both panels",
    "  Panels auto-reload when their directory changes on disk",
    "  (watch = false in config disables). Slow directories load in the",
    "  background: old listing + spinner stay up, Esc cancels the load.",
    "  (the active panel starts where your shell is; the other one",
    "   where it was left, unless a directory is named on the command",
    "   line - restore_other_dir = false turns that off)",
    "  See also: {{Listing}}, {{Directories}}, {{Marking}}",
    "",
    "# Listing",
    "  M-t             cycle listing format: brief (names in columns,",
    "                  brief_columns in the config) / full / long",
    "                  (an active long panel takes the whole width, MC's",
    "                  one-panel view; Tab or cycling back restores the split)",
    "  F9 > Left/Right   listing format: brief (names), full, long (ls -l,",
    "                  full-width), user defined, tree; the panel footer",
    "                  shows free space (F9 > Options > Layout turns it off)",
    "  F9 > Left/Right > User defined   the panel draws listing_format from the",
    "                  config: a panel size (half/full), an optional repeat",
    "                  count 1-9, then fields - name size bsize type mark",
    "                  mtime atime ctime perm mode nlink ngid nuid owner",
    "                  group inode, plus space and | - each with an optional",
    "                  :width (:width+ grows). MC's own Full listing is",
    "                  \"half type name | size | mtime\"",
    "  M-.             show/hide dotfiles",
    "  M-n             sort by name (again = reverse); the panel's own",
    "                  F9 menu (Left or Right) has the rest: extension,",
    "                  size, modify / access / change time, owner,",
    "                  group, and Unsorted - the order the listing",
    "                  arrived in, which on a panelized listing is the",
    "                  answer the command gave",
    "  Sort (F9 > Left/Right): by version too (file2 before file10), with",
    "                  directories mixed in, or names case-sensitively;",
    "                  each panel keeps its own order across sessions",
    "  C-f             filter which files the panel shows: a pattern,",
    "                  plus Files only, Case sensitive and Shell",
    "                  patterns (off = a regular expression). '*'",
    "                  clears it; the panel says what it is filtering by",
    "                  (a shell pattern is a list: '*.c,*.h' is either",
    "                  of them, and '*.c,*.h|*_test.*' takes the second",
    "                  list back out again)",
    "  C-x f           the named filter sets ([[filter]] in the config):",
    "                  Space ticks one, a switches them all off or on,",
    "                  Enter applies. Several at once show what any of",
    "                  them shows, minus what any of them hides",
    "  M-e             the codepage this panel's filenames are written",
    "                  in (Left/Right menu > Character set). Unix names",
    "                  are bytes; this is where you say what they mean,",
    "                  and names typed here are written back in it.",
    "  [[highlight]] in the config colours entries: match = \"*.tar.gz\" (a",
    "                  glob) or type = \"exe\" (dir linkdir exe link broken",
    "                  file), color = mc's names / #rrggbb / default, and an",
    "                  optional bold; the first matching rule wins",
    "  Inside a git work tree the title shows [branch] and entries get a",
    "  status column: M modified, A added, ? untracked, ! ignored (dim).",
    "  See also: {{Menus and options}}, {{Config}}",
    "",
    "# Directories",
    "  C-\\             directory hotlist (Enter cd, a add, d delete).",
    "                  Under your own entries is everywhere rcmd has",
    "                  been, ranked by how often you go there weighted",
    "                  by how recently - and kept between sessions.",
    "                  C-s narrows the list by what you type",
    "  M-Up            directory hotlist (same as C-\\)",
    "  C-x h           add this directory to the hotlist",
    "  C-x 0-9         the ten numbered places: go to the hotlist entry",
    "                  labelled with that digit, or set an empty slot",
    "                  to this directory",
    "  F9 > Left/Right > Tree   the panel becomes a directory tree: Up and",
    "                  Down walk it, Left/Right go to parent/child, Enter",
    "                  opens the selection in the *other* panel and the",
    "                  tree stays put,",
    "                  F4 switches dynamic/static navigation, C-r rescans,",
    "                  F5 / F6 / F8 copy, move and delete the selected",
    "                  directory, F7 makes one inside it",
    "  F9 > Command > Directory tree   the same figure in a dialog, where",
    "                  Enter takes *this* panel there and closes; typing",
    "                  jumps to a directory, F2 rescans, F3 forgets a branch",
    "  C-x a           everywhere a panel can go: the archives and",
    "                  connections the panels are on, and under them",
    "                  what the machine has mounted, with the room left",
    "                  on each. Enter goes there, f frees an archive or",
    "                  a connection - the panel goes back to a local",
    "                  directory, and an idle connection is forgotten;",
    "                  a mount point is not rcmd's to free. M-F1 / M-F2",
    "                  open the list for the left / right panel by name",
    "  See also: {{Panels}}, {{Command line}} (cd -, $CDPATH)",
    "",
    "# Finding",
    "  M-F7            find file: where to start, the name, and the text",
    "                  to look for inside - with whole words, case for",
    "                  each, a regular expression, every codepage, first",
    "                  hit, recursion, skip hidden, follow symlinks and",
    "                  skip gitignored and inside archives (zip, tar,",
    "                  cpio, packages; Chdir goes in) beside them; dirs to",
    "                  ignore, a size, an age and a depth under them.",
    "                  Results land in a window of their own - a content",
    "                  hit as file:line: text - with Chdir, Again,",
    "                  Panelize, View, Edit; Insert marks rows for F5,",
    "                  F6, F8. Or they stream into the panel with",
    "                  find_window = false. Esc cancels.",
    "  M-. / M-,       in a viewer or editor opened from a hit: the next",
    "                  / previous result, into the next file too",
    "  M-/             fuzzy find: a few letters of a path, ranked as",
    "                  the tree is walked; Enter goes there, F3/F4",
    "                  view and edit it",
    "  C-x !           panelize a command's output (F9 > Command too)",
    "  F9>Left/Right>Panelize: a command's output becomes the listing.",
    "     Saved commands sit above the field - Tab moves between them,",
    "     C-s saves what you typed under a name, F8 drops one. The",
    "     output streams in as it arrives; Esc stops a slow one.",
    "  (C-r restores a normal listing after find/panelize)",
    "  See also: {{Marking}}, {{Viewer}} (searching inside a file)",
    "",
    "# Comparing",
    "  F9>Cmd>Compare files: the cursor file of each panel side by",
    "     side, lined up by the diff - n and p walk the differences,",
    "     a gap marked ~~~ is a line only one of them has, q closes.",
    "     w / i / b: whitespace, case, blank lines do not count;",
    "     F7 search, : go to a line; F5 takes the left's version of",
    "     the difference on screen into the right, S-F5 the other",
    "     way, F2 saves. F9>Cmd>Diff against HEAD: the cursor file",
    "     beside what the last commit has of it; Git: stage, unstage",
    "     (marked files or the cursor's) and switch branch are there too",
    "  C-x d           compare directories, mc's three ways: Quick (size",
    "                  and date), Size only, or Thorough - which reads",
    "                  the files and is the only one that can tell two",
    "                  files with the same size and date apart. Marks",
    "                  what differs on both sides; Esc stops a thorough",
    "                  run part way.",
    "  F9>Cmd>Synchronize: the same comparison over both trees - a",
    "                  server on either side is fine - and then what",
    "                  it means: a plan with one row per difference, an",
    "                  arrow saying which way it goes (the newer side",
    "                  wins, a file only one side has crosses over),",
    "                  Space skipping a row, ←/→ turning one round (an",
    "                  arrow at the empty side deletes), m a mirror of",
    "                  one side, +/- rows by mask, F3 the row's diff, a",
    "                  for all of them, Enter running it. What it copies",
    "                  replaces what it lands on without asking: that is",
    "                  the question the plan already answered. A local",
    "                  delete goes to the trash.",
    "  See also: {{Remote panels}}, {{File operations}}",
    "",
    "# Marking",
    "  Insert, C-t     toggle mark and advance",
    "  +               select by pattern: a glob list or (with Shell",
    "                  patterns unticked) a regular expression, plus",
    "                  Files only and Case sensitive - Tab walks,",
    "                  Space ticks. '*.c,*.h' is either of them and",
    "                  '|*.o' is everything except",
    "  (the same dialog also asks Size (>1M, <=100k, 1M-2G) and Newer",
    "   than (30m, 24h, 7d, 2w); empty asks nothing, and a directory is",
    "   never held to either)",
    "  - or \\          unselect the same way",
    "  *               invert selection",
    "  (the four keys above work while the command line is empty)",
    "  C-x m           put back the marks the last operation spent",
    "  C-x t / p       paste tagged names / the panel path to the cmdline",
    "  See also: {{File operations}}, {{Mouse}} (the right button marks)",
    "",
    "# File operations  (marked entries, or the cursor entry)",
    "  F5              copy - a form: a source mask, where to, then MC's",
    "                  switches for what a copy means (preserve attributes,",
    "                  follow links, dive into subdirs, stable symlinks,",
    "                  verify, sync to disk), then OK / Background /",
    "                  Cancel. Space flips a box, Up/Down move,",
    "                  Background starts the job detached. An overwrite",
    "                  goes to a hidden name and is renamed in at the",
    "                  end; reflinks and holes are kept; free space is",
    "                  checked first",
    "  Masks rename as they copy: source *.tar.gz with destination",
    "                  dir/*.tgz makes foo.tar.gz into dir/foo.tgz. The",
    "                  mask's wildcards are numbered left to right - * in",
    "                  the destination is the first, \\1..\\9 any of them,",
    "                  \\0 the whole name - and \\u \\l \\U \\L \\E change case.",
    "                  Files the mask does not match are left where they are.",
    "  F6              move / rename (the same form)",
    "  S-F5/F6         copy / rename the cursor file in place",
    "  F7              make directory",
    "  S-F4            edit a new file (created on first save)",
    "  F8              delete to trash",
    "  S-F8            delete permanently",
    "  M-Del           wipe: overwrite every byte, then delete. The",
    "                  confirm says what that is and is not worth",
    "  cd trash://     the trash as a panel (F9>Command>Trash too): the",
    "                  line under it says where each thing came from,",
    "                  Enter says it too, F6 puts back, F8 deletes for",
    "                  good, F3 and F5 look and copy out",
    "  C-x u           undo: the moves, bulk renames, F8s to the trash",
    "                  and restores of this session, newest first. Enter",
    "                  undoes the one picked; the undo goes on top, so",
    "                  C-x u Enter again is the redo. A swap in a bulk",
    "                  rename swaps back",
    "  F9 > File > Bulk rename   edit the marked names as text: each",
    "                  line is \"number TAB name\" - change names to",
    "                  rename (swaps are fine), delete lines to delete;",
    "                  save, close, and confirm the preview",
    "  C-g             apply a command to each marked file, one at a",
    "                  time (%f is that file) - where [[commands]] hands",
    "                  them all to one invocation",
    "  F9>File>Checksum file: a sha256sum-format file for what is",
    "                  marked, and Check the checksum file hashes them",
    "                  again and says how many did not match. The copy",
    "                  form's Verify box is the other half: every copy",
    "                  read back and compared with its source",
    "  Overwrite prompt: both files' size and date, then MC's answers -",
    "                  this file: Overwrite / Append / Reget (resume);",
    "                  all files: All / Update (only where the source is",
    "                  newer) / Size differs / None. Up/Down move between",
    "                  the rows. Append and Reget need a local file on both",
    "                  sides. Hotkeys: o=overwrite a=all s=skip S=skip all",
    "  Error prompt hotkeys:     r=retry s=skip S=skip all",
    "  See also: {{Jobs}}, {{Marking}}, {{Attributes and links}},",
    "  {{Archives}}",
    "",
    "# Jobs",
    "  Esc             cancel a running operation",
    "  The progress dialog shows the file in hand, how many items are",
    "                  done, the throughput and the time left, a bar for",
    "                  the whole job and a second one for the current file",
    "  b               send the running operation to the background",
    "  p               pause it; p again goes on (C-x j too)",
    "  F5 > Queue      start when nothing else writes to that device,",
    "                  so two copies to one stick run in turn",
    "  C-x j           jobs list: Enter foregrounds, p pauses, c cancels; the",
    "                  status line shows aggregate background progress",
    "  C-x r           what the last job skipped, and why",
    "  A job that ran in the background, or for more than ten seconds,",
    "                  rings when it is done (and a desktop notice where",
    "                  the terminal passes one on); while jobs run, the",
    "                  title says how far along they are",
    "  See also: {{File operations}}",
    "",
    "# Attributes and links",
    "  C-x c           chmod: MC's bit matrix - the twelve attribute bits",
    "                  as check boxes with the octal beside them (Space",
    "                  flips a box, typing an octal moves the boxes), the",
    "                  file's name/mode/owner/group on the right, and Set /",
    "                  Set marked / Clear marked - the last two add or",
    "                  remove the checked bits and leave each entry's",
    "                  others alone. A recurse box under the octal walks",
    "                  into directories - that runs as a job, with progress",
    "                  and a Cancel button",
    "  C-x e           chattr: the file flags lsattr shows (append only,",
    "                  immutable, no dump, no copy on write...) as check",
    "                  boxes, with chmod's Set / Set marked / Clear marked.",
    "                  Local files only; some flags want root",
    "  C-x o           chown: the system's users and groups as two pick",
    "                  lists, the entry's own owner preselected. Arrows",
    "                  move, Home/End jump.",
    "                  Tab walks users > groups > recurse > buttons; Space",
    "                  on the recurse row walks into directories, as a job",
    "                  On an sftp panel it stays a typed user[:group]: our",
    "                  account names are not the server's",
    "  C-x l           hard link to the cursor entry - a second name for",
    "                  the same file (local panels only)",
    "  C-x s           symlink holding the entry's full path",
    "  C-x v           symlink holding just its name, so the pair can be",
    "                  moved together",
    "  C-x C-s         change where an existing symlink points",
    "  See also: {{File operations}}, {{Jobs}}",
    "",
    "# Archives",
    "  Enter on zip/tar/tar.{gz,xz,bz2} browses it; F5 copies out,",
    "  F3 views members. Move/delete/mkdir are disabled inside.",
    "  rar, 7z, lha/lzh, arj and cab browse through an installed 7z",
    "  (p7zip; rar needs its codec) or unrar. F3 streams one member;",
    "  F5 unpacks all it copies in one run of the tool, then copies.",
    "  Inside a .zip or .tar: F8 deletes, F6 renames (type a bare name",
    "  - an absolute one would mean leaving the archive), F7 makes a",
    "  directory. Each batch rewrites the container once, so deleting",
    "  five members costs one rewrite, not five. Other formats refuse.",
    "  Copy INTO an archive: F5 with the other panel inside it, or a",
    "  destination written as archive.zip://dir - a member of the same",
    "  name is replaced, not shadowed by a second copy of the name.",
    "  M-F5            pack the marked files into an archive: the name",
    "                  says the format (.zip .tar .tar.gz .tgz .tar.xz",
    "                  .txz .tar.bz2 .tbz2 .tar.zst) and the other panel's",
    "                  directory is where it goes. An archive already",
    "                  there is added to rather than replaced. M-0..M-9",
    "                  on the form set the level, M-- the default",
    "  M-F6            extract the marked archives into the other panel,",
    "                  each into a directory named after it",
    "  [[vfs]] rules   files entered like archives through a list and a",
    "                  copyout command - mc's extfs scripts work as-is",
    "  See also: {{File operations}}, {{Directories}} (C-x a)",
    "",
    "# Remote panels (SFTP, FISH and FTP)",
    "  cd sftp://[user@]host[:port][/path]   connect (F9>Cmd>Remote link)",
    "  cd fish://[user@]host[:port][/path]   same SSH, over a shell",
    "  cd ftp://[user[:password]@]host[:port][/path]",
    "  ~/.ssh/config's HostName, User, Port, IdentityFile, Include,",
    "  ProxyJump (key or agent only) and ProxyCommand apply;",
    "  ~/.netrc gives an ftp:// URL its login. IPv6: sftp://[::1]:22.",
    "  Auth: ssh-agent, then the host's keys and ~/.ssh/id_*, then",
    "  password prompts. An idle connection is kept alive, and one",
    "  that drops is dialed again, unasked, on the next operation.",
    "  Unknown host keys show a fingerprint dialog; accepted keys are",
    "  saved to ~/.ssh/known_hosts. The panel title shows the URL.",
    "  F5/F6 up/download between panels (progress dialogs as usual),",
    "  F7 mkdir, F8 deletes on the server (no remote trash!), F3 views,",
    "  F4 edits a local scratch copy and uploads it back on save.",
    "  cd PATH stays on the server; plain cd or cd ~ returns local.",
    "  Both panels may share one connection; C-x d compares",
    "  local vs remote, then F5 syncs the marked differences.",
    "  F9>Cmd>Connections  saved ones: Ins adds, Enter connects, F8",
    "                  forgets, k keeps the password in the keyring",
    "  FISH is for a server with a shell but no SFTP subsystem: every",
    "  operation is one small command over the same SSH session, and",
    "  the listing comes back NUL-separated, so a filename with a",
    "  space, a newline or a \"->\" in it survives - ls -l cannot say",
    "  that. Same auth, same host-key dialog, same keys.",
    "  FTP: no user means the anonymous login. Listings prefer MLSD",
    "  and fall back to LIST. A transfer needs its own connection, so",
    "  a small pool of logged-in ones is kept and reused - one login",
    "  covers a whole session of listing and copying. FTP has no",
    "  symlinks and no chown; those say so instead of pretending.",
    "  cd rclone://remote[/path]  a panel on anything rclone reaches -",
    "                  S3, Drive, Dropbox, WebDAV and the rest - through",
    "                  the config rclone already has, F5 both ways",
    "  cd docker://box/path  a shell a local command reaches as a panel:",
    "                  docker, podman, k8s://[ns:]pod, adb://[serial],",
    "                  sudo://[user] - FISH without the SSH; a",
    "                  password sudo wants is asked for once",
    "  See also: {{Directories}} (C-x a), {{Comparing}}",
    "",
    "# Openers and commands  (config)",
    "  [[open]] rules make Enter open files:",
    "      [[open]]",
    "      match = \"*.pdf\"",
    "      run = \"zathura %f >/dev/null 2>&1 &\"",
    "  First matching glob wins (case-insensitive), local panels only.",
    "  Openers run without a pause; append & for GUI programs.",
    "  With lynx-like motion Right still only enters directories.",
    "  [[view]] rules filter F3 through a command's stdout:",
    "      [[view]]",
    "      match = \"*.pdf\"",
    "      run = \"pdftotext %f -\"",
    "  S-F3 always shows the raw bytes (no filter).",
    "  [[commands]] are shell templates in the F2 user menu:",
    "      [[commands]]",
    "      name = \"git status\"",
    "      run = \"git status | less\"",
    "      key = \"ctrl+g\"        # optional direct binding",
    "  Macros: %f cursor file, %d this dir, %D other panel's dir,",
    "  %t marked files, %% literal percent - all shell-quoted.",
    "  See also: {{Config}}, {{Viewer}}, {{Command line}}",
    "",
    "# Command line",
    "  (type)          compose a command; Enter runs it in the panel dir",
    "  cd PATH         changes the active panel instead",
    "  M-Enter         insert the selected filename",
    "  C-p / C-n       previous / next history entry (M-p / M-n too)",
    "  M-h             pick from the command history (kept across runs)",
    "  M-a             insert this panel's path (same as C-x p)",
    "  cd -            back to the panel's previous directory; a relative",
    "                  cd that misses here also tries $CDPATH",
    "  %f %d %D %t     expand on the command line, as in MC (%% = percent)",
    "  Tab / M-Tab     complete: a command from $PATH as the first word,",
    "                  a path after it, $NAME and ~user anywhere; several",
    "                  candidates open a list to pick from",
    "  Esc             clear the command line - acts at once while typing",
    "  C-o             open a full shell here; exit returns to rcmd",
    "  See also: {{Editing a line}}, {{Openers and commands}}",
    "",
    "# Editing a line (the command line and every dialog field)",
    "  C-a / C-e       start / end of line",
    "  M-b / M-f       word back / forward (C-Left / C-Right too)",
    "  C-w             cut back to the last space (the shell's C-w)",
    "  M-Backspace/M-d cut the word before / after the cursor",
    "  C-k / C-u       cut to the end / the whole line (C-u on the",
    "                  command line is still swap panels, as in MC)",
    "  C-y             put back what was cut last, in any field",
    "  M-p / M-n       walk the field's own history, kept per question",
    "  M-h             the field's history as a list to pick from",
    "  M-Tab           complete a path (Tab too in one-line dialogs)",
    "  a paste         arrives as text: a line break in it runs nothing",
    "  See also: {{Command line}}",
    "",
    "# Viewer (F3)",
    "  F2              toggle line wrap",
    "  F4              toggle hex dump",
    "  F2 (in hex)     hex edit: a cursor on the bytes. Hex digits type",
    "                  over them, Tab switches to the text column where",
    "                  a character is itself, F6 writes them to the file",
    "                  and Esc stops editing. Only on the file itself -",
    "                  an archive member or a filter's output is a copy.",
    "  F7 or /         search: a dialog with MC's four answers - the",
    "                  pattern is Normal, a Regular expression or",
    "                  Hexadecimal bytes (7f454c46 or 7f 45 4c 46),",
    "                  plus Case sensitive, Whole words and Backwards.",
    "                  Tab/arrows move, Space ticks, Enter searches.",
    "                  n repeats it, options and all, and wraps round",
    "                  past the last; N goes the other way. The first",
    "                  search says how many lines match; matches are",
    "                  highlighted and the found line is marked.",
    "  A lone .gz, .xz, .bz2 or .zst reads as what it holds; a PDF, a",
    "  man page, a tarball, an image or a video as what pdftotext,",
    "  man, tar, exiftool or mediainfo say - where they are installed",
    "  (builtin_view = false: only your own [[view]] rules)",
    "  rcview - (or rcmd -v -) pages through a pipe: cmd | rcview -",
    "  Files with a known syntax (≤2 MB) get syntax colors, like F4",
    "  Left/Right      horizontal scroll",
    "  F5 / M-l / :    goto: a line (201), a byte offset (0x3e8 or",
    "                  1000b), or a share of the file (50%)",
    "  m<digit>        set one of ten marks here; r<digit> returns",
    "  M-r             column ruler under the title",
    "  f               follow mode (tail -f): stick to the growing end",
    "  F8              format: nroff overstrikes (_^Ht, t^Ht) read as",
    "                  underline and bold rather than showing as bytes",
    "  F6              swap the [[view]] filter in and out under the",
    "                  same file - the parsed text or the raw one",
    "  C-f / C-b       the next / previous file of the panel, in the",
    "                  same viewer, keeping wrap, hex and the search",
    "  S-F3            raw view (skip any [[view]] filter)",
    "  M-!             filtered view: a command typed now, its output",
    "                  in the viewer (the field starts as the file name)",
    "  F3/F10/Esc/q    close the viewer",
    "  M-`             the screen list (see {{Editor}})",
    "  M-e             which codepage the file is in: the bytes do not",
    "                  say, so this is where you tell it. The search",
    "                  follows, since it reads what you can see.",
    "  See also: {{Editor}}, {{Openers and commands}} ([[view]] rules)",
    "",
    "# Editor (F4, built-in)",
    "  F2 save (atomic, keeps permissions and the line endings - a file",
    "     with both kinds keeps both)   F12 save as   F10/Esc quit",
    "  A file changed on disk since it was opened is not saved over",
    "     without asking; the editor reopens a file where it was left",
    "  F3 mark (select; S-arrows also select)     F8 delete line",
    "  F5 copy the block (no block: duplicate line)   F6 move (cut) it",
    "  M-w toggle soft-wrap (long lines fold instead of scrolling)",
    "  C-c/x/v copy/cut/paste   C-z undo   C-y redo",
    "  C-a select all   C-arrows word hop   Tab inserts a tab, or",
    "     indents the selected lines; S-Tab takes an indent off",
    "  F7 search: the viewer's dialog - Normal, a Regular expression or",
    "     Hexadecimal, case, whole words, backwards; S-F7 again",
    "  M-b the bracket matching this one   M-Tab complete the word from",
    "     the file's own words (a list when several fit)",
    "  F4 replace: pattern, replacement, then Replace/Skip/All/Quit",
    "  M-`             the screen list: several editors and viewers can",
    "                  be open at once, and this is how you move",
    "                  between them (the panels are its first row)",
    "  F9 opens the editor's own menu bar: File, Edit, Search, Options",
    "  M-l goto line   M-k bookmark this line   M-j/M-i next /",
    "     previous bookmark   M-o drop them all   M-n line numbers,",
    "     which mark lines changed since the save: + added, ~ changed,",
    "     _ lines deleted below",
    "  C-u undoes too, as it does in mc",
    "  Copy and cut reach the desktop clipboard and paste reads it,",
    "     through wl-copy / xclip / xsel / pbcopy where one is there",
    "  M-e or Options > Codepage: read the file in another codepage",
    "     and write it back in that one. It re-reads, so it asks you to",
    "     save first rather than dropping an edit.",
    "  Options > Syntax: highlight as any syntax syntect knows, for a",
    "     file whose name does not say what it is",
    "  Options > General: tab size, fill tabs with spaces, autoindent,",
    "     backspace through tabs, the column soft-wrap folds at, line",
    "     numbers, file~ backups and whether the desktop clipboard is",
    "     shared. They apply at once and are remembered across sessions.",
    "  Enter auto-indents (unless that is switched off). Syntax colors",
    "  appear for known file types.",
    "  On sftp panels F4 edits a local copy, uploaded back on quit.",
    "  editor = \"external\" in the config restores $VISUAL/$EDITOR.",
    "  See also: {{Viewer}}, {{Editing a line}}",
    "",
    "# Mouse  (mouse = false in config disables)",
    "  Click focuses a panel and moves the cursor; double-click enters;",
    "  the right button marks. The wheel scrolls the hovered panel,",
    "  viewer, editor, preview or list dialog. In the find, copy/move,",
    "  select, link and options dialogs a click takes a field, ticks a",
    "  switch or presses a button; a question's Yes and No take one too.",
    "  The bottom keybar and the F9 menu are clickable. In the editor a",
    "  click places the cursor. Hold Shift to select terminal text; in",
    "  the window, Shift+drag does it, and so does a plain drag in the",
    "  Ctrl+O shell - letting go copies it to the clipboard.",
    "  In this help a click on a link follows it.",
    "",
    "# Menus and options",
    "  F9              pulldown menu",
    "  F9 > Left/Right   the menu bar is MC's: Left and Right act on that",
    "                  panel whichever one has the focus - listing format,",
    "                  quick view, info, tree, sort order, filter, panelize,",
    "                  rescan, SFTP link. Using one focuses that panel, so",
    "                  the dialogs it opens cannot land on the other.",
    "  In menus the highlighted letter runs the entry (F9 o p = options)",
    "  F9 > Options    one options form, in sections: Layout (split",
    "                  direction and size, which bars are drawn), Panel (hidden",
    "                  files, lynx-like motion, mouse, auto-reload, git),",
    "                  Confirmation (ask before deleting / overwriting /",
    "                  quitting / dropping a hotlist entry / letting Enter",
    "                  run an opener), Shell and editor, Appearance - applied",
    "                  live and saved at once",
    "  M-x             the command palette: every action by a few letters",
    "                  of its name, with its menu label and its key",
    "  See also: {{Listing}}, {{Config}}",
    "",
    "# Other keys",
    "  Esc KEY         meta prefix, like MC: Esc 1..0 = F1..F10,",
    "                  Esc letter = M-letter, Esc Esc = plain Escape",
    "                  (a lone Esc acts after 1 s - at once if you are",
    "                  typing on the command line)",
    "  F1              this help - from the viewer, the editor or a",
    "                  dialog, opened at the part about it; / searches",
    "                  it and n finds the next",
    "  F4              edit (built-in editor, see {{Editor}})",
    "  M-x             the command palette: every action by name",
    "  F10             quit",
    "  rcmd -P FILE    write last directory to FILE on exit",
    "                  (see README for the rc() shell wrapper)",
    "  rcmd --remote cd /tmp drives a running instance from a script:",
    "                  cd, select, unselect, status, any action by name,",
    "                  pwd / other / cursor / marked to ask where it is,",
    "                  panelize (a list on stdin), prompt and menu (the",
    "                  person answers), subscribe (a line per change). A",
    "                  command rcmd starts gets RCMD_SOCKET: no --to PID",
    "  See also: {{Menus and options}}, {{Using the help}}",
    "",
    "# Config",
    "  ~/.config/rcmd/config.toml is yours, never rewritten; rcmd's own",
    "  state lives in ~/.local/state/rcmd/state.toml",
    "  theme = \"mc\" | \"dark\"      keymap = \"mc\" | \"modern\" (= lynx on)",
    "  [keys] adds custom bindings, e.g. \"ctrl+y\" = \"swap-panels\";",
    "  [keys.viewer] and [keys.editor] rebind inside the viewer/editor",
    "  rcmd --print-config prints every setting, commented, to start from;",
    "  F9 > Command > Edit config file opens it in the editor",
    "  See also: {{Openers and commands}}, {{Menus and options}},",
    "  {{About}}                   (where the files are)",
];

/// One page: its heading is line 0.
pub struct Topic {
    pub title: String,
    pub lines: Vec<String>,
}

/// A link on a page: the line it is on, the character it starts at
/// (as drawn, the braces gone), how wide it is, and the page it opens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Link {
    pub line: usize,
    pub col: usize,
    pub width: usize,
    pub target: usize,
}

/// A piece of a drawn line: plain text, or the `n`th link of the page.
pub enum Piece<'a> {
    Text(&'a str),
    Link(&'a str, usize),
}

pub static TOPICS: LazyLock<Vec<Topic>> = LazyLock::new(|| {
    let mut topics: Vec<Topic> = Vec::new();
    for line in TEXT {
        match line.strip_prefix("# ") {
            Some(heading) => topics.push(Topic {
                title: title_of(heading).to_string(),
                lines: vec![line.to_string()],
            }),
            None => {
                if let Some(topic) = topics.last_mut() {
                    topic.lines.push(line.to_string());
                }
            }
        }
    }
    // a page ends where the next one's heading is: the blank between
    // them belongs to neither
    for topic in &mut topics {
        while topic.lines.last().is_some_and(|l| l.is_empty()) {
            topic.lines.pop();
        }
    }
    topics.push(about());
    topics
});

/// A heading's title: what comes before its note.
fn title_of(heading: &str) -> &str {
    let end = [heading.find("  "), heading.find(" (")]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(heading.len());
    heading[..end].trim()
}

/// The About page, written when first asked for: the paths are this
/// session's own.
fn about() -> Topic {
    let path = |p: Option<std::path::PathBuf>| {
        p.map_or_else(|| "(no $HOME)".to_string(), |p| p.display().to_string())
    };
    let lines = vec![
        "# About".to_string(),
        concat!("  rcmd ", env!("CARGO_PKG_VERSION")).to_string(),
        "  an orthodox two-panel file manager, mc's keys and menus in Rust".to_string(),
        String::new(),
        concat!("  License     ", env!("CARGO_PKG_LICENSE")).to_string(),
        concat!("  Source      ", env!("CARGO_PKG_REPOSITORY")).to_string(),
        format!("  Config      {}", path(crate::config::config_path())),
        "              yours: rcmd reads it and never writes it;".to_string(),
        "              F9 > Command > Edit config file opens it".to_string(),
        format!("  State       {}", path(crate::state::state_path())),
        "              rcmd's own: history, hotlist, panels, options".to_string(),
        String::new(),
        "  rcmd --print-config prints every setting, commented. The".to_string(),
        "  README and CHANGELOG in the source say the rest.".to_string(),
        "  See also: {{Config}}, {{Contents}}".to_string(),
    ];
    Topic {
        title: "About".to_string(),
        lines,
    }
}

pub fn topic(at: usize) -> &'static Topic {
    &TOPICS[at.min(TOPICS.len() - 1)]
}

/// The page titled `title`.
pub fn topic_named(title: &str) -> Option<usize> {
    TOPICS.iter().position(|t| t.title == title)
}

/// A line cut into text and links, as it is drawn.
pub fn pieces(line: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let mut rest = line;
    let mut n = 0;
    while let Some(open) = rest.find("{{") {
        let Some(close) = rest[open..].find("}}") else {
            break;
        };
        if open > 0 {
            out.push(Piece::Text(&rest[..open]));
        }
        out.push(Piece::Link(&rest[open + 2..open + close], n));
        n += 1;
        rest = &rest[open + close + 2..];
    }
    if !rest.is_empty() {
        out.push(Piece::Text(rest));
    }
    out
}

/// A line as drawn: the braces gone.
pub fn plain(line: &str) -> String {
    pieces(line)
        .into_iter()
        .map(|piece| match piece {
            Piece::Text(text) | Piece::Link(text, _) => text,
        })
        .collect()
}

/// Every link of a page, top to bottom.
pub fn links(topic: usize) -> Vec<Link> {
    let mut out = Vec::new();
    for (line, text) in self::topic(topic).lines.iter().enumerate() {
        let mut col = 0;
        for piece in pieces(text) {
            match piece {
                Piece::Text(text) => col += text.chars().count(),
                Piece::Link(title, _) => {
                    let width = title.chars().count();
                    if let Some(target) = topic_named(title) {
                        out.push(Link {
                            line,
                            col,
                            width,
                            target,
                        });
                    }
                    col += width;
                }
            }
        }
    }
    out
}

/// Where F1 opens for `start`: a page whose heading starts with it, or
/// the page and line that start with it - a key a dialog is about.
/// The contents when there is none.
pub fn locate(start: &str) -> (usize, usize) {
    if let Some(title) = start.strip_prefix("# ")
        && let Some(at) = TOPICS.iter().position(|t| t.title.starts_with(title))
    {
        return (at, 0);
    }
    for (at, topic) in TOPICS.iter().enumerate() {
        if let Some(line) = topic.lines.iter().position(|l| l.starts_with(start)) {
            return (at, line);
        }
    }
    (0, 0)
}

/// The help screen: which page, how far down, the link picked, and the
/// way back.
pub struct HelpState {
    pub topic: usize,
    pub top: usize,
    /// Content rows; updated on every draw, drives paging.
    pub rows: usize,
    /// The picked link, an index into [`links`] of the page.
    pub link: Option<usize>,
    /// Where Back goes: page, top and picked link, newest last.
    back: Vec<(usize, usize, Option<usize>)>,
    /// `/` typing a search: the field.
    pub typing: Option<TextField>,
    /// What was searched for last, for `n` and for highlighting.
    pub query: String,
    pub note: Option<String>,
    /// The line of this page the last search landed on, where `n`
    /// goes on from: the page may not scroll far enough to put it on top.
    found: Option<usize>,
    /// Where each link on screen was drawn, for a click: the screen
    /// row, its first and last-plus-one column, the link.
    pub drawn: Vec<(u16, u16, u16, usize)>,
}

impl HelpState {
    /// Help opened on page `topic` at line `line`.
    pub fn at(topic: usize, line: usize) -> HelpState {
        let mut help = HelpState {
            topic,
            top: line,
            rows: 1,
            link: None,
            back: Vec::new(),
            typing: None,
            query: String::new(),
            note: None,
            found: None,
            drawn: Vec::new(),
        };
        help.pick_visible();
        help
    }

    pub fn max_top(&self) -> usize {
        topic(self.topic)
            .lines
            .len()
            .saturating_sub(self.rows.max(1))
    }

    /// Scroll by `delta` lines; a picked link scrolled off is let go
    /// of for the first one on screen.
    pub fn scroll(&mut self, delta: isize) {
        self.top = self.top.saturating_add_signed(delta).min(self.max_top());
        self.pick_visible();
    }

    pub fn scroll_to(&mut self, top: usize) {
        self.top = top.min(self.max_top());
        self.pick_visible();
    }

    fn on_screen(&self, line: usize) -> bool {
        line >= self.top && line < self.top + self.rows.max(1)
    }

    /// Keep the picked link on screen: the first visible one when it
    /// is not, none when no link is.
    pub fn pick_visible(&mut self) {
        let links = links(self.topic);
        if self
            .link
            .and_then(|at| links.get(at))
            .is_some_and(|l| self.on_screen(l.line))
        {
            return;
        }
        self.link = links.iter().position(|l| self.on_screen(l.line));
    }

    /// Tab / S-Tab: the next or previous link of the page, round the
    /// end, brought on screen.
    pub fn step_link(&mut self, forward: bool) {
        let links = links(self.topic);
        if links.is_empty() {
            return;
        }
        let n = links.len();
        let next = match (self.link, forward) {
            (Some(at), true) => (at + 1) % n,
            (Some(at), false) => (at + n - 1) % n,
            (None, true) => links.iter().position(|l| l.line >= self.top).unwrap_or(0),
            (None, false) => links
                .iter()
                .rposition(|l| l.line < self.top + self.rows.max(1))
                .unwrap_or(n - 1),
        };
        self.link = Some(next);
        let line = links[next].line;
        if line < self.top {
            self.top = line;
        } else if line >= self.top + self.rows.max(1) {
            self.top = (line + 1).saturating_sub(self.rows.max(1));
        }
        self.top = self.top.min(self.max_top());
    }

    /// Enter: follow the picked link. False = there is none.
    pub fn follow(&mut self) -> bool {
        match self.link.and_then(|at| links(self.topic).get(at).copied()) {
            Some(link) => {
                self.go(link.target, 0);
                true
            }
            None => false,
        }
    }

    /// Open page `topic` at `line`, remembering this one for Back.
    pub fn go(&mut self, topic: usize, line: usize) {
        if topic == self.topic && line == self.top {
            return;
        }
        self.back.push((self.topic, self.top, self.link));
        self.topic = topic;
        self.found = None;
        self.link = None;
        self.top = line.min(self.max_top());
        self.pick_visible();
    }

    /// Back to the page before; the contents when there was none.
    pub fn go_back(&mut self) {
        match self.back.pop() {
            Some((topic, top, link)) => {
                self.topic = topic;
                self.found = None;
                self.top = top.min(self.max_top());
                self.link = link;
                self.pick_visible();
            }
            None if self.topic != 0 => {
                self.topic = 0;
                self.found = None;
                self.top = 0;
                self.link = None;
                self.pick_visible();
            }
            None => {}
        }
    }

    /// `/`: the first line holding the query from the top of the
    /// screen on; `n`: the next one after the last found.
    pub fn search(&mut self, next: bool) {
        let from = match (next, self.found) {
            (true, Some(line)) => line + 1,
            (true, None) => self.top + 1,
            (false, _) => self.top,
        };
        self.search_from(from);
    }

    /// The next line holding the query, from line `from` of this page
    /// on, through the pages after it and round to this one again.
    fn search_from(&mut self, from: usize) {
        let query = self.query.to_lowercase();
        if query.is_empty() {
            return;
        }
        let pages = TOPICS.len();
        for step in 0..=pages {
            let at = (self.topic + step) % pages;
            let lines = &topic(at).lines;
            let (start, end) = match step {
                0 => (from, lines.len()),
                s if s == pages => (0, from.min(lines.len())),
                _ => (0, lines.len()),
            };
            let found = (start..end).find(|&i| plain(&lines[i]).to_lowercase().contains(&query));
            if let Some(line) = found {
                if at == self.topic {
                    self.scroll_to(line);
                } else {
                    self.go(at, line);
                }
                self.found = Some(line);
                return;
            }
        }
        self.note = Some(format!(" \"{}\" is not in the help ", self.query));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_link_names_a_page() {
        for (at, topic) in TOPICS.iter().enumerate() {
            for line in &topic.lines {
                for piece in pieces(line) {
                    if let Piece::Link(title, _) = piece {
                        assert!(
                            topic_named(title).is_some(),
                            "{}: {{{{{title}}}}} is no page",
                            TOPICS[at].title
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn contents_reaches_every_page() {
        let listed: Vec<usize> = links(0).iter().map(|l| l.target).collect();
        for (at, topic) in TOPICS.iter().enumerate().skip(1) {
            assert!(
                listed.contains(&at),
                "{} is not in the contents",
                topic.title
            );
        }
    }

    #[test]
    fn pages_fit_eighty_columns() {
        for topic in TOPICS.iter().filter(|t| t.title != "About") {
            for line in &topic.lines {
                assert!(plain(line).chars().count() <= 80, "{line}");
            }
        }
    }

    #[test]
    fn titles_come_from_headings() {
        assert_eq!(title_of("Viewer (F3)"), "Viewer");
        assert_eq!(title_of("Mouse  (mouse = false)"), "Mouse");
        assert_eq!(title_of("Using the help"), "Using the help");
    }

    #[test]
    fn links_are_found_where_drawn() {
        let at = topic_named("Using the help").unwrap();
        let link = links(at)[0];
        assert_eq!(link.target, 0);
        let line = plain(&topic(at).lines[link.line]);
        let drawn: String = line.chars().skip(link.col).take(link.width).collect();
        assert_eq!(drawn, "Contents");
    }

    #[test]
    fn locate_finds_pages_and_keys() {
        assert_eq!(locate("# Viewer").0, topic_named("Viewer").unwrap());
        let (at, line) = locate("  M-F7");
        assert_eq!(at, topic_named("Finding").unwrap());
        assert!(topic(at).lines[line].starts_with("  M-F7"));
        assert_eq!(locate("  no such key"), (0, 0));
    }

    #[test]
    fn follow_then_back() {
        let mut help = HelpState::at(0, 0);
        help.rows = 40;
        help.pick_visible();
        help.step_link(true);
        let target = links(0)[help.link.unwrap()].target;
        assert!(help.follow());
        assert_eq!(help.topic, target);
        help.go_back();
        assert_eq!(help.topic, 0);
    }

    #[test]
    fn back_with_no_history_is_the_contents() {
        let mut help = HelpState::at(topic_named("Editor").unwrap(), 5);
        help.go_back();
        assert_eq!((help.topic, help.top), (0, 0));
    }

    #[test]
    fn search_crosses_pages_and_comes_back() {
        let mut help = HelpState::at(0, 0);
        help.rows = 10;
        help.query = "hex edit".into();
        help.search(false);
        assert_eq!(help.topic, topic_named("Viewer").unwrap());
        help.go_back();
        assert_eq!(help.topic, 0);
        help.query = "no such words anywhere".into();
        help.search(false);
        assert!(help.note.is_some());
    }

    #[test]
    fn next_goes_past_a_match_the_page_cannot_scroll_to() {
        // the last lines of a page cannot come to the top: n has to go
        // on from the match, not from the top
        let at = topic_named("Config").unwrap();
        let mut help = HelpState::at(at, 0);
        help.rows = 40;
        help.query = "see also".into();
        help.search(false);
        let (page, line) = (help.topic, help.found.unwrap());
        help.search(true);
        assert_ne!((help.topic, help.found.unwrap()), (page, line));
    }
}
