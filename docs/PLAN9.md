# rcmd 9 - follow-ups to named sort groups

**Status:** drafted 2026-09-29, after 4.65.0 (named sort groups,
categories, the Sort groups dialog and the Kind column). Nothing built
yet. **Baseline:** 4.65.0.

What 4.65.0 left rough, found while building it and while chasing the
`findhits` e2e failure. Each phase is small and ships on its own; none
depends on another.

The same markers as before: **[run]**, **[read]**, **[audit]**.

## Standing constraints (unchanged)

- Keyboard-first; MC hands stay unbroken.
- `rcmd-core` stays TUI-free; threads + mpsc, no async.
- Every phase ends pty-verified, subshell on *and* off.
- A phase that touches `fsops.rs` ships with a test that fails before
  the change.

## Phases

### F0 - find results in a stable order

The find walk hands results over in the order the directories list
them: `jwalk::WalkDir` without `sort` (`find.rs:270`) [read], which on
ext4 is the order of a hash of the names. The results window shows
them as they come, so the same search in the same tree lists the same
files in an order nobody can predict, and walking the hits with
M-. / M-, goes through them in that order. This is what made the
`findhits` e2e test fail on one machine and pass on another [run];
4.65.0 fixed the test, not the order.

Sort each directory's children by name in the walk (`sort(true)`), so
results come depth-first in name order, as `find | sort` would list a
tree. The cost is a sort per directory, next to the stat and the read
the walk already does. Hits inside one file are in line order already
(`find.rs:548`) [read]. The window keeps showing results as they
arrive; they arrive in order now.

Tests: the core find test collects without its own sort and still gets
the names in order; the e2e `findhits` walk no longer needs to ask
which row is which.

### F1 - a group switched off but kept

In the Sort groups dialog a row is ticked or it is not, and OK keeps
only the ticked ones (`apply_sort_groups`) [read]. A category unticked
is offered again next time, so nothing is lost; a group of the user's
own - `*.rs,*.toml` named Code - is gone once unticked, and has to be
typed into `config.toml` again.

`[[sort_group]]` gains `off = true` (serde default false, left out of
the state file when false). A group that is off is kept in the list and
skipped when the groups are compiled. The dialog saves every row but
the categories nobody has used, with its tick as `off`. An old state
file reads as every group on, as it was written.

Tests: compiling skips a group that is off; the e2e suite unticks a
named group, reopens the dialog and finds it there, unticked, and the
listing no longer grouped by it.

### F2 - a group edited in the dialog

The dialog makes groups from categories and from the cursor's
extension, but a group's name and masks can only be changed in
`config.toml` - and once the dialog has saved, the state file owns the
list, so an edit to `config.toml` no longer shows (`state::apply`)
[read]. That leaves no way to rename a group or add a mask to it
without editing the state file by hand.

In the dialog: `F4` edits the row under the cursor - its name, then its
masks, as two input steps with the current text in them - and `Ins`
adds a new group the same way, placed after the ticked rows. `Del`
drops a row that is not a category. The masks are checked as they are
at startup (an unknown `@category` is said on the status line), and a
`type =` group edits its name only.

Tests: the e2e suite renames a group, adds a mask to it and a new
group, and finds all three in the Kind column and the state file.

## Not in this plan

- **The Kind column on an 80-column terminal.** A half panel there
  leaves the name too little room beside it; the line under the panel
  says the cursor's group instead, and that is the answer, not a gap.
- **Groups by what a file is, not what it is called** (`file -b`, as
  `[[open]]`'s `type =` asks). Sorting would have to run `file` on
  every entry of every listing.

## Sequencing

Any order. F0 is the one a user notices without looking for it; F1
before F2, since editing a group that unticking loses is half a
feature.
