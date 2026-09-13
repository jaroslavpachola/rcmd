# Plan

## Pending

- [ ] **Drop local trash fallback after upstream fix:** PR
  [Byron/trash-rs#151](https://github.com/Byron/trash-rs/pull/151) adds
  the home-trash fallback to the crate itself.  When merged and released,
  bump `trash` in Cargo.toml and remove `trash_delete`, `trash_to_home`,
  `home_trash_dir`, `cross_device_move`, `copy_dir_all`,
  `encode_trash_path`, `now_local_iso` and the `OsStr`/`OsStrExt` imports
  from `crates/rcmd-core/src/fsops.rs`; revert `spawn_delete` back to
  calling `trash::delete` directly.

## Done

- [x] **v4.30.3 -- Home-trash fallback for cross-device delete
  (2026-09-13):** F8 on files on mount points whose root is owned by root
  failed with permission denied.  `trash_delete` catches
  `PermissionDenied` and falls back to `~/.local/share/Trash`.
