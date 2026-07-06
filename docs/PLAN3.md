# rcmd 3.0 — the live commander

**Status:** R1 DONE (2026-07-06) — the persistent subshell is in,
default-on with `subshell = false` as the escape hatch; now dogfooding.
R2–R5 open. (Drafted 2026-07-06, alongside the 2.0 release.)
**Prerequisite:** PLAN2.md complete (it is). Baseline: 2.0 — MC-workflow
parity and beyond, SFTP panels, built-in editor, openers/user menu;
threads-not-async settled (D1); 82 unit tests + 82 pty e2e checks.

## Vision

2.0 made rcmd *complete*; 3.0 makes it *alive*. The shell stops being a
place you visit and becomes a resident: Ctrl+O toggles to a persistent
subshell exactly like MC, typed commands run in it without losing
state, and the panels follow what happens there. Around that flagship,
the workflow extras that separate "replacement" from "upgrade": rename
files like text, watch logs live, complete paths as you type.

## Standing constraints (unchanged)

- Keyboard-first; MC hands stay unbroken (audited in 2.0 and enforced
  by e2e key checks).
- `rcmd-core` stays TUI-free; worker threads + mpsc, no async — D1 is
  settled law.
- Every phase ends pty-verified, subshell on *and* off where relevant.
- External escape hatches never go away ($EDITOR, plain command exec).

## Phases

### R1 — the persistent subshell (the flagship, and the risk) — DONE

Shipped 2026-07-06 (`rcmd-tui/src/subshell.rs` + a session loop in
`app.rs`). What the build taught us:

- **E1 resolved: hand-rolled** over `libc::openpty` — no new crate; the
  e2e harness had already proven the pty knowledge, and `libc` was a
  dependency anyway.
- **E2 resolved: hooks + pipe, MC's trick minus the SIGSTOP dance.**
  bash (`--rcfile` wrapper), zsh (`ZDOTDIR` stub) and fish
  (`-C` function) get a prompt hook that writes `pwd` to inherited
  fd 27; each message is also the "prompt is idle" signal that gates
  command injection and the auto-return to panels. Plain `sh`/dash:
  `/proc/<pid>/cwd` + `TIOCGPGRP` (is the shell the foreground pgroup?)
  + 100 ms output quiescence.
- cd sync uses an **"agreed directory"**: whoever (panel or shell)
  moved away from the last agreement is the one synced from — no
  ping-pong, panel wins ties.
- The panels live in the alternate screen, the subshell owns the
  primary one, so the terminal itself is MC's "output screen"; hidden
  output is buffered (1 MB cap) and replayed on the next Ctrl+O.
- **Landmine found: hidden terminal queries.** fish blocks at startup
  (and vim would too) waiting for DA1/DSR answers no one gives while
  the pty is hidden. A tiny shim answers exactly those while hidden;
  when visible, the real terminal answers through the passthrough.
- **Landmine found: `dup2(fd, fd)` keeps CLOEXEC.** When the pipe's
  write end randomly landed on fd 27 itself, the no-op dup2 left
  CLOEXEC set and the hook fd died at exec — caught only because the
  full e2e suite shifts fd numbering. Handled explicitly.
- **Landmine found: shells that block before their first prompt.**
  Ubuntu's global compinit stops zsh at an interactive
  insecure-directories question on CI runners. A command typed during
  startup now waits up to 30 s in the subshell view (the user watches
  the shell boot and can answer such prompts, or Ctrl+O away); the zsh
  scaffolding runs `~/.zshenv` in the real env phase so things like
  `skip_global_compinit` keep working.
- Original goals, all kept:
- A long-lived `$SHELL` child on a secondary pty, spawned at startup;
  `subshell = false` (and any spawn failure) falls back to 2.0's plain
  exec forever.
- **Ctrl+O toggles** panels ↔ the subshell screen; the pty buffer *is*
  MC's "output screen", so the last command's output survives the trip.
- Typed commands run *in* the subshell: prompt-idle detection before
  injecting, panel cd → subshell `cd` sync, subshell cwd → panel follow
  (OSC 7 / shell precmd hook; fallback `/proc/<pid>/cwd` polling).
- Job control stays the shell's business (it owns its pty).
- Shells: POSIX sh, bash, zsh, fish — each gets its own cd-sync recipe
  and an e2e scenario.
- `exit` in the subshell respawns it (with a note), like MC.
- Exit: a month of Ctrl+O muscle memory without a bug report to
  yourself; the entire e2e suite passes with the subshell on and off.

