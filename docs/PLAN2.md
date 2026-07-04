# rcmd 2.0 — beyond Midnight Commander

**Status:** in progress — P0–P4 shipped (2026-07-04); next: P5
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

### P6 — extensibility
- **Openers first** (no scripting needed): `[open]` config section maps
  globs to commands (`"*.pdf" = "zathura %f"`); Enter on a file consults
  it (MC's mc.ext, but sane TOML).
- **User commands**: `[commands]` — named shell templates with `%f`
  `%d` `%t` (tagged files) macros, bindable to keys and listed in a
  F2-style user menu.
- **Lua last** (`mlua`, feature-gated): read-only API surface v0 —
  query panels/selection, run jobs, add menu entries. API frozen small;
  no promises of stability until it has three real users.
- Exit: PDF/image/office files open right from Enter; one personal
  workflow automated per maintainer without recompiling.

### P7 — Windows & the wider world
- Port the unix-gated code: perms column (best-effort), symlink calls,
  and the libc job-control block (Windows: plain process spawn, no
  process groups). `trash` and `crossterm` already cross-platform.
- CI matrix + release binaries for windows-latest; pty harness gets a
  ConPTY variant or is marked unix-only with a Windows smoke script.
- Exit: green CI on Windows and one real user browsing `C:\`.

## Sequencing & effort (rough)

P0 (days) → P1 (1–2 wk) → P2 (1–2 wk) → P3 (2–4 wk, flagship) →
P4 (3–5 wk, riskiest) ∥ P5 (1–2 wk, parallelizable) → P6 (1–2 wk) →
P7 (1–2 wk). P4 and P5 can swap or interleave; nothing after P3 blocks
on P4.

## New crates

| Concern | Crate | Phase |
|---------|-------|-------|
| e2e terminal driving | `expectrl` (or keep Python) | P0 |
| fs watching | `notify` | P2 |
| SSH/SFTP | `ssh2` | P3 |
| editor buffer | `ropey` | P4 |
| syntax highlighting | `syntect` (feature) | P4 |
| git status | `git2` (feature) | P5 |
| scripting | `mlua` (feature) | P6 |

## Risks

- **P3 auth/UX rabbit hole** — SSH edge cases (jump hosts, 2FA) are
  endless. Scope: agent + key + password against plain sshd; everything
  else is post-2.0.
- **P4 is where file managers go to die** — hence: own crate, hard
  feature ceiling, shipped last, external editor never removed.
- **Lua API regret** — a frozen v0 surface and feature gate keep it
  revocable.
- **Watcher storms** (P2) — debounce + automatic disable on >N events/s.

## Decision points

- **D1 (P3): threads vs async** — stated above; the only place the 1.0
  architecture is allowed to bend.
- **D2 (P4): build vs embed editor** — spike ropey+syntect for a week;
  if a maintained embeddable Rust editor core has emerged by then,
  evaluate it before building.
- **D3 (P6): how much Lua** — if `[open]` + `[commands]` cover 90% of
  requests, Lua may stay permanently experimental.

## What 2.0 still refuses to do

FTP (dead protocol; SFTP only), cloud-storage APIs, tabs-as-in-browser,
image rendering in-terminal, a jobs *queue* UI beyond one job + viewer
(revisit post-2.0), and any default keybinding that breaks an MC hand.
