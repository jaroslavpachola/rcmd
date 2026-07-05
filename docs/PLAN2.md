# rcmd 2.0 — beyond Midnight Commander

**Status:** in progress — P0–P6 shipped + P7's MC depth, v1.1.0
released; remaining: P7 SFTP auth → P8 (the 2.0 release)
**Prerequisite:** PLAN.md complete (it is). Baseline: MC-parity dual-pane
manager, ~43 tests, pty-verified, no async runtime, `FsProvider` seam.

## Vision

1.0 answered "can rcmd replace mc?". 2.0 answers "can rcmd be *better*
than mc ever was?" — same orthodox soul, but: instant on huge and remote
filesystems, one tool for find/compare/sync workflows, editable in-place,
scriptable, and installable everywhere with one command. The measure of
done shifts from feature parity to *workflow ownership*: the terminal
tasks you currently leave rcmd for (scp, find, diff -r, mcedit) stop
requiring a departure.

## Standing constraints (carried from 1.0)

- Keyboard-first; F-keys stay sacred. Mouse is additive, never required.
- `rcmd-core` stays TUI-free and unit-testable.
- Threads-not-async holds **until P3 proves it wrong** (see decision D1).
- Every phase ends pty-verified, not just unit-tested.

## Phases

### P0 — 1.0 release engineering (small, do first)
The project graduates from "a repo on one laptop".
- GitHub remote; CI: fmt + clippy -D warnings + test on Linux/macOS.
- The Python pty harness moves in-repo as `tests/e2e/` (or is rewritten
  on `expectrl`); CI runs it headless. This is the regression net for
  everything below.
- `cargo-dist` (or plain Actions) release binaries; LICENSE file;
  `rcmd --version`; CHANGELOG from the commit log. Tag **v1.0.0**.
- Exit: a stranger installs a release binary on a clean box and browses.

### P1 — power tools (the find/compare replacement)
- **Find file** (Alt+F7): name glob + optional content substring/regex,
  streamed results into a panelized listing (a `Panel` whose entries come
  from a result set, not `read_dir` — groundwork: entries already carry
  full paths for targets).
- **Panelize from command** (F9 → Command): run a command, its stdout
  lines become the panel listing (`git ls-files -m`, `rg -l TODO`...).
- **Directory compare** (Ctrl+X D): mark differing files in both panels
  (size/mtime quick mode, content-hash thorough mode as a job).
- **Compare-driven sync**: with compare marks in place, F5 already does
  the rest — document the workflow.
- Exit: `find | xargs`, `diff -rq` and "which files changed?" round trips
  happen inside rcmd.

### P2 — responsiveness at scale (prereq for remote)
- Directory listing becomes interruptible: >~20k entries or >100 ms
  moves to a worker with a spinner in the panel title; typing never
  blocks. (This machinery is exactly what slow remote listings need.)
- Ctrl+Space: directory size as a cancellable job, shown in the Size
  column, MC-style.
- Filesystem watching via `notify`: panels auto-reload on external
  changes (debounced; off over remote/VFS).
- Benchmark fixtures: 100k-entry directory in the e2e suite.
- Exit: a 100k-entry directory and a cold NFS mount never freeze the UI.

### P3 — remote VFS over SSH (the flagship)
- Split the seam: `FsProvider` (read) + new `FsWrite` (mkdir, remove,
  rename, open_write, set_permissions). `LocalFs` implements both;
  archives stay read-only+append as today.
- **SftpFs** on `ssh2` (blocking — fits the worker-thread model; agent,
  key, and password auth; known_hosts respected).
- `cd sftp://user@host/path` on the command line + a connection dialog
  in F9 → Command; panel title shows the remote URL; hotlist can hold
  remote entries.
- Jobs generalize to provider→provider streaming (upload, download,
  remote-to-remote via local relay). Progress/overwrite/retry dialogs
  unchanged — that protocol was built for this.
