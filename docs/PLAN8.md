# rcmd 8.0 - disks, and what to clear off them

**Status:** drafted 2026-09-27; D0 shipped as 4.68.0 (2026-09-29),
D1 to D4 not built yet.
**Baseline:** 4.63.1.

A disk manager: which volumes there are, mounted or not, how full they
are and how fast they fill, what could be cleared off them, and an AI
to ask about any of it. It starts small and read-only, and inside rcmd.

The same markers as before: **[run]**, **[read]**, **[audit]**.

## Where it lives - provisional

**This is built inside rcmd for now, and that placement is temporary.**
It lives here because most of what it needs is already here [read]:
`mounts.rs` (the mount list and `FsFacts`), the disk usage mode's
walker and size cache (U1), the duplicate finder (U3), trash and
delete, `keyring.rs`, and a listing provider to copy (`proc://`, U7).
A first version is mostly glue.

How big it grows is not known yet. It moves out, into a crate and
binary of its own in this workspace (`rcmd-disk`, depending on
`rcmd-core`, with rcmd keeping only an entry point), as soon as any of
these is wanted:

- **Monitoring while rcmd is closed** - history on a timer, or alerts
  ("/ is full in 3 days"). That is a daemon or a systemd user timer,
  not a file manager.
- **Managing, not just looking** - mounting, partitioning, formatting,
  resizing, fstab. That needs polkit or root helpers, which rcmd does
  not need today.
- **Several machines** - the disks of servers or a NAS side by side.
- **Other users** - published for people who want a GUI to answer
  "what is eating my disk", not an orthodox file manager.
- **Its own dependencies weighing on rcmd** - D-Bus, an HTTP client,
  SMART tooling - or its own release rhythm.

To keep that move cheap, everything below that is not drawing goes in
one module, `rcmd-core::disks` (submodules as it grows), which depends
on nothing in the TUI. The TUI and egui only render its listings.

## Standing constraints (unchanged)

- Keyboard-first; MC hands stay unbroken.
- `rcmd-core` stays TUI-free; threads + mpsc, no async. The AI phase
  uses a blocking HTTP client on a thread, like every other job.
- Every phase ends pty-verified, subshell on *and* off.
- Nothing is deleted without the usual confirmation, and only through
  the existing trash and delete paths.
- Read-only towards the disks themselves: no phase mounts, unmounts or
  writes to a volume.

## Phases

### D0 - a volume listing - DONE (2026-09-29, 4.68.0)

Shipped reading `/proc/self/mountinfo` rather than `/proc/self/mounts`:
its root field tells a bind mount (a directory of a volume listed
already) from a mount of a whole filesystem, and binds go behind the
hidden toggle with the pseudo-filesystems. Each `statvfs` runs on a
thread of its own and the listing waits 800 ms for them all, so a
network mount that does not answer is listed without its sizes. A
volume is named by its mount point with `/` written `∕` (U+2215), one
path component that still reads as the path. Nothing on the list is
copied or deleted. The palette now puts an action typed out in full
first: `disks` was losing to `disk-usage`.

`disks://` as a listing, like `proc://`: a row per mounted filesystem -
mount point, device, filesystem type, size, used, free, a usage bar,
inodes used. Pseudo-filesystems (proc, sysfs, cgroup, tmpfs, overlay,
squashfs of snaps) are hidden until the panel's hidden-files toggle
shows them. Enter goes to the mount point on the other panel, F3 shows
the `FsFacts` of it. Reached from F9 > Command and the palette
(`disks`).

`mounts()` goes through `df -P` so that macOS has an answer [read];
on Linux D0 reads `/proc/self/mounts` and `statvfs` directly, which
also gives the type and the inodes in one pass, and keeps `df` as the
fallback.

### D1 - volumes that are not mounted

Block devices from `/sys/class/block` (size, removable, rotational,
model), with filesystem type, label and UUID from `lsblk -J -o ...` -
one process, as `mounts()` does it. A partition that is not mounted is
listed with its size and type, and "not mounted" where used and free
would be: free space on a volume nobody mounted is not known without
mounting it or reading its superblock, and D1 does neither (see Open).
Whole disks group their partitions under them.

### D2 - cleanup candidates

Rules, not guesses: each one names a place, how to size it, how risky
clearing it is, and what clearing it does. A first set:

- **Low risk:** `~/.cache`, the trash, package caches (apt, pip, npm,
  the cargo registry), journald over a size (`journalctl --vacuum-size`,
  shown as a command, not run), dangling docker or podman images.
- **Build leftovers:** `target/` and `node_modules/` in projects not
  touched for N days (configurable).
- **Look first:** duplicates (U3), the largest files not read for a
  year, core dumps, ISOs and VM images.

They show as a panelized listing sorted by size, each row with its
risk and the total at the top. F8 deletes as anywhere else; a
"command" candidate runs through the command line so it is seen before
it runs. The rules live in the config, so a user adds their own.

### D3 - a history of each volume

A snapshot of every volume (keyed by UUID, not by mount point) is
appended to `~/.local/state/rcmd/disks.jsonl` when rcmd starts and when
`disks://` is opened, at most one an hour. The listing gains a trend
column (a sparkline of the last 30 days) and, where it is growing, a
projected "full in N days". Only while rcmd runs - a timer is the
first thing that moves this out (see above).

### D4 - ask an AI

An "Ask" dialog from `disks://` and from a D2 listing. The model gets
read-only tools - the volume list, the history, the largest directories
under a path, the cleanup candidates - and answers in text: what a big
directory is, whether it is safe to clear, what to do about a volume
that keeps filling. It can propose a D2-style action; the user takes it
with the usual keys. It never runs anything itself.

What leaves the machine: sizes, filesystem types and the category of a
place, not file names, unless a switch in the dialog says to send paths
for this question. The API key is kept by `keyring.rs`; with no key the
dialog says how to set one and everything else works as before.
Claude through its API first; whether to make the backend pluggable (a
local model) is Open.

## Not in this plan

SMART and health, mounting a volume to size it, anything that changes
a volume (partitions, formatting, fstab), alerts, other machines,
Windows. Each of them is on the list of reasons to move out.

## Open, and not mine to decide

- **Free space on unmounted volumes**: a temporary read-only mount
  through udisks2 (no root, every filesystem, but it mounts), or
  reading superblocks (touches nothing, one parser per filesystem), or
  leaving it unknown.
- **AI backend**: Claude only, or pluggable with a local model.
- **When to move out**: the list above says what forces it; whether to
  move earlier, before D3 or D4, is a choice.

## Sequencing

D0 first; D1 and D2 in any order after it; D3 before D4, which uses
its history.
