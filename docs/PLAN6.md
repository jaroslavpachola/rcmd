# rcmd 6.0 - connections that stay, and the leftovers

**Status:** drafted 2026-09-25 from PLAN5's carried-forward list and a
code audit of 4.46.0 (three read-only passes: remote connections,
archive writing and find, the editors and dialogs).
**Baseline:** 4.46.0 - PLAN5 complete (4.31.0 to 4.40.0), the help as
pages, selection in the window and the archive speed work after it.

The same markers as PLAN5:

- **[run]** reproduced by running code.
- **[read]** confirmed by reading the code on both sides of the claim.
- **[audit]** reported by the audit with file and line, not re-checked.

## Vision

5.0 made rcmd the manager you trust with your files. 6.0 is about
the places it still lets go: a connection that drops and stays dead
until you free it by hand, an `~/.ssh/config` that `ssh` understands
and rcmd only half reads, and a `sudo://` that works only if you
already ran `sudo -v`. Then the small things PLAN5 carried forward and
put off.

## Standing constraints (unchanged)

- Keyboard-first; MC hands stay unbroken.
- `rcmd-core` stays TUI-free; threads + mpsc, no async.
- Every phase ends pty-verified, subshell on *and* off.
- External escape hatches never go away.
- A phase that touches `fsops.rs` ships with a test that fails before
  the change.

## Corrections to PLAN5's list

- **FISH uploads are not buffered.** `SshWrite` writes each chunk to
  the channel as it comes (`fish.rs:276-302`). [read] The note was
  written before S4's rewrite landed and was carried forward
  unchecked.
- **The diff view already highlights within a line**, by word
  (`diff::inline`, `diff.rs:200`; drawn in `ui.rs:2372`). [audit] What
  is missing is narrower; see T0.

## Phases

### T0 - the bugs (a release of its own) - DONE (2026-09-25, 4.47.0)

Shipped as below. Two files that differ only in their line endings
still compare as identical: the diff is of the text, and the endings
are now kept per line, so they survive a merge either way.

- **A merge in the diff view rewrites every line ending.** `DiffSide`
  strips each line's ending, calls the whole side CRLF if any single
  line is (`app/diffview.rs:39-46`), and joins everything with that one
  ending on save (`:61-65`). A file of 100 LF lines and one CRLF line,
  merged and saved, comes back with 101 CRLF lines. [audit] Keep an
  ending per line and splice it with the line.
- **The inline highlight ignores -w and -i.** `diff::inline` compares
  the raw text, so a whitespace-only change is still lit up after `-w`
  has said the lines are equal. [audit]
- **Three list dialogs ignore clicks.** Connections, the remote menu
  and file history map their rows (`draw_history`, `ui.rs:763-818`),
  but `select_dialog_row` has no arm for them. [audit]
- **The command line puts its cursor by character count**, not by
  width (`draw_cmdline`, `ui.rs:2052-2076`): after CJK text or an
  emoji the cursor sits to the left of where typing goes. [audit]
