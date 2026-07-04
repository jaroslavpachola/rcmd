# rcmd

A Midnight Commander replacement in Rust: orthodox dual-pane file manager
with MC keybindings, built on ratatui. See [docs/PLAN.md](docs/PLAN.md) for
the roadmap.

## Status

M1 — daily-driver cut. Marking/selection, copy/move/mkdir/delete with
MC-style dialogs (progress, cancel, overwrite and Skip/Retry/Abort
prompts), sort modes, hidden-file toggle.

## Run

```sh
cargo run -p rcmd-tui     # or: just run
```

## Keys

| Key | Action |
|-----|--------|
| Tab | Switch panel |
| ↑ ↓ PgUp PgDn Home End | Move cursor |
| Enter | Enter directory |
| Backspace | Parent directory |
| Insert, Ctrl+T | Mark entry and advance |
| `+` / `-` (or `\`) | Select / unselect by glob |
| `*` | Invert marks |
| F5 | Copy marked (or cursor) entry |
| F6 | Move / rename |
| F7 | Make directory |
| F8 | Delete to trash |
| Shift+F8 | Delete permanently |
| Alt+N / E / S / T | Sort by name / ext / size / mtime (again = reverse) |
| Alt+. | Toggle hidden files |
| Ctrl+R | Reload panel |
| Esc | Cancel dialog / running operation |
| F10, q | Quit |

In dialogs: arrows/Tab move between buttons, Enter confirms, Esc cancels;
overwrite and error prompts also take hotkeys (o/a/s/S, r/s/S).

## Development

```sh
cargo test --workspace                              # tests (rcmd-core is TUI-free)
cargo clippy --workspace --all-targets -- -D warnings
```

Workspace layout: `crates/rcmd-core` (panel/fs logic, no TUI deps),
`crates/rcmd-tui` (ratatui frontend, binary `rcmd`).
