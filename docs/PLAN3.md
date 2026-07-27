# rcmd 3.0 — the live commander

**Status:** R1 DONE (2026-07-06) — the persistent subshell is in,
default-on with `subshell = false` as the escape hatch; now dogfooding.
R2 DONE (2026-07-27). R3 DONE (2026-07-27) — the whole menu shipped,
nothing pruned. R4 DONE (2026-07-27) — again the whole menu, including
the job queue (E3 resolved). R5 open. (Drafted 2026-07-06, alongside the 2.0 release.)
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

### R2 — SFTP auth depth (carried from 2.0's P7) — DONE

Shipped 2026-07-27. As planned, plus one structural upgrade: the
worker now asks the server for its allowed methods first (the "none"
probe behind `auth_methods()`) and tries only what can work, in
OpenSSH order — publickey, keyboard-interactive, password. So a
kbd-interactive-only server never shows a passphrase prompt, and a
pubkey-only server never asks for a password.

- **Passphrase-protected keys**: encryption detected by peeking at the
  key file (PEM `Proc-Type:`/PKCS#8 headers; the OpenSSH v1 format's
  ciphername after the magic), then a masked prompt with 3 attempts;
  empty input skips the key. Prompt shows `~/.ssh/<name>` so it fits
  the 56-column dialog.
- **Keyboard-interactive**: `ssh2`'s `KeyboardInteractivePrompt`
  routed through the same AskPassword dialog, one dialog per prompt,
  several prompts per round handled; per-prompt `echo` respected
  (unmasked input when the server asks for it).
- e2e covers both paths: paramiko serving pubkey-only auth against an
  encrypted PEM ECDSA key (wrong passphrase retried), and a
  kbd-interactive-only server sending two prompts in one round.

### R3 — workflow bells — DONE (2026-07-27, whole menu shipped)

Each item its own green commit (fmt, clippy -D, unit + full pty e2e in
both subshell modes). Notes per item:

- **Bulk rename via the editor** (F9 > File > Bulk rename): shipped
  vidir-style — marked names become a `<index>\t<name>` buffer in
  `rcmd-edit` (always the built-in editor: $EDITOR can't signal
  "session over"); the index column makes deleted lines and renames
  unambiguous. Changed lines = renames (two-phase temp names; swaps
  and chains covered by unit tests; occupied targets refused and the
  item restored), deleted lines = trash deletes through the job
  engine, everything behind a mandatory preview dialog. A buffer that
  doesn't parse applies nothing. Parsing/apply live in
  `rcmd-core::rename`.
- **Viewer follow mode**: `f` in F3 (loop-tick fstat rather than
  notify — works for any path, no watch rewiring); sticks to the
  bottom, truncation/rotation rebuilds the index. Landmine found:
  growing a fully-indexed file whose last byte was a line break needs
  the frontier line-start registered explicitly, or the first appended
  line is invisible.
- **Command-line Tab completion**: Tab completes once the line has
  text (empty line still switches panels; M-Tab always completes);
  common-prefix advance + candidate list in the status line;
  `rcmd-core::complete`, escape-aware.
- **Find file, gitignore-aware**: on by default inside a work tree
  (git2 `is_path_ignored` + `.git` itself), checkbox in the dialog;
  the find worker just takes an opaque skip predicate so rcmd-core
  stays git-free.
- **Recent directories** in the hotlist: both panels' histories
  merged newest-first under a "Recent:" header, deduped, pinned and
  current location excluded, capped at 15.
- **MC alias batch**: all shipped (M-y/M-u, M-?, M-c quick cd, C-l,
  C-x t / C-x p, S-F4 edit-new via `Editor::create`, S-F5/S-F6 with
  the bare name prefilled), plus legacy F16–F18 codes for the shifted
  F-keys; everything remappable via `[keys]`.

### R4 — depth debt — DONE (2026-07-27, whole menu shipped)

One green commit per item (fmt, clippy -D, units, full e2e both
subshell modes; suite now 167 checks). Notes:

- **Editor depth**: `$1`–`$9` capture groups in replace (expansion only
  trusts captures that re-find the exact highlighted match); mcedit
  F5/F6 (F5 duplicates the block or line and fills the clipboard, F6
  cuts for pasting elsewhere); soft-wrap behind Alt+W — segments reuse
  the horizontal-clipping renderer with a per-segment left edge, so
  tabs/selection/syntax colors came free; wrap-aware viewport walks at
  most a screenful, clicks map through the wrapped rows.
- **C-x c/o/s dialogs**: chmod (octal), chown (`user[:group]`, names
  via getpwnam/getgrnam locally, numeric on sftp), symlink to the
  cursor entry. New `FsWrite::set_owner` verb: `lchown` locally, the
  UIDGID setstat attribute over sftp (missing half backfilled from
  lstat) — so all three work on remote panels.
- **Copy into tar**: full rewrite-append as planned — existing entries
  stream into a temp with the same compression (`append_data` re-fixes
  long names), new trees follow with per-file progress and the usual
  retry/skip, temp renames over. Plain/.gz/.xz/.bz2; zip keeps its
  in-place append. Compressor trailers flushed explicitly (an enum
  sink with `finish`), never trusted to Drop.
- **Quick-view hex** (F4 while the preview is focused), **click-to-sort
  headers** (same toggle as F9 > Sort; layout re-derived from the fixed
  column widths), **Ctrl+Space over sftp/archives** (provider-walking
  twin of the local scan).
- **Job queue** (E3, designed from scratch — deliberately small):
  `jobs: Vec<Job>` with at most one *foreground* job (its dialog is
  modal, exactly the old behavior); `b` detaches it — panels come back,
  the status line shows "N job(s) running — pct%", and new jobs can
  start meanwhile. C-x j / F9 > Command > Jobs lists them: Enter
  foregrounds, c cancels. An overwrite/error question pulls a
  background job back to the front by itself; quitting is refused
  while jobs run. e2e drives it deterministically by copying from a
  FIFO (the job blocks until the test opens the writing end).

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
- **E3 (R4): job queue UI shape** — RESOLVED: one foreground job
  (modal dialog, as before) + any number of detached background jobs,
  `b` to detach, C-x j to list/foreground/cancel, asks auto-foreground
  (see R4 notes).

## What 3.0 refuses to do

FTP, cloud-storage APIs, tabs-as-in-browser, in-terminal image
rendering, LSP-anything in the editor, plugin systems. Windows and Lua
remain parked, not planned.
