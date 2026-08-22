# rcmd - a Midnight Commander replacement in Rust

**Status:** ✅ COMPLETED (2026-07-04) - all milestones M0–M5 shipped, plus
the debt list. Kept as a historical record; the follow-up roadmap is
[PLAN2.md](PLAN2.md).

## Retrospective (written on completion)

All six milestones and every tracked debt landed in seven commits on the
day the plan was written: dual-pane navigation (M0), marks + F5–F8 job
engine with MC-style dialogs (M1), command line/Ctrl+O/exit-to-cwd (M2),
F3 chunked viewer + F4 $EDITOR + F9 menu + F1 help (M3), config/keymaps/
quick-search/filter/hotlist/themes (M4), archives as VFS (M5), then
mtime preservation, viewer wrap, xz/bz2, copy-into-zip, and job-control-
safe command execution. ~43 tests, clippy-clean throughout.

What the plan got right: the no-async decision cost nothing through M5
(worker threads + mpsc handled every job, including archive extraction);
the core/TUI split kept all logic unit-testable; virtual line breaks in
the viewer and OsString-everywhere prevented the classic bugs on day one.

What deviated: the FsProvider seam was carved in M5, not "from day one" -
retrofitting it took under an hour precisely because panel logic was
already thin over `read_dir`, so the early-seam advice mattered less than
keeping the call surface small. Copy-INTO-zip (planned as post-M5
stretch) turned out cheap thanks to zip's append mode. The unplanned
winner was the pty test harness: every milestone was verified by driving
the real binary in a pseudo-terminal, which caught keymap and terminal-
protocol issues (Shift+F8 → F20, Ctrl+\ → Ctrl+4) no unit test would.

The one open item is behavioral: the M1 exit criterion - *stop launching
mc for a week* - is decided in daily use, not in CI.

## Vision

A fast, memory-safe, single-binary orthodox file manager that feels like
Midnight Commander on day one: two panels, an F-key action bar, a command
line at the bottom, and keyboard-first operation. Not a reinvention of the
file-manager UI (yazi/broot already explore that space) - a faithful
modernization of the MC workflow.

## Why (and why not just use existing tools)

| Tool | What it is | Why it's not this project |
|------|-----------|---------------------------|
| mc | The original, C, ncurses | Aging codebase, slow releases; the thing being replaced |
| yazi | Async Rust file manager | Miller-columns/ranger paradigm, not orthodox dual-pane |
| broot | Rust tree navigator | Navigation tool, not a two-panel manager |
| joshuto | Rust ranger clone | Again ranger paradigm; single-pane + preview |
| far2l | FAR port for Linux | Closest in spirit, but C++ and a heavier platform layer |

The orthodox dual-pane niche in Rust is genuinely unoccupied.

## Non-goals (at least until 1.0)

- Remote VFS (FTP/SFTP/fish) - big surface, defer; design the VFS trait so it can slot in later
- Mouse-driven workflows beyond basic click-to-focus/select
- Windows support (target Linux first, macOS second; keep unix-only code behind `cfg`)
- Plugin system / scripting
- Replacing `mcedit` as a general-purpose editor - F4 can open `$EDITOR` initially

## Crate stack

| Concern | Crate | Notes |
|---------|-------|-------|
| TUI framework | `ratatui` | De-facto standard, immediate-mode rendering |
| Terminal backend | `crossterm` | Input, raw mode, colors; bundled with ratatui |
| Errors | `anyhow` + `thiserror` | anyhow in app, thiserror in lib layers |
| Config | `serde` + `toml` | `~/.config/rcmd/config.toml` |
| Dir sizes / walking | `jwalk` or hand-rolled | Parallel walk for Ctrl+Space dir size |
| File type detection | `infer` / extension map | For viewer mode + color coding |
| Trash | `trash` | F8 default should be trash, Shift+F8 permanent |
| Archives (later) | `zip`, `tar`, `flate2`, `sevenz-rust` | Behind the VFS trait |
| Syntax highlighting (later) | `syntect` | For F3 viewer; heavy dep, feature-gate it |
| Unix metadata | `nix` / `rustix` | Permissions, ownership, symlink handling |

Deliberately **no async runtime**: file operations run on worker threads with
a `std::sync::mpsc` progress channel back to the UI loop. Tokio buys nothing
for local FS work and complicates the core. Revisit only if remote VFS lands.

## Architecture

```
┌─────────────────────────────────────────────────┐
│ main loop (60fps-ish, event-driven)             │
│   crossterm events ──► update(App, Event)       │
│   App state        ──► view(&App, Frame)        │
└─────────────────────────────────────────────────┘
        │                          ▲
        ▼ job requests             │ progress messages (mpsc)
┌─────────────────────────────────────────────────┐
│ worker pool: copy/move/delete/du jobs           │
│   cancellable, report bytes+files done          │
└─────────────────────────────────────────────────┘
```

