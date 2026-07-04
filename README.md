# rcmd

A Midnight Commander replacement in Rust: orthodox dual-pane file manager
with MC keybindings, built on ratatui. See [docs/PLAN.md](docs/PLAN.md) for
the roadmap.

## Status

M0 — walking skeleton. Two panels, MC colors, keyboard navigation.
No file operations yet.

## Run

```sh
cargo run -p rcmd-tui     # or: just run
```

## Keys (so far)

| Key | Action |
|-----|--------|
| Tab | Switch panel |
| ↑ ↓ PgUp PgDn Home End | Move cursor |
| Enter | Enter directory |
| Backspace | Parent directory |
| Ctrl+R | Reload panel |
| F10, q | Quit |

## Development

```sh
cargo test --workspace                              # tests (rcmd-core is TUI-free)
cargo clippy --workspace --all-targets -- -D warnings
```

Workspace layout: `crates/rcmd-core` (panel/fs logic, no TUI deps),
`crates/rcmd-tui` (ratatui frontend, binary `rcmd`).
