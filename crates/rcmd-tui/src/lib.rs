//! The file manager itself, as a library.
//!
//! Everything the `rcmd` binary is made of lives here so that a second
//! front end can be built on it: `rmut-egui` draws the same [`ui::draw`]
//! into a window instead of a terminal, and drives the same [`app::App`]
//! state machine. The binary's own `main.rs` keeps only argument parsing
//! and the terminal's start/stop.

pub mod app;
pub mod config;
pub mod format;
pub mod git;
pub mod keymap;
pub mod mcimport;
pub mod remote;
pub mod state;
pub mod subshell;
pub mod theme;
pub mod ui;