- **Elm-ish core:** `App` state struct, `Event -> Message -> update()` mutation,
  pure `view()` render. Keeps the whole UI unit-testable without a terminal.
- **`Panel`** struct ×2: cwd, entry list, cursor, selection set, sort mode,
  filter, history. Active/inactive is just an index into `[Panel; 2]`.
- **`FsProvider` trait** from day one (`read_dir`, `metadata`, `open_read`, …)
  with a single `LocalFs` impl. This is the seam where archive/remote VFS
  plugs in later without touching panel code.
- **Modal overlay stack:** dialogs (copy confirm, mkdir prompt, error box,
  F9 menu) are a `Vec<Box<dyn Modal>>` - top of stack gets input first.
  Exactly how MC feels and trivially composable.
- **Long operations** (copy of a big tree, recursive delete, dir sizing) are
  jobs: spawned on a worker thread, UI shows an MC-style progress dialog with
  cancel. Never block the event loop on I/O bigger than one `read_dir`.

### Crate layout

```
rcmd/
  Cargo.toml            # workspace
  crates/
    rcmd-core/          # panels, jobs, FsProvider, sorting, selection - no TUI deps
    rcmd-tui/           # ratatui views, keymap, dialogs, main loop
  # binary target lives in rcmd-tui; core is pure logic + std
```

Splitting core from TUI keeps logic testable with plain `cargo test` and
leaves the door open for an alternate frontend.

## Milestones

### M0 - walking skeleton (~a weekend)
Two panels rendering a directory listing, Tab to switch, arrows/PgUp/PgDn/
Home/End to move, Enter to descend, standard MC colors, F10/quit. No file
operations yet. Goal: it *feels* like MC when you navigate.

### M1 - the daily-driver cut
- Insert/mark selection, `+`/`-`/`*` glob select
- F5 copy, F6 move/rename, F7 mkdir, F8 delete (to trash) - all with
  MC-style confirm dialogs and progress + cancel
- Sort modes (name/ext/size/mtime, reverse), Ctrl+R reload
- Symlink display, permission/size/mtime columns, hidden-file toggle
- Error dialogs that offer Skip/Retry/Abort like MC does

**Exit criterion: can uninstall nothing, but stop launching `mc` for a week.**

### M2 - command line & shell integration
- Bottom command line: type-to-run in cwd, `cd` handling, Ctrl+O shell
  suspend (swap to a real shell in the panel's cwd, return on exit)
- `%f`/`%d`-style macros optional; at minimum Alt+Enter inserts filename
- Exit-to-cwd support (the `mc-wrapper` trick: write last dir to a file,
  shell function `cd`s there)

### M3 - view & edit
- F3 internal viewer: text with encoding fallback, hex mode toggle,
  search, no full-file load (mmap or chunked)
- F4 opens `$EDITOR` first; internal editor is a separate later decision
- F9 pulldown menu (discoverable UI for everything above)

### M4 - polish & config
- `config.toml` + keymap remapping, MC and "modern" preset keymaps
- Panelize/filter, quick search (Ctrl+S / Alt+S)
- Directory hotlist (Ctrl+\)
- Skin/theme support (MC blue default, plus a truecolor theme)

### M5 - archives as VFS (stretch)
- Enter on .zip/.tar.* descends into it via an `ArchiveFs: FsProvider`
- Copy out of archives; copy *into* is a later stretch

## Key design decisions to make early

1. **Keymap fidelity vs. modernity** - default to MC bindings exactly
   (F-keys, Ins, `*`), offer a second preset later. F-keys are the identity
   of the tool; don't compromise them.
2. **Delete semantics** - F8 → trash, Shift+F8 → permanent. Safer than MC,
   still one keystroke.
3. **Unicode/width handling** - use `unicode-width` for cell math from the
   start; retrofitting is painful. Test with CJK + emoji filenames early.
4. **Non-UTF-8 filenames** - panels must carry `OsString`/`PathBuf`, render
   lossily, never round-trip through `String`. Classic Rust FM bug source.

## Risks

- **Scope creep toward mc parity** - mc is ~30 years of features. Mitigation:
  M1 exit criterion is behavioral ("stop launching mc"), not a checklist.
- **Terminal weirdness** (resize, kitty protocol, tmux) - ratatui/crossterm
  absorb most of it; test in tmux from M0.
- **The internal editor** - biggest single time sink if attempted. Firm
  decision: `$EDITOR` until everything else ships.

## First session when starting `~/git/rcmd`

1. `cargo new` workspace with `rcmd-core` + `rcmd-tui` as above
2. `just` recipes: `run`, `test`, `lint` (clippy + fmt)
3. M0 skeleton: event loop, `Panel` struct, two-pane render, navigation