- **A tar with a suffix nobody recognizes is written as bzip2.**
  `TarSink::create` and `tar_source` both end in a bare `else` that
  means bz2 (`fsops.rs:3141-3170`). Nothing reaches it today, but the
  first new suffix (T5's `.tar.zst`) would. Make every arm explicit.
  [audit]

### T1 - connections that come back

Today nothing reconnects. A dropped SFTP or FISH session stays in the
connection cache while a panel holds it, a retry in a job calls the
same dead session again, and typing the URL again reuses it. [audit]

- **Reconnect once, then retry.** `SftpFs` and FISH's `Ssh` keep the
  URL they were dialed with. An operation that fails with libssh2's
  socket errors (send, receive, disconnect, timeout) dials again
  without prompts (agent and unencrypted keys work, and so does a
  password kept from the first login; an unknown or changed host key
  still fails) and runs the operation once more. For FISH the retry is
  at channel open, before anything is sent, so it is always safe.
- **A failed keepalive** marks the session dead, so the next operation
  redials immediately instead of waiting out the 30 s I/O timeout.
- **FTP** keeps its log-in-again, but a dead link no longer
  permanently records MLSD as unsupported (`ftp.rs:460-464`). [audit]

### T2 - `~/.ssh/config` as ssh reads it

- **`Include`**, with globs, relative to `~/.ssh`, to OpenSSH's depth
  limit, first value still winning across files.
- **`ProxyJump` and `ProxyCommand`.** libssh2 only needs a socket
  (`set_tcp_stream<S: AsRawFd>`), so a socketpair whose other end is
  the stdin and stdout of `ssh -W host:port jump` (or of `sh -c
  <ProxyCommand>`) carries the session. The proxy child lives exactly
  as long as the session. `BatchMode=yes`: the jump host needs a key
  or the agent, because rcmd owns the terminal.

### T3 - `sudo://` that asks

`sudo -n` fails with "a password is required" unless `sudo -v` was run
first (`fish.rs:543-550`). [audit] On that error, ask for the password
through the existing prompt, run `sudo -S -p '' -v` once with it on
stdin, and keep `sudo -n` for every operation after that. A keepalive
of `sudo -n -v` stops the timestamp expiring while the panel sits
idle.

### T4 - the line editor by grapheme

Left, Right, Backspace and Delete move and delete by code point
(`field.rs:105-167`): Backspace takes an accent off its letter, a flag
comes apart into two letters, and an emoji family takes five presses to
cross. [audit] Snap those four to grapheme boundaries
(`unicode-segmentation` is already in the lock file through ratatui),
and measure `field_row` per grapheme. The cursor stays a character
index, so none of its 134 uses elsewhere change.

### T5 - packing with a choice

- **A compression level** on the pack form (M-F5), from 0 to 9, blank
  meaning the default: gz, xz, bz2 and zip each take one.
- **`.tar.zst` writing** through ruzstd's encoder, which has no new
  dependency but offers only "fastest" (about zstd's level 1). That is
  still better than refusing.

### T6 - git from the panel

`git.rs` reads status and branch, and diff against HEAD exists
(`open_diff_head`). Missing: **stage and unstage the marked files**,
and **switch branch** from a list (safe checkout; a conflict reports
itself in the status line). git2 is already linked, and no network
features are needed.

### T7 - changed lines in the editor gutter

Keep the text as it was when the file was opened or last saved (a
`ropey::Rope` clone costs nothing), diff it against the buffer on a
thread after edits settle, and mark added, changed and deleted-below
lines in the gutter. With line numbers off, the marks get a column of
their own.

### T8 - find inside archives

A-F7 walks local directories only (`find.rs:235-266`). Add an option
to descend into the archives it meets (native kinds only: the
external-tool ones cost a process each) and report a hit as
`archive/inner/path`. Results go to the find window, where Chdir opens
the archive at the hit. A panelized listing assumes one provider, so
archive hits stay out of it.

### T9 - the small ones

- **Clicks in the dialogs that have none yet**: Confirm's buttons
  first, then Chmod, Chown, Options and the fuzzy finder.
- **The trash's `directorysizes` cache** updated on restore and purge,
  so the desktop does not recount.

## Sequencing

T0 first, released on its own. T1 before T2, since both rework
`ssh_session` and the redial should go in first. T3 to T9 in any order;
each is its own release.

## Carried forward from PLAN5, still not planned

Reflinks beyond local-to-local; archives as a synchronize side; a
writable `[[vfs]]`; rclone setting a file's time; login steps for
podman and kubectl; 7z writing and archive passwords; archives on
remote panels and inside one another; zip names in a chosen codepage;
the subshell prompt for a shell that has wandered off; `marked_stats`
without a walk per draw (it is a loop over one directory - measurable
only past 100,000 entries); ORTHODOX-DIFF's other `Adopt-later` rows.

## Risks

- **A redial is a login nobody asked for.** It must never prompt, and
  it must never accept a host key the first connect did not: a
  redial that meets a new key fails the way a first connect would.
- **A retried operation that is not idempotent.** SFTP retries a call
  that failed at the socket, which cannot have reached the server
  half-way for stat or read. For rename, remove and mkdir, a retry that
  gets "no such file" or "exists" after a redial is reported as it is.
- **`ProxyCommand` runs a shell command from a config file.** That is
  the same trust `ssh` already gives it; rcmd adds nothing.