- **Decision D1 lands here**: if blocking SFTP on worker threads (with
  P2's interruptible listings) feels right, threads win permanently; if
  connection multiplexing demands it, this is the one sanctioned moment
  to introduce async — confined to the VFS layer, never the UI loop.
- Exit: a week of real server work (edit remote configs via F4-to-temp,
  push release artifacts) without typing `scp` or `sftp`.

**DONE (2026-07-04).** Shipped as designed: `FsWrite` write-half with
`FsProvider::writer()` capability discovery, `SftpFs` on blocking `ssh2`
(agent → key files → password; known_hosts checked, unknown keys get a
fingerprint dialog and are saved), `cd sftp://…` + F9 → Command → SFTP
link, `spawn_transfer` for upload/download/remote↔remote with
rename-first moves, remote F3/F4 (scratch copy, auto-upload on save),
F7/F8, local-vs-remote compare, hotlist remote entries, connection
sharing between panels via a weak cache. E2e drives a real SFTP server
(paramiko) through the full flow.

**Decision D1 — resolved: threads win, permanently.** Blocking libssh2
calls on worker threads compose cleanly with P2's pending loads, and the
connect worker prefetches the first listing, so the UI never blocks. One
mutex-serialized session per host is imperceptible next to network RTT,
and jobs/panels share it safely. No async runtime enters the codebase;
the question is closed for 2.0.

### P4 — the internal editor (the deferred beast, now on purpose)
1.0 proved $EDITOR is a fine crutch; 2.0 builds the mcedit successor —
last, with the most caution, behind its own crate:
- `rcmd-edit`: `ropey` buffer, multi-cursor-free (keep it simple),
  unlimited undo/redo, incremental regex search/replace, block
  selection, soft-wrap reusing the viewer's logic.
- Syntax highlighting via `syntect` (feature-gated; plain fallback).
- F4 opens it by default; `editor = "external"` restores $EDITOR.
- Hard scope line: no LSP, no splits, no plugins-in-editor. It is for
  configs and quick fixes, not for replacing your IDE.
- Exit: month of config edits without leaving rcmd; sub-100 ms open on a
  50 MB log (ropey makes this feasible).

**DONE (2026-07-04).** `rcmd-edit` crate: ropey buffer, single mutation
primitive (`splice`) recording coalesced undo groups with revision-id
modified tracking, mcedit-style sticky mark (F3) + Shift+arrow
selection, internal clipboard (^C/^X/^V), smartcase regex search (F7)
and interactive replace (F4: Replace/Skip/All/Quit), auto-indent Enter,
atomic save preserving permissions and CRLF, binary files refused.
Syntect highlighting behind the `syntax` feature (default on): parse
states checkpointed every 32 lines, invalidated from the edited line,
skipped for files >2 MB or lines >2000 chars. F4 opens it everywhere —
including sftp panels via the scratch-copy/upload-on-close path;
`editor = "external"` restores $VISUAL/$EDITOR.
Measured (release, through a pty): 50 MB log opens in ~215 ms
(the 100 ms goal was optimistic for full rope construction — accepted),
Ctrl+End and typing at EOF ~60 ms, dominated by poll granularity.
Scope cuts vs the sketch: soft-wrap deferred (horizontal scroll, like
mcedit's default) and "block selection" delivered as mcedit-style
stream marking, not rectangular columns; replacement strings are
literal (no $1 groups). No LSP, no splits, as decreed.

### P5 — UX depth
- **Mouse**: click to focus/move cursor, double-click Enter, wheel
  scroll, clickable menu/keybar. Additive only.
- **Panel history**: Alt+←/→ walk each panel's directory history;
  Alt+↑ jump to hotlist.
- **Quick view** (Ctrl+X Q): other panel becomes a live preview of the
  cursor file (viewer engine embedded in a panel).
- **Git awareness** (feature-gated on `git2`): a one-cell status column
  (M/A/?/ignored-dim) inside repos; branch name in the panel title.
- Exit: each feature individually verified in the pty harness.

**DONE (2026-07-04).** All four features shipped, each with its own pty
scenario (suite now 65 checks). Mouse: SGR events from crossterm, layout
rects recorded per draw for hit-testing (`Areas`), keybar clicks
synthesize F-keys so every mode gets them for free, menu geometry shared
between drawing and clicking (`ui::menu_layout`), editor clicks invert
the tab-aware `screen_col`, capture released around Ctrl+O; `mouse`
config key (default on). History: locations stored as display paths in
`Panel` (archive-internal stops excluded), committed only when the
matching listing lands so failed/abandoned navigations self-correct;
sftp:// entries reconnect via the connection cache. Quick view: reuses
`FileView`, refreshed per loop tick, reduced key set while the preview
pane is focused (scroll/Tab/quit only — nothing acts on the hidden
listing). Git: `git2` without default features (no openssl), scans on
throwaway threads keyed by (side, cwd) with results dropped when stale;
rescans on job-done/shell-return/editor-close/watcher-reload; deep
changes collapse onto the subdirectory entry; branch resolution handles
detached and unborn HEADs. Scope cuts: no drag-and-drop or mouse marking
(Insert/glob do it), no click-to-sort column headers, preview is
text-only (no hex), git column is per-directory status, not per-panel
refresh on every keystroke.

### P6 — extensibility (revised: no Lua)
- **Openers first** (no scripting needed): `[open]` config section maps
  globs to commands (`"*.pdf" = "zathura %f"`); Enter on a file consults
  it (MC's mc.ext, but sane TOML).
- **User commands**: `[commands]` — named shell templates with `%f`
  `%d` `%t` (tagged files) macros, bindable to keys and listed in a
  F2-style user menu.
- ~~Lua~~ — **cut from 2.0** (decision D3, resolved 2026-07-05): openers
  + user commands are the extensibility story. `mlua` returns post-2.0
  only if `[commands]` demonstrably can't express a real workflow.
- Openers respect lynx motion: in the `modern` keymap Right stays
  dirs-only (MC's lynx semantics); only Enter consults `[open]`.
- Exit: PDF/image/office files open right from Enter; one personal
  workflow automated without recompiling.

**DONE (2026-07-05).** Shipped as `[[open]]` / `[[commands]]` arrays of
tables rather than the sketched inline tables — TOML arrays preserve
file order, so "first matching rule wins" is real instead of
alphabetical. Openers: matched case-insensitively against the cursor
file on Enter (and double-click), run through a new `Exec::Quiet` path
(the old editor exec, renamed) — no "press Enter" pause, GUI apps take
a trailing `&`; local panels only; the `enter` keymap action (modern
Right) never consults them. User commands: F2 menu (digit hotkeys 1-9,
Enter runs) + optional `key = "..."` per command bound straight into
the keymap at startup; they run as ordinary commands (with pause).
Macros `%f %d %D %t %%`, shell-quoted, expanded against the active
panel. e2e: opener-on-Enter, menu, `%d`, and `%t`-with-binding checks
(suite 76); test_find needed a menu-navigation fix (the new "User
menu..." row shifted Command-menu positions — position-coupled e2e
navigation is fragile, noted). Lua stays out, as decided.

### P7 — depth & polish (revised 2026-07-05: + MC depth)
**SFTP auth** — the deliberate P3 scope cuts held up in practice except
where they lock users out entirely:
- **Passphrase-protected keys**: when `~/.ssh/id_*` needs a passphrase,
  prompt for it (masked, like the password dialog) instead of silently
  skipping to password auth. `ssh2` takes the passphrase directly.
- **Keyboard-interactive auth**: servers that disable `password` in
  favor of `keyboard-interactive` (default on some distros) currently
  fail; route its prompts through the existing ConnectAsk dialog.
- Both reuse the ConnectEvent/ConnectReply protocol — no new UI.

**MC depth** — the properties/format features MC hands miss most:
- **Info panel** (Ctrl+X i): the other panel shows the full stat of the
  cursor file — perms, owner, group, size, all three times, inode,
  links, symlink target — reusing the quick-view pane pattern. Needs an
  `Entry` stat extension (uid/gid/atime/ctime) across the providers
  (local + sftp; archives best-effort).
- **Free space** in the panel footer (statvfs; local always, sftp where
  the server supports the statvfs extension).
- **Listing modes** per panel: brief (name-only multi-column), full
  (current), long (perms owner group size date name); cycled from the
  F9 menu, persisted in config.
- Nitpicks while in there: Alt+i (other panel → same directory),
  Alt+o (other panel → directory under cursor).
- Exit: a passphrase key and a kbd-interactive-only sshd both connect
  (e2e covers at least the passphrase path — paramiko can serve both);
  info panel, free space, and all three listing modes pty-verified.

**MC depth DONE (2026-07-05, ahead of schedule at user request).**
`Entry` grew an `EntryStat` (uid/gid/atime/ctime/nlink/inode; local
fills all, sftp what the protocol carries, archives none — the UI says
"n/a"). Ctrl+X i info pane reuses the quick-view pane slot (mutually
exclusive), owner/group resolved through cached getpwuid_r/getgrgid_r
(numeric on remote panels). Free space via statvfs, cached per side
with a 3 s TTL, shown in local panel footers (when no filter label) and
the info pane; sftp skipped (ssh2 only exposes fstatvfs on handles).
Listing modes live on `Panel` (`list_mode`), rendered as different
Table column sets; new F9 "View" menu (appended after Sort so existing
menu geometry — and the mouse e2e — kept their coordinates). Alt+i and
Alt+o shipped as planned, local panels only. e2e suite now 72 checks.
Remaining in P7: the SFTP auth items above.

### P8 — 2.0 release engineering
- Version 2.0.0, CHANGELOG, tag; release tarball as in 1.1.
- **Install story**: `cargo install --git <repo> rcmd-tui` documented as
  the one-command install (works today, no renames needed). Full
  crates.io publish is decision D4 — the names `rcmd` *and* `rcmd-core`
  are already squatted by unrelated crates, so publishing means renaming
  the library crates (`rcmd-tui` itself is free); do it only if there is
  demand beyond the git install.
- Docs pass: README feature tour is current; PLAN2 gets its completion
  retrospective like PLAN.md did.
- Exit: v2.0.0 tagged; a stranger installs with one command and owns the
  find/compare/sync/remote/edit workflows the Vision promised.

## Sequencing & effort (rough)

P0 (days) → P1 (1–2 wk) → P2 (1–2 wk) → P3 (2–4 wk, flagship) →
P4 (3–5 wk, riskiest) ∥ P5 (1–2 wk, parallelizable)   [all shipped] →
P6 (~1 wk) → P7 (~1 wk) → P8 (days).

## Post-2.0 candidates (cut, not condemned)

- **Windows port** (was P7): unix-gated code is confined to the perms
  column, symlinks, and the libc job-control block; `trash` and
  `crossterm` are already cross-platform. Needs a ConPTY answer for the
  pty harness. Do it when a real Windows user shows up.
- **Lua scripting** (was in P6): only if `[commands]` proves too weak.
- Editor soft-wrap; `$1` capture groups in replace (literal today).
- Copy *into* tar archives (zip-append exists; tar needs a rewrite).
- Quick-view hex mode; click-to-sort column headers; mouse marking.
- chmod / chown / create-symlink dialogs (C-x c/o/s; `FsWrite` already
  has set_mode and symlink — mostly dialog work).
- Directory tree view (MC's tree panel).
- Jobs queue UI (still just one job + viewer).
- FsProvider dir-size over sftp (Ctrl+Space stays local-only).

## New crates

| Concern | Crate | Phase |
|---------|-------|-------|
| e2e terminal driving | `expectrl` (or keep Python) | P0 |
| fs watching | `notify` | P2 |
| SSH/SFTP | `ssh2` | P3 |
| editor buffer | `ropey` | P4 |
| syntax highlighting | `syntect` (feature) | P4 |
| git status | `git2` (feature) | P5 |
| ~~scripting~~ | ~~`mlua`~~ | cut with Lua |

## Risks

- ~~P3 auth/UX rabbit hole~~ — held: agent + key + password shipped;
  P7 adds exactly two more methods and then the line holds again
  (jump hosts and 2FA stay post-2.0).
- ~~P4 editor~~ — shipped within its ceiling; the ceiling stays law.
- ~~Lua API regret~~ — resolved by not building it.
- ~~Watcher storms~~ — debounce shipped in P2, no incidents.
- **kbd-interactive protocol quirks** (P7): servers can send multiple
  prompts per round; the dialog must loop, not assume one password.
- **crates.io naming** (P8/D4): `rcmd` and `rcmd-core` are squatted;
  publishing requires library renames — default is to not publish.

## Decision points

- **D1 (P3): threads vs async** — RESOLVED: threads, permanently.
- **D2 (P4): build vs embed editor** — RESOLVED: built (`rcmd-edit`).
- **D3 (P6): how much Lua** — RESOLVED 2026-07-05: none in 2.0;
  `[open]` + `[commands]` carry extensibility.
- **D4 (P8): crates.io publish** — OPEN: default no (name squatting
  forces renames); `cargo install --git` is the documented install.

## What 2.0 still refuses to do

FTP (dead protocol; SFTP only), cloud-storage APIs, tabs-as-in-browser,
image rendering in-terminal, a jobs *queue* UI beyond one job + viewer,
Windows and Lua (both post-2.0 candidates now), and any default
keybinding that breaks an MC hand.