Scope notes: Ctrl+O always returns to panels, even from a full-screen
app in the subshell (MC-compatible — it shadows nano's save there);
`Exec::Quiet` (openers, external editor, temp viewers) deliberately
stays on the one-shot path because remote-edit upload and temp cleanup
need synchronous completion. CI runs the e2e suite twice (subshell on
and off) plus per-shell scenarios: sh, bash, zsh, fish.
`RCMD_SUBSHELL_LOG=/path` traces the state machine while dogfooding.

### R2 — SFTP auth depth (carried from 2.0's P7)
- **Passphrase-protected keys**: when `~/.ssh/id_*` needs a passphrase,
  prompt for it (masked) instead of silently falling through to
  password auth; `ssh2` takes the passphrase directly.
- **Keyboard-interactive auth**: route its prompts through the existing
  ConnectEvent/ConnectReply dialogs; servers may send *several* prompts
  per round — loop, never assume one password.
- Exit: a passphrase key and a kbd-interactive-only sshd both connect;
  e2e covers at least the passphrase path (paramiko can serve both).

### R3 — workflow bells (cherry-pickable menu)
- **Bulk rename via the editor**: marked files open as a text buffer in
  `rcmd-edit`; the saved diff becomes renames/deletes through the job
  engine and its dialogs. Two-phase rename (temp names) handles swaps
  and collisions; a preview dialog confirms before touching anything.
- **Viewer follow mode**: tail -f toggle in F3, re-index on notify
  events, stick to the bottom.
- **Command-line Tab completion** for paths (files/dirs only, no
  command completion).
- **Find file, gitignore-aware**: skip ignored trees by default inside
  a work tree (toggle in the dialog).
- **Recent directories** in the hotlist dialog (panel history already
  records them; merged, deduped, below the pinned entries).
- **MC alias batch**: M-y/M-u history back/forward, M-? find file,
  M-c quick cd, C-l repaint, C-x t / C-x p (tagged names / path to the
  command line), S-F4 edit-new-file (`Editor::create` finally wired),
  S-F5/S-F6 copy/rename in place.

### R4 — depth debt (cherry-pickable menu)
- Editor: soft-wrap; `$1`..`$9` capture groups in replace; mcedit-style
  F5/F6 block copy/move as aliases for the clipboard ops.
- chmod / chown / create-symlink dialogs (C-x c/o/s — `FsWrite` already
  has the verbs; chown needs one new one).
- Copy *into* tar archives (full rewrite-append); quick-view hex mode;
  click-to-sort column headers; Ctrl+Space dir-size over sftp.
- **Job queue**: more than one running job — a jobs list dialog,
  background transfers, per-job progress in the status line.

### R5 — packaging & the wider world
- `[profile.release]`: thin LTO + strip (there is currently none).
- A static `x86_64-unknown-linux-musl` tarball next to the glibc one —
  runs on any distro, the best "installable everywhere" move that
  needs no crates.io.
- Demo GIF in the README (vhs, or the pty harness itself).
- Windows, Lua, macOS builds, crates.io (D4): all stay parked unless
  real demand shows up — unchanged from the 2.0 decisions.

## Sequencing & effort (rough)

R1 (2–4 wk, flagship + risk) → R2 (days) → R3 (1–2 wk) → R4 (1–2 wk) →
R5 (days). R2–R5 do not block on R1 — if the subshell drags, ship 3.x
minors from the menus. R3/R4 are menus, not promises: cherry-pick per
sitting, prune freely.

## Risks

- **R1 is where MC's bugs lived.** Prompt detection is heuristic, cd
  sync is per-shell, resize-during-toggle and SIGWINCH ordering are
  landmines. Mitigations: feature flag (default-on only after real
  dogfooding), the plain-exec path stays forever, and the whole e2e
  suite runs in both modes in CI.
- **Bulk rename is destructive by nature** — collisions, cycles,
  case-only renames on case-insensitive mounts. Two-phase renames plus
  a mandatory preview dialog.
- **Scope gravity** — R3/R4 could swallow months; the menus exist to be
  pruned, and nothing in them blocks a release.

## Decision points

- **E1 (R1): pty layer** — RESOLVED: hand-rolled over `libc::openpty`
  (see R1 notes; `portable-pty` never became worth a dependency).
- **E2 (R1): cwd tracking** — RESOLVED: prompt hooks writing to an
  inherited pipe fd for bash/zsh/fish, `/proc` + foreground-pgroup
  fallback for plain sh (see R1 notes).
- **E3 (R4): job queue UI shape** — MC never had one worth copying;
  design from scratch, small.

## What 3.0 refuses to do

FTP, cloud-storage APIs, tabs-as-in-browser, in-terminal image
rendering, LSP-anything in the editor, plugin systems. Windows and Lua
remain parked, not planned.
