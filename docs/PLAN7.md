# rcmd 7.0 - the orthodox leftovers

**Status:** drafted 2026-09-25, after PLAN6 closed at 4.56.0 and its
two stragglers shipped as 4.57.0. **Baseline:** 4.57.0.

PLAN5 and PLAN6 came from audits of what the code does. This one comes
from what the rest of the orthodox family does and rcmd still does not:
ORTHODOX-DIFF's `Adopt-later` rows, and PLAN5's "Reach" list where it
was left. Each phase is small enough to ship on its own, and none
depends on another.

The same markers as before: **[run]**, **[read]**, **[audit]**.

## Standing constraints (unchanged)

- Keyboard-first; MC hands stay unbroken.
- `rcmd-core` stays TUI-free; threads + mpsc, no async.
- Every phase ends pty-verified, subshell on *and* off.
- A phase that touches `fsops.rs` ships with a test that fails before
  the change.

## Phases

### U0 - an archive anywhere - DONE (2026-09-25, 4.61.0)

Shipped with the copy made as F3 makes one, in the foreground, rather
than through the job engine: a big archive on a slow server holds the
screen while it comes down, as viewing a big file there already does.
The panel keeps a stack of where each nested archive was entered from,
so archives nest any number deep.


Enter opens an archive only on a local panel (`Panel::enter`,
`panel.rs:583`) [read]. An archive on an SFTP panel or inside another
archive is just a file. Copy it to a temporary file through its
provider, with the job engine's progress for a big one, and open that.
The temporary file lives as long as the panel stays inside the
archive. `..` at the top comes back to where the archive was, not to
the temporary directory. The archive is read-only there, like every
archive but a local zip or tar.

### U1 - disk usage mode (ncdu's) - DONE (2026-09-25, 4.60.0)

Shipped as a panel mode reached from F9 > Command and the palette
(`disk-usage`), with no key of its own. The bar replaces the Full
listing's date column; the other listings sort by size without one.
The size cache is emptied by anything that may have changed the tree
(a finished job, the shell, a save), so a delete shows in the parents'
sizes on the next look.


A listing mode that shows every directory's recursive size, computed on
a thread (the `C-space` walker, all of them at once), with a bar
relative to the biggest entry, sorted by size. Enter drills in, `..`
climbs back, F8 deletes from inside it, and the sizes of the parents
drop by what went. The sizes are cached per directory for the session,
so climbing back does not walk again.

### U2 - flat view (TC's Ctrl+B) - DONE (2026-09-25, 4.59.0)

Every file under the current directory in one listing, relative paths
as names - what a find for `*` and panelize gives, one key away and
without the dialog. F5, F6 and F8 work on it as on any panelized
listing. A key toggles it back.

### U3 - duplicate finder

A find of its own: files grouped by size, then by a hash of the first
64 KiB, then by a whole-file hash, streamed into the find window as
groups are confirmed. Marks go on every file of a group but the first,
so F8 after it keeps one of each.

### U4 - panel scrollbars - DONE (2026-09-25, 4.58.0)

A one-column bar on the panel's right border when the listing is longer
than the panel, draggable and clickable, as mc and Far draw it. Off in
the config for anyone who wants the column back.

### U5 - a calculator on the command line - DONE (2026-09-25, 4.58.0)

Shipped with one difference: Enter shows the answer and leaves
`= answer` on the command line at once, rather than on a second Enter,
so the next operator can simply be typed after it.

`= 2*(3+4)` or `= 0x1f + 1` evaluated in place, the result shown on the
status line and put on the command line on a second Enter. Integers,
floats, hex, octal and binary in, `+ - * / % ** ( )` and the size
suffixes (`= 3G / 4K`). No crate: a small recursive-descent parser in
core.

### U6 - sort groups (Far's)

Masks that pin classes of file to the top of a listing, whatever the
sort key: `*.rs *.toml` first, `*.o *.tmp` last. They share the mask
language with `[[highlight]]`, and are configured the same way.

### U7 - a process panel

`proc://` as a listing: a process per row, its name, pid, user, CPU
and resident memory as columns, sortable. F8 sends SIGTERM (Shift+F8
SIGKILL) after asking, and F3 shows its command line and environment.
Linux only, read from `/proc`.

## Open, and not mine to decide

- **Yank registers or a collector panel** (ORTHODOX-DIFF §2): they
  solve the same problem, and only one of them should exist.
- **Panel tabs, column blocks, editor macros** (§9): unchanged.

## Sequencing

Any order. U0 and U1 change the most for a user; U4 and U5 are the
smallest.
